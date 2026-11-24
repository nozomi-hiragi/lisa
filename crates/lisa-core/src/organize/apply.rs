//! 整理の計画の実行、落ちたあとの再開、取り消し、触ったフォルダの収集。
//!
//! - 同時に動く計画は 1 つだけ（プロセス内の Mutex と `<data_dir>/organize.lock` の OS ロック）。
//! - 実行の順は `mkdir` → `move` → `trash`（それぞれ計画の `seq` 順）。
//! - 1 件ごとに `running` を DB に書いて確定してから動かし、済んだら `done`（または `skipped` / `failed`）にする。
//! - `entry` の表には直接書かない。触ったフォルダの一覧を返すので、アプリは `Request::Hint`、CLI / MCP は `Command::Rescan` で読み直させる。

use std::collections::BTreeSet;
use std::fs::{self, File, TryLockError};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::db::{self, Db};
use crate::hash;
use crate::listing::{self, Kind};
use crate::query::{self, Drive};

use super::Context;
use super::check;
use super::plan::{self, OpKind, OpState, PlanOp, PlanState};
use super::quarantine;

/// 同じプロセスで始まった計画は先行する計画の完了を待つ。
static RUN_LOCK: Mutex<BTreeSet<PathBuf>> = Mutex::new(BTreeSet::new());
static RUN_READY: Condvar = Condvar::new();

struct ProcessLock(PathBuf);

impl Drop for ProcessLock {
    fn drop(&mut self) {
        RUN_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&self.0);
        RUN_READY.notify_all();
    }
}

fn process_lock(data_dir: &Path, wait: bool) -> Result<ProcessLock, String> {
    fs::create_dir_all(data_dir).map_err(|error| error.to_string())?;
    let path = data_dir.canonicalize().map_err(|error| error.to_string())?;
    let mut running = RUN_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    while running.contains(&path) {
        if !wait {
            return Err("別の整理が実行中です。重複を再取得して確認してください".into());
        }
        running = RUN_READY
            .wait(running)
            .unwrap_or_else(|error| error.into_inner());
    }
    running.insert(path.clone());
    Ok(ProcessLock(path))
}

/// ロックファイルの名前（`<data_dir>/organize.lock`）。
const LOCK_FILE: &str = "organize.lock";

/// 他の記述子が一瞬だけロックを持っている場合に取り直す長さ。
const LOCK_RETRY: std::time::Duration = std::time::Duration::from_millis(1500);

/// 実行の結果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyReport {
    pub plan_id: i64,
    pub state: PlanState,
    pub done: usize,
    pub skipped: usize,
    pub failed: usize,
    /// 実行で触ったフォルダ（元の親と行き先の親）。アプリが `Request::Hint` に使う。
    pub touched_folders: Vec<PathBuf>,
}

/// 取り消しで飛ばした 1 件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndoSkipped {
    pub seq: i64,
    pub src: PathBuf,
    pub reason: String,
}

/// 取り消しの結果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndoReport {
    pub plan_id: i64,
    pub state: PlanState,
    pub undone: usize,
    pub skipped: Vec<UndoSkipped>,
    /// 取り消しで触ったフォルダ。
    pub touched_folders: Vec<PathBuf>,
}

pub(super) struct PlanLock {
    // プロセス内の次の操作を起こす前に、OS ロックを解放する。
    _file: File,
    _process: ProcessLock,
}

/// ファイルを閉じるかプロセスが終了すると OS が解放する。
/// 待っているプロセスが別の inode を開かないよう、ロックファイルは残す。
pub(super) fn lock_plans(data_dir: &Path, plan_id: i64) -> Result<PlanLock, String> {
    lock_plans_with(data_dir, plan_id, process_lock(data_dir, true)?)
}

pub(super) fn try_lock_plans(data_dir: &Path) -> Result<PlanLock, String> {
    lock_plans_with(data_dir, 0, process_lock(data_dir, false)?)
}

fn lock_plans_with(
    data_dir: &Path,
    plan_id: i64,
    process: ProcessLock,
) -> Result<PlanLock, String> {
    let open_error = |error| format!("整理のロックを開けません: {error}");
    fs::create_dir_all(data_dir).map_err(open_error)?;
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(data_dir.join(LOCK_FILE))
        .map_err(open_error)?;
    // 同じプロセスの別スレッドが子プロセスを起こした直後などに、ロックが数ミリ秒だけ他の記述子に
    // 持たれていることがある。別の計画が本当に動いているかは、少し待って取り直してから判断する。
    let started = std::time::Instant::now();
    let mut result = file.try_lock();
    while matches!(result, Err(TryLockError::WouldBlock)) && started.elapsed() < LOCK_RETRY {
        std::thread::sleep(std::time::Duration::from_millis(10));
        result = file.try_lock();
    }
    match result {
        Ok(()) => {
            #[cfg(test)]
            plan_start_hook::HOOK.with(|hook| {
                if let Some(run) = hook.take() {
                    run();
                }
            });
            Ok(PlanLock {
                _file: file,
                _process: process,
            })
        }
        Err(TryLockError::WouldBlock) => Err(format!(
            "計画 #{plan_id} を開始できません。整理計画の実行中または取り消し中です"
        )),
        Err(TryLockError::Error(error)) => Err(format!("整理のロックを取れません: {error}")),
    }
}

/// 途中で落ちて `running` のまま残っている計画をすべて見つけ、続きを実行する。
pub fn recover_running_plans(db: &mut Db, ctx: &Context) -> Result<Vec<ApplyReport>, String> {
    let running = plan::list_plans(db, Some(PlanState::Running), 100).map_err(|e| e.to_string())?;
    let mut reports = Vec::new();
    // 古いものから順に再開する。
    for p in running.into_iter().rev() {
        reports.push(execute_plan(db, ctx, p.id)?);
    }
    Ok(reports)
}

/// 許可済み（`approved`）または途中で落ちた（`running`）計画を順に実行する。
pub fn execute_plan(db: &mut Db, ctx: &Context, plan_id: i64) -> Result<ApplyReport, String> {
    use super::remote_keep::{self, relay::RelayAction};
    let remote = remote_keep::remote_plan(db, plan_id)?;
    let _attempt = remote
        .as_ref()
        .map(|_| remote_keep::lock_attempt(&ctx.data_dir, plan_id))
        .transpose()?;
    let mut recovering_remote = false;
    if remote.is_some() {
        let plan = plan::load_plan_required(db, plan_id)?;
        if !matches!(plan.state, PlanState::Approved | PlanState::Running) {
            return Err("この端末間計画は実行できる状態ではありません".into());
        }
        recovering_remote = plan.state == PlanState::Running;
    }
    // 保持側と隔離側のロックの取得順を端末 ID で統一する。
    let reserve_first = remote
        .as_ref()
        .is_some_and(|intent| intent.keep.device_id < intent.route.target_device);
    let mut lease = None;
    let mut remote_failure = None;
    if reserve_first && let Some(intent) = &remote {
        match reserve_remote(db, ctx, plan_id, intent) {
            Ok(held) => lease = Some(held),
            Err(error) if recovering_remote => remote_failure = Some(error),
            Err(error) => return Err(error),
        }
    }
    let _lock = lock_plans(&ctx.data_dir, plan_id)?;

    let plan = plan::load_plan_required(db, plan_id)?;
    if !matches!(plan.state, PlanState::Approved | PlanState::Running) {
        return Err(format!(
            "計画 #{plan_id} は実行できる状態ではありません（今の状態: {}）",
            plan.state.as_str()
        ));
    }

    // 別の計画が running のままになっていないか確かめる。
    let others = plan::list_plans(db, Some(PlanState::Running), 10).map_err(|e| e.to_string())?;
    if let Some(other) = others.iter().find(|p| p.id != plan_id) {
        return Err(format!(
            "別の計画（#{}）が実行中のまま残っています。先に再開してください。",
            other.id
        ));
    }

    let candidates = plan::plan_ops(db, plan_id).map_err(|e| e.to_string())?;
    super::remote_keep::check_overlap(
        db.conn(),
        candidates
            .iter()
            .flat_map(|op| std::iter::once(op.src.as_path()).chain(op.dst.as_deref())),
    )?;

    if !reserve_first && let Some(intent) = &remote {
        match reserve_remote(db, ctx, plan_id, intent) {
            Ok(held) => lease = Some(held),
            Err(error) if recovering_remote => remote_failure = Some(error),
            Err(error) => return Err(error),
        }
    }
    let lease = lease;
    if plan.state == PlanState::Approved {
        plan::set_plan_state(db, plan_id, PlanState::Running, None).map_err(|e| e.to_string())?;
    }

    let drives = query::drives(db).map_err(|e| e.to_string())?;

    // 1. 途中で落ちて `running` のままの操作があれば、ディスクの状態から `done` か `pending` に直す。
    recover_running_ops(db, ctx, &drives, plan_id)?;

    // 2. `mkdir` → `move` → `trash`（それぞれ seq 順）で `pending` の操作を実行する。
    let mut ops = plan::plan_ops(db, plan_id).map_err(|e| e.to_string())?;
    ops.sort_by_key(|o| (o.op.phase(), o.seq));

    let mut linked_touched = Vec::new();
    for op in &ops {
        if op.state != OpState::Pending {
            continue;
        }
        if let Some(reason) = &remote_failure {
            super::markers::abandon(db, op)?;
            plan::set_op_state(
                db,
                plan_id,
                op.seq,
                OpState::Skipped,
                None,
                Some(reason),
                Some(db::now_ms()),
            )
            .map_err(|e| e.to_string())?;
            continue;
        }
        if plan.skip_risky && op.risky.is_some() {
            super::markers::abandon(db, op)?;
            plan::set_op_state(
                db,
                plan_id,
                op.seq,
                OpState::Skipped,
                None,
                Some("危ないため飛ばしました"),
                Some(db::now_ms()),
            )
            .map_err(|e| e.to_string())?;
            continue;
        }

        // 実行の直前にもう一度確かめる。合わなければ skipped にして先へ進む。
        if let Err(reason) = check::recheck_op_before_apply(db, ctx, op) {
            if remote.is_some() {
                remote_failure = Some(format!("{reason}。重複を再取得して確認してください"));
            }
            super::markers::abandon(db, op)?;
            plan::set_op_state(
                db,
                plan_id,
                op.seq,
                OpState::Skipped,
                None,
                Some(&reason),
                Some(db::now_ms()),
            )
            .map_err(|e| e.to_string())?;
            continue;
        }

        // trash は隔離フォルダが同じボリュームに取れるかも先に確かめる（取れなければ skipped）。
        let drive_mount = drive_mount_of(&drives, op);
        if op.op == OpKind::Trash
            && quarantine::choose_quarantine_base(&ctx.data_dir, drive_mount.as_deref(), &op.src)
                .is_none()
        {
            super::markers::abandon(db, op)?;
            plan::set_op_state(
                db,
                plan_id,
                op.seq,
                OpState::Skipped,
                None,
                Some("隔離できない"),
                Some(db::now_ms()),
            )
            .map_err(|e| e.to_string())?;
            continue;
        }

        let marker_preparation = super::markers::prepare(
            db,
            op,
            remote.as_ref().map(|intent| (ctx, intent)),
            lease.as_ref().and_then(|lease| lease.location.clone()),
        );
        let preparation_warning = marker_preparation
            .as_ref()
            .err()
            .map(|reason| format!("隔離済み／目印を作成できませんでした: {reason}"));

        // 1 件ごとに「始めた（running）」を DB に確定してから動かす。
        plan::set_op_state(
            db,
            plan_id,
            op.seq,
            OpState::Running,
            None,
            preparation_warning.as_deref(),
            None,
        )
        .map_err(|e| e.to_string())?;

        let updated_links = super::linked_delete::prepare_delete(db, ctx, op)?;
        linked_touched.extend(updated_links);
        let result = if let Some(intent) = &remote {
            let source = usize::try_from(op.seq)
                .ok()
                .and_then(|index| intent.sources.get(index));
            let result = source
                .ok_or("隔離対象の記録が見つかりません".to_string())
                .and_then(|source| {
                    remote_keep::isolate_source(db, ctx, op, source, || {
                        remote_keep::call(ctx, intent, RelayAction::Renew).map(|_| ())
                    })
                })
                .map(|path| OpExecOutcome::Done {
                    exact_dst: Some(path),
                    warning: None,
                });
            if let Err(error) = &result {
                remote_failure = Some(format!("{error}。重複を再取得して確認してください"));
            }
            result
        } else {
            run_one_op(plan_id, op, drive_mount.as_deref(), &ctx.data_dir)
        };
        #[cfg(all(unix, feature = "verify-remote-keep"))]
        if remote.is_some()
            && matches!(result, Ok(OpExecOutcome::Done { .. }))
            && std::env::var("LISA_VERIFY_KEEP_CRASH_AFTER_RENAME").as_deref() == Ok("1")
        {
            // 合成ケースで物理操作と状態確定のあいだにプロセスを終了する。
            std::process::exit(71);
        }
        match result {
            Ok(OpExecOutcome::Done { exact_dst, warning }) => {
                super::linked_delete::update_missing(db, op, true)?;
                let marker_warning = match marker_preparation {
                    Ok(()) => super::markers::publish(db, op, exact_dst.as_deref()).unwrap_or_else(
                        |reason| Some(format!("隔離済み／目印の状態を記録できません: {reason}")),
                    ),
                    Err(_) => preparation_warning,
                };
                let warning = join_reasons(warning.as_deref(), marker_warning);
                if op.op == OpKind::Mkdir {
                    plan::set_op_result_exact_dst(
                        db,
                        plan_id,
                        op.seq,
                        OpState::Done,
                        exact_dst.as_deref(),
                        warning.as_deref(),
                        Some(db::now_ms()),
                    )
                    .map_err(|e| e.to_string())?;
                } else {
                    plan::set_op_state(
                        db,
                        plan_id,
                        op.seq,
                        OpState::Done,
                        exact_dst.as_deref(),
                        warning.as_deref(),
                        Some(db::now_ms()),
                    )
                    .map_err(|e| e.to_string())?;
                }
            }
            Ok(OpExecOutcome::Skipped(reason)) => {
                super::markers::abandon(db, op)?;
                plan::set_op_state(
                    db,
                    plan_id,
                    op.seq,
                    OpState::Skipped,
                    None,
                    Some(&reason),
                    Some(db::now_ms()),
                )
                .map_err(|e| e.to_string())?;
            }
            Err(reason) => {
                super::markers::abandon(db, op)?;
                plan::set_op_state(
                    db,
                    plan_id,
                    op.seq,
                    OpState::Failed,
                    None,
                    Some(&reason),
                    Some(db::now_ms()),
                )
                .map_err(|e| e.to_string())?;
            }
        }
    }

    // 3. 全体の結果をまとめて plan を確定する。
    let final_ops = plan::plan_ops(db, plan_id).map_err(|e| e.to_string())?;
    let mut done = 0;
    let mut skipped = 0;
    let mut failed = 0;
    for op in &final_ops {
        match op.state {
            OpState::Done => done += 1,
            OpState::Skipped => skipped += 1,
            OpState::Failed => failed += 1,
            _ => {}
        }
    }
    let state = if skipped > 0 || failed > 0 {
        PlanState::Partial
    } else {
        PlanState::Done
    };
    plan::set_plan_state(db, plan_id, state, Some(db::now_ms())).map_err(|e| e.to_string())?;

    let mut touched = touched_folders(&final_ops);
    touched.extend(linked_touched);
    touched.sort_unstable();
    touched.dedup();
    Ok(ApplyReport {
        plan_id,
        state,
        done,
        skipped,
        failed,
        touched_folders: touched,
    })
}

fn reserve_remote<'a>(
    db: &mut Db,
    ctx: &'a Context,
    plan_id: i64,
    intent: &'a super::remote_keep::RemotePlan,
) -> Result<super::remote_keep::ActiveLease<'a>, String> {
    match super::remote_keep::ActiveLease::acquire(ctx, intent) {
        Ok(lease) => Ok(lease),
        Err(error) => {
            db.conn().execute(
                "UPDATE plan SET state = 'pending', decided_at = NULL WHERE id = ?1 AND state = 'approved'",
                [plan_id],
            ).map_err(|e| format!("{error} / 許可状態を確認できませんでした: {e}"))?;
            Err(error)
        }
    }
}

enum OpExecOutcome {
    Done {
        warning: Option<String>,
        /// `trash` は隔離した場所、`mkdir` は新しく作った一番上のフォルダ（すでにあったら `None`）、
        /// `move` は `None`（元の `dst` を維持）。
        exact_dst: Option<PathBuf>,
    },
    Skipped(String),
}

fn run_one_op(
    plan_id: i64,
    op: &PlanOp,
    drive_mount: Option<&Path>,
    data_dir: &Path,
) -> Result<OpExecOutcome, String> {
    match op.op {
        OpKind::Mkdir => {
            if op.src.is_dir() {
                return Ok(OpExecOutcome::Done {
                    exact_dst: None,
                    warning: None,
                });
            }
            // 新しく作られる一番浅いフォルダを覚えておく（取り消しで元からあった親まで消さないため）。
            let created_root = shallowest_missing_dir(&op.src);
            fs::create_dir_all(&op.src).map_err(|e| format!("フォルダを作れませんでした: {e}"))?;
            Ok(OpExecOutcome::Done {
                exact_dst: created_root,
                warning: None,
            })
        }
        OpKind::Move => {
            let dst = op
                .dst
                .as_deref()
                .ok_or_else(|| "行き先がありません".to_string())?;
            if super::move_destination_exists(&op.src, dst)
                .map_err(|e| format!("行き先を確かめられません: {e}"))?
            {
                return Ok(OpExecOutcome::Skipped(
                    "行き先にすでにファイルやフォルダがあります".into(),
                ));
            }
            let dst_parent = dst
                .parent()
                .ok_or_else(|| "行き先の親フォルダがありません".to_string())?;
            if op.src.parent() == Some(dst_parent) {
                return match rename_without_replace(&op.src, dst) {
                    Ok(()) => Ok(OpExecOutcome::Done {
                        exact_dst: None,
                        warning: None,
                    }),
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(
                        OpExecOutcome::Skipped("行き先にすでにファイルやフォルダがあります".into()),
                    ),
                    Err(e) => Err(format!("名前を変更できませんでした: {e}")),
                };
            }
            if use_rename(&op.src, dst_parent) {
                match fs::rename(&op.src, dst) {
                    Ok(()) => {
                        return Ok(OpExecOutcome::Done {
                            exact_dst: None,
                            warning: None,
                        });
                    }
                    Err(e) if is_cross_device_error(&e) => {
                        // 別ボリューム扱いだったのでコピーへ切り替える。
                    }
                    Err(e) => return Err(format!("移動できませんでした: {e}")),
                }
            }
            let (_, warning) =
                copy_verify_and_quarantine(plan_id, op.seq, &op.src, dst, drive_mount, data_dir)?;
            Ok(OpExecOutcome::Done {
                exact_dst: None,
                warning,
            })
        }
        OpKind::Trash => {
            let quarantined = quarantine::move_to_quarantine(
                plan_id,
                op.seq,
                OpKind::Trash,
                &op.src,
                drive_mount,
                data_dir,
                |src, dst| fs::rename(src, dst),
            )?;
            Ok(OpExecOutcome::Done {
                exact_dst: Some(quarantined),
                warning: None,
            })
        }
    }
}

/// 途中で落ちた `running` の操作を、ディスク上の元と行き先の有無から `done` / `pending` / `failed` に直す。
fn recover_running_ops(
    db: &mut Db,
    ctx: &Context,
    drives: &[Drive],
    plan_id: i64,
) -> Result<(), String> {
    let ops = plan::plan_ops(db, plan_id).map_err(|e| e.to_string())?;
    for op in ops.into_iter().filter(|o| o.state == OpState::Running) {
        let now = db::now_ms();
        match op.op {
            OpKind::Mkdir => {
                if op.src.is_dir() {
                    plan::set_op_state(db, plan_id, op.seq, OpState::Done, None, None, Some(now))
                        .map_err(|e| e.to_string())?;
                } else {
                    plan::set_op_state(db, plan_id, op.seq, OpState::Pending, None, None, None)
                        .map_err(|e| e.to_string())?;
                }
            }
            OpKind::Move => {
                let dst = op.dst.as_deref();
                let presence = entry_exists(&op.src).and_then(|src_exists| {
                    let dst_exists = dst.map(entry_exists).transpose()?.unwrap_or(false);
                    Ok((src_exists, dst_exists))
                });
                let (src_exists, dst_exists) = match presence {
                    Ok(presence) => presence,
                    Err(e) => {
                        plan::set_op_state(
                            db,
                            plan_id,
                            op.seq,
                            OpState::Failed,
                            None,
                            Some(&format!("中断後の照合に失敗: {e}")),
                            Some(now),
                        )
                        .map_err(|e| e.to_string())?;
                        continue;
                    }
                };
                match (src_exists, dst_exists) {
                    (false, true) => {
                        plan::set_op_state(
                            db,
                            plan_id,
                            op.seq,
                            OpState::Done,
                            None,
                            None,
                            Some(now),
                        )
                        .map_err(|e| e.to_string())?;
                    }
                    (true, false) => {
                        plan::set_op_state(db, plan_id, op.seq, OpState::Pending, None, None, None)
                            .map_err(|e| e.to_string())?;
                    }
                    (true, true) => {
                        // 別ボリュームのコピー後に元を隔離する直前で落ちた可能性がある。
                        let dst = dst.expect("dst_exists is true");
                        if op.src.parent() == dst.parent() {
                            // 同じディレクトリ内ではコピーしない。大小無視で両方見えるときは実名で再開点を判定する。
                            let (state, reason, at) =
                                if super::is_name_alias(dst, &op.src).unwrap_or(false) {
                                    (OpState::Done, None, Some(now))
                                } else if super::is_name_alias(&op.src, dst).unwrap_or(false) {
                                    (OpState::Pending, None, None)
                                } else {
                                    (
                                        OpState::Failed,
                                        Some("中断後に元と行き先の両方があります"),
                                        Some(now),
                                    )
                                };
                            plan::set_op_state(db, plan_id, op.seq, state, None, reason, at)
                                .map_err(|e| e.to_string())?;
                        } else {
                            let result = entries_match_content(&op.src, dst)
                                .map_err(|e| format!("中断後の照合に失敗: {e}"))
                                .and_then(|()| {
                                    let mount = drive_mount_of(drives, &op);
                                    quarantine::move_to_quarantine(
                                        plan_id,
                                        op.seq,
                                        OpKind::Move,
                                        &op.src,
                                        mount.as_deref(),
                                        &ctx.data_dir,
                                        rename_after_collision_check,
                                    )
                                    .map_err(|e| format!("中断後の隔離に失敗: {e}"))
                                });
                            let (state, reason) = match result {
                                Ok(_) => (OpState::Done, None),
                                Err(e) => (OpState::Failed, Some(e)),
                            };
                            plan::set_op_state(
                                db,
                                plan_id,
                                op.seq,
                                state,
                                None,
                                reason.as_deref(),
                                Some(now),
                            )
                            .map_err(|e| e.to_string())?;
                        }
                    }
                    (false, false) => {
                        plan::set_op_state(
                            db,
                            plan_id,
                            op.seq,
                            OpState::Failed,
                            None,
                            Some("中断後に元も行き先も見つかりません"),
                            Some(now),
                        )
                        .map_err(|e| e.to_string())?;
                    }
                }
            }
            OpKind::Trash => {
                let src_exists = fs::symlink_metadata(&op.src).is_ok();
                let mount = drive_mount_of(drives, &op);
                let q_dest = op
                    .dst
                    .clone()
                    .filter(|p| fs::symlink_metadata(p).is_ok())
                    .or_else(|| {
                        quarantine::find_existing_quarantine_dest(
                            plan_id,
                            &op.src,
                            mount.as_deref(),
                            &ctx.data_dir,
                        )
                    });
                match (src_exists, q_dest) {
                    (false, Some(dest)) => {
                        super::linked_delete::update_missing(db, &op, true)?;
                        let warning = super::markers::publish(db, &op, Some(&dest)).unwrap_or_else(
                            |reason| {
                                Some(format!("隔離済み／目印の状態を記録できません: {reason}"))
                            },
                        );
                        let warning = join_reasons(op.reason.as_deref(), warning);
                        plan::set_op_state(
                            db,
                            plan_id,
                            op.seq,
                            OpState::Done,
                            Some(&dest),
                            warning.as_deref(),
                            Some(now),
                        )
                        .map_err(|e| e.to_string())?;
                    }
                    (true, None) => {
                        plan::set_op_state(db, plan_id, op.seq, OpState::Pending, None, None, None)
                            .map_err(|e| e.to_string())?;
                    }
                    (true, Some(_)) => {
                        plan::set_op_state(
                            db,
                            plan_id,
                            op.seq,
                            OpState::Failed,
                            None,
                            Some("中断後に元と隔離先の両方があります"),
                            Some(now),
                        )
                        .map_err(|e| e.to_string())?;
                    }
                    (false, None) => {
                        plan::set_op_state(
                            db,
                            plan_id,
                            op.seq,
                            OpState::Failed,
                            None,
                            Some("中断後に元も隔離先も見つかりません"),
                            Some(now),
                        )
                        .map_err(|e| e.to_string())?;
                    }
                }
            }
        }
    }
    Ok(())
}

/// 実行済みの計画（`done` / `partial`）を、逆の順（`trash` → `move` → `mkdir`、各 `seq` 降順）で取り消す。
/// 許可は要らず、上書きはしない。戻す先が埋まっている・元が変わったものは飛ばして理由を返す。
pub fn undo_plan(db: &mut Db, ctx: &Context, plan_id: i64) -> Result<UndoReport, String> {
    let _lock = lock_plans(&ctx.data_dir, plan_id)?;

    let plan = plan::load_plan_required(db, plan_id)?;
    if plan.state == PlanState::Undone {
        return Err(format!("計画 #{plan_id} はすでに取り消しています"));
    }
    if !matches!(plan.state, PlanState::Done | PlanState::Partial) {
        return Err(format!(
            "計画 #{plan_id} は取り消せる状態ではありません（今の状態: {}）",
            plan.state.as_str()
        ));
    }

    let peer_managed = plan::peer_managed_plan(db, plan_id).map_err(|e| e.to_string())?;
    let mut ops = plan::plan_ops(db, plan_id).map_err(|e| e.to_string())?;
    super::remote_keep::check_overlap(
        db.conn(),
        ops.iter()
            .filter(|op| op.state == OpState::Done)
            .flat_map(|op| std::iter::once(op.src.as_path()).chain(op.dst.as_deref())),
    )?;
    // 実行の逆の順: (phase, seq) の降順。
    ops.sort_by_key(|a| std::cmp::Reverse((a.op.phase(), a.seq)));

    let mut undone = 0usize;
    let mut skipped = Vec::new();
    let mut undone_ops = Vec::new();

    for op in ops.into_iter().filter(|o| o.state == OpState::Done) {
        let retryable = peer_managed && op.op == OpKind::Trash && plan.purged_at.is_none();
        match undo_one_op(&plan, &op) {
            Ok(warning) => {
                super::linked_delete::update_missing(db, &op, false)?;
                let marker_warning = super::markers::remove_after_undo(db, &op)
                    .err()
                    .map(|reason| format!("復元済み／目印が残っています: {reason}"));
                let warning = join_reasons(warning.as_deref(), marker_warning);
                let reason = if retryable {
                    warning
                } else {
                    join_reasons(op.reason.as_deref(), warning)
                };
                plan::set_op_state(
                    db,
                    plan_id,
                    op.seq,
                    OpState::Undone,
                    None,
                    reason.as_deref(),
                    Some(db::now_ms()),
                )
                .map_err(|e| e.to_string())?;
                undone += 1;
                let mut marked = op;
                marked.state = OpState::Undone;
                undone_ops.push(marked);
            }
            Err(reason) => {
                let reason = if retryable {
                    reason
                } else {
                    append_reason(reason, op.reason.as_deref())
                };
                let state = if retryable {
                    OpState::Done
                } else {
                    OpState::Skipped
                };
                plan::set_op_state(
                    db,
                    plan_id,
                    op.seq,
                    state,
                    None,
                    Some(&reason),
                    Some(db::now_ms()),
                )
                .map_err(|e| e.to_string())?;
                skipped.push(UndoSkipped {
                    seq: op.seq,
                    src: op.src,
                    reason,
                });
            }
        }
    }

    // 残っている done の操作が無ければ計画全体を undone にする。
    let remaining_done = plan::plan_ops(db, plan_id)
        .map_err(|e| e.to_string())?
        .into_iter()
        .any(|o| o.state == OpState::Done);
    let new_state = if !remaining_done {
        PlanState::Undone
    } else {
        plan.state
    };
    if new_state != plan.state {
        plan::set_plan_state(db, plan_id, new_state, None).map_err(|e| e.to_string())?;
    }

    let touched = touched_folders(&undone_ops);
    Ok(UndoReport {
        plan_id,
        state: new_state,
        undone,
        skipped,
        touched_folders: touched,
    })
}

fn undo_one_op(plan: &plan::Plan, op: &PlanOp) -> Result<Option<String>, String> {
    match op.op {
        OpKind::Trash => {
            if plan.purged_at.is_some() {
                return Err("隔離から OS のゴミ箱へ送ったため戻せません".into());
            }
            let Some(q_path) = op.dst.as_deref() else {
                return Err("隔離先の記録がありません".into());
            };
            let q_meta = fs::symlink_metadata(q_path)
                .map_err(|_| "隔離先にファイルやフォルダがありません".to_string())?;
            if fs::symlink_metadata(&op.src).is_ok() {
                return Err("戻す先にすでにファイルやフォルダがあります".into());
            }
            verify_unchanged_for_undo(op, &q_meta)?;
            let Some(parent) = op.src.parent() else {
                return Err("戻す先の親フォルダがありません".into());
            };
            if !parent.is_dir() {
                return Err("戻す先の親フォルダがありません".into());
            }
            rename_without_replace(q_path, &op.src)
                .map_err(|e| format!("隔離から戻せませんでした: {e}"))?;
            Ok(None)
        }
        OpKind::Move => {
            let Some(dst) = op.dst.as_deref() else {
                return Err("移動先の記録がありません".into());
            };
            let dst_meta = fs::symlink_metadata(dst)
                .map_err(|_| "移動先にファイルやフォルダがありません".to_string())?;
            if super::move_destination_exists(dst, &op.src)
                .map_err(|e| format!("戻す先を確かめられません: {e}"))?
            {
                return Err("戻す先にすでにファイルやフォルダがあります".into());
            }
            verify_unchanged_for_undo(op, &dst_meta)?;
            let Some(parent) = op.src.parent() else {
                return Err("戻す先の親フォルダがありません".into());
            };
            if !parent.is_dir() {
                return Err("戻す先の親フォルダがありません".into());
            }
            if dst.parent() == Some(parent) {
                return rename_without_replace(dst, &op.src)
                    .map(|()| None)
                    .map_err(|e| format!("元の名前に戻せませんでした: {e}"));
            }
            if use_rename(dst, parent) {
                match fs::rename(dst, &op.src) {
                    Ok(()) => return Ok(None),
                    Err(e) if is_cross_device_error(&e) => {}
                    Err(e) => return Err(format!("元の場所へ戻せませんでした: {e}")),
                }
            }
            // 別ボリュームの移動を戻す: コピーの内容と日時を確かめてから移動先を消す。
            copy_verify_and_remove_src(plan.id, op.seq, dst, &op.src)
        }
        OpKind::Mkdir => {
            // 元からあったフォルダ（dst が None）は消さない。
            let Some(created_root) = op.dst.as_deref() else {
                return Ok(None);
            };
            match fs::symlink_metadata(&op.src) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(format!("フォルダを確かめられません: {e}")),
                Ok(m) if !m.is_dir() => return Err("フォルダではなくなっています".into()),
                Ok(_) => {}
            }
            if !is_dir_empty(&op.src)? {
                return Err("フォルダが空ではないため消しませんでした".into());
            }
            let mut cur: Option<&Path> = Some(&op.src);
            while let Some(dir) = cur {
                if !dir.is_dir() || !is_dir_empty(dir)? {
                    break;
                }
                fs::remove_dir(dir).map_err(|e| format!("フォルダを消せませんでした: {e}"))?;
                if dir == created_root {
                    break;
                }
                cur = dir.parent();
            }
            Ok(None)
        }
    }
}

fn verify_unchanged_for_undo(op: &PlanOp, meta: &fs::Metadata) -> Result<(), String> {
    let kind = check::meta_kind(meta);
    if kind == Kind::File
        && let Some(rec_size) = op.size
        && meta.len() != rec_size
    {
        return Err("大きさが変わったため戻しませんでした".into());
    }
    if op.mtime.is_some() && listing::mtime_ms(meta) != op.mtime {
        return Err("更新日時が変わったため戻しませんでした".into());
    }
    Ok(())
}

fn is_dir_empty(dir: &Path) -> Result<bool, String> {
    let mut rd = fs::read_dir(dir).map_err(|e| format!("フォルダを読めません: {e}"))?;
    Ok(rd.next().is_none())
}

/// 実行・取り消しで触ったフォルダ（元の親と行き先の親）を重複なく返す。
/// アプリはこれを `Request::Hint` で読む係に渡し、CLI / MCP は `Command::Rescan` で送る。
pub fn touched_folders(ops: &[PlanOp]) -> Vec<PathBuf> {
    let mut set = BTreeSet::new();
    for op in ops {
        if !matches!(op.state, OpState::Done | OpState::Undone) {
            continue;
        }
        if let Some(parent) = op.src.parent()
            && !parent.as_os_str().is_empty()
        {
            set.insert(parent.to_path_buf());
        }
        match op.op {
            OpKind::Move => {
                if let Some(dst) = op.dst.as_deref()
                    && let Some(parent) = dst.parent()
                    && !parent.as_os_str().is_empty()
                {
                    set.insert(parent.to_path_buf());
                }
            }
            OpKind::Mkdir => {
                if let Some(root) = op.dst.as_deref()
                    && let Some(parent) = root.parent()
                    && !parent.as_os_str().is_empty()
                {
                    set.insert(parent.to_path_buf());
                }
            }
            OpKind::Trash => {
                // 隔離フォルダは読む係が飛ばすので、元の親だけ。
            }
        }
    }
    set.into_iter().collect()
}

fn drive_mount_of(drives: &[Drive], op: &PlanOp) -> Option<PathBuf> {
    if let Some(id) = op.drive_id.as_deref()
        && let Some(m) = drives
            .iter()
            .find(|d| d.id == id)
            .and_then(|d| d.mount.clone())
    {
        return Some(m);
    }
    drives
        .iter()
        .filter_map(|d| d.mount.as_deref())
        .filter(|m| op.src.starts_with(m))
        .max_by_key(|m| m.components().count())
        .map(Path::to_path_buf)
}

fn shallowest_missing_dir(path: &Path) -> Option<PathBuf> {
    let mut missing = None;
    let mut cur = Some(path);
    while let Some(p) = cur {
        if fs::symlink_metadata(p).is_ok() {
            break;
        }
        missing = Some(p.to_path_buf());
        cur = p.parent();
    }
    missing
}

// 事前確認のあとに別プロセスが同名項目を作っても、上書きせずに失敗する。
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(super) fn rename_without_replace(src: &Path, dst: &Path) -> io::Result<()> {
    use std::ffi::{CString, c_char, c_int, c_uint};
    use std::os::unix::ffi::OsStrExt;

    #[cfg(target_os = "macos")]
    unsafe extern "C" {
        fn renamex_np(src: *const c_char, dst: *const c_char, flags: c_uint) -> c_int;
    }
    #[cfg(target_os = "linux")]
    unsafe extern "C" {
        fn renameat2(
            old_dir: c_int,
            src: *const c_char,
            new_dir: c_int,
            dst: *const c_char,
            flags: c_uint,
        ) -> c_int;
    }

    let src = CString::new(src.as_os_str().as_bytes())?;
    let dst = CString::new(dst.as_os_str().as_bytes())?;
    // SAFETY: 両ポインターは呼び出し中有効な NUL 終端文字列を指す。
    #[cfg(target_os = "macos")]
    let result = unsafe {
        const RENAME_EXCL: c_uint = 4;
        renamex_np(src.as_ptr(), dst.as_ptr(), RENAME_EXCL)
    };
    // SAFETY: 両ポインターは呼び出し中有効な NUL 終端文字列を指す。
    #[cfg(target_os = "linux")]
    let result = unsafe {
        const AT_FDCWD: c_int = -100;
        const RENAME_NOREPLACE: c_uint = 1;
        renameat2(
            AT_FDCWD,
            src.as_ptr(),
            AT_FDCWD,
            dst.as_ptr(),
            RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(windows)]
pub(super) fn rename_without_replace(src: &Path, dst: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::MoveFileW;

    if [src, dst]
        .iter()
        .any(|path| path.as_os_str().as_encoded_bytes().contains(&0))
    {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let wide = |path: &Path| {
        path.as_os_str()
            .encode_wide()
            .chain([0])
            .collect::<Vec<_>>()
    };
    // 親を正規化して UNC と長いパスを扱い、末尾のリンク自身を移す。
    let absolute_entry = |path: &Path| -> io::Result<PathBuf> {
        let parent = path
            .parent()
            .ok_or(io::ErrorKind::InvalidInput)?
            .canonicalize()?;
        Ok(parent.join(path.file_name().ok_or(io::ErrorKind::InvalidInput)?))
    };
    let src = wide(&absolute_entry(src)?);
    let dst = wide(&absolute_entry(dst)?);
    // SAFETY: 両ポインターは呼び出し中有効な NUL 終端ワイド文字列を指す。
    if unsafe { MoveFileW(src.as_ptr(), dst.as_ptr()) } != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub(super) fn rename_without_replace(_src: &Path, _dst: &Path) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

fn rename_after_collision_check(src: &Path, dst: &Path) -> io::Result<()> {
    match rename_without_replace(src, dst) {
        // FAT など排他的 rename に対応しない媒体では、計画ロック中の従来の衝突確認を使う。
        Err(e) if exclusive_rename_unsupported(&e) => match fs::symlink_metadata(dst) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => fs::rename(src, dst),
            Err(e) => Err(e),
            Ok(_) => Err(io::ErrorKind::AlreadyExists.into()),
        },
        result => result,
    }
}

fn exclusive_rename_unsupported(error: &io::Error) -> bool {
    #[cfg(target_os = "linux")]
    if error.raw_os_error() == Some(libc::EINVAL) {
        return true;
    }
    #[cfg(unix)]
    if error.raw_os_error() == Some(libc::ENOTSUP) {
        return true;
    }
    error.kind() == io::ErrorKind::Unsupported
}

fn use_rename(src: &Path, dst_parent: &Path) -> bool {
    #[cfg(test)]
    if timestamp_test::FORCE_COPY.get() {
        return false;
    }
    quarantine::same_volume(src, dst_parent)
}

fn is_cross_device_error(err: &io::Error) -> bool {
    if err.kind() == io::ErrorKind::CrossesDevices {
        return true;
    }
    #[cfg(unix)]
    if err.raw_os_error() == Some(18) {
        return true;
    }
    #[cfg(windows)]
    if err.raw_os_error() == Some(17) {
        return true;
    }
    false
}

/// 別ボリュームへの移動: 内容と日時を照合して正式名に置き、元を隔離する。
pub(crate) fn copy_verify_and_quarantine(
    plan_id: i64,
    seq: i64,
    src: &Path,
    dst: &Path,
    drive_mount: Option<&Path>,
    data_dir: &Path,
) -> Result<(PathBuf, Option<String>), String> {
    let warning = copy_verify_and_publish(plan_id, seq, src, dst)?;
    quarantine::move_to_quarantine(
        plan_id,
        seq,
        OpKind::Move,
        src,
        drive_mount,
        data_dir,
        rename_after_collision_check,
    )
    .map_err(|e| append_reason(e, warning.as_deref()))
    .map(|path| (path, warning))
}

/// 別ボリュームからの取り消し: 検証したコピーを元の場所へ置き、移動先を消す。
fn copy_verify_and_remove_src(
    plan_id: i64,
    seq: i64,
    src: &Path,
    dst: &Path,
) -> Result<Option<String>, String> {
    let warning = copy_verify_and_publish(plan_id, seq, src, dst)?;
    remove_any(src)
        .map_err(|e| append_reason(path_error(src, "移動先の掃除", e), warning.as_deref()))?;
    Ok(warning)
}

pub(super) fn copy_verify_and_publish(
    plan_id: i64,
    seq: i64,
    src: &Path,
    dst: &Path,
) -> Result<Option<String>, String> {
    let parent = dst
        .parent()
        .ok_or_else(|| "行き先の親フォルダがありません".to_string())?;
    let staging = parent.join(format!(
        ".lisa-moving-{plan_id}-{seq}-{}",
        std::process::id()
    ));
    // 作成に成功したディレクトリ内だけを使い、既存の一時名には触らない。
    fs::create_dir(&staging).map_err(|e| {
        let reason = path_error(&staging, "一時フォルダの作成", &e);
        if e.kind() == io::ErrorKind::AlreadyExists {
            return format!("{reason}。前回の残りを確認して削除してから再実行してください");
        }
        reason
    })?;
    let copied = staging.join("entry");
    let result = copy_and_verify_entry(src, &copied)
        .and_then(|()| {
            entries_match_content(src, &copied).map_err(|e| format!("コピー元の再確認: {e}"))
        })
        .and_then(|()| {
            rename_after_collision_check(&copied, dst)
                .map_err(|e| path_error(dst, "正式名への確定", e))
        });
    let cleanup = if result.is_ok() {
        #[cfg(test)]
        if timestamp_test::FAIL_STAGING_CLEANUP.get() {
            return Ok(Some(cleanup_warning(
                &staging,
                io::ErrorKind::PermissionDenied,
            )));
        }
        fs::remove_dir(&staging)
    } else {
        remove_any(&staging)
    };
    match (result, cleanup) {
        (Ok(()), Ok(())) => Ok(None),
        (Err(reason), Ok(())) => Err(reason),
        (Ok(()), Err(e)) => Ok(Some(cleanup_warning(&staging, e))),
        (Err(reason), Err(e)) => Err(format!(
            "{reason} / {}",
            path_error(&staging, "一時コピーの掃除", e)
        )),
    }
}

fn cleanup_warning(path: &Path, error: impl std::fmt::Display) -> String {
    format!(
        "警告: {}。一時フォルダが残っています",
        path_error(path, "一時コピーの掃除", error)
    )
}

fn append_reason(reason: String, extra: Option<&str>) -> String {
    match extra {
        Some(extra) => format!("{reason} / {extra}"),
        None => reason,
    }
}

fn join_reasons(previous: Option<&str>, warning: Option<String>) -> Option<String> {
    match warning {
        Some(warning) => Some(append_reason(warning, previous)),
        None => previous.map(str::to_owned),
    }
}

fn path_error(path: &Path, step: &str, error: impl std::fmt::Display) -> String {
    format!("{} の{step}に失敗: {error}", path.display())
}

fn entry_metadata(path: &Path) -> Result<fs::Metadata, String> {
    fs::symlink_metadata(path).map_err(|e| path_error(path, "項目の取得", e))
}

fn entry_exists(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(path_error(path, "項目の取得", e)),
    }
}

fn read_mtime(path: &Path, meta: &fs::Metadata, step: &str) -> Result<SystemTime, String> {
    #[cfg(test)]
    timestamp_test::fail(path, step)?;
    meta.modified().map_err(|e| path_error(path, step, e))
}

fn verify_mtime(path: &Path, meta: &fs::Metadata, expected: SystemTime) -> Result<(), String> {
    let actual = read_mtime(path, meta, "更新日時の再取得")?;
    #[cfg(test)]
    let actual = timestamp_test::observed(path, actual);
    if actual == expected {
        return Ok(());
    }
    Err(path_error(
        path,
        "更新日時の照合",
        format!(
            "期待値 {}、実測値 {}（Unix 秒）",
            unix_seconds(expected),
            unix_seconds(actual)
        ),
    ))
}

fn unix_seconds(time: SystemTime) -> String {
    let (sign, duration) = match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => ("", duration),
        Err(e) => ("-", e.duration()),
    };
    format!(
        "{sign}{}.{:09}",
        duration.as_secs(),
        duration.subsec_nanos()
    )
}

#[cfg(unix)]
pub(super) fn set_mtime(path: &Path, time: SystemTime) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let path = CString::new(path.as_os_str().as_bytes())?;
    let nanos = match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_nanos() as i128,
        Err(e) => -(e.duration().as_nanos() as i128),
    };
    let seconds = nanos.div_euclid(1_000_000_000);
    let fraction = nanos.rem_euclid(1_000_000_000);
    let times = [
        libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_OMIT as _,
        },
        libc::timespec {
            tv_sec: seconds
                .try_into()
                .map_err(|_| io::ErrorKind::InvalidInput)?,
            tv_nsec: fraction as _,
        },
    ];
    // SAFETY: path と times は呼び出し中有効。末尾のシンボリックリンクはたどらない。
    let result = unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            path.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(windows)]
pub(super) fn set_mtime(path: &Path, time: SystemTime) -> io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_WRITE_ATTRIBUTES,
    };

    File::options()
        .access_mode(FILE_WRITE_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?
        .set_modified(time)
}

#[cfg(not(any(unix, windows)))]
pub(super) fn set_mtime(_path: &Path, _time: SystemTime) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

fn copy_and_verify_entry(src: &Path, dst: &Path) -> Result<(), String> {
    let meta = entry_metadata(src)?;
    let expected_mtime = read_mtime(src, &meta, "更新日時の取得")?;
    match check::meta_kind(&meta) {
        Kind::File => {
            let expected = copy_file_hashing(src, dst)?;
            if entry_hash(dst)? != expected {
                return Err(path_error(
                    dst,
                    "内容の照合",
                    "コピー先の blake3 が元と合いません",
                ));
            }
        }
        Kind::Dir => {
            fs::create_dir(dst).map_err(|e| path_error(dst, "フォルダの作成", e))?;
            for name in entry_names(src)? {
                copy_and_verify_entry(&src.join(&name), &dst.join(name))?;
            }
        }
        Kind::Symlink => {
            create_symlink(&entry_link(src)?, dst, &meta)
                .map_err(|e| path_error(dst, "リンクの作成", e))?;
        }
        Kind::Other => {
            return Err(path_error(
                src,
                "コピー",
                "特殊なファイルは別ボリュームへ移せません",
            ));
        }
    }
    // ファイルの書き込みハンドルが閉じ、フォルダ内の全項目が完成したあとに設定する。
    #[cfg(test)]
    timestamp_test::fail(dst, "更新日時の設定")?;
    set_mtime(dst, expected_mtime).map_err(|e| path_error(dst, "更新日時の設定", e))?;
    let actual = fs::symlink_metadata(dst).map_err(|e| path_error(dst, "更新日時の再取得", e))?;
    verify_mtime(dst, &actual, expected_mtime)
}

fn copy_file_hashing(src: &Path, dst: &Path) -> Result<hash::Hash, String> {
    let mut reader = File::open(src).map_err(|e| path_error(src, "ファイルを開く処理", e))?;
    let mut writer = File::create_new(dst).map_err(|e| path_error(dst, "ファイルの作成", e))?;
    let mut hasher = hash::Hasher::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| path_error(src, "読み込み", e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        writer
            .write_all(&buf[..n])
            .map_err(|e| path_error(dst, "書き込み", e))?;
    }
    writer
        .flush()
        .map_err(|e| path_error(dst, "書き込み完了", e))?;
    Ok(*hasher.finalize().as_bytes())
}

fn create_symlink(target: &Path, dst: &Path, _meta: &fs::Metadata) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, dst)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileTypeExt;

        if _meta.file_type().is_symlink_dir() {
            std::os::windows::fs::symlink_dir(target, dst)
        } else {
            std::os::windows::fs::symlink_file(target, dst)
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (target, dst);
        Err(io::ErrorKind::Unsupported.into())
    }
}

fn remove_any(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    use std::os::windows::fs::FileTypeExt;

    match fs::symlink_metadata(path) {
        #[cfg(windows)]
        Ok(meta) if meta.file_type().is_symlink_dir() => fs::remove_dir(path),
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn entry_hash(path: &Path) -> Result<hash::Hash, String> {
    hash::hash_file(path).map_err(|e| path_error(path, "ハッシュの取得", e))
}

fn entry_link(path: &Path) -> Result<PathBuf, String> {
    fs::read_link(path).map_err(|e| path_error(path, "リンク先の取得", e))
}

fn entry_names(path: &Path) -> Result<BTreeSet<std::ffi::OsString>, String> {
    fs::read_dir(path)
        .and_then(|items| {
            items
                .map(|item| item.map(|item| item.file_name()))
                .collect()
        })
        .map_err(|e| path_error(path, "フォルダの読み取り", e))
}

/// コピー元の再確認と中断復旧で、更新日時を含むツリー全体を照合する。
fn entries_match_content(a: &Path, b: &Path) -> Result<(), String> {
    let ma = entry_metadata(a)?;
    let mb = entry_metadata(b)?;
    let kind = check::meta_kind(&ma);
    if kind != check::meta_kind(&mb) {
        return Err(path_error(b, "種類の照合", "種類が一致しません"));
    }
    let expected_mtime = read_mtime(a, &ma, "更新日時の取得")?;
    verify_mtime(b, &mb, expected_mtime)?;
    let same = match kind {
        Kind::File => ma.len() == mb.len() && entry_hash(a)? == entry_hash(b)?,
        Kind::Symlink => entry_link(a)? == entry_link(b)?,
        Kind::Dir => {
            let names = entry_names(a)?;
            if names != entry_names(b)? {
                return Err(path_error(b, "名前の照合", "配下の名前が一致しません"));
            }
            for name in names {
                entries_match_content(&a.join(&name), &b.join(name))?;
            }
            true
        }
        Kind::Other => return Err(path_error(b, "内容の照合", "特殊なファイルです")),
    };
    if !same {
        return Err(path_error(b, "内容の照合", "内容が一致しません"));
    }
    // ハッシュや子の照合中に更新されていないことも確かめる。
    verify_mtime(a, &entry_metadata(a)?, expected_mtime)?;
    verify_mtime(b, &entry_metadata(b)?, expected_mtime)
}

#[cfg(test)]
pub(super) mod timestamp_test {
    use super::*;
    use std::cell::Cell;
    use std::time::Duration;

    thread_local! {
        pub(crate) static FAIL_STAGING_CLEANUP: Cell<bool> = const { Cell::new(false) };
        pub(crate) static FORCE_COPY: Cell<bool> = const { Cell::new(false) };
        pub(crate) static FAULT: Cell<Option<&'static str>> = const { Cell::new(None) };
    }

    pub(super) fn fail(path: &Path, step: &str) -> Result<(), String> {
        if path.ends_with("fault.bin") && FAULT.get() == Some(step) {
            return Err(path_error(path, step, "注入した I/O エラー"));
        }
        Ok(())
    }

    pub(super) fn observed(path: &Path, time: SystemTime) -> SystemTime {
        if path.ends_with("fault.bin") && FAULT.get() == Some("丸め") {
            UNIX_EPOCH + Duration::from_secs(time.duration_since(UNIX_EPOCH).unwrap().as_secs())
        } else {
            time
        }
    }

    #[cfg(unix)]
    #[test]
    fn only_unsupported_exclusive_rename_errors_allow_fallback() {
        let unsupported =
            |errno| exclusive_rename_unsupported(&io::Error::from_raw_os_error(errno));
        assert!(unsupported(libc::ENOTSUP));
        assert_eq!(unsupported(libc::EINVAL), cfg!(target_os = "linux"));
        assert!(!unsupported(libc::EEXIST));
        assert!(!unsupported(libc::EACCES));
    }
}

#[cfg(test)]
pub(super) mod plan_start_hook {
    use std::cell::RefCell;

    type Hook = Box<dyn FnOnce()>;
    thread_local! {
        pub(super) static HOOK: RefCell<Option<Hook>> = RefCell::new(None);
    }

    pub(in crate::organize) fn set(hook: impl FnOnce() + 'static) {
        HOOK.with(|slot| slot.replace(Some(Box::new(hook))));
    }
}
