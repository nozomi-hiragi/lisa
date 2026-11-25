//! 隔離前の目印記録と、隔離完了後の公開・復元時の確認。

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::db::Db;
use crate::dups::ContentKey;
use crate::listing::Kind;
use crate::merged::{self, Location, Marker, links};

use super::plan::{OpKind, PlanOp};
use super::{check, file_identity};

#[derive(Deserialize, Serialize)]
pub(super) struct Journal {
    pub(super) marker: Marker,
    pub(super) parent: (u64, [u8; 16]),
    pub(super) state: State,
    #[serde(default)]
    relay_device: Option<String>,
}

#[derive(Deserialize, Serialize)]
pub(super) enum State {
    Pending,
    Staged {
        temporary: PathBuf,
        path: PathBuf,
        identity: (u64, [u8; 16]),
    },
    Published {
        path: PathBuf,
        identity: (u64, [u8; 16]),
    },
    Failed(String),
}

fn load(db: &Db, op: &PlanOp) -> Result<Option<Journal>, String> {
    journal(db, op.plan_id, op.seq)
}

pub(super) fn journal(db: &Db, plan_id: i64, seq: i64) -> Result<Option<Journal>, String> {
    let json: Option<String> = db
        .conn()
        .query_row(
            "SELECT journal FROM merge_marker WHERE plan_id=?1 AND seq=?2",
            params![plan_id, seq],
            |row| row.get(0),
        )
        .optional()
        .map_err(error)?;
    json.map(|json| serde_json::from_str(&json).map_err(error))
        .transpose()
}

fn save(db: &Db, journal: &Journal) -> Result<(), String> {
    save_journal(db.conn(), journal)
}

pub(super) fn save_journal(conn: &rusqlite::Connection, journal: &Journal) -> Result<(), String> {
    conn.execute(
        "INSERT INTO merge_marker (plan_id, seq, journal) VALUES (?1, ?2, ?3)
         ON CONFLICT(plan_id, seq) DO UPDATE SET journal=excluded.journal",
        params![
            journal.marker.plan_id,
            journal.marker.item_id,
            serde_json::to_string(journal).map_err(error)?
        ],
    )
    .map_err(error)?;
    Ok(())
}

pub(super) fn prepare(
    db: &mut Db,
    op: &PlanOp,
    remote: Option<(&super::Context, &super::remote_keep::RemotePlan)>,
    remote_target: Option<Location>,
) -> Result<(), String> {
    if op.op != OpKind::Trash || (op.keep.is_none() && remote.is_none()) {
        return Ok(());
    }
    let parent = op
        .src
        .parent()
        .and_then(file_identity)
        .ok_or("元の親を識別できません")?;
    let drive = op
        .drive_id
        .as_deref()
        .ok_or("元のボリュームが分かりません")?;
    let source = Location::local(db, drive, &op.src).ok_or("元の所在が分かりません")?;
    let (key, target) = if let Some((_, remote)) = remote {
        (
            remote.keep.key.clone(),
            remote_target.ok_or("残す側の所在を取得できません")?,
        )
    } else {
        let keep = check::keep_path_for_apply(db, op.plan_id, op.keep.as_deref().unwrap())?
            .ok_or("残す側が見つかりません")?;
        if keep == op.src || keep.starts_with(&op.src) || super::same_file_identity(&op.src, &keep)
        {
            return Err("隔離後に使える別の実体がありません".into());
        }
        let drives = crate::query::drives(db).map_err(error)?;
        let target = drives
            .iter()
            .filter_map(|drive| Some((drive, drive.mount.as_ref()?)))
            .filter(|(_, mount)| keep.starts_with(mount))
            .max_by_key(|(_, mount)| mount.components().count())
            .and_then(|(drive, _)| Location::local(db, &drive.id, &keep))
            .ok_or("残す側の所在が分かりません")?;
        let directory = fs::symlink_metadata(&op.src).map_err(error)?.is_dir();
        let hash = op.hash.ok_or("照合済みの完全ハッシュがありません")?;
        let size = op.size.ok_or("元のサイズが分かりません")?;
        (
            ContentKey {
                directory,
                hash,
                size,
            },
            target,
        )
    };
    let mut journal = match load(db, op)? {
        Some(journal)
            if matches!(journal.state, State::Pending)
                && journal.parent == parent
                && journal.marker.source == source
                && journal.marker.target == target =>
        {
            journal
        }
        _ => Journal {
            marker: Marker::new(source, target, key, op.plan_id, op.seq),
            parent,
            state: State::Pending,
            relay_device: remote.map(|(_, remote)| remote.route.requester_device.clone()),
        },
    };
    save(db, &journal)?;
    let update = links::Update {
        previous_source: None,
        marker: journal.marker.clone(),
        generation: 1,
        state: links::State::Reserved,
    };
    links::apply(db, &update, journal.relay_device.as_deref(), false)?;
    if let Some((ctx, remote)) = remote {
        let registered = ctx
            .link_client
            .ok_or_else(|| "リンク中継を使えません".to_string())
            .and_then(|client| client(&ctx.data_dir, &remote.route, &update));
        if let Err(reason) = registered {
            let warning = format!("隔離済み／目印を作成できませんでした: {reason}");
            journal.state = State::Failed(warning);
            save(db, &journal)?;
            change_link(db, &journal, links::State::Removed)?;
            return Err(reason);
        }
    }
    Ok(())
}

fn change_link(db: &mut Db, journal: &Journal, state: links::State) -> Result<(), String> {
    let Some((generation, current)) = links::state_at(db, &journal.marker.link_id)? else {
        return Ok(());
    };
    if current == state || current == links::State::Removed {
        return Ok(());
    }
    let generation = if state == links::State::Active {
        2
    } else {
        (generation + 1).max(3)
    };
    links::apply(
        db,
        &links::Update {
            previous_source: None,
            marker: journal.marker.clone(),
            generation,
            state,
        },
        journal.relay_device.as_deref(),
        journal.relay_device.is_some(),
    )?;
    Ok(())
}

pub(super) fn abandon(db: &mut Db, op: &PlanOp) -> Result<(), String> {
    if let Some(journal) = load(db, op)? {
        change_link(db, &journal, links::State::Removed)?;
    }
    Ok(())
}

pub fn flush_links(db: &mut Db, ctx: &super::Context) -> Result<(), String> {
    {
        let Ok(_lock) = super::apply::try_lock_plans(&ctx.data_dir) else {
            return Ok(());
        };
        super::linked_move::flush_moves(db, ctx)?;
        check_for_removed_markers(db)?;
    }
    super::linked_move::flush_retargets(db, ctx, None)?;
    let Some(client) = ctx.link_client else {
        return Ok(());
    };
    links::expire(db)?;
    let mut unreachable = std::collections::HashSet::new();
    let mut attempts = 0;
    for update in links::pending(db)? {
        let Some(remote) = super::remote_keep::remote_plan(db, update.marker.plan_id)? else {
            continue;
        };
        if unreachable.contains(&remote.route.requester_device) {
            continue;
        }
        if client(&ctx.data_dir, &remote.route, &update).is_ok() {
            links::acknowledge(db, &update.marker.link_id, update.generation)?;
        } else {
            unreachable.insert(remote.route.requester_device);
        }
        attempts += 1;
        if attempts == 4 {
            break;
        }
    }
    Ok(())
}

fn check_for_removed_markers(db: &mut Db) -> Result<(), String> {
    let candidates: Vec<String> = db.conn().prepare_cached(
        "SELECT j.journal FROM merged_link l JOIN merge_marker j
         ON j.plan_id=json_extract(l.marker,'$.plan_id') AND j.seq=json_extract(l.marker,'$.item_id')
         WHERE l.state='active' AND l.source_device=(SELECT value FROM meta WHERE key='this_device')
           AND l.link_id=json_extract(j.journal,'$.marker.link_id')
           AND NOT EXISTS (SELECT 1 FROM merged_link_rewrite r WHERE r.link_id=l.link_id)
           AND NOT EXISTS (SELECT 1 FROM merged_link_move_retry r WHERE r.link_id=l.link_id)
           AND NOT EXISTS (
               SELECT 1 FROM merged_link_move_undo u
               JOIN plan_op p ON p.plan_id=u.plan_id AND p.seq=u.seq
               JOIN json_each(u.movements) movement ON movement.key=l.link_id
               WHERE p.state='done'
           )
           AND NOT EXISTS (
               SELECT 1 FROM merged_link_move m
               JOIN plan_op p ON p.plan_id=m.plan_id AND p.seq=m.seq
               JOIN json_each(m.movements) movement ON movement.key=l.link_id
               WHERE p.state='running'
           )
         ORDER BY l.marker_checked_at, l.link_id LIMIT 128"
    ).map_err(error)?.query_map([], |row| row.get(0)).map_err(error)?
        .collect::<rusqlite::Result<_>>().map_err(error)?;
    for candidate in candidates {
        let journal: Journal = serde_json::from_str(&candidate).map_err(error)?;
        if let State::Published { path, .. } = &journal.state
            && path.parent().and_then(file_identity) == Some(journal.parent)
            && matches!(fs::symlink_metadata(path), Err(error) if error.kind() == io::ErrorKind::NotFound)
        {
            change_link(db, &journal, links::State::Removed)?;
        }
        db.conn()
            .execute(
                "UPDATE merged_link SET marker_checked_at=?2 WHERE link_id=?1",
                params![journal.marker.link_id.to_string(), crate::db::now_ms()],
            )
            .map_err(error)?;
    }
    Ok(())
}

pub(super) fn folder_hash(path: &Path) -> Result<crate::hash::Hash, String> {
    hash_folder(path, None)
}

pub(crate) fn target_folder_hash(db: &Db, path: &Path) -> Result<crate::hash::Hash, String> {
    hash_folder(path, Some(db))
}

fn hash_folder(path: &Path, db: Option<&Db>) -> Result<crate::hash::Hash, String> {
    if !fs::symlink_metadata(path).map_err(error)?.is_dir() {
        return Err("照合対象がフォルダではありません".into());
    }
    let listing = crate::listing::read_dir(path).map_err(error)?;
    if !listing.failed.is_empty() {
        return Err("フォルダに読めない項目があります".into());
    }
    let mut real = Vec::with_capacity(listing.entries.len());
    let mut links = Vec::new();
    for entry in &listing.entries {
        if entry.other_volume || entry.cloud_only || (entry.merged.is_some() && db.is_none()) {
            return Err("フォルダに完全照合できない項目があります".into());
        }
        if let (Some(db), Some(marker)) = (db, &entry.merged)
            && verified(db.conn(), entry).map_err(error)?
        {
            links.push(marker);
            continue;
        }
        let child = path.join(&entry.name);
        let hash = match entry.kind {
            Kind::File => Some(crate::hash::hash_file(&child).map_err(error)?),
            Kind::Dir => Some(hash_folder(&child, db)?),
            _ => None,
        };
        real.push(crate::hash::NamedChild {
            name: entry.name_nfc.clone(),
            raw_name: entry.name_raw.clone(),
            kind: entry.kind,
            hash,
            size: entry.size,
        });
    }
    Ok(*blake3::hash(&crate::hash::tree_bytes_with_markers(real, links)).as_bytes())
}

pub(super) fn publish(
    db: &mut Db,
    op: &PlanOp,
    quarantined: Option<&Path>,
) -> Result<Option<String>, String> {
    let Some(mut journal) = load(db, op)? else {
        return Ok(None);
    };
    if let State::Failed(reason) = &journal.state {
        return Ok(Some(reason.clone()));
    }
    let result = if matches!(journal.state, State::Published { .. }) {
        Ok(())
    } else {
        publish_pending(db, op, &mut journal)
    };
    match result {
        Ok(()) => {
            change_link(db, &journal, links::State::Active)?;
            if let (Some(quarantined), State::Published { path, .. }) = (quarantined, journal.state)
            {
                return Ok(super::quarantine::record_merge_marker(
                    quarantined,
                    op.plan_id,
                    op.seq,
                    &path,
                )
                .err()
                .map(|reason| {
                    format!("目印は作成済み／隔離マニフェストに記録できませんでした: {reason}")
                }));
            }
            Ok(None)
        }
        Err(reason) => {
            if let State::Staged {
                temporary,
                identity,
                ..
            } = &journal.state
                && file_identity(temporary) == Some(*identity)
            {
                let _ = fs::remove_file(temporary);
            }
            let warning = format!("隔離済み／目印を作成できませんでした: {reason}");
            journal.state = State::Failed(warning.clone());
            save(db, &journal)?;
            change_link(db, &journal, links::State::Removed)?;
            Ok(Some(warning))
        }
    }
}

fn publish_pending(db: &Db, op: &PlanOp, journal: &mut Journal) -> Result<(), String> {
    let parent = op.src.parent().ok_or("元の親がありません")?;
    let name = journal
        .marker
        .source
        .relative_path
        .last()
        .ok_or("元の名前がありません")?;
    let extension = if journal.marker.kind == Kind::Dir {
        "lmd"
    } else {
        "lmf"
    };
    for number in 1..=10_000 {
        if file_identity(parent) != Some(journal.parent) {
            return Err("元の親が移動または変更されました".into());
        }
        if matches!(journal.state, State::Pending) {
            let suffix = if number == 1 {
                format!(".{extension}")
            } else {
                format!(" ({number}).{extension}")
            };
            let mut end = name.len().min(255 - suffix.len());
            while !name.is_char_boundary(end) {
                end -= 1;
            }
            let path = parent.join(format!("{}{suffix}", &name[..end]));
            let temporary = parent.join(format!(".lisa-marker-{}.tmp", uuid::Uuid::new_v4()));
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .map_err(error)?;
            let staged = (|| {
                file.write_all(&serde_json::to_vec_pretty(&journal.marker).map_err(error)?)
                    .map_err(error)?;
                file.sync_all().map_err(error)?;
                file_identity(&temporary).ok_or_else(|| "一時ファイルを識別できません".to_string())
            })();
            drop(file);
            let identity = match staged {
                Ok(identity) => identity,
                Err(reason) => {
                    let _ = fs::remove_file(&temporary);
                    return Err(reason);
                }
            };
            journal.state = State::Staged {
                temporary,
                path,
                identity,
            };
            save(db, journal)?;
        }
        let State::Staged {
            temporary,
            path,
            identity,
        } = &journal.state
        else {
            return Err("目印の公開状態が一致しません".into());
        };
        if file_identity(temporary) == Some(*identity) {
            match super::apply::rename_without_replace(temporary, path) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                    fs::remove_file(temporary).map_err(error)?;
                    journal.state = State::Pending;
                    save(db, journal)?;
                    continue;
                }
                Err(err) => return Err(error(err)),
            }
        }
        if file_identity(path) != Some(*identity)
            || merged::read(path).as_ref() != Some(&journal.marker)
        {
            return Err("中断後に同じ目印を確認できません".into());
        }
        journal.state = State::Published {
            path: path.clone(),
            identity: *identity,
        };
        return save(db, journal);
    }
    Err("空いている目印の名前が見つかりません".into())
}

pub(super) fn remove_after_undo(db: &mut Db, op: &PlanOp) -> Result<(), String> {
    let Some(journal) = load(db, op)? else {
        return Ok(());
    };
    let State::Published { ref path, identity } = journal.state else {
        return Ok(());
    };
    let metadata = match fs::symlink_metadata(path) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return change_link(db, &journal, links::State::Removed);
        }
        Err(err) => return Err(error(err)),
        Ok(metadata) => metadata,
    };
    let expected = serde_json::to_vec_pretty(&journal.marker).map_err(error)?;
    if path.parent().and_then(file_identity) != Some(journal.parent)
        || file_identity(path) != Some(identity)
        || merged::read_bytes(path, &metadata).as_ref() != Some(&expected)
    {
        return Err(format!(
            "編集または差替えを検出しました: {}",
            path.display()
        ));
    }
    fs::remove_file(path).map_err(error)?;
    change_link(db, &journal, links::State::Removed)
}

fn error(value: impl std::fmt::Display) -> String {
    value.to_string()
}

pub(crate) fn verified(
    conn: &rusqlite::Connection,
    entry: &crate::listing::Entry,
) -> rusqlite::Result<bool> {
    let (Some(marker), Some(identity)) = (&entry.merged, entry.marker_identity) else {
        return Ok(false);
    };
    let json: Option<String> = conn
        .prepare_cached("SELECT journal FROM merge_marker WHERE plan_id=?1 AND seq=?2")?
        .query_row(params![marker.plan_id, marker.item_id], |row| row.get(0))
        .optional()?;
    let journal = json.and_then(|json| serde_json::from_str::<Journal>(&json).ok());
    Ok(journal.is_some_and(|journal| {
        journal.marker == *marker
            && Some(journal.parent) == entry.marker_parent
            && matches!(journal.state, State::Published { path, identity: expected }
            if identity == expected && path.file_name() == Some(entry.name.as_os_str()))
    }))
}
