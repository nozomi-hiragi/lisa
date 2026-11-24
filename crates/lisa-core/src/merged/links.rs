//! マージ済みリンクの両端に残す状態と、送信待ちの管理。

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{Location, Marker};
use crate::db::{self, Db};

pub const OUTBOX_LIMIT: i64 = 4096;
pub const OUTBOX_MAX_AGE_MS: i64 = 30 * 24 * 60 * 60 * 1000;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Reserved,
    Active,
    Removed,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            Self::Reserved => "reserved",
            Self::Active => "active",
            Self::Removed => "removed",
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct Update {
    pub marker: Marker,
    pub generation: i64,
    pub state: State,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Applied {
    New,
    Duplicate,
    Stale,
}

/// 発行元とリンクの所在が一致すると確認してから呼ぶ。
pub fn apply(
    db: &mut Db,
    update: &Update,
    relay_device: Option<&str>,
    send_later: bool,
) -> Result<Applied, String> {
    let tx = db
        .conn_mut()
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(error)?;
    let result = apply_in(&tx, update, relay_device, send_later, db::now_ms())?;
    tx.commit().map_err(error)?;
    Ok(result)
}

fn apply_in(
    conn: &Connection,
    update: &Update,
    relay_device: Option<&str>,
    send_later: bool,
    now: i64,
) -> Result<Applied, String> {
    let marker = &update.marker;
    if !marker.valid() || update.generation < 1 {
        return Err("リンク更新の書式が正しくありません".into());
    }
    let id = &marker.link_id;
    let previous = conn
        .query_row(
            "SELECT generation, state, marker, relay_device FROM merged_link WHERE link_id=?1",
            [&id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .optional()
        .map_err(error)?;
    let marker_json = serde_json::to_string(marker).map_err(error)?;
    if let Some((generation, state, old_json, relay)) = previous {
        if relay.as_deref() != relay_device {
            return Err("リンク更新の依頼元が一致しません".into());
        }
        if update.generation < generation {
            return Ok(Applied::Stale);
        }
        if update.generation == generation {
            return if state == update.state.as_str() && old_json == marker_json {
                Ok(Applied::Duplicate)
            } else {
                Err("同世代のリンク更新が競合しました".into())
            };
        }
        let old: Marker = serde_json::from_str(&old_json).map_err(error)?;
        if old.source != marker.source
            || old.target != marker.target
            || old.kind != marker.kind
            || old.hash != marker.hash
            || state == State::Removed.as_str()
            || update.state == State::Reserved
        {
            return Err("リンクの元・中身・更新の順序が一致しません".into());
        }
    } else if update.state != State::Reserved || update.generation != 1 {
        return Err("リンクの事前登録がありません".into());
    }

    let target_path = serde_json::to_string(&marker.target.relative_path).map_err(error)?;
    let sync_state = if send_later || update.state == State::Reserved {
        "pending"
    } else {
        "confirmed"
    };
    conn.execute(
        "INSERT INTO merged_link
           (link_id, source_device, target_device, target_volume, target_path, generation, state,
            marker, relay_device, checked_at, sync_state)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
         ON CONFLICT(link_id) DO UPDATE SET target_device=excluded.target_device,
           target_volume=excluded.target_volume, target_path=excluded.target_path,
           generation=excluded.generation, state=excluded.state, marker=excluded.marker,
           checked_at=excluded.checked_at, sync_state=excluded.sync_state, sync_reason=NULL",
        params![
            id,
            marker.source.device_id,
            marker.target.device_id,
            marker.target.volume_id,
            target_path,
            update.generation,
            update.state.as_str(),
            marker_json,
            relay_device,
            now,
            sync_state
        ],
    )
    .map_err(error)?;
    if send_later {
        conn.execute(
            "INSERT INTO merged_link_outbox(link_id,generation,queued_at) VALUES (?1,?2,?3)
             ON CONFLICT(link_id) DO UPDATE SET generation=excluded.generation, queued_at=excluded.queued_at",
            params![id, update.generation, now],
        ).map_err(error)?;
        expire_in(conn, now)?;
    }
    Ok(Applied::New)
}

fn expire_in(conn: &Connection, now: i64) -> Result<(), String> {
    conn.execute(
        "UPDATE merged_link SET sync_state='failed', sync_reason='リンクを同期できませんでした'
         WHERE link_id IN (
           SELECT link_id FROM merged_link_outbox WHERE queued_at <= ?1
           UNION SELECT link_id FROM (SELECT link_id FROM merged_link_outbox ORDER BY queued_at DESC, link_id DESC
                                      LIMIT -1 OFFSET ?2)
         )",
        params![now.saturating_sub(OUTBOX_MAX_AGE_MS), OUTBOX_LIMIT],
    ).map_err(error)?;
    conn.execute(
        "UPDATE plan_op SET reason = CASE WHEN reason IS NULL THEN 'リンクを同期できませんでした'
             ELSE reason || '／リンクを同期できませんでした' END
         WHERE (plan_id, seq) IN (
           SELECT json_extract(marker,'$.plan_id'), json_extract(marker,'$.item_id') FROM merged_link
           JOIN merged_link_outbox USING(link_id) WHERE sync_state='failed'
         )",
        [],
    ).map_err(error)?;
    conn.execute(
        "DELETE FROM merged_link_outbox WHERE link_id IN (
           SELECT link_id FROM merged_link WHERE sync_state='failed'
         )",
        [],
    )
    .map_err(error)?;
    Ok(())
}

pub fn expire(db: &mut Db) -> Result<(), String> {
    let tx = db
        .conn_mut()
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(error)?;
    expire_in(&tx, db::now_ms())?;
    tx.commit().map_err(error)
}

pub fn acknowledge(db: &mut Db, link_id: &str, generation: i64) -> Result<(), String> {
    let tx = db
        .conn_mut()
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(error)?;
    let changed = tx
        .execute(
            "DELETE FROM merged_link_outbox WHERE link_id=?1 AND generation=?2",
            params![link_id, generation],
        )
        .map_err(error)?;
    if changed != 0 {
        tx.execute(
            "UPDATE merged_link SET sync_state='confirmed', sync_reason=NULL, checked_at=?3
             WHERE link_id=?1 AND generation=?2",
            params![link_id, generation, db::now_ms()],
        )
        .map_err(error)?;
    }
    tx.commit().map_err(error)
}

pub fn pending(db: &Db) -> Result<Vec<Update>, String> {
    db.conn()
        .prepare_cached(
            "SELECT marker, m.generation, state FROM merged_link_outbox o
         JOIN merged_link m ON m.link_id=o.link_id AND m.generation=o.generation
         ORDER BY o.queued_at, m.link_id LIMIT 32",
        )
        .map_err(error)?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(error)?
        .map(|row| {
            let (json, generation, state) = row.map_err(error)?;
            Ok(Update {
                marker: serde_json::from_str(&json).map_err(error)?,
                generation,
                state: if state == "removed" {
                    State::Removed
                } else {
                    State::Active
                },
            })
        })
        .collect()
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Link {
    pub marker: Marker,
    pub generation: i64,
    pub state: State,
    pub checked_at: i64,
    pub sync_state: String,
    pub sync_reason: Option<String>,
}

pub fn sources_for_entry(db: &Db, entry: i64) -> Result<Vec<Link>, String> {
    let path = crate::query::path_of(db, entry)
        .map_err(error)?
        .ok_or("実体の所在を確認できません")?;
    let location =
        Location::local(db, &path.drive_id, &path.path).ok_or("実体の所在を確認できません")?;
    sources(db, &location)
}

pub(crate) fn stored_source(
    db: &Db,
    link_id: &str,
) -> Result<Option<(Marker, i64, String)>, String> {
    let row: Option<(String, i64, String)> = db
        .conn()
        .query_row(
            "SELECT marker,generation,relay_device FROM merged_link WHERE link_id=?1
          AND state='active' AND sync_state='confirmed' AND relay_device IS NOT NULL",
            [link_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(error)?;
    row.map(|(json, generation, relay)| {
        Ok((
            serde_json::from_str(&json).map_err(error)?,
            generation,
            relay,
        ))
    })
    .transpose()
}

pub fn relocation_permitted(
    db: &Db,
    marker: &Marker,
    generation: i64,
    operation: &str,
    begin: bool,
    retire: bool,
) -> Result<Authorization, String> {
    if Uuid::parse_str(operation)
        .ok()
        .is_none_or(|id| id.is_nil() || id.to_string() != operation)
    {
        return Err("操作 ID が正しくありません".into());
    }
    let digest = marker_digest(marker)?;
    let row: Option<(Option<String>, i64)> = db
        .conn()
        .query_row(
            "SELECT relocation_operation,revoked FROM merged_link_relay
          WHERE link_id=?1 AND source_device=?2 AND target_device=?3 AND generation=?4
          AND state='active' AND marker_hash=?5",
            params![
                marker.link_id,
                marker.source.device_id,
                marker.target.device_id,
                generation,
                digest.as_bytes().as_slice()
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(error)?;
    match row {
        Some((Some(id), 2)) if retire && id == operation => Ok(Authorization::AlreadyApplied),
        Some((Some(id), 0)) if id == operation => Ok(Authorization::Forward),
        // 前の操作が 1 日以上動いていなければ（依頼元が諦めた・別の計画でやり直す）、
        // 戻し先の一時領域も同じ期限で片付くので、新しい操作に引き継ぐ。
        Some((Some(id), 0)) if begin && transfer_idle(db, &id)? => {
            let changed = db
                .conn()
                .execute(
                    "UPDATE merged_link_relay SET relocation_operation=?2 WHERE link_id=?1
                 AND revoked=0 AND relocation_operation=?3",
                    params![marker.link_id, operation, id],
                )
                .map_err(error)?;
            if changed == 1 {
                Ok(Authorization::Forward)
            } else {
                Err("転送の許可が競合しました".into())
            }
        }
        Some((None, 0)) if begin => {
            let changed = db
                .conn()
                .execute(
                    "UPDATE merged_link_relay SET relocation_operation=?2 WHERE link_id=?1
                 AND revoked=0 AND relocation_operation IS NULL",
                    params![marker.link_id, operation],
                )
                .map_err(error)?;
            if changed == 1 {
                Ok(Authorization::Forward)
            } else {
                Err("転送の許可が競合しました".into())
            }
        }
        _ => Err("このリンクを転送する許可を確認できません".into()),
    }
}

/// 転送の進み具合が 1 日以上更新されていない操作か。
fn transfer_idle(db: &Db, operation: &str) -> Result<bool, String> {
    let touched: Option<i64> = db
        .conn()
        .query_row(
            "SELECT touched_at FROM merged_link_transfer_progress WHERE operation_id=?1",
            [operation],
            |row| row.get(0),
        )
        .optional()
        .map_err(error)?;
    Ok(touched.is_none_or(|at| at < db::now_ms() - 24 * 60 * 60 * 1000))
}

pub fn relocation_retired(db: &Db, marker: &Marker, operation: &str) -> Result<(), String> {
    let changed = db.conn().execute(
        "UPDATE merged_link_relay SET revoked=2 WHERE link_id=?1 AND relocation_operation=?2 AND revoked=0",
        params![marker.link_id, operation],
    ).map_err(error)?;
    if changed != 1 {
        return Err("完了時に転送許可を照合できません".into());
    }
    Ok(())
}

pub fn remember_transfer_target(
    db: &Db,
    operation: &str,
    marker: &Marker,
    generation: i64,
) -> Result<(), String> {
    let changed = db
        .conn()
        .execute(
            "INSERT INTO merged_link_relocation_target VALUES (?1,?2,?3)
         ON CONFLICT(operation_id) DO UPDATE SET generation=excluded.generation
          WHERE marker=excluded.marker AND generation=excluded.generation",
            params![
                operation,
                serde_json::to_string(marker).map_err(error)?,
                generation
            ],
        )
        .map_err(error)?;
    if changed != 1 {
        return Err("操作の戻し先が一致しません".into());
    }
    Ok(())
}

pub fn transfer_target(db: &Db, operation: &str) -> Result<(Marker, i64), String> {
    let (json, generation): (String, i64) = db
        .conn()
        .query_row(
            "SELECT marker,generation FROM merged_link_relocation_target WHERE operation_id=?1",
            [operation],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|_| "戻し先の公開を確認できません".to_string())?;
    Ok((serde_json::from_str(&json).map_err(error)?, generation))
}

pub fn relocation_repointed(db: &Db, old: &Marker, operation: &str) -> Result<bool, String> {
    db.conn()
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM merged_link_relocation_done
         WHERE operation_id=?1 AND link_id=?2 AND marker_hash=?3)",
            params![
                operation,
                old.link_id,
                marker_digest(old)?.as_bytes().as_slice()
            ],
            |row| row.get(0),
        )
        .map_err(error)
}

pub fn finish_repoint(
    db: &mut Db,
    old: &Marker,
    new: &Marker,
    generation: i64,
    operation: &str,
) -> Result<(), String> {
    let tx = db
        .conn_mut()
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(error)?;
    let changed = tx.execute(
        "UPDATE merged_link_relay SET target_device=?2,marker_hash=?3,generation=?4,relocation_operation=NULL
         WHERE link_id=?1 AND generation=?5 AND relocation_operation=?6 AND state='active' AND revoked=0",
        params![old.link_id, new.target.device_id, marker_digest(new)?.as_bytes().as_slice(),
            generation+1,generation,operation],
    ).map_err(error)?;
    if changed != 1 {
        return Err("付け替えた中継許可を確認できません".into());
    }
    tx.execute(
        "INSERT INTO merged_link_relocation_done VALUES (?1,?2,?3)",
        params![
            operation,
            old.link_id,
            marker_digest(old)?.as_bytes().as_slice()
        ],
    )
    .map_err(error)?;
    tx.commit().map_err(error)
}

pub fn save_transfer_progress(
    db: &Db,
    operation: &str,
    received: u64,
    total: u64,
) -> Result<(), String> {
    let conn = db.conn();
    let now = db::now_ms();
    conn.execute(
        "DELETE FROM merged_link_transfer_progress WHERE touched_at < ?1",
        [now - 24 * 60 * 60 * 1000],
    )
    .map_err(error)?;
    conn.execute(
        "INSERT INTO merged_link_transfer_progress (operation_id,received,total,touched_at)
           VALUES (?1,?2,?3,?4)
           ON CONFLICT(operation_id) DO UPDATE SET received=max(received,excluded.received), touched_at=excluded.touched_at",
        params![operation, received.min(total) as i64, total as i64, now],
    ).map_err(error)?;
    Ok(())
}

pub fn transfer_progress(db: &Db) -> Result<Option<(u64, u64)>, String> {
    db.conn()
        .query_row(
            "SELECT received,total FROM merged_link_transfer_progress
         WHERE touched_at>?1 AND received<total ORDER BY touched_at DESC LIMIT 1",
            [db::now_ms() - 15_000],
            |row| Ok((row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)? as u64)),
        )
        .optional()
        .map_err(error)
}

pub(crate) fn state_at(db: &Db, id: &str) -> Result<Option<(i64, State)>, String> {
    let row: Option<(i64, String)> = db
        .conn()
        .query_row(
            "SELECT generation,state FROM merged_link WHERE link_id=?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(error)?;
    Ok(row.map(|(generation, state)| {
        (
            generation,
            match state.as_str() {
                "reserved" => State::Reserved,
                "active" => State::Active,
                _ => State::Removed,
            },
        )
    }))
}

pub fn status(db: &Db, link_id: &str) -> Result<Option<String>, String> {
    db.conn()
        .prepare_cached("SELECT sync_state FROM merged_link WHERE link_id=?1")
        .map_err(error)?
        .query_row([link_id], |row| row.get(0))
        .optional()
        .map_err(error)
}

pub fn sources(db: &Db, target: &Location) -> Result<Vec<Link>, String> {
    query_sources(db, target, false)
}

pub(crate) fn sources_under(db: &Db, target: &Location) -> Result<Vec<Link>, String> {
    query_sources(db, target, true)
}

fn query_sources(db: &Db, target: &Location, include_children: bool) -> Result<Vec<Link>, String> {
    let path = serde_json::to_string(&target.relative_path).map_err(error)?;
    let mut statement = db
        .conn()
        .prepare_cached(
            "SELECT marker, generation, state, checked_at, sync_state, sync_reason FROM merged_link
         WHERE target_device=?1 AND target_volume=?2 AND (?4 OR target_path=?3) AND state<>'removed'
         ORDER BY source_device, link_id",
        )
        .map_err(error)?;
    let mut rows = statement
        .query(params![
            target.device_id,
            target.volume_id,
            path,
            include_children
        ])
        .map_err(error)?;
    let mut links = Vec::new();
    while let Some(row) = rows.next().map_err(error)? {
        let marker: String = row.get(0).map_err(error)?;
        let marker: Marker = serde_json::from_str(&marker).map_err(error)?;
        if !marker
            .target
            .relative_path
            .starts_with(&target.relative_path)
        {
            continue;
        }
        links.push(Link {
            marker,
            generation: row.get(1).map_err(error)?,
            state: if row.get::<_, String>(2).map_err(error)? == "active" {
                State::Active
            } else {
                State::Reserved
            },
            checked_at: row.get(3).map_err(error)?,
            sync_state: row.get(4).map_err(error)?,
            sync_reason: row.get(5).map_err(error)?,
        });
    }
    Ok(links)
}

pub(crate) fn remove_received(
    conn: &Connection,
    marker: &Marker,
    generation: i64,
    relay: &str,
) -> Result<(), String> {
    let changed = conn
        .execute(
            "UPDATE merged_link SET state='removed',generation=generation+1,checked_at=?4
         WHERE link_id=?1 AND generation=?2 AND relay_device=?3 AND state='active'",
            params![marker.link_id, generation, relay, db::now_ms()],
        )
        .map_err(error)?;
    if changed != 1 {
        return Err("リンクの世代が変わりました".into());
    }
    Ok(())
}

pub(crate) fn retarget(
    conn: &Connection,
    previous: &Link,
    updated: &Marker,
    relay: Option<&str>,
) -> Result<(), String> {
    if previous.marker.link_id != updated.link_id
        || previous.marker.source != updated.source
        || previous.marker.hash != updated.hash
    {
        return Err("リンクの付け替え先が一致しません".into());
    }
    let target_path = serde_json::to_string(&updated.target.relative_path).map_err(error)?;
    let json = serde_json::to_string(updated).map_err(error)?;
    let changed = conn
        .execute(
            "UPDATE merged_link SET target_device=?2,target_volume=?3,target_path=?4,marker=?5,
          generation=generation+1,checked_at=?6
         WHERE link_id=?1 AND generation=?7 AND state='active' AND relay_device IS ?8",
            params![
                updated.link_id,
                updated.target.device_id,
                updated.target.volume_id,
                target_path,
                json,
                db::now_ms(),
                previous.generation,
                relay
            ],
        )
        .map_err(error)?;
    if changed == 0 {
        let current: Option<String> = conn.query_row(
            "SELECT marker FROM merged_link WHERE link_id=?1 AND generation=?2 AND state='active'",
            params![updated.link_id, previous.generation + 1], |row| row.get(0),
        ).optional().map_err(error)?;
        if current.as_deref() != Some(json.as_str()) {
            return Err("リンクの世代が変わりました".into());
        }
    }
    Ok(())
}

pub(crate) fn flag_actual_missing(
    conn: &Connection,
    ids: &[String],
    missing: bool,
) -> Result<(), String> {
    let mut change = conn.prepare_cached(
        "UPDATE merged_link SET sync_state=?2,checked_at=?3 WHERE link_id=?1 AND state='active'",
    ).map_err(error)?;
    for id in ids {
        change
            .execute(params![
                id,
                if missing { "missing" } else { "confirmed" },
                db::now_ms()
            ])
            .map_err(error)?;
    }
    Ok(())
}

fn error(value: impl std::fmt::Display) -> String {
    value.to_string()
}

/// 中継許可の一覧。秘密とそのハッシュは返さない。
#[derive(Serialize)]
pub struct Permit {
    pub link_id: String,
    pub source_device: String,
    pub target_device: String,
    pub generation: i64,
    pub revoked: bool,
}

pub fn permits(db: &Db) -> Result<Vec<Permit>, String> {
    db.conn()
        .prepare(
            "SELECT link_id, source_device, target_device, generation, revoked
         FROM merged_link_relay ORDER BY source_device, target_device, link_id",
        )
        .map_err(error)?
        .query_map([], |row| {
            Ok(Permit {
                link_id: row.get(0)?,
                source_device: row.get(1)?,
                target_device: row.get(2)?,
                generation: row.get(3)?,
                revoked: row.get(4)?,
            })
        })
        .map_err(error)?
        .collect::<rusqlite::Result<_>>()
        .map_err(error)
}

pub fn issue(db: &Db, marker: &Marker, request: &str, secret: &str) -> Result<(), String> {
    let source = &marker.source.device_id;
    let target = &marker.target.device_id;
    if !marker.valid()
        || source == target
        || Uuid::parse_str(request).is_err()
        || secret.len() != 64
    {
        return Err("リンク中継許可の対象が正しくありません".into());
    }
    let digest = blake3::hash(secret.as_bytes());
    let expected = marker_digest(marker)?;
    let changed = db.conn().execute(
        "INSERT INTO merged_link_relay(link_id,source_device,target_device,request_id,secret_hash,marker_hash,generation,state)
         VALUES (?1,?2,?3,?4,?5,?6,1,'reserved')
         ON CONFLICT(link_id) DO UPDATE SET generation=merged_link_relay.generation
         WHERE source_device=excluded.source_device AND target_device=excluded.target_device
           AND request_id=excluded.request_id AND secret_hash=excluded.secret_hash
           AND marker_hash=excluded.marker_hash AND revoked=0",
        params![marker.link_id, source, target, request, digest.as_bytes().as_slice(), expected.as_bytes().as_slice()],
    ).map_err(error)?;
    if changed == 0 {
        return Err("リンク中継許可が競合しました".into());
    }
    Ok(())
}

fn marker_digest(marker: &Marker) -> Result<blake3::Hash, String> {
    Ok(blake3::hash(&serde_json::to_vec(marker).map_err(error)?))
}

pub fn revoke(db: &Db, link_id: Uuid) -> Result<(), String> {
    db.conn()
        .execute(
            "UPDATE merged_link_relay SET revoked=1 WHERE link_id=?1",
            [link_id.to_string()],
        )
        .map_err(error)?;
    Ok(())
}

/// 終端操作が到達して応答を失ったとき、同じ世代には再送の成功だけ返せる。
#[derive(PartialEq, Eq)]
pub enum Authorization {
    Forward,
    AlreadyApplied,
}

pub fn authorize(
    db: &Db,
    update: &Update,
    source: &str,
    secret: &str,
) -> Result<Authorization, String> {
    let marker = &update.marker;
    if !marker.valid() || marker.source.device_id != source || update.state == State::Reserved {
        return Err("リンク中継の要求が一致しません".into());
    }
    let expected = marker_digest(marker)?;
    let stored = db.conn().query_row(
        "SELECT generation, state, revoked FROM merged_link_relay
         WHERE link_id=?1 AND source_device=?2 AND target_device=?3 AND secret_hash=?4 AND marker_hash=?5",
        params![marker.link_id, source, marker.target.device_id,
            blake3::hash(secret.as_bytes()).as_bytes().as_slice(), expected.as_bytes().as_slice()],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?)),
    ).optional().map_err(error)?;
    match stored {
        Some((generation, state, revoked))
            if update.generation > 1
                && update.generation == generation
                && state == update.state.as_str()
                && revoked != 1 =>
        {
            Ok(Authorization::AlreadyApplied)
        }
        Some((generation, _, 0)) if update.generation > generation => Ok(Authorization::Forward),
        _ => Err("リンク中継許可を確認できません".into()),
    }
}

pub fn forwarded(db: &Db, update: &Update) -> Result<(), String> {
    db.conn()
        .execute(
            "UPDATE merged_link_relay SET generation=?2, revoked=?3, state=?4
         WHERE link_id=?1 AND generation<?2 AND revoked=0",
            params![
                update.marker.link_id.to_string(),
                update.generation,
                if update.state == State::Removed { 2 } else { 0 },
                update.state.as_str()
            ],
        )
        .map_err(error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marker() -> Marker {
        fn place(device: &str) -> Location {
            Location {
                device_id: device.into(),
                device_name: device.into(),
                volume_id: "volume-id".into(),
                volume_name: "volume".into(),
                relative_path: vec!["同じ 名前.txt".into()],
                display_path: "/volume/同じ 名前.txt".into(),
            }
        }
        Marker::new(
            place("source"),
            place("target"),
            crate::dups::ContentKey {
                directory: false,
                size: 1,
                hash: [1; 32],
            },
            1,
            0,
        )
    }

    #[test]
    fn duplicate_and_older_generations_cannot_restore_removed_link() {
        let mut db = Db::open_in_memory().unwrap();
        let mut update = Update {
            marker: marker(),
            generation: 1,
            state: State::Reserved,
        };
        assert_eq!(
            apply(&mut db, &update, Some("relay"), false),
            Ok(Applied::New)
        );
        assert_eq!(
            apply(&mut db, &update, Some("relay"), false),
            Ok(Applied::Duplicate)
        );
        update.generation = 2;
        update.state = State::Active;
        assert_eq!(
            apply(&mut db, &update, Some("relay"), true),
            Ok(Applied::New)
        );
        let active = update.clone();
        update.generation = 3;
        update.state = State::Removed;
        assert_eq!(
            apply(&mut db, &update, Some("relay"), true),
            Ok(Applied::New)
        );
        acknowledge(&mut db, &update.marker.link_id, active.generation).unwrap();
        let generation: i64 = db
            .conn()
            .query_row("SELECT generation FROM merged_link_outbox", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(generation, 3);
        assert_eq!(
            apply(&mut db, &update, Some("relay"), false),
            Ok(Applied::Duplicate)
        );
        assert_eq!(
            apply(&mut db, &active, Some("relay"), false),
            Ok(Applied::Stale)
        );
        assert!(sources(&db, &update.marker.target).unwrap().is_empty());
    }

    #[test]
    fn a_terminal_permit_only_accepts_receipt_retries() {
        let db = Db::open_in_memory().unwrap();
        let marker = marker();
        let secret = "a".repeat(64);
        issue(&db, &marker, &Uuid::new_v4().to_string(), &secret).unwrap();
        let mut update = Update {
            marker,
            generation: 2,
            state: State::Removed,
        };
        assert!(authorize(&db, &update, "source", "wrong").is_err());
        assert!(matches!(
            authorize(&db, &update, "source", &secret),
            Ok(Authorization::Forward)
        ));
        forwarded(&db, &update).unwrap();
        assert!(matches!(
            authorize(&db, &update, "source", &secret),
            Ok(Authorization::AlreadyApplied)
        ));
        update.generation = 3;
        assert!(authorize(&db, &update, "source", &secret).is_err());
        assert!(permits(&db).unwrap()[0].revoked);
    }
}
