//! 整理の計画（`plan`、`plan_op` の表）の型と DB 操作。

use std::path::{Path, PathBuf};

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::db::{self, Db};
use crate::hash::Hash;

use super::check::{self, PreparedOp};
use super::{Context, PENDING_EXPIRE_MS};

/// 計画に渡す 1 件の操作（入力の形）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum OpInput {
    Mkdir {
        path: PathBuf,
    },
    Move {
        from: PathBuf,
        to: PathBuf,
    },
    Trash {
        path: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        keep: Option<PathBuf>,
    },
}

/// 操作の種類。実行の順は `Mkdir` → `Move` → `Trash`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpKind {
    Mkdir,
    Move,
    Trash,
}

impl OpKind {
    pub fn as_str(self) -> &'static str {
        match self {
            OpKind::Mkdir => "mkdir",
            OpKind::Move => "move",
            OpKind::Trash => "trash",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        match s {
            "mkdir" => Some(OpKind::Mkdir),
            "move" => Some(OpKind::Move),
            "trash" => Some(OpKind::Trash),
            _ => None,
        }
    }

    /// 実行の順番（数字が小さい方が先）。
    pub fn phase(self) -> u8 {
        match self {
            OpKind::Mkdir => 0,
            OpKind::Move => 1,
            OpKind::Trash => 2,
        }
    }
}

/// 計画の状態（`plan.state`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanState {
    Pending,
    Approved,
    Running,
    Done,
    Partial,
    Rejected,
    Cancelled,
    Expired,
    Undone,
}

impl PlanState {
    pub fn as_str(self) -> &'static str {
        match self {
            PlanState::Pending => "pending",
            PlanState::Approved => "approved",
            PlanState::Running => "running",
            PlanState::Done => "done",
            PlanState::Partial => "partial",
            PlanState::Rejected => "rejected",
            PlanState::Cancelled => "cancelled",
            PlanState::Expired => "expired",
            PlanState::Undone => "undone",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(PlanState::Pending),
            "approved" => Some(PlanState::Approved),
            "running" => Some(PlanState::Running),
            "done" => Some(PlanState::Done),
            "partial" => Some(PlanState::Partial),
            "rejected" => Some(PlanState::Rejected),
            "cancelled" => Some(PlanState::Cancelled),
            "expired" => Some(PlanState::Expired),
            "undone" => Some(PlanState::Undone),
            _ => None,
        }
    }

    /// もう状態が進まない（待っても変わらない）か。
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            PlanState::Done
                | PlanState::Partial
                | PlanState::Rejected
                | PlanState::Cancelled
                | PlanState::Expired
                | PlanState::Undone
        )
    }
}

/// 1 件の操作の状態（`plan_op.state`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpState {
    Pending,
    Running,
    Done,
    Skipped,
    Failed,
    Undone,
}

impl OpState {
    pub fn as_str(self) -> &'static str {
        match self {
            OpState::Pending => "pending",
            OpState::Running => "running",
            OpState::Done => "done",
            OpState::Skipped => "skipped",
            OpState::Failed => "failed",
            OpState::Undone => "undone",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(OpState::Pending),
            "running" => Some(OpState::Running),
            "done" => Some(OpState::Done),
            "skipped" => Some(OpState::Skipped),
            "failed" => Some(OpState::Failed),
            "undone" => Some(OpState::Undone),
            _ => None,
        }
    }
}

/// 計画を作るときの確かめの結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckStatus {
    Ok,
    Risky(String),
    Refused(String),
}

impl CheckStatus {
    /// `ok` / `risky:<理由>` / `refused:<理由>` の形にする。
    pub fn as_check_str(&self) -> String {
        match self {
            CheckStatus::Ok => "ok".to_string(),
            CheckStatus::Risky(reason) => format!("risky:{reason}"),
            CheckStatus::Refused(reason) => format!("refused:{reason}"),
        }
    }

    pub fn is_refused(&self) -> bool {
        matches!(self, CheckStatus::Refused(_))
    }

    pub fn risky_reason(&self) -> Option<&str> {
        match self {
            CheckStatus::Risky(r) => Some(r.as_str()),
            _ => None,
        }
    }
}

/// 計画を作ったときの 1 件ごとの確かめの結果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpCheck {
    pub seq: i64,
    pub op: OpKind,
    pub src: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dst: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drive_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rel_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// `ok` / `risky:<理由>` / `refused:<理由>`。
    pub check: String,
}

/// 計画の要約（`plan.summary` の JSON）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanSummary {
    pub total: usize,
    pub mkdir: usize,
    #[serde(rename = "move")]
    pub move_ops: usize,
    pub trash: usize,
    pub ok: usize,
    pub risky: usize,
    pub refused: usize,
    pub move_bytes: u64,
    pub free_bytes: u64,
}

/// `create_plan` の返事。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateResult {
    /// 計画を DB に残したときの番号。`dry_run` や `refused` があるときは `None`。
    pub id: Option<i64>,
    pub created: bool,
    pub dry_run: bool,
    pub summary: PlanSummary,
    pub checks: Vec<OpCheck>,
}

/// DB の `plan` の 1 行。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub id: i64,
    pub state: PlanState,
    pub by: String,
    pub note: Option<String>,
    pub summary: PlanSummary,
    pub skip_risky: bool,
    pub created_at: i64,
    pub decided_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub purged_at: Option<i64>,
}

/// DB の `plan_op` の 1 行。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanOp {
    pub plan_id: i64,
    pub seq: i64,
    pub op: OpKind,
    pub drive_id: Option<String>,
    pub src: PathBuf,
    pub dst: Option<PathBuf>,
    pub keep: Option<PathBuf>,
    pub size: Option<u64>,
    pub mtime: Option<i64>,
    #[serde(skip_serializing)]
    pub hash: Option<Hash>,
    pub risky: Option<String>,
    pub state: OpState,
    pub reason: Option<String>,
    pub done_at: Option<i64>,
}

const PLAN_COLUMNS: &str =
    "id, state, by, note, summary, skip_risky, created_at, decided_at, finished_at, purged_at";

const PLAN_OP_COLUMNS: &str =
    "plan_id, seq, op, drive_id, src, dst, keep, size, mtime, hash, risky, state, reason, done_at";

fn row_to_plan(r: &rusqlite::Row<'_>) -> db::Result<Plan> {
    let state_str: String = r.get(1)?;
    let state = PlanState::from_name(&state_str).unwrap_or(PlanState::Pending);
    let summary_str: String = r.get(4)?;
    let summary = serde_json::from_str::<PlanSummary>(&summary_str).unwrap_or_default();
    Ok(Plan {
        id: r.get(0)?,
        state,
        by: r.get(2)?,
        note: r.get(3)?,
        summary,
        skip_risky: r.get::<_, i64>(5)? != 0,
        created_at: r.get(6)?,
        decided_at: r.get(7)?,
        finished_at: r.get(8)?,
        purged_at: r.get(9)?,
    })
}

fn row_to_plan_op(r: &rusqlite::Row<'_>) -> db::Result<PlanOp> {
    let op_str: String = r.get(2)?;
    let op = OpKind::from_name(&op_str).unwrap_or(OpKind::Move);
    let state_str: String = r.get(11)?;
    let state = OpState::from_name(&state_str).unwrap_or(OpState::Pending);
    let hash = r
        .get::<_, Option<Vec<u8>>>(9)?
        .and_then(|b| b.try_into().ok());
    Ok(PlanOp {
        plan_id: r.get(0)?,
        seq: r.get(1)?,
        op,
        drive_id: r.get(3)?,
        src: PathBuf::from(r.get::<_, String>(4)?),
        dst: r.get::<_, Option<String>>(5)?.map(PathBuf::from),
        keep: r.get::<_, Option<String>>(6)?.map(PathBuf::from),
        size: r.get::<_, Option<i64>>(7)?.map(|s| s.max(0) as u64),
        mtime: r.get(8)?,
        hash,
        risky: r.get(10)?,
        state,
        reason: r.get(12)?,
        done_at: r.get(13)?,
    })
}

/// 操作の並びを確かめ、問題がなければ（かつ `dry_run` でなければ）`pending` の計画として DB に残す。
/// `refused` が 1 つでもあれば計画を作らず、理由だけを返す。ファイルは何も動かさない。
pub fn create_plan(
    db: &mut Db,
    ctx: &Context,
    by: &str,
    note: Option<&str>,
    ops: &[OpInput],
    dry_run: bool,
) -> Result<CreateResult, String> {
    let prepared = check::prepare_ops(db, ctx, ops)?;
    let summary = summarize(&prepared);
    let checks: Vec<OpCheck> = prepared.iter().map(PreparedOp::to_check).collect();

    if dry_run || summary.refused > 0 {
        return Ok(CreateResult {
            id: None,
            created: false,
            dry_run,
            summary,
            checks,
        });
    }

    let now = db::now_ms();
    let summary_json = serde_json::to_string(&summary).map_err(|e| e.to_string())?;
    let tx = db.conn_mut().transaction().map_err(|e| e.to_string())?;
    let plan_id: i64 = tx
        .query_row(
            "INSERT INTO plan (state, by, note, summary, skip_risky, created_at)
             VALUES (?1, ?2, ?3, ?4, 0, ?5) RETURNING id",
            params![PlanState::Pending.as_str(), by, note, summary_json, now],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;

    {
        let mut stmt = tx
            .prepare(
                "INSERT INTO plan_op
                 (plan_id, seq, op, drive_id, src, dst, keep, size, mtime, hash, risky, state)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            )
            .map_err(|e| e.to_string())?;
        for op in &prepared {
            let src_s = path_to_string(&op.src);
            let dst_s = op.dst.as_deref().map(path_to_string);
            let keep_s = op.keep.as_deref().map(path_to_string);
            let hash_slice = op.hash.as_ref().map(|h| &h[..]);
            stmt.execute(params![
                plan_id,
                op.seq,
                op.op.as_str(),
                op.drive_id,
                src_s,
                dst_s,
                keep_s,
                op.size.map(|s| s as i64),
                op.mtime,
                hash_slice,
                op.status.risky_reason(),
                OpState::Pending.as_str(),
            ])
            .map_err(|e| e.to_string())?;
        }
    }
    tx.commit().map_err(|e| e.to_string())?;

    Ok(CreateResult {
        id: Some(plan_id),
        created: true,
        dry_run: false,
        summary,
        checks,
    })
}

fn summarize(prepared: &[PreparedOp]) -> PlanSummary {
    let mut s = PlanSummary {
        total: prepared.len(),
        ..PlanSummary::default()
    };
    for op in prepared {
        match op.op {
            OpKind::Mkdir => s.mkdir += 1,
            OpKind::Move => {
                s.move_ops += 1;
                if !op.status.is_refused() {
                    s.move_bytes = s.move_bytes.saturating_add(op.size.unwrap_or(0));
                }
            }
            OpKind::Trash => {
                s.trash += 1;
                if !op.status.is_refused() {
                    s.free_bytes = s.free_bytes.saturating_add(op.size.unwrap_or(0));
                }
            }
        }
        match op.status {
            CheckStatus::Ok => s.ok += 1,
            CheckStatus::Risky(_) => s.risky += 1,
            CheckStatus::Refused(_) => s.refused += 1,
        }
    }
    s
}

/// 計画と、その中の操作の一覧（`seq` 順）を読む。無ければ `None`。
pub fn get_plan(db: &Db, id: i64) -> db::Result<Option<(Plan, Vec<PlanOp>)>> {
    let plan = db
        .conn()
        .query_row(
            &format!("SELECT {PLAN_COLUMNS} FROM plan WHERE id = ?1"),
            [id],
            row_to_plan,
        )
        .optional()?;
    let Some(plan) = plan else {
        return Ok(None);
    };
    let ops = plan_ops(db, id)?;
    Ok(Some((plan, ops)))
}

/// 計画 `id` の操作だけを読む（`seq` 順）。
pub fn plan_ops(db: &Db, id: i64) -> db::Result<Vec<PlanOp>> {
    let mut stmt = db.conn().prepare(&format!(
        "SELECT {PLAN_OP_COLUMNS} FROM plan_op WHERE plan_id = ?1 ORDER BY seq"
    ))?;
    let rows = stmt.query_map([id], row_to_plan_op)?;
    rows.collect()
}

/// 計画の一覧を新しい順で返す。`state` を渡せばその状態だけ。
pub fn list_plans(db: &Db, state: Option<PlanState>, limit: usize) -> db::Result<Vec<Plan>> {
    let limit = limit.max(1) as i64;
    match state {
        Some(st) => {
            let mut stmt = db.conn().prepare(&format!(
                "SELECT {PLAN_COLUMNS} FROM plan WHERE state = ?1 ORDER BY id DESC LIMIT ?2"
            ))?;
            let rows = stmt.query_map(params![st.as_str(), limit], row_to_plan)?;
            rows.collect()
        }
        None => {
            let mut stmt = db.conn().prepare(&format!(
                "SELECT {PLAN_COLUMNS} FROM plan ORDER BY id DESC LIMIT ?1"
            ))?;
            let rows = stmt.query_map([limit], row_to_plan)?;
            rows.collect()
        }
    }
}

/// 24 時間以上 `pending` のままの計画を `expired` にする。変えた件数を返す。
pub fn expire_stale_plans(db: &mut Db) -> db::Result<usize> {
    expire_stale_plans_at(db, db::now_ms())
}

/// 時刻 `now_ms` を基準に期限切れの `pending` を `expired` にする（テスト用にも使う）。
pub fn expire_stale_plans_at(db: &mut Db, now_ms: i64) -> db::Result<usize> {
    let cutoff = now_ms.saturating_sub(PENDING_EXPIRE_MS);
    db.conn().execute(
        "UPDATE plan SET state = ?1, decided_at = ?2
         WHERE state = ?3 AND created_at <= ?4",
        params![
            PlanState::Expired.as_str(),
            now_ms,
            PlanState::Pending.as_str(),
            cutoff
        ],
    )
}

/// 許可待ちの計画を許可する（`pending` → `approved`）。実行はまだしない。
pub fn approve_plan(db: &mut Db, id: i64, skip_risky: bool) -> Result<Plan, String> {
    let _ = expire_stale_plans(db);
    let now = db::now_ms();
    let plan = load_plan_required(db, id)?;
    if plan.state != PlanState::Pending {
        return Err(format!(
            "計画 #{id} は許可待ちではありません（今の状態: {}）",
            plan.state.as_str()
        ));
    }
    let changed = db
        .conn()
        .execute(
            "UPDATE plan SET state = ?1, skip_risky = ?2, decided_at = ?3
             WHERE id = ?4 AND state = ?5",
            params![
                PlanState::Approved.as_str(),
                i64::from(skip_risky),
                now,
                id,
                PlanState::Pending.as_str(),
            ],
        )
        .map_err(|e| e.to_string())?;
    if changed == 0 {
        return Err(format!("計画 #{id} の状態が変わりました"));
    }
    load_plan_required(db, id)
}

/// 許可待ちの計画を断る（`pending` → `rejected`）。
pub fn reject_plan(db: &mut Db, id: i64) -> Result<Plan, String> {
    transition_pending(db, id, PlanState::Rejected)
}

/// 許可待ちの計画を取り下げる（`pending` → `cancelled`）。
pub fn cancel_plan(db: &mut Db, id: i64) -> Result<Plan, String> {
    transition_pending(db, id, PlanState::Cancelled)
}

fn transition_pending(db: &mut Db, id: i64, next: PlanState) -> Result<Plan, String> {
    let _ = expire_stale_plans(db);
    let now = db::now_ms();
    let plan = load_plan_required(db, id)?;
    if plan.state != PlanState::Pending {
        return Err(format!(
            "計画 #{id} は許可待ちではありません（今の状態: {}）",
            plan.state.as_str()
        ));
    }
    let changed = db
        .conn()
        .execute(
            "UPDATE plan SET state = ?1, decided_at = ?2
             WHERE id = ?3 AND state = ?4",
            params![next.as_str(), now, id, PlanState::Pending.as_str()],
        )
        .map_err(|e| e.to_string())?;
    if changed == 0 {
        return Err(format!("計画 #{id} の状態が変わりました"));
    }
    load_plan_required(db, id)
}

pub(crate) fn load_plan_required(db: &Db, id: i64) -> Result<Plan, String> {
    db.conn()
        .query_row(
            &format!("SELECT {PLAN_COLUMNS} FROM plan WHERE id = ?1"),
            [id],
            row_to_plan,
        )
        .optional()
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("計画 #{id} が見つかりません"))
}

pub(crate) fn set_plan_state(
    db: &mut Db,
    id: i64,
    state: PlanState,
    finished_at: Option<i64>,
) -> db::Result<()> {
    match finished_at {
        Some(ts) => {
            db.conn().execute(
                "UPDATE plan SET state = ?1, finished_at = ?2 WHERE id = ?3",
                params![state.as_str(), ts, id],
            )?;
        }
        None => {
            db.conn().execute(
                "UPDATE plan SET state = ?1 WHERE id = ?2",
                params![state.as_str(), id],
            )?;
        }
    }
    Ok(())
}

pub(crate) fn set_plan_purged(db: &mut Db, id: i64, purged_at: i64) -> db::Result<()> {
    db.conn().execute(
        "UPDATE plan SET purged_at = ?1 WHERE id = ?2",
        params![purged_at, id],
    )?;
    Ok(())
}

pub(crate) fn set_op_state(
    db: &mut Db,
    plan_id: i64,
    seq: i64,
    state: OpState,
    dst: Option<&Path>,
    reason: Option<&str>,
    done_at: Option<i64>,
) -> db::Result<()> {
    let dst_s = dst.map(path_to_string);
    db.conn().execute(
        "UPDATE plan_op
         SET state = ?1,
             dst = COALESCE(?2, dst),
             reason = ?3,
             done_at = ?4
         WHERE plan_id = ?5 AND seq = ?6",
        params![state.as_str(), dst_s, reason, done_at, plan_id, seq],
    )?;
    Ok(())
}

/// `mkdir` がすでにあったフォルダで `dst` を空にしたいときなど、明示的に `dst` を書き換える。
pub(crate) fn set_op_result_exact_dst(
    db: &mut Db,
    plan_id: i64,
    seq: i64,
    state: OpState,
    dst: Option<&Path>,
    reason: Option<&str>,
    done_at: Option<i64>,
) -> db::Result<()> {
    let dst_s = dst.map(path_to_string);
    db.conn().execute(
        "UPDATE plan_op
         SET state = ?1, dst = ?2, reason = ?3, done_at = ?4
         WHERE plan_id = ?5 AND seq = ?6",
        params![state.as_str(), dst_s, reason, done_at, plan_id, seq],
    )?;
    Ok(())
}

pub(crate) fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
