//! 整理の計画の確かめ（作るときと、実行の直前の 2 回）。
//!
//! - 守る場所（OS・アプリ、Lisa のデータのフォルダ、隔離フォルダ、ドライブの一番上、クラウドのみ、別ボリューム）は断る。
//! - 同じパスを 2 回触る計画、`to` が他の `from` の中に入る計画は断る。
//! - 危ないもの（最後の 1 つ、役割が `data` の中の `trash` や `data` フォルダの `move`、ハッシュの無い `trash`）に印を付ける。
//!   ただし `cache`・`install`・`log` の中の `trash` は危なくない。

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use rusqlite::{OptionalExtension, params};

use crate::catalog::db::DbTree;
use crate::catalog::{self, CatalogSet, Role};
use crate::db::{Db, Stored};
use crate::hash::{self, Hash};
use crate::listing::{self, Kind};
use crate::name;
use crate::query::{self, Drive, Located};

use super::plan::{CheckStatus, OpCheck, OpInput, OpKind, PlanOp};
use super::{Context, MAX_OPS};

/// 確かめを終えた 1 件（DB に書く前の形）。
#[derive(Debug, Clone)]
pub(crate) struct PreparedOp {
    pub seq: i64,
    pub op: OpKind,
    pub drive_id: Option<String>,
    pub rel_path: Option<String>,
    pub src: PathBuf,
    pub dst: Option<PathBuf>,
    pub keep: Option<PathBuf>,
    pub size: Option<u64>,
    pub mtime: Option<i64>,
    pub hash: Option<Hash>,
    pub status: CheckStatus,
}

impl PreparedOp {
    pub fn to_check(&self) -> OpCheck {
        OpCheck {
            seq: self.seq,
            op: self.op,
            src: self.src.clone(),
            dst: self.dst.clone(),
            keep: self.keep.clone(),
            drive_id: self.drive_id.clone(),
            rel_path: self.rel_path.clone(),
            size: self.size,
            check: self.status.as_check_str(),
        }
    }
}

/// 1 件の解決済み情報（衝突チェックと本体チェックの中間表現）。
struct ResolvedInput {
    seq: i64,
    op: OpKind,
    src: PathBuf,
    dst: Option<PathBuf>,
    keep: Option<PathBuf>,
    drive_id: Option<String>,
    mount: Option<PathBuf>,
    rel_path: Option<String>,
    located: Option<Located>,
    early_refused: Option<String>,
}

/// 計画の操作をすべて確かめて返す。操作が空、または上限を超えるときは Err。
pub(crate) fn prepare_ops(
    db: &Db,
    ctx: &Context,
    ops: &[OpInput],
    remote_keep: bool,
) -> Result<Vec<PreparedOp>, String> {
    if ops.is_empty() {
        return Err("操作が 1 件もありません".into());
    }
    if ops.len() > MAX_OPS {
        return Err(format!(
            "1 つの計画は {MAX_OPS} 件までです（{} 件渡されました）",
            ops.len()
        ));
    }

    let drives = query::drives(db).map_err(|e| e.to_string())?;
    let home = ctx.home_dir.clone().or_else(default_home_dir);

    // 1. まず全件のパスを正規化し、DB のドライブに解決する。
    let mut resolved = Vec::with_capacity(ops.len());
    for (idx, input) in ops.iter().enumerate() {
        resolved.push(resolve_input(db, &drives, idx as i64, input)?);
    }

    // 2. 計画全体でのパスの衝突を調べる（同じパスを 2 回触る、to が他の from の中に入る等）。
    let conflicts = find_plan_conflicts(&resolved);

    // 3. 同じ計画の mkdir で作られる予定のフォルダを集める（move の親の確かめ用）。
    let planned_mkdirs: Vec<PathBuf> = resolved
        .iter()
        .enumerate()
        .filter(|(i, r)| {
            r.op == OpKind::Mkdir && r.early_refused.is_none() && !conflicts.contains_key(i)
        })
        .map(|(_, r)| r.src.clone())
        .collect();

    // 4. 同じ計画で trash される予定の entry.id を集める（最後の 1 つ判定で除外するため）。
    let trashed_entry_ids: HashSet<i64> = resolved
        .iter()
        .enumerate()
        .filter(|(i, r)| {
            r.op == OpKind::Trash && r.early_refused.is_none() && !conflicts.contains_key(i)
        })
        .filter_map(|(_, r)| r.located.as_ref().and_then(|l| l.chain.last().copied()))
        .collect();
    let trashed_paths: Vec<PathBuf> = resolved
        .iter()
        .enumerate()
        .filter(|(i, r)| {
            r.op == OpKind::Trash && r.early_refused.is_none() && !conflicts.contains_key(i)
        })
        .map(|(_, r)| r.src.clone())
        .collect();

    // 5. 1 件ずつ確かめる。
    let mut out = Vec::with_capacity(resolved.len());
    for (i, mut item) in resolved.into_iter().enumerate() {
        if let Some(reason) = item
            .early_refused
            .take()
            .or_else(|| conflicts.get(&i).cloned())
        {
            out.push(PreparedOp {
                seq: item.seq,
                op: item.op,
                drive_id: item.drive_id,
                rel_path: item.rel_path,
                src: item.src,
                dst: item.dst,
                keep: item.keep,
                size: None,
                mtime: None,
                hash: None,
                status: CheckStatus::Refused(reason),
            });
            continue;
        }
        out.push(check_single_op(
            db,
            ctx,
            &drives,
            home.as_deref(),
            &planned_mkdirs,
            &trashed_entry_ids,
            &trashed_paths,
            remote_keep,
            item,
        )?);
    }

    Ok(out)
}

fn resolve_input(
    db: &Db,
    drives: &[Drive],
    seq: i64,
    input: &OpInput,
) -> Result<ResolvedInput, String> {
    match input {
        OpInput::Mkdir { path } => {
            let clean = match clean_absolute_path(path) {
                Ok(p) => p,
                Err(reason) => {
                    return Ok(ResolvedInput {
                        seq,
                        op: OpKind::Mkdir,
                        src: path.clone(),
                        dst: None,
                        keep: None,
                        drive_id: None,
                        mount: None,
                        rel_path: None,
                        located: None,
                        early_refused: Some(reason),
                    });
                }
            };
            match resolve_drive_path(drives, &clean) {
                Some((drive_id, mount, full)) => {
                    let rel = rel_path_string(&mount, &full);
                    Ok(ResolvedInput {
                        seq,
                        op: OpKind::Mkdir,
                        src: full,
                        dst: None,
                        keep: None,
                        drive_id: Some(drive_id),
                        mount: Some(mount),
                        rel_path: rel,
                        located: None,
                        early_refused: None,
                    })
                }
                None => Ok(ResolvedInput {
                    seq,
                    op: OpKind::Mkdir,
                    src: clean,
                    dst: None,
                    keep: None,
                    drive_id: None,
                    mount: None,
                    rel_path: None,
                    located: None,
                    early_refused: Some("DB のドライブの外です".into()),
                }),
            }
        }
        OpInput::Move { from, to } => {
            let clean_from = match clean_absolute_path(from) {
                Ok(p) => p,
                Err(reason) => {
                    return Ok(ResolvedInput {
                        seq,
                        op: OpKind::Move,
                        src: from.clone(),
                        dst: Some(to.clone()),
                        keep: None,
                        drive_id: None,
                        mount: None,
                        rel_path: None,
                        located: None,
                        early_refused: Some(reason),
                    });
                }
            };
            let clean_to = match clean_absolute_path(to) {
                Ok(p) => p,
                Err(reason) => {
                    return Ok(ResolvedInput {
                        seq,
                        op: OpKind::Move,
                        src: clean_from,
                        dst: Some(to.clone()),
                        keep: None,
                        drive_id: None,
                        mount: None,
                        rel_path: None,
                        located: None,
                        early_refused: Some(format!("行き先: {reason}")),
                    });
                }
            };
            let located = query::locate(db, &clean_from).map_err(|e| e.to_string())?;
            let (src, drive_id, mount, rel_path, early) = match &located {
                Some(loc) => {
                    let mount = drives
                        .iter()
                        .find(|d| d.id == loc.drive_id)
                        .and_then(|d| d.mount.clone());
                    let rel = mount.as_deref().and_then(|m| rel_path_string(m, &loc.path));
                    (
                        loc.path.clone(),
                        Some(loc.drive_id.clone()),
                        mount,
                        rel,
                        None,
                    )
                }
                None => (
                    clean_from.clone(),
                    None,
                    None,
                    None,
                    Some("元が DB にありません".into()),
                ),
            };
            let (dst, dst_err) = match resolve_drive_path(drives, &clean_to) {
                Some((_, _, full)) => (full, None),
                None => (clean_to, Some("行き先が DB のドライブの外です".into())),
            };
            Ok(ResolvedInput {
                seq,
                op: OpKind::Move,
                src,
                dst: Some(dst),
                keep: None,
                drive_id,
                mount,
                rel_path,
                located,
                early_refused: early.or(dst_err),
            })
        }
        OpInput::Trash { path, keep } => {
            let clean = match clean_absolute_path(path) {
                Ok(p) => p,
                Err(reason) => {
                    return Ok(ResolvedInput {
                        seq,
                        op: OpKind::Trash,
                        src: path.clone(),
                        dst: None,
                        keep: keep.clone(),
                        drive_id: None,
                        mount: None,
                        rel_path: None,
                        located: None,
                        early_refused: Some(reason),
                    });
                }
            };
            let keep_clean = match keep {
                Some(k) => match clean_absolute_path(k) {
                    Ok(p) => {
                        let resolved_k = query::locate(db, &p)
                            .map_err(|e| e.to_string())?
                            .map(|l| l.path)
                            .unwrap_or(p);
                        Some(resolved_k)
                    }
                    Err(_) => Some(k.clone()),
                },
                None => None,
            };
            let located = query::locate(db, &clean).map_err(|e| e.to_string())?;
            let (src, drive_id, mount, rel_path, early) = match &located {
                Some(loc) => {
                    let mount = drives
                        .iter()
                        .find(|d| d.id == loc.drive_id)
                        .and_then(|d| d.mount.clone());
                    let rel = mount.as_deref().and_then(|m| rel_path_string(m, &loc.path));
                    (
                        loc.path.clone(),
                        Some(loc.drive_id.clone()),
                        mount,
                        rel,
                        None,
                    )
                }
                None => (clean, None, None, None, Some("元が DB にありません".into())),
            };
            Ok(ResolvedInput {
                seq,
                op: OpKind::Trash,
                src,
                dst: None,
                keep: keep_clean,
                drive_id,
                mount,
                rel_path,
                located,
                early_refused: early,
            })
        }
    }
}

/// 同じパスを 2 回触る・`to` が他の `from` の中に入る等の衝突を検出する。
fn find_plan_conflicts(items: &[ResolvedInput]) -> HashMap<usize, String> {
    let mut conflicts: HashMap<usize, String> = HashMap::new();
    let mut touched_exact: HashMap<PathBuf, usize> = HashMap::new();

    for (i, item) in items.iter().enumerate() {
        if let Some(&prev) = touched_exact.get(&item.src) {
            conflicts
                .entry(i)
                .or_insert_with(|| format!("操作 #{prev} と同じパスを 2 回触っています"));
            conflicts
                .entry(prev)
                .or_insert_with(|| format!("操作 #{i} と同じパスを 2 回触っています"));
        } else {
            touched_exact.insert(item.src.clone(), i);
        }
        if let Some(dst) = &item.dst {
            if *dst == item.src {
                conflicts
                    .entry(i)
                    .or_insert_with(|| "元と行き先が同じパスです".into());
            } else if dst.starts_with(&item.src) {
                conflicts
                    .entry(i)
                    .or_insert_with(|| "行き先が元の中に入っています".into());
            }
            if let Some(&prev) = touched_exact.get(dst) {
                if prev != i {
                    conflicts
                        .entry(i)
                        .or_insert_with(|| format!("操作 #{prev} と同じパスを 2 回触っています"));
                    conflicts
                        .entry(prev)
                        .or_insert_with(|| format!("操作 #{i} と同じパスを 2 回触っています"));
                }
            } else {
                touched_exact.insert(dst.clone(), i);
            }
        }
    }

    // 移動元・隔離元（move/trash の src）の中に入る操作（to や他の src）を弾く。
    for (i, a) in items.iter().enumerate() {
        if !matches!(a.op, OpKind::Move | OpKind::Trash) {
            continue;
        }
        for (j, b) in items.iter().enumerate() {
            if i == j {
                continue;
            }
            if let Some(dst) = &b.dst
                && dst.starts_with(&a.src)
                && *dst != a.src
            {
                conflicts
                    .entry(j)
                    .or_insert_with(|| format!("行き先が操作 #{i} の対象の中に入っています"));
            }
            if b.src.starts_with(&a.src) && b.src != a.src {
                conflicts
                    .entry(j)
                    .or_insert_with(|| format!("操作 #{i} の対象の中をもう一度触っています"));
            }
        }
    }

    conflicts
}

#[allow(clippy::too_many_arguments)]
fn check_single_op(
    db: &Db,
    ctx: &Context,
    drives: &[Drive],
    home: Option<&Path>,
    planned_mkdirs: &[PathBuf],
    trashed_entry_ids: &HashSet<i64>,
    trashed_paths: &[PathBuf],
    remote_keep: bool,
    item: ResolvedInput,
) -> Result<PreparedOp, String> {
    match item.op {
        OpKind::Mkdir => {
            if let Some(reason) =
                is_protected_path(&item.src, item.mount.as_deref(), &ctx.data_dir, home, false)
            {
                return Ok(refused(item, reason));
            }
            match fs::symlink_metadata(&item.src) {
                Ok(meta) => {
                    if !meta.is_dir() {
                        return Ok(refused(item, "同じ名前のファイルがあります".into()));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    if let Some(reason) = check_ancestors_are_dirs(&item.src) {
                        return Ok(refused(item, reason));
                    }
                }
                Err(e) => {
                    return Ok(refused(item, format!("場所を確かめられません: {e}")));
                }
            }
            Ok(PreparedOp {
                seq: item.seq,
                op: OpKind::Mkdir,
                drive_id: item.drive_id,
                rel_path: item.rel_path,
                src: item.src,
                dst: None,
                keep: None,
                size: None,
                mtime: None,
                hash: None,
                status: CheckStatus::Ok,
            })
        }
        OpKind::Move => {
            let Some(loc) = item.located.clone() else {
                return Ok(refused(item, "元が DB にありません".into()));
            };
            let Some(dst) = item.dst.clone() else {
                return Ok(refused(item, "行き先がありません".into()));
            };
            let dst_mount = resolve_drive_path(drives, &dst).map(|(_, m, _)| m);
            if let Some(reason) =
                is_protected_path(&item.src, item.mount.as_deref(), &ctx.data_dir, home, true)
            {
                return Ok(refused(item, reason));
            }
            if let Some(reason) =
                is_protected_path(&dst, dst_mount.as_deref(), &ctx.data_dir, home, false)
            {
                return Ok(refused(item, format!("行き先: {reason}")));
            }

            let entry = match check_db_and_disk_source(db, &loc) {
                Ok(v) => v,
                Err(reason) => return Ok(refused(item, reason)),
            };

            // 行き先が無い（上書きしない）。
            match super::move_destination_exists(&item.src, &dst) {
                Ok(false) => {}
                Ok(true) => {
                    return Ok(refused(
                        item,
                        "行き先にすでにファイルやフォルダがあります".into(),
                    ));
                }
                Err(error) => {
                    return Ok(refused(item, format!("行き先を確かめられません: {error}")));
                }
            }
            // 行き先の親があるか、同じ計画の mkdir で作られる。
            let Some(parent) = dst.parent() else {
                return Ok(refused(item, "行き先の親フォルダがありません".into()));
            };
            if !parent_exists_or_planned(parent, planned_mkdirs) {
                return Ok(refused(
                    item,
                    "行き先の親フォルダがありません（同じ計画に mkdir もありません）".into(),
                ));
            }
            if let Some(reason) = check_ancestors_are_dirs(parent) {
                return Ok(refused(item, format!("行き先: {reason}")));
            }

            let size = entry_display_size(db, &entry.stored)?;
            let role_info = role_of_chain(db, &ctx.catalogs, &loc)?;
            let status = if entry.stored.kind == Kind::Dir && role_info.direct == Some(Role::Data) {
                CheckStatus::Risky("カタログの役割が data のフォルダごと動かします".into())
            } else {
                CheckStatus::Ok
            };

            Ok(PreparedOp {
                seq: item.seq,
                op: OpKind::Move,
                drive_id: item.drive_id,
                rel_path: item.rel_path,
                src: item.src,
                dst: Some(dst),
                keep: None,
                size: Some(size),
                mtime: entry.disk_mtime,
                hash: entry.stored.hash,
                status,
            })
        }
        OpKind::Trash => {
            let Some(loc) = item.located.clone() else {
                return Ok(refused(item, "元が DB にありません".into()));
            };
            if let Some(reason) =
                is_protected_path(&item.src, item.mount.as_deref(), &ctx.data_dir, home, true)
            {
                return Ok(refused(item, reason));
            }
            let entry = match check_db_and_disk_source(db, &loc) {
                Ok(v) => v,
                Err(reason) => return Ok(refused(item, reason)),
            };

            let size = entry_display_size(db, &entry.stored)?;
            let role_info = role_of_chain(db, &ctx.catalogs, &loc)?;
            let status = evaluate_trash_risk(
                db,
                &loc,
                &entry.stored,
                item.keep.as_deref(),
                role_info.effective,
                trashed_entry_ids,
                trashed_paths,
                remote_keep,
            )?;

            Ok(PreparedOp {
                seq: item.seq,
                op: OpKind::Trash,
                drive_id: item.drive_id,
                rel_path: item.rel_path,
                src: item.src,
                dst: None,
                keep: item.keep,
                size: Some(size),
                mtime: entry.disk_mtime,
                hash: entry.stored.hash,
                status,
            })
        }
    }
}

fn refused(item: ResolvedInput, reason: String) -> PreparedOp {
    PreparedOp {
        seq: item.seq,
        op: item.op,
        drive_id: item.drive_id,
        rel_path: item.rel_path,
        src: item.src,
        dst: item.dst,
        keep: item.keep,
        size: None,
        mtime: None,
        hash: None,
        status: CheckStatus::Refused(reason),
    }
}

struct VerifiedSource {
    stored: Stored,
    disk_mtime: Option<i64>,
}

/// DB の行とディスク上の実体が合っているか確かめる（ドライブの一番上・クラウドのみ・別ボリュームもここで弾く）。
fn check_db_and_disk_source(db: &Db, loc: &Located) -> Result<VerifiedSource, String> {
    if loc.chain.len() <= 1 {
        return Err("ドライブの一番上は動かせません".into());
    }
    let id = *loc.chain.last().ok_or("DB の行が見つかりません")?;
    let stored = db
        .entry(id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "元が DB にありません".to_string())?;
    if stored.other_volume {
        return Err("別のボリュームの入口は動かせません".into());
    }
    if stored.cloud_only {
        return Err("クラウドにだけあるものは動かせません".into());
    }
    // 祖先に cloud_only や other_volume が立っていないか。
    for &anc in &loc.chain[1..loc.chain.len() - 1] {
        if let Some(a) = db.entry(anc).map_err(|e| e.to_string())? {
            if a.cloud_only {
                return Err("クラウドにだけあるフォルダの中は動かせません".into());
            }
            if a.other_volume {
                return Err("別のボリュームの中は動かせません".into());
            }
        }
    }

    let meta = fs::symlink_metadata(&loc.path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            "ディスクに見つかりません".to_string()
        } else {
            format!("ディスクを確かめられません: {e}")
        }
    })?;
    if listing::cloud_only(&meta) {
        return Err("クラウドにだけあるものは動かせません".into());
    }
    let disk_kind = meta_kind(&meta);
    if disk_kind != stored.kind {
        return Err("種類が DB と違っています".into());
    }
    let disk_mtime = listing::mtime_ms(&meta);
    if stored.kind == Kind::File && meta.len() != stored.size {
        return Err("大きさが DB と違っています（変わっています）".into());
    }
    if stored.mtime.is_some() && disk_mtime != stored.mtime {
        return Err("更新日時が DB と違っています（変わっています）".into());
    }
    Ok(VerifiedSource { stored, disk_mtime })
}

#[allow(clippy::too_many_arguments)]
fn evaluate_trash_risk(
    db: &Db,
    loc: &Located,
    stored: &Stored,
    keep: Option<&Path>,
    effective_role: Option<Role>,
    trashed_entry_ids: &HashSet<i64>,
    trashed_paths: &[PathBuf],
    // 遠隔の残す側は実行直前に予約と完全ハッシュで確認するので、「最後の 1 つ」とは数えない。
    remote_keep: bool,
) -> Result<CheckStatus, String> {
    // cache・install・log の中の trash は危なくない（作り直せる・入れ直せる）。
    if matches!(
        effective_role,
        Some(Role::Cache | Role::Install | Role::Log)
    ) {
        return Ok(CheckStatus::Ok);
    }

    let mut reasons = Vec::new();
    if effective_role == Some(Role::Data) {
        reasons.push("カタログの役割が data の中です".to_string());
    }

    match stored.kind {
        Kind::File | Kind::Dir => match stored.hash {
            None => {
                reasons.push("ハッシュがまだありません（重複か分かりません）".to_string());
            }
            Some(hash) => {
                if let Some(keep_path) = keep {
                    if !is_valid_keep(
                        db,
                        keep_path,
                        stored,
                        &hash,
                        trashed_entry_ids,
                        trashed_paths,
                    )? {
                        reasons.push("残す予定の keep が見つからないか中身が違います".to_string());
                    }
                } else if !remote_keep
                    && !has_other_copy(db, loc, stored, &hash, trashed_entry_ids)?
                {
                    reasons.push("同じ中身の残りが他にありません（最後の 1 つ）".to_string());
                }
            }
        },
        Kind::Symlink | Kind::Other => {}
    }

    if reasons.is_empty() {
        Ok(CheckStatus::Ok)
    } else {
        Ok(CheckStatus::Risky(reasons.join(" / ")))
    }
}

fn is_valid_keep(
    db: &Db,
    keep_path: &Path,
    src_stored: &Stored,
    expected_hash: &Hash,
    trashed_entry_ids: &HashSet<i64>,
    trashed_paths: &[PathBuf],
) -> Result<bool, String> {
    let Some(keep_loc) = query::locate(db, keep_path).map_err(|e| e.to_string())? else {
        return Ok(false);
    };
    let Some(&keep_id) = keep_loc.chain.last() else {
        return Ok(false);
    };
    if keep_id == src_stored.id {
        return Ok(false);
    }
    if keep_loc
        .chain
        .iter()
        .any(|id| trashed_entry_ids.contains(id))
    {
        return Ok(false);
    }
    if trashed_paths
        .iter()
        .any(|tp| keep_loc.path == *tp || keep_loc.path.starts_with(tp))
    {
        return Ok(false);
    }
    let Some(keep_entry) = db.entry(keep_id).map_err(|e| e.to_string())? else {
        return Ok(false);
    };
    if keep_entry.cloud_only || keep_entry.kind != src_stored.kind {
        return Ok(false);
    }
    if keep_entry.hash.as_ref() != Some(expected_hash) {
        return Ok(false);
    }
    // ディスクにも存在しているか確かめる。
    let Ok(meta) = fs::symlink_metadata(&keep_loc.path) else {
        return Ok(false);
    };
    if meta_kind(&meta) != keep_entry.kind {
        return Ok(false);
    }
    if keep_entry.kind == Kind::File && meta.len() != keep_entry.size {
        return Ok(false);
    }
    Ok(true)
}

fn has_other_copy(
    db: &Db,
    loc: &Located,
    stored: &Stored,
    hash: &Hash,
    trashed_entry_ids: &HashSet<i64>,
) -> Result<bool, String> {
    let kind_s = query::kind_str(stored.kind);
    let mut stmt = db
        .conn()
        .prepare("SELECT id FROM entry WHERE hash = ?1 AND kind = ?2 AND cloud_only = 0")
        .map_err(|e| e.to_string())?;
    let ids: Vec<i64> = stmt
        .query_map(params![&hash[..], kind_s], |r| r.get(0))
        .map_err(|e| e.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;

    let src_id = *loc.chain.last().unwrap_or(&stored.id);
    for id in ids {
        if id == src_id {
            continue;
        }
        // 同じ計画で消される予定のもの（またはその中）は残りとして数えない。
        if trashed_entry_ids.contains(&id) {
            continue;
        }
        if let Some(other_loc) = query::path_of(db, id).map_err(|e| e.to_string())?
            && !other_loc
                .chain
                .iter()
                .any(|cid| trashed_entry_ids.contains(cid))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 実行の直前に 1 件をもう一度確かめる。合わなければ `Err(skip の理由)`。
pub(crate) fn recheck_op_before_apply(db: &Db, ctx: &Context, op: &PlanOp) -> Result<(), String> {
    let drives = query::drives(db).map_err(|e| e.to_string())?;
    let home = ctx.home_dir.clone().or_else(default_home_dir);
    let src_mount = op
        .drive_id
        .as_deref()
        .and_then(|id| drives.iter().find(|d| d.id == id))
        .and_then(|d| d.mount.as_deref())
        .or_else(|| {
            resolve_drive_path(&drives, &op.src)
                .map(|(_, _, _)| ())
                .and_then(|()| {
                    drives
                        .iter()
                        .filter_map(|d| d.mount.as_deref())
                        .filter(|m| op.src.starts_with(m))
                        .max_by_key(|m| m.components().count())
                })
        });

    match op.op {
        OpKind::Mkdir => {
            if let Some(reason) =
                is_protected_path(&op.src, src_mount, &ctx.data_dir, home.as_deref(), false)
            {
                return Err(reason);
            }
            match fs::symlink_metadata(&op.src) {
                Ok(meta) if meta.is_dir() => Ok(()),
                Ok(_) => Err("同じ名前のファイルがあります".into()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    if let Some(reason) = check_ancestors_are_dirs(&op.src) {
                        Err(reason)
                    } else {
                        Ok(())
                    }
                }
                Err(e) => Err(format!("場所を確かめられません: {e}")),
            }
        }
        OpKind::Move => {
            let Some(dst) = op.dst.as_deref() else {
                return Err("行き先がありません".into());
            };
            let dst_mount = resolve_drive_path(&drives, dst).map(|(_, m, _)| m);
            if let Some(reason) =
                is_protected_path(&op.src, src_mount, &ctx.data_dir, home.as_deref(), true)
            {
                return Err(reason);
            }
            if let Some(reason) = is_protected_path(
                dst,
                dst_mount.as_deref(),
                &ctx.data_dir,
                home.as_deref(),
                false,
            ) {
                return Err(format!("行き先: {reason}"));
            }
            let loc = query::locate(db, &op.src)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "元が DB にありません".to_string())?;
            let verified = check_db_and_disk_source(db, &loc)?;
            check_matches_recorded(op, &verified)?;

            if super::move_destination_exists(&op.src, dst)
                .map_err(|e| format!("行き先を確かめられません: {e}"))?
            {
                return Err("行き先にすでにファイルやフォルダがあります".into());
            }
            let Some(parent) = dst.parent() else {
                return Err("行き先の親フォルダがありません".into());
            };
            if !parent.is_dir() {
                return Err("行き先の親フォルダがありません".into());
            }
            Ok(())
        }
        OpKind::Trash => {
            if let Some(reason) =
                is_protected_path(&op.src, src_mount, &ctx.data_dir, home.as_deref(), true)
            {
                return Err(reason);
            }
            let loc = query::locate(db, &op.src)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "元が DB にありません".to_string())?;
            let verified = check_db_and_disk_source(db, &loc)?;
            check_matches_recorded(op, &verified)?;

            // trash のファイルは、実行の直前に中身をハッシュし直して DB（と計画）のハッシュと同じか見る。
            if verified.stored.kind == Kind::File
                && let Some(expected) = op.hash.or(verified.stored.hash)
            {
                let actual = hash::hash_file(&op.src)
                    .map_err(|e| format!("ハッシュを確かめられませんでした: {e}"))?;
                if actual != expected {
                    return Err("ファイルの中身が DB のハッシュと違っています".into());
                }
            }
            // 同じ計画で先に move した keep は、移動先の現物で確かめる。
            if let Some(keep) = op.keep.as_deref() {
                let path = keep_path_for_apply(db, op.plan_id, keep)?
                    .ok_or("残す予定の keep が見つかりません")?;
                if verified.stored.kind == Kind::File
                    && let Some(expected) = op.hash
                {
                    let metadata = fs::symlink_metadata(&path).map_err(|error| {
                        format!("残すファイルを確かめられませんでした: {error}")
                    })?;
                    if !metadata.is_file() || metadata.len() != verified.stored.size {
                        return Err("残すファイルの種類か大きさが変わっています".into());
                    }
                    let actual = hash::hash_file(&path).map_err(|error| {
                        format!("残すファイルのハッシュを確かめられませんでした: {error}")
                    })?;
                    if actual != expected {
                        return Err("残すファイルの中身が変わっています".into());
                    }
                }
            }
            Ok(())
        }
    }
}

fn keep_path_for_apply(db: &Db, plan_id: i64, keep: &Path) -> Result<Option<PathBuf>, String> {
    if fs::symlink_metadata(keep).is_ok() {
        return Ok(Some(keep.to_path_buf()));
    }
    let keep_s = super::plan::path_to_string(keep);
    let moved_dst: Option<String> = db
        .conn()
        .query_row(
            "SELECT dst FROM plan_op
             WHERE plan_id = ?1 AND op = 'move' AND state = 'done' AND src = ?2",
            params![plan_id, keep_s],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?
        .flatten();
    Ok(moved_dst
        .map(PathBuf::from)
        .filter(|path| fs::symlink_metadata(path).is_ok()))
}

/// 計画を作ったときの記録（大きさ・更新日時・ハッシュ）と、今のディスクが合っているか確かめる。
fn check_matches_recorded(op: &PlanOp, verified: &VerifiedSource) -> Result<(), String> {
    if verified.stored.kind == Kind::File
        && let Some(rec_size) = op.size
        && verified.stored.size != rec_size
    {
        return Err("計画を作ったときと大きさが違っています".into());
    }
    if op.mtime.is_some() && verified.disk_mtime != op.mtime {
        return Err("計画を作ったときと更新日時が違っています".into());
    }
    if let Some(rec_hash) = op.hash
        && let Some(cur_hash) = verified.stored.hash
        && rec_hash != cur_hash
    {
        return Err("計画を作ったときとハッシュが違っています".into());
    }
    Ok(())
}

/// フォルダは中の合計、それ以外はファイルの大きさ。
fn entry_display_size(db: &Db, stored: &Stored) -> Result<u64, String> {
    if stored.kind == Kind::Dir {
        let (_, _, bytes) = query::folder_total(db, stored.id).map_err(|e| e.to_string())?;
        Ok(bytes)
    } else {
        Ok(stored.size)
    }
}

pub(crate) fn meta_kind(meta: &fs::Metadata) -> Kind {
    let t = meta.file_type();
    if t.is_symlink() {
        Kind::Symlink
    } else if t.is_dir() {
        Kind::Dir
    } else if t.is_file() {
        Kind::File
    } else {
        Kind::Other
    }
}

/// パスを絶対パスとして整える（`..` を許さず、余分な `.` や末尾の `/` を落とす）。
pub(crate) fn clean_absolute_path(path: &Path) -> Result<PathBuf, String> {
    if path.as_os_str().is_empty() {
        return Err("パスが空です".into());
    }
    if !path.is_absolute() {
        return Err("絶対パスで指定してください".into());
    }
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::ParentDir => return Err("パスに .. は使えません".into()),
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    Ok(out)
}

/// パスが属する DB のドライブと、そのドライブ上の完全なパスを返す（Mac の firmlink も解決する）。
pub(crate) fn resolve_drive_path(
    drives: &[Drive],
    path: &Path,
) -> Option<(String, PathBuf, PathBuf)> {
    let mounted: Vec<(&str, &Path)> = drives
        .iter()
        .filter_map(|d| d.mount.as_deref().map(|m| (d.id.as_str(), m)))
        .collect();
    if let Some(&(id, mount)) = mounted
        .iter()
        .filter(|(_, m)| path.starts_with(m))
        .max_by_key(|(_, m)| m.components().count())
    {
        return Some((id.to_string(), mount.to_path_buf(), path.to_path_buf()));
    }
    // Mac: `/Users/…` のような firmlink 側のパスを `/System/Volumes/Data/Users/…` へ直す。
    if let Ok(rel) = path.strip_prefix("/") {
        for &(id, mount) in &mounted {
            if mount == Path::new("/System/Volumes/Data") {
                return Some((id.to_string(), mount.to_path_buf(), mount.join(rel)));
            }
        }
        for &(id, mount) in &mounted {
            if mount != Path::new("/") {
                let candidate = mount.join(rel);
                if has_existing_non_root_ancestor(&candidate, mount) {
                    return Some((id.to_string(), mount.to_path_buf(), candidate));
                }
            }
        }
    }
    None
}

fn has_existing_non_root_ancestor(path: &Path, mount: &Path) -> bool {
    let mut cur = Some(path);
    while let Some(p) = cur {
        if p == mount {
            return false;
        }
        if fs::symlink_metadata(p).is_ok() {
            return true;
        }
        cur = p.parent();
    }
    false
}

pub(crate) fn rel_path_string(mount: &Path, full: &Path) -> Option<String> {
    let rel = full.strip_prefix(mount).ok()?;
    let parts: Vec<String> = rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    Some(parts.join("/"))
}

fn parent_exists_or_planned(parent: &Path, planned_mkdirs: &[PathBuf]) -> bool {
    if parent.is_dir() {
        return true;
    }
    // mkdir は親も作るので、planned_mkdirs のどれかが parent と同じか、その下にあれば作られる。
    planned_mkdirs
        .iter()
        .any(|m| m == parent || m.starts_with(parent))
}

fn check_ancestors_are_dirs(path: &Path) -> Option<String> {
    let mut cur = path.parent();
    while let Some(p) = cur {
        match fs::symlink_metadata(p) {
            Ok(meta) => {
                if !meta.is_dir() {
                    return Some(format!(
                        "途中のパスがフォルダではありません: {}",
                        p.display()
                    ));
                }
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                cur = p.parent();
            }
            Err(_) => break,
        }
    }
    None
}

/// 守る場所か調べる。守る場所なら断る理由を返す。
pub(crate) fn is_protected_path(
    path: &Path,
    drive_mount: Option<&Path>,
    data_dir: &Path,
    home_dir: Option<&Path>,
    is_src_move_or_trash: bool,
) -> Option<String> {
    // 1. ドライブの一番上そのもの。
    if path.parent().is_none() || drive_mount.is_some_and(|m| paths_equal_os(path, m)) {
        return Some("ドライブの一番上は動かせません".into());
    }

    // 2. 隔離フォルダ（.lisa-trash と data_dir/trash）。
    if path
        .components()
        .any(|c| name::is_quarantine_dir(c.as_os_str()))
    {
        return Some("隔離フォルダは動かせません".into());
    }
    let data_trash = data_dir.join("trash");
    if is_same_or_under_with_firmlink(path, &data_trash) {
        return Some("隔離フォルダは動かせません".into());
    }

    // 3. Lisa のデータのフォルダ（移動元・隔離元なら、data_dir を含む親も断る）。
    if is_same_or_under_with_firmlink(path, data_dir)
        || (is_src_move_or_trash && is_strict_ancestor_with_firmlink(path, data_dir))
    {
        return Some("Lisa のデータのフォルダは動かせません".into());
    }

    // 4. OS とアプリの場所（Mac と Windows）。
    if let Some(reason) = is_os_protected_path(path, home_dir, is_src_move_or_trash) {
        return Some(reason);
    }

    None
}

fn is_os_protected_path(
    path: &Path,
    home_dir: Option<&Path>,
    is_src_move_or_trash: bool,
) -> Option<String> {
    let raw = path.to_string_lossy();

    // Windows 形式のパス（`C:\...` や `C:/...`）の判定。
    if let Some((_drive, rest)) = split_windows_drive(&raw) {
        let segs: Vec<String> = rest
            .split(['\\', '/'])
            .filter(|s| !s.is_empty())
            .map(str::to_ascii_lowercase)
            .collect();
        if segs.is_empty() {
            return Some("ドライブの一番上は動かせません".into());
        }
        let first = segs[0].as_str();
        if first == "windows"
            || first == "programdata"
            || first.starts_with("program files")
            || (first == "users" && segs.len() >= 3 && segs[2] == "appdata")
        {
            return Some("OS やアプリの保護された場所です".into());
        }
        // 移動元・隔離元が `C:\Users` や `C:\Users\<user>` のように AppData を含む親のときも守る。
        if is_src_move_or_trash && first == "users" && segs.len() <= 2 {
            return Some("OS やアプリの保護された場所を含みます".into());
        }
        if let Some(home) = home_dir {
            let appdata = home.join("AppData");
            if path_starts_with_ci(path, &appdata)
                || (is_src_move_or_trash && path_starts_with_ci(&appdata, path))
            {
                return Some("OS やアプリの保護された場所です".into());
            }
        }
        return None;
    }

    // Unix / macOS 形式のパス。`/System/Volumes/Data` が前に付いていれば外してから比べる。
    let logical = strip_mac_data_volume(path);
    let segs: Vec<String> = logical
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().to_ascii_lowercase()),
            _ => None,
        })
        .collect();
    if segs.is_empty() {
        return Some("ドライブの一番上は動かせません".into());
    }
    let first = segs[0].as_str();
    if matches!(first, "system" | "library" | "applications") {
        return Some("OS やアプリの保護された場所です".into());
    }
    if first == "users" && segs.len() >= 3 && segs[2] == "library" {
        return Some("OS やアプリの保護された場所です".into());
    }
    if is_src_move_or_trash && first == "users" && segs.len() <= 2 {
        return Some("OS やアプリの保護された場所を含みます".into());
    }
    if let Some(home) = home_dir {
        let user_lib = strip_mac_data_volume(&home.join("Library"));
        if path_starts_with_ci(&logical, &user_lib)
            || (is_src_move_or_trash && path_starts_with_ci(&user_lib, &logical))
        {
            return Some("OS やアプリの保護された場所です".into());
        }
    }
    None
}

fn split_windows_drive(s: &str) -> Option<(char, &str)> {
    let bytes = s.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
    {
        Some((bytes[0] as char, &s[3..]))
    } else {
        None
    }
}

fn strip_mac_data_volume(path: &Path) -> PathBuf {
    if let Ok(rel) = path.strip_prefix("/System/Volumes/Data") {
        Path::new("/").join(rel)
    } else {
        path.to_path_buf()
    }
}

fn is_same_or_under_with_firmlink(path: &Path, base: &Path) -> bool {
    let a = strip_mac_data_volume(path);
    let b = strip_mac_data_volume(base);
    path_starts_with_os(&a, &b)
}

fn is_strict_ancestor_with_firmlink(path: &Path, child: &Path) -> bool {
    let a = strip_mac_data_volume(path);
    let b = strip_mac_data_volume(child);
    !paths_equal_os(&a, &b) && path_starts_with_os(&b, &a)
}

fn path_starts_with_os(path: &Path, base: &Path) -> bool {
    if cfg!(any(target_os = "macos", windows)) {
        path_starts_with_ci(path, base)
    } else {
        path.starts_with(base)
    }
}

fn paths_equal_os(a: &Path, b: &Path) -> bool {
    if cfg!(any(target_os = "macos", windows)) {
        fold_components(a) == fold_components(b)
    } else {
        a == b
    }
}

fn path_starts_with_ci(path: &Path, base: &Path) -> bool {
    let a = fold_components(path);
    let b = fold_components(base);
    !b.is_empty() && a.len() >= b.len() && a[..b.len()] == b[..]
}

fn fold_components(path: &Path) -> Vec<String> {
    let raw = path.to_string_lossy();
    if let Some((drive, rest)) = split_windows_drive(&raw) {
        let mut out = vec![format!("{}:", drive.to_ascii_lowercase())];
        out.extend(
            rest.split(['\\', '/'])
                .filter(|s| !s.is_empty())
                .map(str::to_ascii_lowercase),
        );
        return out;
    }
    let mut out = Vec::new();
    for c in path.components() {
        match c {
            Component::RootDir => out.push("/".to_string()),
            Component::Normal(s) => out.push(s.to_string_lossy().to_ascii_lowercase()),
            Component::Prefix(p) => out.push(p.as_os_str().to_string_lossy().to_ascii_lowercase()),
            _ => {}
        }
    }
    out
}

fn default_home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

struct RoleInfo {
    /// 自分または祖先の中で最も深い役割（仕分けが型より優先）。
    effective: Option<Role>,
    /// 自分自身に直接付いている役割。
    direct: Option<Role>,
}

/// `loc.chain` に付いているカタログの役割（`catalog_match` + `part` と `assignment`）を調べる。
fn role_of_chain(db: &Db, catalogs: &CatalogSet, loc: &Located) -> Result<RoleInfo, String> {
    let target_depth = loc.chain.len().saturating_sub(1);
    // (chain の深さ, assignment かどうか) が大きいものが勝つ。
    let mut best: Option<(usize, bool, Role)> = None;
    let mut direct: Option<(bool, Role)> = None;

    let mut consider = |depth: usize, is_assign: bool, role: Role| {
        if best.is_none_or(|(bd, ba, _)| (depth, is_assign) > (bd, ba)) {
            best = Some((depth, is_assign, role));
        }
        if depth == target_depth && direct.is_none_or(|(da, _)| is_assign >= da) {
            direct = Some((is_assign, role));
        }
    };

    // 1. 仕分け（assignment）の役割。chain の各祖先の NFC 相対パスと比べる。
    let mut chain_rel_paths: Vec<String> = Vec::with_capacity(loc.chain.len());
    let mut prefix = String::new();
    for (idx, &eid) in loc.chain.iter().enumerate() {
        if idx == 0 {
            chain_rel_paths.push(String::new());
            continue;
        }
        let name: Option<String> = db
            .conn()
            .prepare_cached("SELECT name_nfc FROM entry WHERE id = ?1")
            .map_err(|e| e.to_string())?
            .query_row([eid], |r| r.get(0))
            .optional()
            .map_err(|e| e.to_string())?;
        let name = name.unwrap_or_default();
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(&name);
        chain_rel_paths.push(prefix.clone());
    }

    {
        let mut stmt = db
            .conn()
            .prepare(
                "SELECT path, role FROM assignment
                 WHERE drive_id = ?1 AND role IS NOT NULL AND lost_at IS NULL",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([&loc.drive_id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(|e| e.to_string())?;
        for row in rows {
            let (apath, role_s) = row.map_err(|e| e.to_string())?;
            let Some(role) = Role::from_name(&role_s) else {
                continue;
            };
            if let Some(depth) = chain_rel_paths.iter().position(|p| p == &apath) {
                consider(depth, true, role);
            }
        }
    }

    // 2. カタログの当たり（catalog_match）と `[[part]]`。
    let chain_index: HashMap<i64, usize> = loc
        .chain
        .iter()
        .enumerate()
        .map(|(i, &id)| (id, i))
        .collect();
    let tree = DbTree::new(db);
    let mut match_stmt = db
        .conn()
        .prepare_cached("SELECT catalog_id FROM catalog_match WHERE entry_id = ?1")
        .map_err(|e| e.to_string())?;
    for &anc_id in &loc.chain {
        let cat_ids: Vec<String> = match_stmt
            .query_map([anc_id], |r| r.get(0))
            .map_err(|e| e.to_string())?
            .collect::<Result<_, _>>()
            .map_err(|e| e.to_string())?;
        for cid in cat_ids {
            let Some(loaded) = catalogs.get(&cid) else {
                continue;
            };
            if loaded.catalog.parts.is_empty() {
                continue;
            }
            for (part_id, role) in catalog::parts(&loaded.catalog, &tree, anc_id) {
                if let Some(&depth) = chain_index.get(&part_id) {
                    consider(depth, false, role);
                }
            }
        }
    }

    Ok(RoleInfo {
        effective: best.map(|(_, _, r)| r),
        direct: direct.map(|(_, r)| r),
    })
}
