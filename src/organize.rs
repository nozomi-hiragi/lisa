//! 整理（`lisa_core::organize`）のアプリ側の裏方。
//!
//! - 裏のスレッド（`lisa-organize`）で 2 秒ごとに `plan` の `pending` と履歴を軽く確認し、
//!   変わったときだけ UI へ送る（UI を止めない）。
//! - 起動時に `running` のまま落ちていた計画を再開（`recover_running_plans`）し、期限切れの隔離フォルダを
//!   1 日 1 回 OS のゴミ箱へ送る（`purge_expired`）。
//! - 画面のボタンから頼まれた実行・取り消し・掃除は裏のスレッドで行い、触ったフォルダを読み直すよう
//!   読む係へ `Request::Hint` で渡す。
//! - 「場所を見る」で選んだ計画の対象は `PlanHighlightChains` に載せ、`src/dups.rs` の仕組みで木の上を光らせる。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};

use bevy::prelude::*;
use bevy::window::RequestRedraw;
use bevy::winit::{EventLoopProxy, EventLoopProxyWrapper, WinitUserEvent};
use lisa_core::db::Db;
use lisa_core::organize::{
    self, Context, DEFAULT_TRASH_DAYS, OpInput, OpKind, OpState, Plan, PlanOp, PlanState,
};
use lisa_core::query;

use crate::model::tree::FileTree;
use crate::scan::{ScanControl, Store};
use crate::view::{Location, Nav};

/// `pending` の計画を見に行く間隔。
const POLL_EVERY: Duration = Duration::from_secs(2);
/// 期限切れの隔離フォルダを掃除する間隔（1 日 1 回）。
const PURGE_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
/// 履歴パネルに出す件数の上限。
const HISTORY_LIMIT: usize = 50;
/// 画面の一時的なお知らせを出しておく時間。
pub(crate) const NOTICE_DURATION: Duration = Duration::from_secs(8);

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
    /// 遠隔の残す側がある計画の、残す側の要約。
    pub keep: Option<organize::remote_keep::KeepSummary>,
}

/// 履歴パネルで開いている計画の詳細。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanDetails {
    pub plan_id: i64,
    pub ops: Vec<PlanOp>,
    /// 各操作に対応する `entry.id` の道のり（あれば）。
    pub chains: Vec<Option<Vec<i64>>>,
    pub keep: Option<organize::remote_keep::KeepSummary>,
}

/// 履歴パネルで確認する操作。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HistoryConfirmation {
    Undo(i64),
    PurgePlan(i64),
    PurgeAll,
    Dismiss(i64),
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
    pub dismissible: HashSet<i64>,
    pub dismissing: Option<i64>,
    /// 履歴パネルで展開している計画の ID。
    pub selected_plan: Option<i64>,
    /// 展開している計画の操作一覧。
    pub selected_details: Option<PlanDetails>,
    /// 「場所を見る」で木の上を光らせている計画の ID。
    pub highlighted_plan: Option<i64>,
    /// 右クリックの「切り取り」で選んだ移動元。貼り付けるまで保持する。
    pub cut_source: Option<PathBuf>,
    /// 履歴パネルの確認。
    pub history_confirmation: Option<HistoryConfirmation>,
    /// 裏のスレッドで実行・取り消し・掃除が動いているか。
    pub busy: bool,
    rename_pending: bool,
    pub(crate) linked_delete: Option<LinkedDeletePrompt>,
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

    pub fn cancel_history_confirmation(&mut self, confirmation: HistoryConfirmation) {
        if self.history_confirmation == Some(confirmation) {
            self.history_confirmation = None;
        }
    }

    pub fn confirm_history_action(&mut self, confirmation: HistoryConfirmation) {
        if self.history_confirmation != Some(confirmation) || self.busy || self.dismissing.is_some()
        {
            return;
        }
        self.history_confirmation = None;
        match confirmation {
            HistoryConfirmation::Undo(id) => self.undo(id),
            HistoryConfirmation::PurgePlan(id) => self.purge_plan(id),
            HistoryConfirmation::PurgeAll => self.purge_all(),
            HistoryConfirmation::Dismiss(id) => {
                let sent = self.sender.as_ref().is_some_and(|sender| {
                    sender
                        .lock()
                        .is_ok_and(|sender| sender.send(Action::Dismiss { id }).is_ok())
                });
                if sent {
                    self.dismissing = Some(id);
                } else {
                    self.set_notice(format!("計画 #{id} を履歴から削除できませんでした"));
                }
            }
        }
    }

    fn finish_dismiss(&mut self, id: i64, result: Result<(), String>) {
        self.dismissing = None;
        match result {
            Ok(()) => {
                if self.selected_plan == Some(id) {
                    self.select_plan(None);
                }
                self.history.retain(|plan| plan.id != id);
                self.dismissible.remove(&id);
            }
            Err(error) => self.set_notice(error),
        }
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

    /// 右クリックの「名前の変更…」で確定した名前を適用する。
    pub fn user_rename(&mut self, from: PathBuf, new_name: String) -> Result<(), String> {
        if self.busy || self.rename_pending {
            return Err("整理処理中です。完了までお待ちください".into());
        }
        let sender = self
            .sender
            .as_ref()
            .and_then(|sender| sender.lock().ok())
            .ok_or("今は名前を変更できません")?;
        sender
            .send(Action::UserRename { from, new_name })
            .map_err(|_| "今は名前を変更できません")?;
        self.rename_pending = true;
        self.busy = true;
        Ok(())
    }

    pub fn set_notice(&mut self, message: impl Into<String>) {
        self.notice = Some((message.into(), Instant::now()));
    }

    pub(crate) fn confirm_linked_delete(
        &mut self,
        path: PathBuf,
        decision: organize::DeleteRequest,
    ) {
        self.busy = true;
        self.send(Action::UserTrashLinked { path, decision });
    }

    pub fn current_notice(&self) -> Option<&str> {
        self.notice
            .as_ref()
            .filter(|(_, at)| at.elapsed() < NOTICE_DURATION)
            .map(|(msg, _)| msg.as_str())
    }
}

pub(crate) struct LinkedDeletePrompt {
    pub path: PathBuf,
    pub links: Vec<lisa_core::merged::links::Link>,
}

enum Action {
    Approve {
        id: i64,
        skip_risky: bool,
    },
    Reject {
        id: i64,
    },
    Undo {
        id: i64,
    },
    PurgePlan {
        id: i64,
    },
    PurgeAll,
    LoadDetails {
        id: i64,
    },
    Dismiss {
        id: i64,
    },
    UserTrash {
        path: PathBuf,
    },
    UserTrashLinked {
        path: PathBuf,
        decision: organize::DeleteRequest,
    },
    UserMove {
        from: PathBuf,
        dest_dir: PathBuf,
    },
    UserRename {
        from: PathBuf,
        new_name: String,
    },
}

enum Event {
    Snapshot {
        pending: Vec<PendingPlan>,
        history: Vec<Plan>,
        dismissible: HashSet<i64>,
    },
    Dismissed {
        id: i64,
        result: Result<(), String>,
    },
    Details(PlanDetails),
    LinkedDelete(LinkedDeletePrompt),
    Renamed {
        from: PathBuf,
        to: PathBuf,
    },
    /// 移動した（名前の変更も含む）。プレビューの絵を付け替える。
    Moved {
        from: PathBuf,
        to: PathBuf,
        is_dir: bool,
    },
    Touched(Vec<PathBuf>),
    Notice(String),
    Idle,
    RenameIdle,
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

pub(crate) fn receive_events(
    mut state: ResMut<OrganizeState>,
    mut highlight: ResMut<PlanHighlightChains>,
    control: Option<Res<ScanControl>>,
    (tree, nav, store, mut location): (
        Res<FileTree>,
        Res<Nav>,
        Option<Res<Store>>,
        ResMut<Location>,
    ),
    mut previews: Option<ResMut<crate::preview::PreviewRegistry>>,
    mut dups: ResMut<crate::dups::Dups>,
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
            Event::Snapshot {
                pending,
                history,
                dismissible,
            } => {
                state.pending = pending;
                state.history = history;
                state.dismissible = dismissible;
                // 光らせていた計画がもう pending に無ければ光りを消す。
                if let Some(hl_id) = state.highlighted_plan
                    && !state.pending.iter().any(|p| p.plan.id == hl_id)
                {
                    state.highlighted_plan = None;
                }
            }
            Event::Dismissed { id, result } => state.finish_dismiss(id, result),
            Event::LinkedDelete(prompt) => state.linked_delete = Some(prompt),
            Event::Details(details) => {
                if state.selected_plan == Some(details.plan_id) {
                    state.selected_details = Some(details);
                }
            }
            Event::Moved { from, to, is_dir } => {
                if let Some(previews) = previews.as_deref_mut() {
                    previews.moved(&from, &to, is_dir);
                }
            }
            Event::Renamed { from, to } => {
                // 表示名は NFC に正規化される。再走査では実パスから新しい entry.id を引く。
                if let Some(path) =
                    crate::ui::tooltip::local_item_path(&tree, store.as_deref(), nav.top().node)
                    && let Ok(suffix) = path.strip_prefix(&from)
                {
                    *location = Location::Renamed(if suffix.as_os_str().is_empty() {
                        to
                    } else {
                        to.join(suffix)
                    });
                }
            }
            Event::Touched(folders) => {
                dups.invalidate();
                dups.refresh_after_scan = true;
                if let Some(control) = &control {
                    for folder in folders {
                        control.refresh_after_organize(folder);
                    }
                }
            }
            Event::Notice(msg) => {
                state.set_notice(msg);
            }
            Event::RenameIdle => {
                state.rename_pending = false;
                state.busy = false;
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
type Fingerprint = (Vec<(i64, PlanState, Option<i64>, bool, bool)>, Vec<i64>);

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

    let mut last_transfer = None;
    let mut last_link_sync: Option<Instant> = None;
    let mut link_sync_worker: Option<std::thread::JoinHandle<()>> = None;
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
        match organize::clean_expired_receives(&db) {
            Ok(failures) => {
                for failure in failures {
                    warn!("一時転送の片付けを次回に回します: {failure}");
                }
            }
            Err(error) => warn!("一時転送の掃除に失敗しました: {error}"),
        }
        run_daily_purge(&mut db, &ctx.data_dir, settings.as_deref());
        last_purge = Some(Instant::now());
    }

    // 初回の読み込み。
    if !poll_db(&mut last_fp, watched_detail, &send) {
        return;
    }

    loop {
        if !dummy
            && last_link_sync.is_none_or(|time| time.elapsed() >= Duration::from_secs(30))
            && link_sync_worker
                .as_ref()
                .is_none_or(|worker| worker.is_finished())
        {
            last_link_sync = Some(Instant::now());
            link_sync_worker = std::thread::Builder::new()
                .name("lisa-link-sync".into())
                .spawn(|| {
                    if let Some((mut db, ctx, _)) = open_env()
                        && let Err(error) = organize::flush_links(&mut db, &ctx)
                    {
                        warn!("リンクの同期を続けられませんでした: {error}");
                    }
                })
                .ok();
        }
        match rx.recv_timeout(POLL_EVERY) {
            Ok(Action::Dismiss { id }) => {
                let result = dismiss_history(id);
                if result.is_ok() && watched_detail == Some(id) {
                    watched_detail = None;
                }
                if !send(Event::Dismissed { id, result }) {
                    return;
                }
                if !poll_db(&mut last_fp, watched_detail, &send) {
                    return;
                }
            }
            Ok(action) => {
                let Some((mut db, ctx, settings)) = open_env() else {
                    if !send(Event::Notice("DB がまだありません".into())) {
                        return;
                    }
                    send(if matches!(action, Action::UserRename { .. }) {
                        Event::RenameIdle
                    } else {
                        Event::Idle
                    });
                    continue;
                };
                match action {
                    Action::Dismiss { .. } => unreachable!(),
                    Action::Approve { id, skip_risky } => {
                        match organize::approve_plan_with_context(&mut db, &ctx, id, skip_risky)
                            .and_then(|_| organize::execute_plan(&mut db, &ctx, id))
                        {
                            Ok(report) => {
                                notify_plan_moves(&db, id, OpState::Done, &send);
                                if !report.touched_folders.is_empty() {
                                    send(Event::Touched(report.touched_folders));
                                }
                                send(Event::Notice(format!(
                                    "計画 #{id} を実行しました（完了 {} / 飛ばした {} / 失敗 {}）",
                                    report.done, report.skipped, report.failed
                                )));
                            }
                            Err(err) => {
                                warn!("計画 #{id} を実行できませんでした: {err}");
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
                                notify_plan_moves(&db, id, OpState::Undone, &send);
                                if !report.touched_folders.is_empty() {
                                    send(Event::Touched(report.touched_folders));
                                }
                                let notice = if report.skipped.is_empty() {
                                    format!(
                                        "計画 #{id} を取り消しました（戻した {}）",
                                        report.undone
                                    )
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
                                warn!("計画 #{id} を取り消せませんでした: {err}");
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
                        match organize::linked_delete_sources(&db, &path) {
                            Ok(links) if links.is_empty() => {
                                handle_user_trash(&mut db, &ctx, path, None, &send)
                            }
                            Ok(links) => {
                                send(Event::LinkedDelete(LinkedDeletePrompt { path, links }));
                            }
                            Err(error) => {
                                send(Event::Notice(format!("リンク元を確認できません: {error}")));
                            }
                        }
                        send(Event::Idle);
                    }
                    Action::UserTrashLinked { path, decision } => {
                        handle_user_trash(&mut db, &ctx, path, Some(decision), &send);
                        send(Event::Idle);
                    }
                    Action::UserRename { from, new_name } => {
                        handle_user_rename(&mut db, &ctx, from, &new_name, &send);
                        send(Event::RenameIdle);
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
                    && let Some(path) = crate::scan::db_path().filter(|path| path.is_file())
                    && let Ok(db) = Db::open(&path)
                    && let Ok(progress) = lisa_core::merged::links::transfer_progress(&db)
                    && progress != last_transfer
                {
                    last_transfer = progress;
                    if let Some((received, total)) = progress {
                        send(Event::Notice(format!(
                            "リンクの復元を転送中（{} / {}）",
                            crate::ui::progress::bytes(received),
                            crate::ui::progress::bytes(total)
                        )));
                    }
                }

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

fn handle_user_trash(
    db: &mut Db,
    ctx: &Context,
    path: PathBuf,
    decision: Option<organize::DeleteRequest>,
    send: &impl Fn(Event) -> bool,
) {
    let label = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let note = format!("右クリックから削除: {label}");
    let operation = match decision {
        Some(decision) => OpInput::TrashLinked { path, decision },
        None => OpInput::Trash { path, keep: None },
    };
    match organize::create_plan(db, ctx, "user", Some(&note), &[operation], false) {
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
                        notify_plan_moves(db, id, OpState::Done, send);
                        if !report.touched_folders.is_empty() {
                            send(Event::Touched(report.touched_folders));
                        }
                        let notice = if report.done > 0 {
                            format!("「{label}」を移動しました（計画 #{id}。取り消せます）")
                        } else {
                            let reason = organize::get_plan(db, id)
                                .ok()
                                .flatten()
                                .and_then(|(_, ops)| ops.into_iter().find_map(|op| op.reason))
                                .unwrap_or_else(|| "整理履歴を確認してください".into());
                            format!("「{label}」を移動できませんでした（計画 #{id}）: {reason}")
                        };
                        send(Event::Notice(notice));
                    }
                    Err(err) => {
                        warn!("右クリックからの移動（計画 #{id}）に失敗しました: {err}");
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
            warn!("右クリックからの移動を断られました（{label}）: {reason}");
            send(Event::Notice(format!("移動できません: {reason}")));
        }
        Err(err) => {
            warn!("右クリックからの移動の計画を作れませんでした（{label}）: {err}");
            send(Event::Notice(format!("移動できません: {err}")));
        }
    }
}

/// 計画で動いたものを知らせる（プレビューの付け替え。親が同じなら名前の変更として場所も追う）。
fn notify_plan_moves(db: &Db, id: i64, state: OpState, send: &impl Fn(Event) -> bool) {
    let Ok(ops) = organize::plan_ops(db, id) else {
        return;
    };
    for op in ops {
        if op.op != OpKind::Move || op.state != state {
            continue;
        }
        let Some(dst) = op.dst else {
            continue;
        };
        let renamed = dst.parent() == op.src.parent();
        let (from, to) = if state == OpState::Undone {
            (dst, op.src)
        } else {
            (op.src, dst)
        };
        let is_dir_fs = to.symlink_metadata().is_ok_and(|meta| meta.is_dir());
        let is_dir = is_dir_fs
            || [from.as_path(), to.as_path()].iter().any(|path| {
                lisa_core::query::locate(db, path)
                    .ok()
                    .flatten()
                    .and_then(|found| found.chain.last().copied())
                    .and_then(|id| lisa_core::query::item(db, id).ok().flatten())
                    .is_some_and(|item| item.kind == "dir")
            });
        // ファイルシステムが名前を正規化した場合は、実際に保存された綴りを使う。
        let to = if is_dir_fs {
            to.canonicalize()
                .ok()
                .and_then(|path| path.file_name().map(|name| to.with_file_name(name)))
                .unwrap_or(to)
        } else {
            to
        };
        send(Event::Moved {
            from: from.clone(),
            to: to.clone(),
            is_dir,
        });
        if renamed {
            send(Event::Renamed { from, to });
        }
    }
}

fn handle_user_rename(
    db: &mut Db,
    ctx: &Context,
    from: PathBuf,
    new_name: &str,
    send: &impl Fn(Event) -> bool,
) {
    let label = from.display().to_string();
    let result = (|| {
        let to = organize::rename_destination(&from, new_name)?;
        let old_name = from.file_name().unwrap_or_default().to_string_lossy();
        let note = format!("右クリックから名前変更: {old_name} -> {new_name}");
        let res = organize::create_plan(
            db,
            ctx,
            "user",
            Some(&note),
            &[OpInput::Move { from, to }],
            false,
        )?;
        let id = res.id.ok_or_else(|| {
            res.checks
                .first()
                .map(|c| c.check.clone())
                .unwrap_or_else(|| "断られました".into())
        })?;
        if res.summary.risky > 0 {
            return Ok(format!("名前の変更には確認が必要です（計画 #{id}）"));
        }
        organize::approve_plan(db, id, false)?;
        let report = organize::execute_plan(db, ctx, id)?;
        if report.done != 0 {
            notify_plan_moves(db, id, OpState::Done, send);
        }
        if !report.touched_folders.is_empty() {
            send(Event::Touched(report.touched_folders));
        }
        if report.done == 0 {
            return Err(format!(
                "計画 #{id} の実行に失敗しました。整理履歴を確認してください"
            ));
        }
        Ok(format!(
            "「{new_name}」に名前を変更しました（計画 #{id}。取り消せます）"
        ))
    })();
    let notice = result.unwrap_or_else(|e| {
        warn!("名前を変更できませんでした（{label} -> {new_name}）: {e}");
        format!("名前を変更できません: {e}")
    });
    send(Event::Notice(notice));
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
    let Ok(history) = organize::list_history(&db, HISTORY_LIMIT) else {
        return true;
    };
    let Ok(mut pending_rows) = organize::list_plans(&db, Some(PlanState::Pending), HISTORY_LIMIT)
    else {
        return true;
    };
    // 複数あれば古い順に 1 つずつ出す。
    pending_rows.reverse();

    let data_dir = db_path.parent().expect("DB の親フォルダ");
    let dismissible: HashSet<_> = history
        .iter()
        .filter(|plan| organize::can_dismiss_history(&db, data_dir, plan))
        .map(|plan| plan.id)
        .collect();

    let fp: Fingerprint = (
        history
            .iter()
            .map(|p| {
                (
                    p.id,
                    p.state,
                    p.purged_at,
                    p.restore_requested,
                    dismissible.contains(&p.id),
                )
            })
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
        let keep = organize::remote_keep::keep_summary(&db, plan.id);
        pending.push(PendingPlan {
            plan,
            ops,
            chains,
            keep,
        });
    }

    if !send(Event::Snapshot {
        pending,
        history,
        dismissible,
    }) {
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
        keep: organize::remote_keep::keep_summary(db, plan_id),
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

fn dismiss_history(id: i64) -> Result<(), String> {
    let db_path = crate::scan::db_path()
        .filter(|path| path.is_file())
        .ok_or_else(|| format!("計画 #{id} を履歴から削除できませんでした: DB がありません"))?;
    let db = Db::open(&db_path)
        .map_err(|e| format!("計画 #{id} を履歴から削除できませんでした: {e}"))?;
    let data_dir = db_path.parent().expect("DB の親フォルダ");
    organize::dismiss_history(&db, data_dir, id)
}

fn open_env() -> Option<(Db, Context, Option<PathBuf>)> {
    let data_dir = crate::scan::data_dir()?;
    let db_path = data_dir.join("lisa.db");
    if !db_path.exists() {
        return None;
    }
    let db = Db::open(&db_path).ok()?;
    let settings = crate::settings::settings_path();
    let mut ctx = Context::load(data_dir, settings.as_deref());
    ctx.keep_client = Some(lisa_mcp::relay_client);
    ctx.link_client = Some(lisa_mcp::link_relay_client);
    ctx.relocation_client = Some(lisa_mcp::relocation_client);
    Some((db, ctx, settings))
}

#[cfg(test)]
mod rename_tests {
    use super::*;

    #[test]
    fn only_one_rename_can_be_queued_until_it_finishes() {
        let mut state = OrganizeState::default();
        let path = PathBuf::from("before.txt");
        assert!(state.user_rename(path.clone(), "after.txt".into()).is_err());
        assert!(!state.busy);

        let (tx, rx) = channel();
        state.sender = Some(Mutex::new(tx));
        assert!(state.user_rename(path.clone(), "after.txt".into()).is_ok());
        assert!(matches!(rx.try_recv(), Ok(Action::UserRename { .. })));
        state.busy = false; // 別件の Idle が先に届いても名前変更はまだ完了していない。
        assert!(state.user_rename(path, "another.txt".into()).is_err());
        assert!(rx.try_recv().is_err());
    }
}

#[cfg(test)]
mod history_tests {
    use super::*;

    #[test]
    fn history_confirmation_cancel_and_repeated_click_send_at_most_one_action() {
        let (tx, rx) = channel();
        let confirmation = HistoryConfirmation::Dismiss(42);
        let mut state = OrganizeState {
            selected_plan: Some(42),
            history_confirmation: Some(confirmation),
            sender: Some(Mutex::new(tx)),
            ..default()
        };
        state.cancel_history_confirmation(confirmation);
        state.confirm_history_action(confirmation);
        assert_eq!(state.selected_plan, Some(42));
        assert!(rx.try_recv().is_err());

        state.history_confirmation = Some(confirmation);
        state.confirm_history_action(confirmation);
        state.confirm_history_action(confirmation);
        assert!(matches!(rx.try_recv(), Ok(Action::Dismiss { id: 42 })));
        assert!(rx.try_recv().is_err());
        assert_eq!(state.dismissing, Some(42));

        state.finish_dismiss(
            42,
            Err("計画 #42 を履歴から削除できませんでした: database is locked".into()),
        );
        assert_eq!(state.selected_plan, Some(42));
        assert!(
            state
                .current_notice()
                .unwrap()
                .contains("database is locked")
        );
        assert_eq!(state.dismissing, None);

        state.history_confirmation = Some(confirmation);
        state.confirm_history_action(confirmation);
        assert!(matches!(rx.try_recv(), Ok(Action::Dismiss { id: 42 })));
        state.finish_dismiss(42, Ok(()));
        assert_eq!(state.selected_plan, None);
        assert_eq!(state.dismissing, None);
    }
}
