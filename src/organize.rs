//! 整理（`lisa_core::organize`）のアプリ側の裏方。
//!
//! - 裏のスレッド（`lisa-organize`）で 2 秒ごとに `plan` の `pending` と履歴を軽く確認し、
//!   変わったときだけ UI へ送る（UI を止めない）。
//! - 起動時に `running` のまま落ちていた計画を再開（`recover_running_plans`）し、期限切れの隔離フォルダを
//!   1 日 1 回 OS のゴミ箱へ送る（`purge_expired`）。
//! - 画面のボタンから頼まれた実行・取り消し・掃除は裏のスレッドで行い、触ったフォルダを読み直すよう
//!   読む係へ `Request::Hint` で渡す。
//! - 「場所を見る」で選んだ計画の対象は `PlanHighlightChains` に載せ、`src/dups.rs` の仕組みで木の上を光らせる。

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};

use bevy::prelude::*;
use bevy::window::RequestRedraw;
use bevy::winit::{EventLoopProxy, EventLoopProxyWrapper, WinitUserEvent};
use lisa_core::db::Db;
use lisa_core::organize::{self, Context, DEFAULT_TRASH_DAYS, OpInput, Plan, PlanOp, PlanState};
use lisa_core::query;

use crate::scan::ScanControl;

/// `pending` の計画を見に行く間隔。
const POLL_EVERY: Duration = Duration::from_secs(2);
/// 期限切れの隔離フォルダを掃除する間隔（1 日 1 回）。
const PURGE_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
/// 履歴パネルに出す件数の上限。
const HISTORY_LIMIT: usize = 50;
/// 画面の一時的なお知らせを出しておく時間。
const NOTICE_DURATION: Duration = Duration::from_secs(8);

pub struct OrganizePlugin;

impl Plugin for OrganizePlugin {
    fn build(&self, app: &mut App) {
        let mut state = OrganizeState::default();
        if std::env::var_os("LISA_ORGANIZE_OPEN").is_some_and(|v| !v.is_empty()) {
            state.panel_open = true;
        }
        app.insert_resource(state)
            .init_resource::<PlanHighlightChains>()
            .add_systems(Startup, start_worker)
            .add_systems(Update, receive_events);
    }
}

/// 木の上で光らせる対象（ドライブの一番上からの `entry.id` の列）。`src/dups.rs` が参照する。
#[derive(Resource, Default, PartialEq, Eq)]
pub struct PlanHighlightChains(pub Vec<Vec<i64>>);

/// 許可待ちの計画 1 件（ポップアップ表示用。操作一覧と、木の上で光らせる道のりを含む）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingPlan {
    pub plan: Plan,
    pub ops: Vec<PlanOp>,
    /// 対象の `entry.id` の道のり（「場所を見る」用）。
    pub chains: Vec<Vec<i64>>,
}

/// 履歴パネルで開いている計画の詳細。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanDetails {
    pub plan_id: i64,
    pub ops: Vec<PlanOp>,
    /// 各操作に対応する `entry.id` の道のり（あれば）。
    pub chains: Vec<Option<Vec<i64>>>,
}

/// OS のゴミ箱へ送る前の確認内容。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConfirmPurge {
    Plan(i64),
    All,
}

/// UI と裏のスレッドの橋渡しをする状態。
#[derive(Resource, Default)]
pub struct OrganizeState {
    /// 右の履歴パネルを開いているか。
    pub panel_open: bool,
    /// 許可待ちの計画（古い順）。先頭の 1 件をポップアップに出す。
    pub pending: Vec<PendingPlan>,
    /// 計画の履歴（新しい順）。
    pub history: Vec<Plan>,
    /// 履歴パネルで展開している計画の ID。
    pub selected_plan: Option<i64>,
    /// 展開している計画の操作一覧。
    pub selected_details: Option<PlanDetails>,
    /// 「場所を見る」で木の上を光らせている計画の ID。
    pub highlighted_plan: Option<i64>,
    /// 右クリックの「切り取り」で選んだ移動元。貼り付けるまで保持する。
    pub cut_source: Option<PathBuf>,
    /// OS のゴミ箱へ送る前の確認。
    pub confirming_purge: Option<ConfirmPurge>,
    /// 裏のスレッドで実行・取り消し・掃除が動いているか。
    pub busy: bool,
    /// 一時的なお知らせ（結果やエラー）。
    pub notice: Option<(String, Instant)>,
    sender: Option<Mutex<Sender<Action>>>,
    receiver: Option<Mutex<Receiver<Event>>>,
}

impl OrganizeState {
    fn send(&self, action: Action) {
        if let Some(sender) = &self.sender
            && let Ok(sender) = sender.lock()
        {
            let _ = sender.send(action);
        }
    }

    pub fn approve(&mut self, id: i64, skip_risky: bool) {
        self.busy = true;
        if self.highlighted_plan == Some(id) {
            self.highlighted_plan = None;
        }
        self.send(Action::Approve { id, skip_risky });
    }

    pub fn reject(&mut self, id: i64) {
        if self.highlighted_plan == Some(id) {
            self.highlighted_plan = None;
        }
        self.send(Action::Reject { id });
    }

    pub fn undo(&mut self, id: i64) {
        self.busy = true;
        self.send(Action::Undo { id });
    }

    pub fn purge_plan(&mut self, id: i64) {
        self.busy = true;
        self.send(Action::PurgePlan { id });
    }

    pub fn purge_all(&mut self) {
        self.busy = true;
        self.send(Action::PurgeAll);
    }

    pub fn select_plan(&mut self, id: Option<i64>) {
        self.selected_plan = id;
        if let Some(id) = id {
            self.send(Action::LoadDetails { id });
        } else {
            self.selected_details = None;
        }
    }

    /// 右クリックの「削除…」。1 件の trash 計画を `by = "user"` で作り、危なければポップアップ、安全ならそのまま実行。
    pub fn user_trash(&mut self, path: PathBuf) {
        self.busy = true;
        self.send(Action::UserTrash { path });
    }

    /// 右クリックの「ここに貼り付け」で移動を実行したとき。1 件の move 計画を `by = "user"` で作る。
    pub fn user_move(&mut self, from: PathBuf, dest_dir: PathBuf) {
        self.busy = true;
        self.send(Action::UserMove { from, dest_dir });
    }

    pub fn set_notice(&mut self, message: impl Into<String>) {
        self.notice = Some((message.into(), Instant::now()));
    }

    pub fn current_notice(&self) -> Option<&str> {
        self.notice
            .as_ref()
            .filter(|(_, at)| at.elapsed() < NOTICE_DURATION)
            .map(|(msg, _)| msg.as_str())
    }
}

enum Action {
    Approve { id: i64, skip_risky: bool },
    Reject { id: i64 },
    Undo { id: i64 },
    PurgePlan { id: i64 },
    PurgeAll,
    LoadDetails { id: i64 },
    UserTrash { path: PathBuf },
    UserMove { from: PathBuf, dest_dir: PathBuf },
}

enum Event {
    Snapshot {
        pending: Vec<PendingPlan>,
        history: Vec<Plan>,
    },
    Details(PlanDetails),
    Touched(Vec<PathBuf>),
    Notice(String),
    Idle,
}

fn start_worker(mut state: ResMut<OrganizeState>, proxy: Res<EventLoopProxyWrapper>) {
    let (action_tx, action_rx) = channel();
    let (event_tx, event_rx) = channel();
    state.sender = Some(Mutex::new(action_tx));
    state.receiver = Some(Mutex::new(event_rx));

    let proxy = (**proxy).clone();
    let dummy = std::env::var_os("LISA_DUMMY").is_some();
    let spawned = std::thread::Builder::new()
        .name("lisa-organize".into())
        .spawn(move || run_worker(action_rx, event_tx, proxy, dummy));
    if let Err(error) = spawned {
        warn!("整理のスレッドを立てられませんでした: {error}");
    }
}

fn receive_events(
    mut state: ResMut<OrganizeState>,
    mut highlight: ResMut<PlanHighlightChains>,
    control: Option<Res<ScanControl>>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    let events: Vec<Event> = match &state.receiver {
        Some(rx) => match rx.lock() {
            Ok(rx) => rx.try_iter().collect(),
            Err(_) => Vec::new(),
        },
        None => Vec::new(),
    };

    let mut changed = false;
    for event in events {
        changed = true;
        match event {
            Event::Snapshot { pending, history } => {
                state.pending = pending;
                state.history = history;
                // 光らせていた計画がもう pending に無ければ光りを消す。
                if let Some(hl_id) = state.highlighted_plan
                    && !state.pending.iter().any(|p| p.plan.id == hl_id)
                {
                    state.highlighted_plan = None;
                }
            }
            Event::Details(details) => {
                if state.selected_plan == Some(details.plan_id) {
                    state.selected_details = Some(details);
                }
            }
            Event::Touched(folders) => {
                if let Some(control) = &control {
                    for folder in folders {
                        control.visible(folder.clone());
                        control.hint(folder);
                    }
                }
            }
            Event::Notice(msg) => {
                state.set_notice(msg);
            }
            Event::Idle => {
                state.busy = false;
            }
        }
    }

    // 時間切れのお知らせを消す。
    if state
        .notice
        .as_ref()
        .is_some_and(|(_, at)| at.elapsed() >= NOTICE_DURATION)
    {
        state.notice = None;
        changed = true;
    }

    // 「場所を見る」のハイライトを PlanHighlightChains に反映する。
    let desired_chains: Vec<Vec<i64>> = state
        .highlighted_plan
        .and_then(|id| state.pending.iter().find(|p| p.plan.id == id))
        .map(|p| p.chains.clone())
        .unwrap_or_default();
    if highlight.0 != desired_chains {
        highlight.0 = desired_chains;
        changed = true;
    }

    if changed {
        redraw.write(RequestRedraw);
    }
}

/// 変化検知用の軽い指紋（変化が無いときは UI へ送らない）。
type Fingerprint = (Vec<(i64, PlanState, Option<i64>)>, Vec<i64>);

fn run_worker(
    rx: Receiver<Action>,
    tx: Sender<Event>,
    proxy: EventLoopProxy<WinitUserEvent>,
    dummy: bool,
) {
    let send = |event: Event| -> bool {
        let ok = tx.send(event).is_ok();
        let _ = proxy.send_event(WinitUserEvent::WakeUp);
        ok
    };

    let mut last_fp: Option<Fingerprint> = None;
    let mut last_purge: Option<Instant> = None;
    let mut watched_detail: Option<i64> = None;

    // 起動時: 途中で落ちた計画の再開と、期限切れの掃除（ダミーモードでは触らない）。
    if !dummy && let Some((mut db, ctx, settings)) = open_env() {
        match organize::recover_running_plans(&mut db, &ctx) {
            Ok(reports) => {
                for report in reports {
                    info!(
                        "中断していた整理の計画 #{} を再開しました（状態: {}、完了 {}）",
                        report.plan_id,
                        report.state.as_str(),
                        report.done
                    );
                    if !report.touched_folders.is_empty() {
                        send(Event::Touched(report.touched_folders));
                    }
                }
            }
            Err(error) => warn!("中断していた計画の再開に失敗しました: {error}"),
        }
        run_daily_purge(&mut db, &ctx.data_dir, settings.as_deref());
        last_purge = Some(Instant::now());
    }

    // 初回の読み込み。
    if !poll_db(&mut last_fp, watched_detail, &send) {
        return;
    }

    loop {
        match rx.recv_timeout(POLL_EVERY) {
            Ok(action) => {
                let Some((mut db, ctx, settings)) = open_env() else {
                    if !send(Event::Notice("DB がまだありません".into())) {
                        return;
                    }
                    send(Event::Idle);
                    continue;
                };
                match action {
                    Action::Approve { id, skip_risky } => {
                        match organize::approve_plan(&mut db, id, skip_risky)
                            .and_then(|_| organize::execute_plan(&mut db, &ctx, id))
                        {
                            Ok(report) => {
                                if !report.touched_folders.is_empty() {
                                    send(Event::Touched(report.touched_folders));
                                }
                                send(Event::Notice(format!(
                                    "計画 #{id} を実行しました（完了 {} / 飛ばした {} / 失敗 {}）",
                                    report.done, report.skipped, report.failed
                                )));
                            }
                            Err(err) => {
                                send(Event::Notice(format!(
                                    "計画 #{id} を実行できませんでした: {err}"
                                )));
                            }
                        }
                        send(Event::Idle);
                        last_fp = None;
                    }
                    Action::Reject { id } => {
                        match organize::reject_plan(&mut db, id) {
                            Ok(_) => send(Event::Notice(format!("計画 #{id} を断りました"))),
                            Err(err) => send(Event::Notice(format!(
                                "計画 #{id} を断れませんでした: {err}"
                            ))),
                        };
                        last_fp = None;
                    }
                    Action::Undo { id } => {
                        match organize::undo_plan(&mut db, &ctx, id) {
                            Ok(report) => {
                                if !report.touched_folders.is_empty() {
                                    send(Event::Touched(report.touched_folders));
                                }
                                let notice = if report.skipped.is_empty() {
                                    format!("計画 #{id} を取り消しました（戻した {}）", report.undone)
                                } else {
                                    format!(
                                        "計画 #{id} の取り消し結果（戻した {} / 飛ばした {}: {}）",
                                        report.undone,
                                        report.skipped.len(),
                                        report.skipped[0].reason
                                    )
                                };
                                send(Event::Notice(notice));
                            }
                            Err(err) => {
                                send(Event::Notice(format!(
                                    "計画 #{id} を取り消せませんでした: {err}"
                                )));
                            }
                        }
                        send(Event::Idle);
                        last_fp = None;
                    }
                    Action::PurgePlan { id } => {
                        match organize::purge_plan(&mut db, &ctx.data_dir, id) {
                            Ok(report) => {
                                let msg = if report.errors.is_empty() {
                                    format!(
                                        "計画 #{id} の隔離フォルダを OS のゴミ箱へ送りました（{} 個）",
                                        report.trashed_dirs.len()
                                    )
                                } else {
                                    format!(
                                        "計画 #{id} の掃除で一部失敗しました: {}",
                                        report.errors[0].1
                                    )
                                };
                                send(Event::Notice(msg));
                            }
                            Err(err) => {
                                send(Event::Notice(format!(
                                    "計画 #{id} を空にできませんでした: {err}"
                                )));
                            }
                        }
                        send(Event::Idle);
                        last_fp = None;
                    }
                    Action::PurgeAll => {
                        match organize::purge_expired(&mut db, &ctx.data_dir, 0) {
                            Ok(report) => {
                                send(Event::Notice(format!(
                                    "隔離フォルダを OS のゴミ箱へ送りました（計画 {} 件、フォルダ {} 個）",
                                    report.purged_plans.len(),
                                    report.trashed_dirs.len()
                                )));
                            }
                            Err(err) => {
                                send(Event::Notice(format!(
                                    "隔離フォルダを空にできませんでした: {err}"
                                )));
                            }
                        }
                        send(Event::Idle);
                        last_fp = None;
                    }
                    Action::LoadDetails { id } => {
                        watched_detail = Some(id);
                        if let Some(details) = load_details(&db, id) {
                            send(Event::Details(details));
                        }
                        continue;
                    }
                    Action::UserTrash { path } => {
                        handle_user_trash(&mut db, &ctx, path, &send);
                        send(Event::Idle);
                        last_fp = None;
                    }
                    Action::UserMove { from, dest_dir } => {
                        handle_user_move(&mut db, &ctx, from, dest_dir, &send);
                        send(Event::Idle);
                        last_fp = None;
                    }
                }
                let _ = settings;
                if !poll_db(&mut last_fp, watched_detail, &send) {
                    return;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if !dummy
                    && last_purge.is_none_or(|t| t.elapsed() >= PURGE_EVERY)
                    && let Some((mut db, ctx, settings)) = open_env()
                {
                    run_daily_purge(&mut db, &ctx.data_dir, settings.as_deref());
                    last_purge = Some(Instant::now());
                    last_fp = None;
                }
                if !poll_db(&mut last_fp, watched_detail, &send) {
                    return;
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn handle_user_trash(db: &mut Db, ctx: &Context, path: PathBuf, send: &impl Fn(Event) -> bool) {
    let label = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let note = format!("右クリックから削除: {label}");
    match organize::create_plan(
        db,
        ctx,
        "user",
        Some(&note),
        &[OpInput::Trash { path, keep: None }],
        false,
    ) {
        Ok(res) if res.created => {
            let id = res.id.expect("created plan has id");
            if res.summary.risky > 0 {
                // 危ない（最後の 1 つ・data・ハッシュ無し）のときだけ確認ポップアップを出す。
                send(Event::Notice(format!(
                    "「{label}」の削除は確認が必要です（ポップアップで許可してください）"
                )));
            } else {
                match organize::approve_plan(db, id, false)
                    .and_then(|_| organize::execute_plan(db, ctx, id))
                {
                    Ok(report) => {
                        if !report.touched_folders.is_empty() {
                            send(Event::Touched(report.touched_folders));
                        }
                        send(Event::Notice(format!(
                            "「{label}」を隔離フォルダへ移しました（計画 #{id}。取り消せます）"
                        )));
                    }
                    Err(err) => {
                        send(Event::Notice(format!("削除できませんでした: {err}")));
                    }
                }
            }
        }
        Ok(res) => {
            let reason = res
                .checks
                .first()
                .map(|c| c.check.as_str())
                .unwrap_or("断られました");
            send(Event::Notice(format!("削除できません: {reason}")));
        }
        Err(err) => {
            send(Event::Notice(format!("削除できません: {err}")));
        }
    }
}

fn handle_user_move(
    db: &mut Db,
    ctx: &Context,
    from: PathBuf,
    dest_dir: PathBuf,
    send: &impl Fn(Event) -> bool,
) {
    let Some(file_name) = from.file_name() else {
        send(Event::Notice("移動する対象の名前が分かりません".into()));
        return;
    };
    let label = file_name.to_string_lossy().into_owned();
    let to = dest_dir.join(file_name);
    let note = format!("右クリックから移動: {label}");
    match organize::create_plan(
        db,
        ctx,
        "user",
        Some(&note),
        &[OpInput::Move { from, to }],
        false,
    ) {
        Ok(res) if res.created => {
            let id = res.id.expect("created plan has id");
            if res.summary.risky > 0 {
                // data フォルダごとの移動など危ないときは確認ポップアップを出す。
                send(Event::Notice(format!(
                    "「{label}」の移動は確認が必要です（ポップアップで許可してください）"
                )));
            } else {
                match organize::approve_plan(db, id, false)
                    .and_then(|_| organize::execute_plan(db, ctx, id))
                {
                    Ok(report) => {
                        if !report.touched_folders.is_empty() {
                            send(Event::Touched(report.touched_folders));
                        }
                        send(Event::Notice(format!(
                            "「{label}」を移動しました（計画 #{id}。取り消せます）"
                        )));
                    }
                    Err(err) => {
                        send(Event::Notice(format!("移動できませんでした: {err}")));
                    }
                }
            }
        }
        Ok(res) => {
            let reason = res
                .checks
                .first()
                .map(|c| c.check.as_str())
                .unwrap_or("断られました");
            send(Event::Notice(format!("移動できません: {reason}")));
        }
        Err(err) => {
            send(Event::Notice(format!("移動できません: {err}")));
        }
    }
}

fn run_daily_purge(db: &mut Db, data_dir: &Path, settings_path: Option<&Path>) {
    let days = trash_days_from_settings(settings_path);
    match organize::purge_expired(db, data_dir, days) {
        Ok(report) if !report.purged_plans.is_empty() => {
            info!(
                "期限切れ（{days} 日超）の隔離フォルダを OS のゴミ箱へ送りました（計画 {} 件、フォルダ {} 個）",
                report.purged_plans.len(),
                report.trashed_dirs.len()
            );
        }
        Ok(_) => {}
        Err(error) => warn!("期限切れの隔離フォルダの掃除に失敗しました: {error}"),
    }
}

fn trash_days_from_settings(settings_path: Option<&Path>) -> u64 {
    let Some(path) = settings_path else {
        return DEFAULT_TRASH_DAYS;
    };
    let Ok(bytes) = std::fs::read(path) else {
        return DEFAULT_TRASH_DAYS;
    };
    serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|v| v.get("trash_days").and_then(|d| d.as_u64()))
        .unwrap_or(DEFAULT_TRASH_DAYS)
}

fn poll_db(
    last_fp: &mut Option<Fingerprint>,
    watched_detail: Option<i64>,
    send: &impl Fn(Event) -> bool,
) -> bool {
    let Some(db_path) = crate::scan::db_path().filter(|p| p.exists()) else {
        return true;
    };
    let Ok(mut db) = Db::open(&db_path) else {
        return true;
    };
    let _ = organize::expire_stale_plans(&mut db);
    let Ok(history) = organize::list_plans(&db, None, HISTORY_LIMIT) else {
        return true;
    };
    let Ok(mut pending_rows) = organize::list_plans(&db, Some(PlanState::Pending), HISTORY_LIMIT)
    else {
        return true;
    };
    // 複数あれば古い順に 1 つずつ出す。
    pending_rows.reverse();

    let fp: Fingerprint = (
        history
            .iter()
            .map(|p| (p.id, p.state, p.purged_at))
            .collect(),
        pending_rows.iter().map(|p| p.id).collect(),
    );
    if last_fp.as_ref() == Some(&fp) {
        return true;
    }
    *last_fp = Some(fp);

    let mut pending = Vec::with_capacity(pending_rows.len());
    for plan in pending_rows {
        let ops = organize::plan_ops(&db, plan.id).unwrap_or_default();
        let chains = resolve_op_chains(&db, &ops);
        pending.push(PendingPlan { plan, ops, chains });
    }

    if !send(Event::Snapshot { pending, history }) {
        return false;
    }
    if let Some(id) = watched_detail
        && let Some(details) = load_details(&db, id)
        && !send(Event::Details(details))
    {
        return false;
    }
    true
}

fn load_details(db: &Db, plan_id: i64) -> Option<PlanDetails> {
    let (_, ops) = organize::get_plan(db, plan_id).ok()??;
    let chains = ops
        .iter()
        .map(|op| query::locate(db, &op.src).ok().flatten().map(|l| l.chain))
        .collect();
    Some(PlanDetails {
        plan_id,
        ops,
        chains,
    })
}

fn resolve_op_chains(db: &Db, ops: &[PlanOp]) -> Vec<Vec<i64>> {
    let mut chains = Vec::new();
    for op in ops {
        if let Ok(Some(loc)) = query::locate(db, &op.src) {
            chains.push(loc.chain);
        }
    }
    chains
}

fn open_env() -> Option<(Db, Context, Option<PathBuf>)> {
    let data_dir = crate::scan::data_dir()?;
    let db_path = data_dir.join("lisa.db");
    if !db_path.exists() {
        return None;
    }
    let db = Db::open(&db_path).ok()?;
    let settings = crate::settings::settings_path();
    let ctx = Context::load(data_dir, settings.as_deref());
    Some((db, ctx, settings))
}
