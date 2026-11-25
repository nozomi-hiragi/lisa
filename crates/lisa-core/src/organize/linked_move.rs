//! 移動に伴う目印の所在と参照先の付け替え。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::db::Db;
use crate::merged::{Location, links};

use super::{
    Context, PlanOp, TransferAction, TransferRequest, file_identity, linked_delete, markers,
    plan::OpKind,
};

pub(super) const SYNCHRONIZING: &str = "リンクの照合が未完了のため移動を保留します";

pub(super) fn unfinished_local_move(db: &Db, op: &PlanOp) -> Result<bool, String> {
    db.conn().query_row(
        "SELECT EXISTS(SELECT 1 FROM merged_link_move_retry r JOIN merged_link l USING(link_id)
         WHERE r.operation=?1 AND l.source_device=(SELECT value FROM meta WHERE key='this_device'))",
        [format!("move-{}-{}-0",op.plan_id,op.seq)], |row| row.get(0),
    ).map_err(error)
}

#[derive(Deserialize, Serialize)]
struct Movement {
    target: Option<(Location, Location)>,
    source: Option<(Location, Location)>,
    marker_path: Option<(PathBuf, PathBuf)>,
    identity: Option<(u64, [u8; 16])>,
}

fn from_path(db: &Db, path: &Path) -> Option<Location> {
    Location::local_for_path(db, path).or_else(|| {
        crate::query::locate(db, path)
            .ok()
            .flatten()
            .and_then(|entry| Location::local_for_path(db, &entry.path))
    })
}

pub(super) fn check(db: &Db, src: &Path, dst: &Path) -> Result<(), String> {
    if let Some(location) = from_path(db, src)
        && links::sources_under(db, &location)?
            .iter()
            .any(|link| link.state == links::State::Reserved || link.sync_state == "pending")
    {
        return Err(SYNCHRONIZING.into());
    }
    if src.extension() != dst.extension()
        && matches!(
            src.extension().and_then(|ext| ext.to_str()),
            Some("lmf" | "lmd")
        )
        && let Some(marker) = crate::merged::read(src)
        && let Some(journal) = markers::journal(db, marker.plan_id, marker.item_id)?
        && let markers::State::Published { path, identity } = journal.state
        && (path == src || super::is_name_alias(&path, src).unwrap_or(false))
        && src.parent().and_then(file_identity) == Some(journal.parent)
        && linked_delete::verify_marker(src, &journal.marker, identity).is_ok()
    {
        return Err("マージ済み目印の拡張子（.lmf／.lmd）は変更できません".into());
    }
    Ok(())
}

pub(super) fn prepare(db: &Db, op: &PlanOp) -> Result<(), String> {
    let Some(dst) = op.dst.as_deref().filter(|_| op.op == OpKind::Move) else {
        return Ok(());
    };
    let movements = movements(db, &op.src, dst)?;
    if !movements.is_empty() {
        db.conn()
            .execute(
                "INSERT INTO merged_link_move(plan_id,seq,movements) VALUES (?1,?2,?3)
             ON CONFLICT(plan_id,seq) DO UPDATE SET movements=excluded.movements",
                params![
                    op.plan_id,
                    op.seq,
                    serde_json::to_string(&movements).map_err(error)?
                ],
            )
            .map_err(error)?;
    }
    Ok(())
}

fn movements(db: &Db, src: &Path, dst: &Path) -> Result<BTreeMap<String, Movement>, String> {
    let Some(src_location) = from_path(db, src) else {
        return Ok(BTreeMap::new());
    };
    let src = Path::new(&src_location.display_path);
    let mut movements = BTreeMap::new();
    for link in links::sources_under(db, &src_location)? {
        if link.state == links::State::Reserved || link.sync_state == "pending" {
            return Err(SYNCHRONIZING.into());
        }
        let old = link.marker.target;
        let suffix = old
            .relative_path
            .strip_prefix(src_location.relative_path.as_slice())
            .ok_or("実体のパスが移動元と一致しません")?;
        let path = suffix
            .iter()
            .fold(dst.to_path_buf(), |path, part| path.join(part));
        let new = Location::local_for_path(db, &path).ok_or("実体の移動先を記録できません")?;
        movements.insert(
            link.marker.link_id,
            Movement {
                target: Some((old, new)),
                source: None,
                marker_path: None,
                identity: None,
            },
        );
    }
    let path = src.to_string_lossy();
    let separator = std::path::MAIN_SEPARATOR;
    let prefix = format!("{path}{separator}");
    let next = char::from_u32(u32::from(separator) + 1).expect("パス区切りの次の文字");
    let upper = format!("{path}{next}");
    let mut statement = db
        .conn()
        .prepare(
            "SELECT journal FROM merge_marker
             WHERE json_extract(journal, '$.state.Published.path') = ?1
                OR (json_extract(journal, '$.state.Published.path') >= ?2
                    AND json_extract(journal, '$.state.Published.path') < ?3)",
        )
        .map_err(error)?;
    let journals = statement
        .query_map(params![path, prefix, upper], |row| row.get::<_, String>(0))
        .map_err(error)?;
    for journal in journals {
        let journal: markers::Journal =
            serde_json::from_str(&journal.map_err(error)?).map_err(error)?;
        let markers::State::Published { path, identity } = journal.state else {
            continue;
        };
        let Ok(suffix) = path.strip_prefix(src) else {
            continue;
        };
        if path.parent().and_then(file_identity) != Some(journal.parent)
            || linked_delete::verify_marker(&path, &journal.marker, identity).is_err()
        {
            continue;
        }
        let new_path = if suffix.as_os_str().is_empty() {
            dst.to_owned()
        } else {
            dst.join(suffix)
        };
        let source_name = if new_path.file_name() != path.file_name() {
            new_path.file_stem().ok_or("目印の名前がありません")?
        } else {
            std::ffi::OsStr::new(
                journal
                    .marker
                    .source
                    .relative_path
                    .last()
                    .ok_or("目印の名前がありません")?,
            )
        };
        let new = Location::local_for_path(db, &new_path.with_file_name(source_name))
            .ok_or("目印の移動先を記録できません")?;
        let Some((link, _)) = links::active_link(db, &journal.marker.link_id)? else {
            continue;
        };
        if link.sync_state == "pending" {
            return Err(SYNCHRONIZING.into());
        }
        let movement = movements.entry(journal.marker.link_id).or_insert(Movement {
            target: None,
            source: None,
            marker_path: None,
            identity: None,
        });
        movement.source = Some((journal.marker.source, new));
        movement.marker_path = Some((path, new_path));
        movement.identity = Some(identity);
    }
    Ok(movements)
}

pub(super) fn prepare_undo(db: &Db, op: &PlanOp, src: &Path) -> Result<(), String> {
    let mut movements = movements(db, src, &op.src)?;
    // 保存形式は正方向と同じ向きに揃える。
    for movement in movements.values_mut() {
        if let Some((from, to)) = &mut movement.target {
            std::mem::swap(from, to);
        }
        if let Some((from, to)) = &mut movement.source {
            std::mem::swap(from, to);
        }
        if let Some((from, to)) = &mut movement.marker_path {
            std::mem::swap(from, to);
        }
    }
    db.conn().execute(
        "INSERT INTO merged_link_move_undo(plan_id,seq,movements,identity) VALUES (?1,?2,?3,?4)
         ON CONFLICT(plan_id,seq) DO UPDATE SET movements=excluded.movements,identity=excluded.identity,copied_identity=NULL",
        params![op.plan_id, op.seq, serde_json::to_string(&movements).map_err(error)?,
                file_identity(src).map(|identity| serde_json::to_string(&identity)).transpose().map_err(error)?],
    ).map_err(error)?;
    Ok(())
}

pub(super) fn undo_identity(db: &Db, op: &PlanOp) -> Result<Option<(u64, [u8; 16])>, String> {
    let identity: Option<String> = db
        .conn()
        .query_row(
            "SELECT identity FROM merged_link_move_undo WHERE plan_id=?1 AND seq=?2",
            params![op.plan_id, op.seq],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .map_err(error)?
        .flatten();
    identity
        .map(|json| serde_json::from_str(&json).map_err(error))
        .transpose()
}

pub(super) fn record_undo_copy(db: &Db, op: &PlanOp, staged: &Path) -> Result<(), String> {
    let identity = file_identity(staged).ok_or("取り消し用コピーを識別できません")?;
    db.conn()
        .execute(
            "UPDATE merged_link_move_undo SET copied_identity=?3 WHERE plan_id=?1 AND seq=?2",
            params![
                op.plan_id,
                op.seq,
                serde_json::to_string(&identity).map_err(error)?
            ],
        )
        .map_err(error)?;
    Ok(())
}

pub(super) fn undo_copy_matches(db: &Db, op: &PlanOp) -> Result<bool, String> {
    let identity: Option<String> = db
        .conn()
        .query_row(
            "SELECT copied_identity FROM merged_link_move_undo WHERE plan_id=?1 AND seq=?2",
            params![op.plan_id, op.seq],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .map_err(error)?
        .flatten();
    let Some(identity) = identity else {
        return Ok(false);
    };
    let expected = serde_json::from_str(&identity).map_err(error)?;
    Ok(file_identity(&op.src) == Some(expected))
}

pub(super) fn undo_marker_matches(db: &Db, op: &PlanOp) -> Result<bool, String> {
    let Some(marker) = crate::merged::read(&op.src) else {
        return Ok(false);
    };
    let recorded: bool = db.conn().query_row(
        "SELECT EXISTS(SELECT 1 FROM merged_link_move_undo u JOIN json_each(u.movements) m
         WHERE u.plan_id=?1 AND u.seq=?2 AND m.key=?3 AND json_extract(m.value,'$.marker_path[0]')=?4)",
        params![op.plan_id,op.seq,marker.link_id,op.src.to_string_lossy()], |row| row.get(0),
    ).map_err(error)?;
    if !recorded {
        return Ok(false);
    }
    let Some(journal) = markers::journal(db, marker.plan_id, marker.item_id)? else {
        return Ok(false);
    };
    let markers::State::Published { path, identity } = journal.state else {
        return Ok(false);
    };
    Ok(path == op.src
        && path.parent().and_then(file_identity) == Some(journal.parent)
        && linked_delete::verify_marker(&path, &journal.marker, identity).is_ok())
}

pub(super) fn finish(
    db: &mut Db,
    ctx: &Context,
    op: &PlanOp,
    undo: bool,
    touched: &mut Vec<PathBuf>,
) -> Option<String> {
    match finish_links(db, ctx, op, undo, touched) {
        Ok(warnings) if warnings.is_empty() => None,
        Ok(warnings) => Some(warnings.join("／")),
        Err(reason) => Some(reason),
    }
}

fn finish_links(
    db: &mut Db,
    ctx: &Context,
    op: &PlanOp,
    undo: bool,
    touched: &mut Vec<PathBuf>,
) -> Result<Vec<String>, String> {
    let record: Option<String> = db
        .conn()
        .query_row(
            if undo {
                "SELECT movements FROM merged_link_move_undo WHERE plan_id=?1 AND seq=?2"
            } else {
                "SELECT movements FROM merged_link_move WHERE plan_id=?1 AND seq=?2"
            },
            params![op.plan_id, op.seq],
            |row| row.get(0),
        )
        .optional()
        .map_err(error)?;
    let Some(record) = record else {
        return Ok(Vec::new());
    };
    let movements: BTreeMap<String, Movement> = serde_json::from_str(&record).map_err(error)?;
    let operation = format!("move-{}-{}-{}", op.plan_id, op.seq, u8::from(undo));
    let mut warnings = Vec::new();
    let rewritten_destination = movements.values().any(|movement| {
        movement.marker_path.as_ref().is_some_and(|(_, path)| {
            Some(path.as_path()) == op.dst.as_deref() || path.parent() == op.dst.as_deref()
        })
    });
    for (id, movement) in movements {
        let json = serde_json::to_string(&movement).map_err(error)?;
        db.conn().execute(
            "INSERT OR IGNORE INTO merged_link_move_retry(operation,link_id,movement,undo) VALUES (?1,?2,?3,?4)",
            params![operation, id, json, undo],
        ).map_err(error)?;
        match retry_move(db, ctx, &operation, &id, movement, undo) {
            Ok(path) => touched.extend(path),
            Err(reason) => warnings.push(format!("目印を追従できませんでした: {reason}")),
        }
    }
    if !undo
        && rewritten_destination
        && let Some(dst) = &op.dst
        && let Ok(metadata) = std::fs::symlink_metadata(dst)
    {
        db.conn().execute(
            "UPDATE merged_link_move SET undo_size=?3,undo_mtime=?4 WHERE plan_id=?1 AND seq=?2",
            params![op.plan_id, op.seq, metadata.len() as i64, crate::listing::mtime_ms(&metadata)],
        ).map_err(error)?;
    }
    Ok(warnings)
}

pub(super) fn undo_metadata(db: &Db, op: &PlanOp) -> Result<(Option<u64>, Option<i64>), String> {
    let recorded = db
        .conn()
        .query_row(
            "SELECT undo_size,undo_mtime FROM merged_link_move WHERE plan_id=?1 AND seq=?2
         AND undo_size IS NOT NULL",
            params![op.plan_id, op.seq],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(error)?;
    Ok(
        recorded.map_or((op.size, op.mtime), |(size, mtime): (i64, Option<i64>)| {
            (Some(size as u64), mtime)
        }),
    )
}

fn retry_move(
    db: &mut Db,
    ctx: &Context,
    operation: &str,
    id: &str,
    movement: Movement,
    undo: bool,
) -> Result<Option<PathBuf>, String> {
    let result = follow(db, ctx, id, movement, operation, undo);
    if result.is_ok() {
        db.conn()
            .execute(
                "DELETE FROM merged_link_move_retry WHERE operation=?1 AND link_id=?2",
                params![operation, id],
            )
            .map_err(error)?;
    } else {
        db.conn().execute("UPDATE merged_link_move_retry SET attempted_at=?3 WHERE operation=?1 AND link_id=?2",
            params![operation,id,crate::db::now_ms()]).map_err(error)?;
    }
    result
}

pub(super) fn flush_moves(db: &mut Db, ctx: &Context) -> Result<(), String> {
    let pending = db.conn().prepare(
        "SELECT operation,link_id,movement,undo FROM merged_link_move_retry ORDER BY attempted_at, rowid LIMIT 8",
    ).map_err(error)?.query_map([], |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,
            row.get::<_,String>(2)?,row.get::<_,bool>(3)?))).map_err(error)?
        .collect::<rusqlite::Result<Vec<_>>>().map_err(error)?;
    for (operation, id, json, undo) in pending {
        let movement = serde_json::from_str(&json).map_err(error)?;
        if let Ok(Some(path)) = retry_move(db, ctx, &operation, &id, movement, undo) {
            let _ = crate::remote::send(&ctx.data_dir, crate::remote::Command::Rescan { path });
        }
    }
    Ok(())
}

fn follow(
    db: &mut Db,
    ctx: &Context,
    id: &str,
    movement: Movement,
    operation: &str,
    undo: bool,
) -> Result<Option<PathBuf>, String> {
    let Some((mut link, relay)) = links::active_link(db, id)? else {
        return Ok(None);
    };
    let mut updated = link.marker.clone();
    let mut previous = link.marker.clone();
    for (current, old, locations) in [
        (&mut updated.target, &mut previous.target, movement.target),
        (&mut updated.source, &mut previous.source, movement.source),
    ] {
        if let Some((from, to)) = locations {
            let (from, to) = if undo { (to, from) } else { (from, to) };
            let to = Location::local_for_path(db, Path::new(&to.display_path)).unwrap_or(to);
            if *current != from && *current != to {
                return Err("リンクの所在が変わりました".into());
            }
            *old = from;
            *current = to;
        }
    }
    let in_progress = rewrite_in_progress(db, operation, id)?;
    if updated == link.marker {
        if in_progress {
            link.marker = previous;
            link.generation -= 1;
            return linked_delete::rewrite_moved_link(
                db,
                operation,
                &link,
                updated,
                None,
                relay.as_deref(),
            );
        }
        return Ok(None);
    }
    if db.meta("this_device").map_err(error)?.as_deref() != Some(&updated.source.device_id) {
        let relay = relay.as_deref().ok_or("目印の中継端末が分かりません")?;
        queue_retarget(db, &link, &updated, relay)?;
        flush_retargets(db, ctx, Some(id))?;
        let pending =
            links::active_link(db, id)?.is_some_and(|(link, _)| link.sync_state == "pending");
        if pending {
            return Err("他端末の目印は再接続時に自動追従します".into());
        }
        return Ok(None);
    }
    let mut relocated = None;
    let canonical_path;
    if let Some((from, to)) = &movement.marker_path {
        let (old_path, path) = if undo { (to, from) } else { (from, to) };
        let journal = markers::journal(db, link.marker.plan_id, link.marker.item_id)?
            .ok_or("目印の記録が見つかりません")?;
        let markers::State::Published {
            path: recorded,
            identity,
        } = journal.state
        else {
            return Err("公開済みの目印がありません".into());
        };
        let same_path = recorded == *old_path
            || (cfg!(target_os = "macos")
                && recorded
                    .components()
                    .map(|part| crate::name::nfc(part.as_os_str()))
                    .eq(old_path
                        .components()
                        .map(|part| crate::name::nfc(part.as_os_str()))));
        if !same_path || (!undo && movement.identity != Some(identity)) {
            return Err("目印の所在または識別情報が変わりました".into());
        }
        let moved_identity = file_identity(path).unwrap_or(identity);
        if moved_identity != identity && moved_identity.0 == identity.0 && !in_progress {
            return Err("目印の差し替えを確認しました".into());
        }
        if let Some(actual) = Location::local_for_path(db, path) {
            let actual = PathBuf::from(actual.display_path);
            if old_path.file_name() != path.file_name()
                && let Some(stem) = actual.file_stem()
                && let Some(source) = Location::local_for_path(db, &actual.with_file_name(stem))
            {
                updated.source = source;
            }
            canonical_path = Some(actual);
        } else {
            canonical_path = None;
        }
        relocated = Some((canonical_path.as_deref().unwrap_or(path), moved_identity));
    }
    linked_delete::rewrite_moved_link(db, operation, &link, updated, relocated, relay.as_deref())
}

fn queue_retarget(
    db: &mut Db,
    link: &links::Link,
    updated: &crate::merged::Marker,
    relay: &str,
) -> Result<(), String> {
    let request = TransferRequest {
        operation_id: uuid::Uuid::new_v4().to_string(),
        marker: link.marker.clone(),
        generation: link.generation,
        expires_at: 0,
        action: TransferAction::Retarget {
            target: updated.target.clone(),
        },
    };
    let tx = db
        .conn_mut()
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(error)?;
    links::retarget(&tx, link, updated, Some(relay))?;
    tx.execute(
        "UPDATE merged_link SET sync_state='pending' WHERE link_id=?1",
        [&updated.link_id],
    )
    .map_err(error)?;
    tx.execute(
        "INSERT INTO merged_link_retarget_outbox(link_id,generation,relay_device,request)
         VALUES (?1,?2,?3,?4)",
        params![
            updated.link_id,
            link.generation + 1,
            relay,
            serde_json::to_string(&request).map_err(error)?
        ],
    )
    .map_err(error)?;
    tx.commit().map_err(error)
}

pub(super) fn flush_retargets(
    db: &mut Db,
    ctx: &Context,
    only_link: Option<&str>,
) -> Result<(), String> {
    let Some(client) = ctx.relocation_client else {
        return Ok(());
    };
    let pending = db.conn().prepare(
        "SELECT link_id,generation,relay_device,request FROM merged_link_retarget_outbox r
         WHERE (?1 IS NULL OR link_id=?1)
           AND generation=(SELECT MIN(generation) FROM merged_link_retarget_outbox WHERE link_id=r.link_id)
         ORDER BY attempted_at,rowid LIMIT 4",
    ).map_err(error)?.query_map([only_link], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?, row.get::<_, String>(3)?))
    }).map_err(error)?.collect::<rusqlite::Result<Vec<_>>>().map_err(error)?;
    for (id, generation, relay, json) in pending {
        let mut request: TransferRequest = serde_json::from_str(&json).map_err(error)?;
        request.expires_at = crate::db::now_ms() + 120_000;
        if let Ok(reply) = client(&ctx.data_dir, &relay, &request)
            && reply.published
        {
            let tx = db
                .conn_mut()
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(error)?;
            if let Some(source) = reply.moved_source
                && let TransferAction::Retarget { target } = &request.action
            {
                links::merge_retarget_reply(
                    &tx,
                    &request.marker,
                    target,
                    request.generation,
                    &source,
                )?;
            }
            tx.execute(
                "DELETE FROM merged_link_retarget_outbox WHERE link_id=?1 AND generation=?2",
                params![id, generation],
            )
            .map_err(error)?;
            tx.execute(
                "UPDATE merged_link SET sync_state='confirmed',sync_reason=NULL WHERE link_id=?1 AND generation=?2",
                params![id, generation],
            ).map_err(error)?;
            tx.commit().map_err(error)?;
        } else {
            db.conn().execute(
                "UPDATE merged_link_retarget_outbox SET attempted_at=?3 WHERE link_id=?1 AND generation=?2",
                params![id, generation, crate::db::now_ms()],
            ).map_err(error)?;
        }
    }
    Ok(())
}

fn rewrite_in_progress(db: &Db, operation: &str, id: &str) -> Result<bool, String> {
    db.conn()
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM merged_link_rewrite WHERE operation_id=?1 AND link_id=?2)",
            params![operation, id],
            |row| row.get(0),
        )
        .map_err(error)
}

fn error(value: impl std::fmt::Display) -> String {
    value.to_string()
}
