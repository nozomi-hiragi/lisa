//! 整理の計画の実行、落ちたあとの再開、取り消し、触ったフォルダの収集。
//!
//! - 同時に動く計画は 1 つだけ（プロセス内の Mutex と `<data_dir>/organize.lock`）。
//! - 実行の順は `mkdir` → `move` → `trash`（それぞれ計画の `seq` 順）。
//! - 1 件ごとに `running` を DB に書いて確定してから動かし、済んだら `done`（または `skipped` / `failed`）にする。
//! - `entry` の表には直接書かない。触ったフォルダの一覧を返すので、アプリは `Request::Hint`、CLI / MCP は `Command::Rescan` で読み直させる。

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

use crate::db::{self, Db};
use crate::hash;
use crate::listing::{self, Kind};
use crate::query::{self, Drive};

use super::Context;
use super::check;
use super::plan::{self, OpKind, OpState, PlanOp, PlanState};
use super::quarantine;

/// 同じプロセスの中で同時に複数の計画が動かないようにするロック。
static RUN_LOCK: Mutex<()> = Mutex::new(());

/// ロックファイルの名前（`<data_dir>/organize.lock`）。
const LOCK_FILE: &str = "organize.lock";

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

#[derive(Debug, Serialize, Deserialize)]
struct LockInfo {
    pid: u32,
    plan_id: i64,
    started_at: i64,
}

struct PlanLock {
    _in_process: MutexGuard<'static, ()>,
    path: PathBuf,
}

impl PlanLock {
    fn acquire(data_dir: &Path, plan_id: i64) -> Result<Self, String> {
        let guard = RUN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let path = data_dir.join(LOCK_FILE);
        if let Ok(bytes) = fs::read(&path)
            && let Ok(info) = serde_json::from_slice::<LockInfo>(&bytes)
            && info.pid != std::process::id()
            && is_pid_alive(info.pid)
        {
            return Err(format!(
                "別の計画（#{}、PID {}）が実行中です",
                info.plan_id, info.pid
            ));
        }
        let _ = fs::create_dir_all(data_dir);
        let info = LockInfo {
            pid: std::process::id(),
            plan_id,
            started_at: db::now_ms(),
        };
        let bytes = serde_json::to_vec(&info).map_err(|e| e.to_string())?;
        let tmp = data_dir.join(format!("{LOCK_FILE}.tmp.{}", std::process::id()));
        if fs::write(&tmp, &bytes).is_ok() {
            let _ = fs::rename(&tmp, &path);
        }
        Ok(Self {
            _in_process: guard,
            path,
        })
    }
}

impl Drop for PlanLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn is_pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        // SAFETY: シグナル 0 はプロセスを止めず、生きているか確かめるだけ。
        unsafe { kill(pid as i32, 0) == 0 }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return false;
            }
            let mut code: u32 = 0;
            let ok = GetExitCodeProcess(handle, &mut code) != 0;
            CloseHandle(handle);
            ok && code == STILL_ACTIVE as u32
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
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
    let _lock = PlanLock::acquire(&ctx.data_dir, plan_id)?;

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

    if plan.state == PlanState::Approved {
        plan::set_plan_state(db, plan_id, PlanState::Running, None).map_err(|e| e.to_string())?;
    }

    let drives = query::drives(db).map_err(|e| e.to_string())?;

    // 1. 途中で落ちて `running` のままの操作があれば、ディスクの状態から `done` か `pending` に直す。
    recover_running_ops(db, ctx, &drives, plan_id)?;

    // 2. `mkdir` → `move` → `trash`（それぞれ seq 順）で `pending` の操作を実行する。
    let mut ops = plan::plan_ops(db, plan_id).map_err(|e| e.to_string())?;
    ops.sort_by_key(|o| (o.op.phase(), o.seq));

    for op in &ops {
        if op.state != OpState::Pending {
            continue;
        }
        if plan.skip_risky && op.risky.is_some() {
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

        // 1 件ごとに「始めた（running）」を DB に確定してから動かす。
        plan::set_op_state(db, plan_id, op.seq, OpState::Running, None, None, None)
            .map_err(|e| e.to_string())?;

        match run_one_op(plan_id, op, drive_mount.as_deref(), &ctx.data_dir) {
            Ok(OpExecOutcome::Done { exact_dst }) => {
                if op.op == OpKind::Mkdir {
                    plan::set_op_result_exact_dst(
                        db,
                        plan_id,
                        op.seq,
                        OpState::Done,
                        exact_dst.as_deref(),
                        None,
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
                        None,
                        Some(db::now_ms()),
                    )
                    .map_err(|e| e.to_string())?;
                }
            }
            Ok(OpExecOutcome::Skipped(reason)) => {
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

    let touched = touched_folders(&final_ops);
    Ok(ApplyReport {
        plan_id,
        state,
        done,
        skipped,
        failed,
        touched_folders: touched,
    })
}

enum OpExecOutcome {
    Done {
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
                return Ok(OpExecOutcome::Done { exact_dst: None });
            }
            // 新しく作られる一番浅いフォルダを覚えておく（取り消しで元からあった親まで消さないため）。
            let created_root = shallowest_missing_dir(&op.src);
            fs::create_dir_all(&op.src).map_err(|e| format!("フォルダを作れませんでした: {e}"))?;
            Ok(OpExecOutcome::Done {
                exact_dst: created_root,
            })
        }
        OpKind::Move => {
            let dst = op
                .dst
                .as_deref()
                .ok_or_else(|| "行き先がありません".to_string())?;
            if fs::symlink_metadata(dst).is_ok() {
                return Ok(OpExecOutcome::Skipped(
                    "行き先にすでにファイルやフォルダがあります".into(),
                ));
            }
            let dst_parent = dst
                .parent()
                .ok_or_else(|| "行き先の親フォルダがありません".to_string())?;
            if quarantine::same_volume(&op.src, dst_parent) {
                match fs::rename(&op.src, dst) {
                    Ok(()) => return Ok(OpExecOutcome::Done { exact_dst: None }),
                    Err(e) if is_cross_device_error(&e) => {
                        // 別ボリューム扱いだったのでコピーへ切り替える。
                    }
                    Err(e) => return Err(format!("移動できませんでした: {e}")),
                }
            }
            copy_verify_and_quarantine(plan_id, op.seq, &op.src, dst, drive_mount, data_dir)?;
            Ok(OpExecOutcome::Done { exact_dst: None })
        }
        OpKind::Trash => {
            let quarantined = quarantine::move_to_quarantine(
                plan_id,
                op.seq,
                OpKind::Trash,
                &op.src,
                drive_mount,
                data_dir,
            )?;
            Ok(OpExecOutcome::Done {
                exact_dst: Some(quarantined),
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
                let src_exists = fs::symlink_metadata(&op.src).is_ok();
                let dst = op.dst.as_deref();
                let dst_exists = dst.is_some_and(|d| fs::symlink_metadata(d).is_ok());
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
                        if entries_match_content(&op.src, dst) {
                            let mount = drive_mount_of(drives, &op);
                            match quarantine::move_to_quarantine(
                                plan_id,
                                op.seq,
                                OpKind::Move,
                                &op.src,
                                mount.as_deref(),
                                &ctx.data_dir,
                            ) {
                                Ok(_) => {
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
                                Err(e) => {
                                    plan::set_op_state(
                                        db,
                                        plan_id,
                                        op.seq,
                                        OpState::Failed,
                                        None,
                                        Some(&format!("中断後の隔離に失敗: {e}")),
                                        Some(now),
                                    )
                                    .map_err(|e| e.to_string())?;
                                }
                            }
                        } else {
                            plan::set_op_state(
                                db,
                                plan_id,
                                op.seq,
                                OpState::Failed,
                                None,
                                Some("中断後に元と行き先の両方があり中身が一致しません"),
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
                        plan::set_op_state(
                            db,
                            plan_id,
                            op.seq,
                            OpState::Done,
                            Some(&dest),
                            None,
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
    let _lock = PlanLock::acquire(&ctx.data_dir, plan_id)?;

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

    let mut ops = plan::plan_ops(db, plan_id).map_err(|e| e.to_string())?;
    // 実行の逆の順: (phase, seq) の降順。
    ops.sort_by_key(|a| std::cmp::Reverse((a.op.phase(), a.seq)));

    let mut undone = 0usize;
    let mut skipped = Vec::new();
    let mut undone_ops = Vec::new();

    for op in ops.into_iter().filter(|o| o.state == OpState::Done) {
        match undo_one_op(&plan, &op) {
            Ok(()) => {
                plan::set_op_state(
                    db,
                    plan_id,
                    op.seq,
                    OpState::Undone,
                    None,
                    None,
                    Some(db::now_ms()),
                )
                .map_err(|e| e.to_string())?;
                undone += 1;
                let mut marked = op;
                marked.state = OpState::Undone;
                undone_ops.push(marked);
            }
            Err(reason) => {
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

fn undo_one_op(plan: &plan::Plan, op: &PlanOp) -> Result<(), String> {
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
            fs::rename(q_path, &op.src).map_err(|e| format!("隔離から戻せませんでした: {e}"))?;
            Ok(())
        }
        OpKind::Move => {
            let Some(dst) = op.dst.as_deref() else {
                return Err("移動先の記録がありません".into());
            };
            let dst_meta = fs::symlink_metadata(dst)
                .map_err(|_| "移動先にファイルやフォルダがありません".to_string())?;
            if fs::symlink_metadata(&op.src).is_ok() {
                return Err("戻す先にすでにファイルやフォルダがあります".into());
            }
            verify_unchanged_for_undo(op, &dst_meta)?;
            let Some(parent) = op.src.parent() else {
                return Err("戻す先の親フォルダがありません".into());
            };
            if !parent.is_dir() {
                return Err("戻す先の親フォルダがありません".into());
            }
            if quarantine::same_volume(dst, parent) {
                match fs::rename(dst, &op.src) {
                    Ok(()) => return Ok(()),
                    Err(e) if is_cross_device_error(&e) => {}
                    Err(e) => return Err(format!("元の場所へ戻せませんでした: {e}")),
                }
            }
            // 別ボリュームの移動を戻す: コピー → blake3 で確かめる → 移動先を消す。
            copy_verify_and_remove_src(plan.id, op.seq, dst, &op.src)?;
            Ok(())
        }
        OpKind::Mkdir => {
            // 元からあったフォルダ（dst が None）は消さない。
            let Some(created_root) = op.dst.as_deref() else {
                return Ok(());
            };
            match fs::symlink_metadata(&op.src) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
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
            Ok(())
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

/// 別ボリュームへの移動: コピー（一時名）→ blake3 で確かめる → 行き先へ rename → 元を隔離。
pub(crate) fn copy_verify_and_quarantine(
    plan_id: i64,
    seq: i64,
    src: &Path,
    dst: &Path,
    drive_mount: Option<&Path>,
    data_dir: &Path,
) -> Result<PathBuf, String> {
    let staging = staging_path(dst, plan_id, seq)?;
    let _ = remove_any(&staging);

    if let Err(e) = copy_and_verify_entry(src, &staging) {
        let _ = remove_any(&staging);
        return Err(e);
    }
    if fs::symlink_metadata(dst).is_ok() {
        let _ = remove_any(&staging);
        return Err("行き先にすでにファイルやフォルダがあります".into());
    }
    if let Err(e) = fs::rename(&staging, dst) {
        let _ = remove_any(&staging);
        return Err(format!("行き先へ置けませんでした: {e}"));
    }

    quarantine::move_to_quarantine(plan_id, seq, OpKind::Move, src, drive_mount, data_dir)
}

/// 別ボリュームからの取り消し: コピー → blake3 で確かめる → 元の場所へ rename → 移動先を消す。
fn copy_verify_and_remove_src(
    plan_id: i64,
    seq: i64,
    src: &Path,
    dst: &Path,
) -> Result<(), String> {
    let staging = staging_path(dst, plan_id, seq)?;
    let _ = remove_any(&staging);

    if let Err(e) = copy_and_verify_entry(src, &staging) {
        let _ = remove_any(&staging);
        return Err(e);
    }
    if fs::symlink_metadata(dst).is_ok() {
        let _ = remove_any(&staging);
        return Err("戻す先にすでにファイルやフォルダがあります".into());
    }
    if let Err(e) = fs::rename(&staging, dst) {
        let _ = remove_any(&staging);
        return Err(format!("戻す先へ置けませんでした: {e}"));
    }
    remove_any(src).map_err(|e| format!("移動先の掃除に失敗しました: {e}"))?;
    Ok(())
}

fn staging_path(dst: &Path, plan_id: i64, seq: i64) -> Result<PathBuf, String> {
    let parent = dst
        .parent()
        .ok_or_else(|| "行き先の親フォルダがありません".to_string())?;
    Ok(parent.join(format!(
        ".lisa-moving-{plan_id}-{seq}-{}",
        std::process::id()
    )))
}

fn copy_and_verify_entry(src: &Path, dst: &Path) -> Result<(), String> {
    let meta = fs::symlink_metadata(src).map_err(|e| format!("元を読めません: {e}"))?;
    let kind = check::meta_kind(&meta);
    match kind {
        Kind::File => {
            let expected = copy_file_hashing(src, dst, &meta)?;
            let actual = hash::hash_file(dst)
                .map_err(|e| format!("コピー先のハッシュを確かめられません: {e}"))?;
            if actual != expected {
                return Err("コピー先の blake3 が元と合いません".into());
            }
            Ok(())
        }
        Kind::Dir => {
            fs::create_dir(dst).map_err(|e| format!("フォルダを作れません: {e}"))?;
            for item in fs::read_dir(src).map_err(|e| format!("フォルダを読めません: {e}"))?
            {
                let item = item.map_err(|e| format!("中身を読めません: {e}"))?;
                copy_and_verify_entry(&item.path(), &dst.join(item.file_name()))?;
            }
            Ok(())
        }
        Kind::Symlink => {
            let target = fs::read_link(src).map_err(|e| format!("リンク先を読めません: {e}"))?;
            create_symlink(&target, dst)?;
            Ok(())
        }
        Kind::Other => Err("特殊なファイルは別ボリュームへ移せません".into()),
    }
}

fn copy_file_hashing(
    src: &Path,
    dst: &Path,
    src_meta: &fs::Metadata,
) -> Result<hash::Hash, String> {
    let mut reader = File::open(src).map_err(|e| format!("元を開けません: {e}"))?;
    let mut writer = File::create(dst).map_err(|e| format!("コピー先を作れません: {e}"))?;
    let mut hasher = hash::Hasher::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("元の読み込みに失敗しました: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        writer
            .write_all(&buf[..n])
            .map_err(|e| format!("コピー先への書き込みに失敗しました: {e}"))?;
    }
    writer
        .flush()
        .map_err(|e| format!("コピー先の書き込み完了に失敗しました: {e}"))?;
    if let Ok(mtime) = src_meta.modified() {
        let _ = writer.set_modified(mtime);
    }
    Ok(*hasher.finalize().as_bytes())
}

fn create_symlink(target: &Path, dst: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, dst)
            .map_err(|e| format!("リンクを作れませんでした: {e}"))
    }
    #[cfg(windows)]
    {
        let res = if target.is_dir() {
            std::os::windows::fs::symlink_dir(target, dst)
        } else {
            std::os::windows::fs::symlink_file(target, dst)
        };
        res.map_err(|e| format!("リンクを作れませんでした: {e}"))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (target, dst);
        Err("リンクのコピーに対応していません".into())
    }
}

fn remove_any(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// 落ちたあとの再開用: 2 つのパスの中身（ファイルは blake3、フォルダは配下のファイル）が一致するか。
fn entries_match_content(a: &Path, b: &Path) -> bool {
    let (Ok(ma), Ok(mb)) = (fs::symlink_metadata(a), fs::symlink_metadata(b)) else {
        return false;
    };
    let ka = check::meta_kind(&ma);
    if ka != check::meta_kind(&mb) {
        return false;
    }
    match ka {
        Kind::File => ma.len() == mb.len() && hash::hash_file(a).ok() == hash::hash_file(b).ok(),
        Kind::Symlink => fs::read_link(a).ok() == fs::read_link(b).ok(),
        Kind::Dir => {
            let (Ok(ra), Ok(rb)) = (fs::read_dir(a), fs::read_dir(b)) else {
                return false;
            };
            let mut na: Vec<_> = ra.filter_map(|e| e.ok().map(|e| e.file_name())).collect();
            let mut nb: Vec<_> = rb.filter_map(|e| e.ok().map(|e| e.file_name())).collect();
            na.sort();
            nb.sort();
            if na != nb {
                return false;
            }
            na.iter()
                .all(|name| entries_match_content(&a.join(name), &b.join(name)))
        }
        Kind::Other => false,
    }
}
