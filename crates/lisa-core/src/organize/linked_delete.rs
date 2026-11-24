//! マージ済みリンクを持つ実体の削除に必要な確認。

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::db::Db;
use crate::merged::{self, Location, Marker, links};

use super::{PlanOp, apply, file_identity, markers};

mod transfer;
pub use transfer::Manifest as TransferManifest;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum DeleteChoice {
    DeleteActual,
    Restore { link_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct DeleteRequest {
    pub choice: DeleteChoice,
    /// 対象が増えた場合は、改めて一覧を見てから確定する。
    pub links: Vec<String>,
}

pub fn linked_delete_sources(db: &Db, path: &Path) -> Result<Vec<links::Link>, String> {
    let Some(location) = crate::query::locate(db, path).map_err(error)? else {
        return Ok(Vec::new());
    };
    let Some(target) = Location::local(db, &location.drive_id, &location.path) else {
        return Ok(Vec::new());
    };
    links::sources_under(db, &target)
}

pub(super) fn require_decision(
    db: &Db,
    target: &Location,
    request: Option<&DeleteRequest>,
) -> Result<(), String> {
    let actual = links::sources_under(db, target)?;
    if actual.is_empty() && request.is_none() {
        return Ok(());
    }
    if actual
        .iter()
        .any(|link| link.state == links::State::Reserved || link.sync_state == "pending")
    {
        return Err("リンクの照合が未完了のため実体の削除を保留します".into());
    }
    let expected = request.ok_or_else(|| {
        let places: Vec<_> = actual
            .iter()
            .map(|link| {
                let source = &link.marker.source;
                format!(
                    "{} › {}（リンク ID {}）",
                    source.device_name, source.display_path, link.marker.link_id
                )
            })
            .collect();
        format!(
            "確認待ち（リンク元 {} 件。trash_linked で戻し先か実体の削除を選んでください）: {}",
            places.len(),
            places.join("／")
        )
    })?;
    let mut seen: Vec<&str> = actual
        .iter()
        .map(|link| link.marker.link_id.as_str())
        .collect();
    let mut confirmed: Vec<&str> = expected.links.iter().map(String::as_str).collect();
    seen.sort_unstable();
    confirmed.sort_unstable();
    if seen != confirmed || seen.is_empty() {
        return Err("リンク元が変わりました。現在のリンクを確認して選び直してください".into());
    }
    if let DeleteChoice::Restore { link_id } = &expected.choice
        && !actual
            .iter()
            .any(|link| link.marker.link_id == *link_id && &link.marker.target == target)
    {
        return Err("この実体全体を戻せる場所が選ばれていません".into());
    }
    Ok(())
}

pub(super) fn preflight(
    db: &Db,
    target: &Location,
    request: &DeleteRequest,
    op: &PlanOp,
) -> Result<(), String> {
    if let Some(progress) = load_progress(db, op)? {
        let actual = links::sources_under(db, target)?;
        if actual.iter().any(|link| {
            !progress.links.iter().any(|old| {
                old.marker.link_id == link.marker.link_id && old.generation == link.generation
            })
        }) {
            return Err("作業中にリンク元が変わりました。旧実体を保持しました".into());
        }
        return Ok(());
    }
    require_decision(db, target, Some(request))?;
    let sources = links::sources_under(db, target)?;
    let remote = sources
        .iter()
        .filter(|link| link.marker.source.device_id != target.device_id)
        .count();
    // 実体の削除は各端末の目印を残したまま「リンク先なし」にするだけなので、構成を問わない。
    // 付け替えは、端末内だけか、すべて別の端末（同じ中継）の場合に限る。
    if remote > 0
        && remote != sources.len()
        && matches!(request.choice, DeleteChoice::Restore { .. })
    {
        return Err(
            "この端末と別の端末のリンク元が混在するため付け替えられません。旧実体を保持しました"
                .into(),
        );
    }
    if matches!(request.choice, DeleteChoice::Restore { .. })
        && sources
            .iter()
            .any(|link| link.marker.target.relative_path != target.relative_path)
    {
        return Err("配下の別の実体にもリンク元があるため、実体を付け替えられません".into());
    }
    Ok(())
}

type Identity = (u64, [u8; 16]);

#[derive(Deserialize, Serialize)]
struct Progress {
    links: Vec<links::Link>,
    destination: Option<Restored>,
    rewrites: Vec<Rewrite>,
    chosen_removed: bool,
    #[serde(default)]
    peer_published: bool,
}

#[derive(Deserialize, Serialize)]
struct Restored {
    place: Location,
    path: PathBuf,
    identity: Identity,
}

#[derive(Deserialize, Serialize)]
struct Rewrite {
    link_id: String,
    identity: Identity,
}

fn load_progress(db: &Db, op: &PlanOp) -> Result<Option<Progress>, String> {
    let data: Option<String> = db
        .conn()
        .query_row(
            "SELECT progress FROM merged_link_delete WHERE plan_id=?1 AND seq=?2",
            params![op.plan_id, op.seq],
            |row| row.get(0),
        )
        .optional()
        .map_err(error)?
        .flatten();
    data.map(|json| serde_json::from_str(&json).map_err(error))
        .transpose()
}

fn save_progress(db: &Db, op: &PlanOp, progress: &Progress) -> Result<(), String> {
    save_progress_on(db.conn(), op, progress)
}

fn save_progress_on(conn: &Connection, op: &PlanOp, progress: &Progress) -> Result<(), String> {
    conn.execute(
        "UPDATE merged_link_delete SET progress=?3 WHERE plan_id=?1 AND seq=?2",
        params![
            op.plan_id,
            op.seq,
            serde_json::to_string(progress).map_err(error)?
        ],
    )
    .map_err(error)?;
    Ok(())
}

pub(super) fn prepare_delete(
    db: &mut Db,
    ctx: &super::Context,
    op: &PlanOp,
) -> Result<Vec<PathBuf>, String> {
    let Some((operation, request)) = load_request(db, op)? else {
        return Ok(Vec::new());
    };
    let target = op
        .drive_id
        .as_deref()
        .and_then(|drive| Location::local(db, drive, &op.src))
        .ok_or("旧実体の所在が分かりません")?;
    preflight(db, &target, &request, op)?;
    let DeleteChoice::Restore { link_id } = request.choice else {
        return Ok(Vec::new());
    };
    let mut progress = match load_progress(db, op)? {
        Some(progress) => progress,
        None => {
            let links = links::sources_under(db, &target)?;
            for link in links
                .iter()
                .filter(|link| link.marker.source.device_id == target.device_id)
            {
                let (journal, path, identity) = owned_marker(db, &link.marker)?;
                verify_marker(&path, &journal.marker, identity)?;
            }
            let progress = Progress {
                links,
                destination: None,
                rewrites: Vec::new(),
                chosen_removed: false,
                peer_published: false,
            };
            save_progress(db, op, &progress)?;
            progress
        }
    };
    let chosen = progress
        .links
        .iter()
        .find(|link| link.marker.link_id == link_id)
        .cloned()
        .ok_or("戻し先のリンクがありません")?;
    if chosen.marker.source.device_id != target.device_id {
        restore_to_peer(db, ctx, op, &operation, &chosen, &mut progress)?;
        return Ok(Vec::new());
    }
    verify_content(db, &op.src, &chosen.marker)?;
    let path = restored_path(db, &chosen.marker.source)?;
    let parent = path.parent().ok_or("戻し先の親フォルダがありません")?;
    let (chosen_journal, chosen_marker_path, chosen_identity) = owned_marker(db, &chosen.marker)?;
    if file_identity(parent) != Some(chosen_journal.parent)
        || chosen_marker_path.parent() != Some(parent)
    {
        return Err(format!("戻し先の親が変わりました: {}", parent.display()));
    }
    let staged = parent.join(format!(".lisa-restore-{operation}.tmp"));
    if progress.destination.is_none() {
        verify_marker(&chosen_marker_path, &chosen.marker, chosen_identity)?;
        if exists(&path)? {
            return Err(format!("戻し先にすでに項目があります: {}", path.display()));
        }
        let quarantine = super::plan::plan_ops(db, chosen.marker.plan_id)
            .map_err(error)?
            .into_iter()
            .find(|op| op.seq == chosen.marker.item_id && op.state == super::OpState::Done)
            .and_then(|op| op.dst)
            .filter(|path| verify_content(db, path, &chosen.marker).is_ok());
        // 一時項目の名前には操作 ID が入っている。前回コピーしたあと記録の前に落ちた場合は、
        // 中身が照合できればそのまま使い、できなければ（途中まで）消して作り直す。
        let reusable = exists(&staged)? && verify_content(db, &staged, &chosen.marker).is_ok();
        if !reusable {
            if let Ok(meta) = fs::symlink_metadata(&staged) {
                if meta.is_dir() {
                    fs::remove_dir_all(&staged).map_err(error)?;
                } else {
                    fs::remove_file(&staged).map_err(error)?;
                }
            }
            if available_space(parent)? < chosen.marker.size {
                return Err(format!(
                    "戻し先の空き容量が足りません: {}",
                    parent.display()
                ));
            }
            let source = quarantine.as_deref().unwrap_or(&op.src);
            apply::copy_verify_and_publish(op.plan_id, op.seq, source, &staged)?;
        }
        verify_content(db, &staged, &chosen.marker)?;
        let identity = file_identity(&staged).ok_or("戻す実体を識別できません")?;
        progress.destination = Some(Restored {
            path: path.clone(),
            place: chosen.marker.source.clone(),
            identity,
        });
        save_progress(db, op, &progress)?;
    }
    let restored = progress
        .destination
        .as_ref()
        .ok_or("戻し先が確定していません")?;
    if restored.path != path {
        return Err("戻し先のボリュームが変更されました".into());
    }
    let identity = restored.identity;
    let destination = restored.place.clone();
    if file_identity(&staged) == Some(identity) {
        apply::rename_without_replace(&staged, &path)
            .map_err(|error| format!("{} に公開できません: {error}", path.display()))?;
    }
    if file_identity(&path) != Some(identity) {
        return Err(format!("戻した実体を識別できません: {}", path.display()));
    }
    verify_content(db, &path, &chosen.marker)?;

    let mut touched = vec![parent.to_path_buf()];
    for link in progress
        .links
        .clone()
        .into_iter()
        .filter(|link| link.marker.link_id != link_id)
    {
        if let Some(parent) = rewrite_link(
            db,
            &operation,
            &link,
            &destination,
            &mut progress,
            None,
            |db, progress| save_progress(db, op, progress),
        )? {
            touched.push(parent);
        }
    }
    if !progress.chosen_removed {
        let retired =
            chosen_marker_path.with_file_name(format!(".lisa-restore-{operation}.retired"));
        if exists(&chosen_marker_path)? {
            verify_marker(&chosen_marker_path, &chosen.marker, chosen_identity)?;
            apply::rename_without_replace(&chosen_marker_path, &retired).map_err(error)?;
        }
        verify_marker(&retired, &chosen.marker, chosen_identity)?;
        links::apply(
            db,
            &links::Update {
                marker: chosen.marker.clone(),
                generation: chosen.generation + 1,
                state: links::State::Removed,
            },
            None,
            false,
        )?;
        progress.chosen_removed = true;
        save_progress(db, op, &progress)?;
        fs::remove_file(&retired).map_err(error)?;
    }
    verify_content(db, &op.src, &chosen.marker)?;
    Ok(touched)
}

fn restore_to_peer(
    db: &mut Db,
    ctx: &super::Context,
    op: &PlanOp,
    operation: &str,
    chosen: &links::Link,
    progress: &mut Progress,
) -> Result<(), String> {
    verify_content(db, &op.src, &chosen.marker)?;
    if progress.chosen_removed {
        return Ok(());
    }
    let (_, _, relay) = links::stored_source(db, &chosen.marker.link_id)?
        .ok_or("戻し先の同期済みリンクを確認できません")?;
    let client = ctx
        .relocation_client
        .ok_or("端末間の付け替えの中継を利用できません")?;
    let send = |action| {
        let request = TransferRequest {
            operation_id: operation.to_owned(),
            marker: chosen.marker.clone(),
            generation: chosen.generation,
            expires_at: crate::db::now_ms() + 90_000,
            action,
        };
        client(&ctx.data_dir, &relay, &request).map_err(|error| {
            let source = &chosen.marker.source;
            format!(
                "{} › {} に戻せません: {error}",
                source.device_name, source.display_path
            )
        })
    };
    if !progress.peer_published {
        let manifest = transfer::Manifest::scan(&op.src)?;
        manifest.validate(&chosen.marker)?;
        let started = send(TransferAction::Begin {
            manifest: manifest.clone(),
        })?;
        if !started.published {
            let mut offsets = started.offsets;
            let source = op.src.clone();
            for _round in 0..3 {
                if offsets.len() != manifest.entries.len() {
                    return Err("戻し先から転送位置を確認できません".into());
                }
                for (index, (part, &offset)) in manifest.entries.iter().zip(&offsets).enumerate() {
                    if offset > part.len {
                        return Err("転送位置の応答が正しくありません".into());
                    }
                    if offset < part.len {
                        send_part(&source, index, part, offset, &send)?;
                    }
                }
                // 受け側が途中で落ちて、先に届いた分を失っていればここで分かるので送り直す。
                let reply = send(TransferAction::Begin {
                    manifest: manifest.clone(),
                })?;
                if reply.published {
                    break;
                }
                offsets = reply.offsets;
                if offsets.len() == manifest.entries.len()
                    && manifest
                        .entries
                        .iter()
                        .zip(&offsets)
                        .all(|(part, &offset)| offset == part.len)
                {
                    break;
                }
            }
            if !send(TransferAction::Publish)?.published {
                return Err("戻し先の公開確認がありません".into());
            }
        }
        progress.peer_published = true;
        save_progress(db, op, progress)?;
    }
    for source in progress
        .links
        .iter()
        .filter(|link| link.marker.link_id != chosen.marker.link_id)
    {
        let (_, _, source_relay) = links::stored_source(db, &source.marker.link_id)?
            .ok_or("付け替えるリンクの中継端末が見つかりません")?;
        if source_relay != relay {
            return Err("付け替えるリンクの中継端末が揃っていません".into());
        }
        let request = TransferRequest {
            operation_id: operation.to_owned(),
            marker: source.marker.clone(),
            generation: source.generation,
            expires_at: crate::db::now_ms() + 90_000,
            action: TransferAction::Repoint {
                chosen: chosen.marker.clone(),
            },
        };
        let reply = client(&ctx.data_dir, &relay, &request).map_err(|error| {
            format!(
                "{} › {} を付け替えられません: {error}",
                source.marker.source.device_name, source.marker.source.display_path
            )
        })?;
        if !reply.published {
            return Err("付け替えの完了確認がありません".into());
        }
    }
    verify_content(db, &op.src, &chosen.marker)?;
    if !send(TransferAction::Retire)?.published {
        return Err("戻し先の目印を除去した確認がありません".into());
    }
    let tx = db.conn_mut().transaction().map_err(error)?;
    for source in &progress.links {
        links::remove_received(&tx, &source.marker, source.generation, &relay)?;
    }
    progress.chosen_removed = true;
    save_progress_on(&tx, op, progress)?;
    tx.commit().map_err(error)
}

/// 1 つのファイルを `LANES` 本で並行に送る。受け側が先に持てる量を超えないよう、連続して届いた
/// 位置（受け側の返事）から先へ出しすぎない。
fn send_part(
    source: &Path,
    index: usize,
    part: &transfer::Part,
    start: u64,
    send: &(dyn Fn(TransferAction) -> Result<TransferReply, String> + Sync),
) -> Result<(), String> {
    use std::sync::atomic::{AtomicU64, Ordering};

    let chunk = transfer::CHUNK_BYTES as u64;
    let next = AtomicU64::new(start);
    let contiguous = AtomicU64::new(start);
    let failed = std::sync::Mutex::new(None::<String>);
    let stopped = || failed.lock().map_or(true, |failed| failed.is_some());
    std::thread::scope(|scope| {
        for _ in 0..transfer::LANES {
            scope.spawn(|| {
                while !stopped() {
                    let offset = next.fetch_add(chunk, Ordering::SeqCst);
                    if offset >= part.len {
                        break;
                    }
                    while offset >= contiguous.load(Ordering::SeqCst) + 6 * chunk && !stopped() {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    let result = transfer::read_chunk(source, part, offset).and_then(|bytes| {
                        send(TransferAction::Write {
                            part: index,
                            offset,
                            data: crate::base64::encode(&bytes),
                        })
                    });
                    let outcome = match result {
                        Ok(reply) => match reply.offsets.as_slice() {
                            [length] if *length <= part.len => {
                                contiguous.fetch_max(*length, Ordering::SeqCst);
                                Ok(())
                            }
                            _ => Err("分割転送の応答が一致しません".to_string()),
                        },
                        Err(error) => Err(error),
                    };
                    if let Err(error) = outcome {
                        if let Ok(mut failed) = failed.lock() {
                            failed.get_or_insert(error);
                        }
                        break;
                    }
                }
            });
        }
    });
    match failed
        .into_inner()
        .unwrap_or_else(|error| error.into_inner())
    {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// 届いたばかりの大きなファイルは、ウイルス対策やこの端末の Lisa のハッシュ計算が読んでいることが
/// あり、Windows はその間フォルダの改名を拒む。拒否のときだけ少し待って取り直す。
fn publish_rename(stage: &Path, destination: &Path) -> io::Result<()> {
    let started = std::time::Instant::now();
    loop {
        match apply::rename_without_replace(stage, destination) {
            Err(error)
                if error.kind() == io::ErrorKind::PermissionDenied
                    && started.elapsed() < std::time::Duration::from_secs(30) =>
            {
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            result => return result,
        }
    }
}

fn rewrite_link(
    db: &mut Db,
    operation: &str,
    previous: &links::Link,
    target: &Location,
    progress: &mut Progress,
    relay: Option<&str>,
    save: impl Fn(&Db, &Progress) -> Result<(), String>,
) -> Result<Option<PathBuf>, String> {
    let (mut journal, path, old_identity) = owned_marker(db, &previous.marker)?;
    let parent = path
        .parent()
        .ok_or("リンク元の親が見つかりません")?
        .to_path_buf();
    if file_identity(&parent) != Some(journal.parent) {
        return Err(format!("リンク元の親が変わりました: {}", parent.display()));
    }
    let mut updated = previous.marker.clone();
    updated.target = target.clone();
    if journal.marker == updated {
        verify_marker(&path, &updated, old_identity)?;
        return Ok(Some(parent));
    }
    let link_id = &previous.marker.link_id;
    let staged = parent.join(format!(".lisa-restore-{operation}-{link_id}.tmp"));
    let backup = parent.join(format!(".lisa-restore-{operation}-{link_id}.old"));
    let new_identity = if let Some(rewrite) = progress
        .rewrites
        .iter()
        .find(|item| &item.link_id == link_id)
    {
        rewrite.identity
    } else {
        verify_marker(&path, &previous.marker, old_identity)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staged)
            .map_err(error)?;
        file.write_all(&serde_json::to_vec_pretty(&updated).map_err(error)?)
            .map_err(error)?;
        file.sync_all().map_err(error)?;
        drop(file);
        let identity = file_identity(&staged).ok_or("新しいリンクを識別できません")?;
        progress.rewrites.push(Rewrite {
            link_id: link_id.clone(),
            identity,
        });
        save(db, progress)?;
        identity
    };
    if file_identity(&path) == Some(old_identity) {
        verify_marker(&path, &previous.marker, old_identity)?;
        apply::rename_without_replace(&path, &backup).map_err(error)?;
    }
    verify_marker(&backup, &previous.marker, old_identity)?;
    if file_identity(&staged) == Some(new_identity) {
        apply::rename_without_replace(&staged, &path).map_err(error)?;
    }
    verify_marker(&path, &updated, new_identity)?;
    journal.marker = updated.clone();
    journal.state = markers::State::Published {
        path,
        identity: new_identity,
    };
    let tx = db.conn_mut().transaction().map_err(error)?;
    links::retarget(&tx, previous, &updated, relay)?;
    markers::save_journal(&tx, &journal)?;
    tx.commit().map_err(error)?;
    fs::remove_file(backup).map_err(error)?;
    Ok(Some(parent))
}

fn owned_marker(db: &Db, marker: &Marker) -> Result<(markers::Journal, PathBuf, Identity), String> {
    let journal = markers::journal(db, marker.plan_id, marker.item_id)?
        .filter(|journal| journal.marker.link_id == marker.link_id)
        .ok_or_else(|| format!("目印の記録が見つかりません: {}", marker.source.display_path))?;
    let markers::State::Published { path, identity } = &journal.state else {
        return Err(format!(
            "公開済みの目印がありません: {}",
            marker.source.display_path
        ));
    };
    let path = path.clone();
    let identity = *identity;
    Ok((journal, path, identity))
}

fn verify_marker(path: &Path, marker: &Marker, identity: Identity) -> Result<(), String> {
    let expected = serde_json::to_vec_pretty(marker).map_err(error)?;
    let bytes = fs::symlink_metadata(path)
        .ok()
        .and_then(|meta| merged::read_bytes(path, &meta));
    if file_identity(path) != Some(identity) || bytes.as_ref() != Some(&expected) {
        return Err(format!(
            "目印の編集・差し替え・移動を確認しました: {}",
            path.display()
        ));
    }
    Ok(())
}

fn restored_path(db: &Db, location: &Location) -> Result<PathBuf, String> {
    let drive = crate::query::drives(db)
        .map_err(error)?
        .into_iter()
        .find(|drive| drive.id == location.volume_id)
        .ok_or_else(|| {
            format!(
                "戻し先のボリュームが見つかりません: {}",
                location.volume_name
            )
        })?;
    let mut path = drive
        .mount
        .ok_or("戻し先のボリュームが接続されていません")?;
    for part in &location.relative_path {
        if !matches!(Path::new(part).components().next(), Some(std::path::Component::Normal(name)) if name == std::ffi::OsStr::new(part))
        {
            return Err("戻し先のパスが正しくありません".into());
        }
        path.push(part);
    }
    Ok(path)
}

fn verify_content(db: &Db, path: &Path, marker: &Marker) -> Result<(), String> {
    let meta = fs::symlink_metadata(path)
        .map_err(|error| format!("{} を確認できません: {error}", path.display()))?;
    let hash = if marker.kind == crate::listing::Kind::Dir && meta.is_dir() {
        markers::target_folder_hash(db, path)?
    } else if marker.kind == crate::listing::Kind::File && meta.is_file() {
        crate::hash::hash_file(path).map_err(error)?
    } else {
        return Err(format!("実体の種類が一致しません: {}", path.display()));
    };
    if crate::query::parse_hash(&marker.hash.value) != Some(hash) {
        return Err(format!(
            "実体の完全ハッシュが一致しません: {}",
            path.display()
        ));
    }
    Ok(())
}

fn exists(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(error(err)),
    }
}

#[cfg(unix)]
fn available_space(path: &Path) -> Result<u64, String> {
    use std::os::unix::ffi::OsStrExt;

    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(error)?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: パスと出力先は有効。成功時だけ書き込み先を読む。
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(error(io::Error::last_os_error()));
    }
    let stats = unsafe { stats.assume_init() };
    Ok((stats.f_bavail as u64).saturating_mul(stats.f_frsize))
}

#[cfg(windows)]
fn available_space(path: &Path) -> Result<u64, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let path: Vec<_> = path.as_os_str().encode_wide().chain([0]).collect();
    let mut result = 0;
    // SAFETY: パスは NUL 終端で、利用可能バイトの出力先は有効。
    if unsafe {
        GetDiskFreeSpaceExW(
            path.as_ptr(),
            &mut result,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(error(io::Error::last_os_error()));
    }
    Ok(result)
}

#[cfg(not(any(unix, windows)))]
fn available_space(_path: &Path) -> Result<u64, String> {
    Err("戻し先の空き容量を確認できません".into())
}

pub(super) fn update_missing(db: &Db, op: &PlanOp, missing: bool) -> Result<(), String> {
    if let Some((_, request)) = load_request(db, op)?
        && request.choice == DeleteChoice::DeleteActual
    {
        links::flag_actual_missing(db.conn(), &request.links, missing)?;
    }
    Ok(())
}

pub(super) fn save_request(
    conn: &Connection,
    plan_id: i64,
    seq: i64,
    request: &DeleteRequest,
) -> Result<(), String> {
    conn.execute(
        "INSERT INTO merged_link_delete(plan_id,seq,operation_id,request) VALUES (?1,?2,?3,?4)",
        params![
            plan_id,
            seq,
            uuid::Uuid::new_v4().to_string(),
            serde_json::to_string(request).map_err(error)?
        ],
    )
    .map_err(error)?;
    Ok(())
}

pub(super) fn load_request(
    db: &Db,
    op: &super::PlanOp,
) -> Result<Option<(String, DeleteRequest)>, String> {
    let data: Option<(String, String)> = db
        .conn()
        .query_row(
            "SELECT operation_id,request FROM merged_link_delete WHERE plan_id=?1 AND seq=?2",
            params![op.plan_id, op.seq],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(error)?;
    data.map(|(id, json)| Ok((id, serde_json::from_str(&json).map_err(error)?)))
        .transpose()
}

fn error(value: impl std::fmt::Display) -> String {
    value.to_string()
}

#[derive(Deserialize, Serialize)]
pub struct TransferRequest {
    pub operation_id: String,
    pub marker: Marker,
    pub generation: i64,
    pub expires_at: i64,
    pub action: TransferAction,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case", tag = "action")]
pub enum TransferAction {
    Inspect,
    Begin {
        manifest: TransferManifest,
    },
    Write {
        part: usize,
        offset: u64,
        data: String,
    },
    Publish,
    Retire,
    Repoint {
        chosen: Marker,
    },
    AddSource {
        source: Marker,
        generation: i64,
    },
}

#[derive(Default, Deserialize, Serialize)]
pub struct TransferReply {
    pub offsets: Vec<u64>,
    pub received: u64,
    pub published: bool,
}

#[derive(Deserialize, Serialize)]
struct ReceiveState {
    marker: Marker,
    relay: String,
    manifest: TransferManifest,
    directory: PathBuf,
    directory_identity: Identity,
    destination: PathBuf,
    parent_identity: Identity,
    staged_identity: Option<Identity>,
    #[serde(default)]
    retired: bool,
    #[serde(default)]
    offsets: Vec<u64>,
}

const MAX_RECEIVES: i64 = 8;

pub fn receive_transfer(
    db: &mut Db,
    relay: &str,
    request: &TransferRequest,
) -> Result<TransferReply, String> {
    // 同じ操作に並行して届く小分けが、記録した進み具合を互いに上書きしないよう 1 件ずつ受ける。
    // 別の操作（別の戻し）は待たせない。
    type Locks = std::collections::HashMap<String, std::sync::Arc<std::sync::Mutex<()>>>;
    static RECEIVING: std::sync::Mutex<Option<Locks>> = std::sync::Mutex::new(None);
    let lock = {
        let mut locks = RECEIVING.lock().unwrap_or_else(|error| error.into_inner());
        let locks = locks.get_or_insert_with(Default::default);
        locks.retain(|_, lock| std::sync::Arc::strong_count(lock) > 1);
        locks
            .entry(request.operation_id.clone())
            .or_default()
            .clone()
    };
    let _receiving = lock.lock().unwrap_or_else(|error| error.into_inner());
    receive_transfer_locked(db, relay, request)
}

fn receive_transfer_locked(
    db: &mut Db,
    relay: &str,
    request: &TransferRequest,
) -> Result<TransferReply, String> {
    if uuid::Uuid::parse_str(&request.operation_id)
        .ok()
        .is_none_or(|id| id.is_nil() || id.to_string() != request.operation_id)
        || !(0..=120_000).contains(&request.expires_at.saturating_sub(crate::db::now_ms()))
    {
        return Err("転送許可の操作 ID または期限が正しくありません".into());
    }
    let marker = &request.marker;
    if let TransferAction::Repoint { chosen } = &request.action {
        return receive_repoint(db, relay, request, chosen);
    }
    // 片付けられなかった一時フォルダは次回に回す（転送は止めない）。
    clean_expired_receives(db)?;
    let stored: Option<String> = db
        .conn()
        .query_row(
            "SELECT receipt FROM merged_link_receive WHERE operation_id=?1 AND link_id=?2",
            params![request.operation_id, marker.link_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(error)?;
    let existing: Option<ReceiveState> = stored
        .map(|json| serde_json::from_str(&json).map_err(error))
        .transpose()?;
    if let Some(state) = &existing
        && state.retired
        && matches!(request.action, TransferAction::Retire)
        && state.marker == *marker
        && state.relay == relay
    {
        return Ok(TransferReply {
            published: true,
            received: marker.size,
            offsets: Vec::new(),
        });
    }
    let (current, generation, allowed_relay) =
        links::stored_source(db, &marker.link_id)?.ok_or("この端末に確定したリンクがありません")?;
    if &current != marker
        || generation != request.generation
        || relay != allowed_relay
        || db.meta("this_device").map_err(error)?.as_deref() != Some(&marker.source.device_id)
    {
        return Err("転送許可とリンク元が一致しません".into());
    }
    let (journal, marker_path, identity) = owned_marker(db, marker)?;
    if !matches!(request.action, TransferAction::Retire) {
        verify_marker(&marker_path, marker, identity)?;
    }
    let destination = restored_path(db, &marker.source)?;
    let parent = destination.parent().ok_or("戻し先の親がありません")?;
    if file_identity(parent) != Some(journal.parent) || marker_path.parent() != Some(parent) {
        return Err(format!("戻し先の親が変わりました: {}", parent.display()));
    }
    if matches!(request.action, TransferAction::Inspect) {
        return Ok(TransferReply::default());
    }
    let directory = parent.join(format!(".lisa-link-transfer-{}", request.operation_id));
    let mut state = match existing {
        Some(state) => state,
        None => {
            let TransferAction::Begin { manifest } = &request.action else {
                return Err("転送の準備がありません".into());
            };
            manifest.validate(marker)?;
            if exists(&destination)? {
                return Err(format!(
                    "戻し先にすでに項目があります: {}",
                    destination.display()
                ));
            }
            if available_space(parent)? < marker.size {
                return Err(format!(
                    "戻し先の空き容量が足りません: {}",
                    parent.display()
                ));
            }
            if active_receives(db)? >= MAX_RECEIVES {
                return Err("戻し先の転送が混み合っています".into());
            }
            fs::create_dir(&directory).map_err(error)?;
            let directory_identity = file_identity(&directory).ok_or("一時領域を識別できません")?;
            let state = ReceiveState {
                marker: marker.clone(),
                relay: relay.to_owned(),
                manifest: manifest.clone(),
                directory,
                directory_identity,
                destination: destination.clone(),
                parent_identity: journal.parent,
                staged_identity: None,
                retired: false,
                offsets: Vec::new(),
            };
            save_receive(db, request, &state)?;
            state
        }
    };
    if state.marker != *marker
        || state.relay != relay
        || state.parent_identity != journal.parent
        || state.destination.parent() != Some(parent)
        || file_identity(&state.directory) != Some(state.directory_identity)
    {
        return Err("中断した転送と戻し先が一致しません".into());
    }
    let stage = state.directory.join("entry");
    let mut reply = TransferReply::default();
    match &request.action {
        TransferAction::Inspect | TransferAction::Repoint { .. } => unreachable!(),
        TransferAction::Begin { manifest } => {
            if &state.manifest != manifest {
                if state.staged_identity.is_some() {
                    return Err("中断前と転送内容が一致しません".into());
                }
                // 中断のあいだに送り元の中身や更新日時が変わった。公開前の一時領域を捨てて始め直す。
                transfer::forget_ahead(&request.operation_id);
                fs::remove_dir_all(&state.directory).map_err(error)?;
                db.conn()
                    .execute(
                        "DELETE FROM merged_link_receive WHERE operation_id=?1 AND link_id=?2",
                        params![request.operation_id, marker.link_id],
                    )
                    .map_err(error)?;
                return receive_transfer_locked(db, relay, request);
            }
            if state.staged_identity.is_some()
                && file_identity(&state.destination) == state.staged_identity
            {
                verify_content(db, &state.destination, marker)?;
                reply.published = true;
                reply.received = marker.size;
                return Ok(reply);
            }
            if !exists(&stage)?
                && let Some(saved) = super::plan::plan_ops(db, marker.plan_id)
                    .map_err(error)?
                    .into_iter()
                    .find(|op| op.seq == marker.item_id && op.state == super::OpState::Done)
                    .and_then(|op| op.dst)
                && verify_content(db, &saved, marker).is_ok()
            {
                apply::copy_verify_and_publish(marker.plan_id, marker.item_id, &saved, &stage)?;
            }
            reply.offsets = transfer::begin(&stage, manifest, marker)?;
            state.offsets = reply.offsets.clone();
            reply.received = state.offsets.iter().sum();
        }
        TransferAction::Write { part, offset, data } => {
            if state.staged_identity.is_some() {
                return Err("転送の確定後には書き込めません".into());
            }
            let index = *part;
            let part = state
                .manifest
                .entries
                .get(*part)
                .ok_or("転送するファイルが正しくありません")?;
            if data.len() > crate::base64::encoded_len(transfer::CHUNK_BYTES) {
                return Err("小分け転送のバイト数が正しくありません".into());
            }
            let bytes = crate::base64::decode(data).map_err(|_| "転送データが正しくありません")?;
            let length = transfer::accept_chunk(
                &format!("{}/{}/{index}", request.operation_id, marker.link_id),
                &stage,
                part,
                *offset,
                bytes,
            )?;
            let saved = state
                .offsets
                .get_mut(index)
                .ok_or("分割転送の準備がありません")?;
            *saved = length;
            reply.offsets = vec![length];
            reply.received = state.offsets.iter().sum();
        }
        TransferAction::Publish => {
            transfer::forget_ahead(&request.operation_id);
            if state.staged_identity.is_none() {
                transfer::verify_stage(db, &stage, &state.manifest, marker)?;
                state.staged_identity = file_identity(&stage);
                if state.staged_identity.is_none() {
                    return Err("転送した実体を識別できません".into());
                }
                save_receive(db, request, &state)?;
            }
            if file_identity(&stage) == state.staged_identity {
                publish_rename(&stage, &state.destination).map_err(|error| {
                    format!("{} に公開できません: {error}", state.destination.display())
                })?;
            }
            if file_identity(&state.destination) != state.staged_identity {
                return Err(format!(
                    "戻した実体を識別できません: {}",
                    state.destination.display()
                ));
            }
            verify_content(db, &state.destination, marker)?;
            reply.published = true;
            reply.received = marker.size;
        }
        TransferAction::AddSource { source, generation } => {
            if state.staged_identity.is_none()
                || file_identity(&state.destination) != state.staged_identity
                || source.target != marker.source
                || source.hash != marker.hash
            {
                return Err("リンクの新しい実体を確認できません".into());
            }
            verify_content(db, &state.destination, marker)?;
            if links::state_at(db, &source.link_id)?.is_none() {
                links::apply(
                    db,
                    &links::Update {
                        marker: source.clone(),
                        generation: 1,
                        state: links::State::Reserved,
                    },
                    Some(relay),
                    false,
                )?;
            }
            links::apply(
                db,
                &links::Update {
                    marker: source.clone(),
                    generation: *generation,
                    state: links::State::Active,
                },
                Some(relay),
                false,
            )?;
            reply.published = true;
            reply.received = marker.size;
        }
        TransferAction::Retire => {
            if state.staged_identity.is_none()
                || file_identity(&state.destination) != state.staged_identity
            {
                return Err("戻す実体が確定していません".into());
            }
            verify_content(db, &state.destination, marker)?;
            let retired = state.directory.join("marker-retired");
            if exists(&marker_path)? {
                verify_marker(&marker_path, marker, identity)?;
                apply::rename_without_replace(&marker_path, &retired).map_err(error)?;
            }
            verify_marker(&retired, marker, identity)?;
            state.retired = true;
            let tx = db.conn_mut().transaction().map_err(error)?;
            links::remove_received(&tx, marker, generation, relay)?;
            save_receive_on(&tx, request, &state)?;
            tx.commit().map_err(error)?;
            let _ = fs::remove_file(&retired);
            let _ = fs::remove_dir(&state.directory);

            reply.published = true;
            reply.received = marker.size;
        }
    }
    save_receive(db, request, &state)?;
    links::save_transfer_progress(db, &request.operation_id, reply.received, marker.size)?;
    Ok(reply)
}

fn receive_repoint(
    db: &mut Db,
    relay: &str,
    request: &TransferRequest,
    chosen: &Marker,
) -> Result<TransferReply, String> {
    let old = &request.marker;
    if old.hash != chosen.hash
        || old.target != chosen.target
        || old.link_id == chosen.link_id
        || db.meta("this_device").map_err(error)?.as_deref() != Some(&old.source.device_id)
    {
        return Err("付け替えるリンクの実体が一致しません".into());
    }
    let (current, generation, allowed) =
        links::stored_source(db, &old.link_id)?.ok_or("付け替えるリンクが確定していません")?;
    let mut updated = old.clone();
    updated.target = chosen.source.clone();
    if relay != allowed
        || !(current == *old && generation == request.generation
            || current == updated && generation == request.generation + 1)
    {
        return Err("付け替えの世代または中継端末が一致しません".into());
    }
    let record: Option<String> = db
        .conn()
        .query_row(
            "SELECT progress FROM merged_link_rewrite WHERE operation_id=?1 AND link_id=?2",
            params![request.operation_id, old.link_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(error)?;
    let mut progress = record
        .map(|json| serde_json::from_str::<Progress>(&json).map_err(error))
        .transpose()?
        .unwrap_or(Progress {
            links: Vec::new(),
            destination: None,
            rewrites: Vec::new(),
            chosen_removed: false,
            peer_published: false,
        });
    let previous = links::Link {
        marker: old.clone(),
        generation: request.generation,
        state: links::State::Active,
        checked_at: crate::db::now_ms(),
        sync_state: "confirmed".into(),
        sync_reason: None,
    };
    rewrite_link(
        db,
        &request.operation_id,
        &previous,
        &chosen.source,
        &mut progress,
        Some(relay),
        |db, progress| {
            db.conn()
                .execute(
                    "INSERT INTO merged_link_rewrite VALUES (?1,?2,?3)
             ON CONFLICT(operation_id,link_id) DO UPDATE SET progress=excluded.progress",
                    params![
                        request.operation_id,
                        old.link_id,
                        serde_json::to_string(progress).map_err(error)?
                    ],
                )
                .map_err(error)?;
            Ok(())
        },
    )?;
    // 再送は目印の中身が一致することで済むので、進み具合の記録は残さない。
    db.conn()
        .execute(
            "DELETE FROM merged_link_rewrite WHERE operation_id=?1 AND link_id=?2",
            params![request.operation_id, old.link_id],
        )
        .map_err(error)?;
    Ok(TransferReply {
        published: true,
        received: 0,
        offsets: Vec::new(),
    })
}

fn save_receive(db: &Db, request: &TransferRequest, state: &ReceiveState) -> Result<(), String> {
    save_receive_on(db.conn(), request, state)
}

fn save_receive_on(
    conn: &Connection,
    request: &TransferRequest,
    state: &ReceiveState,
) -> Result<(), String> {
    conn.execute(
        "INSERT INTO merged_link_receive(operation_id,link_id,receipt,touched_at)
         VALUES (?1,?2,?3,?4)
         ON CONFLICT(operation_id,link_id) DO UPDATE SET receipt=excluded.receipt,touched_at=excluded.touched_at",
        params![request.operation_id, request.marker.link_id, serde_json::to_string(state).map_err(error)?, crate::db::now_ms()],
    ).map_err(error)?;
    Ok(())
}

/// 戻し終えた受信の記録を、再送に答えるために残しておく長さ。
const RETIRED_KEEP_MS: i64 = 60 * 60 * 1000;

/// 止まったまま期限を過ぎた受信と、戻し終えてしばらく経った受信を片付ける。
/// 一時フォルダを消せなかったもの（ほかのプログラムが掴んでいる等）は記録を残して次回に回し、
/// ほかの受信の片付けや新しい転送は止めない。消せなかった一時フォルダの理由を返す。
pub fn clean_expired_receives(db: &Db) -> Result<Vec<String>, String> {
    let now = crate::db::now_ms();
    let data: Vec<(String, String, String, i64)> = db
        .conn()
        .prepare_cached("SELECT operation_id,link_id,receipt,touched_at FROM merged_link_receive")
        .map_err(error)?
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .map_err(error)?
        .collect::<rusqlite::Result<_>>()
        .map_err(error)?;
    let mut failures = Vec::new();
    for (operation, id, json, touched_at) in data {
        let state = serde_json::from_str::<ReceiveState>(&json).ok();
        let expired = touched_at < now.saturating_sub(transfer::MAX_IDLE_MS);
        let finished = state.as_ref().is_some_and(|state| state.retired)
            && touched_at < now.saturating_sub(RETIRED_KEEP_MS);
        if state.is_some() && !expired && !finished {
            continue;
        }
        transfer::forget_ahead(&operation);
        if let Some(state) = &state
            && file_identity(&state.directory) == Some(state.directory_identity)
            && state.directory.parent().and_then(file_identity) == Some(state.parent_identity)
            && let Err(reason) = fs::remove_dir_all(&state.directory)
        {
            failures.push(format!("{}: {reason}", state.directory.display()));
            continue;
        }
        db.conn()
            .execute(
                "DELETE FROM merged_link_receive WHERE operation_id=?1 AND link_id=?2",
                params![operation, id],
            )
            .map_err(error)?;
    }
    Ok(failures)
}

/// 上限に数える受信（戻し終えていない・期限内のもの）。
fn active_receives(db: &Db) -> Result<i64, String> {
    let since = crate::db::now_ms().saturating_sub(transfer::MAX_IDLE_MS);
    let receipts: Vec<String> = db
        .conn()
        .prepare_cached("SELECT receipt FROM merged_link_receive WHERE touched_at>=?1")
        .map_err(error)?
        .query_map([since], |row| row.get(0))
        .map_err(error)?
        .collect::<rusqlite::Result<_>>()
        .map_err(error)?;
    Ok(receipts
        .iter()
        .filter(|json| serde_json::from_str::<ReceiveState>(json).is_ok_and(|state| !state.retired))
        .count() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn place(path: &str) -> Location {
        Location {
            device_id: "新PC".into(),
            device_name: "新PC".into(),
            volume_id: "test:vol".into(),
            volume_name: "T".into(),
            relative_path: vec![path.into()],
            display_path: path.into(),
        }
    }

    /// 受信の記録を、指定した更新時刻・状態で置く。
    fn receipt(db: &Db, root: &Path, operation: &str, touched_at: i64, retired: bool) -> PathBuf {
        let directory = root.join(format!(".lisa-link-transfer-{operation}"));
        fs::create_dir_all(directory.join("entry")).unwrap();
        let marker = Marker::new(
            place("元.txt"),
            place("残す.txt"),
            crate::dups::ContentKey {
                directory: false,
                size: 4,
                hash: [7; 32],
            },
            1,
            0,
        );
        let state = ReceiveState {
            marker,
            relay: "中継".into(),
            manifest: TransferManifest {
                entries: Vec::new(),
            },
            directory_identity: file_identity(&directory).unwrap(),
            directory: directory.clone(),
            destination: root.join("元.txt"),
            parent_identity: file_identity(root).unwrap(),
            staged_identity: None,
            retired,
            offsets: Vec::new(),
        };
        db.conn()
            .execute(
                "INSERT INTO merged_link_receive VALUES (?1,?2,?3,?4)",
                params![
                    operation,
                    "link",
                    serde_json::to_string(&state).unwrap(),
                    touched_at
                ],
            )
            .unwrap();
        directory
    }

    fn rows(db: &Db) -> i64 {
        db.conn()
            .query_row("SELECT COUNT(*) FROM merged_link_receive", [], |row| {
                row.get(0)
            })
            .unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn a_stuck_temporary_folder_does_not_stop_other_receives() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!("lisa-receive-clean-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let db = Db::open(&root.join("lisa.db")).unwrap();
        let old = crate::db::now_ms() - transfer::MAX_IDLE_MS - 1000;
        // 消せない一時フォルダ（ほかのプログラムが掴んでいる状態の代わりに、書込禁止の子を置く）。
        let stuck = receipt(&db, &root, "stuck", old, false);
        fs::write(stuck.join("entry/a.bin"), b"held").unwrap();
        fs::set_permissions(stuck.join("entry"), fs::Permissions::from_mode(0o555)).unwrap();
        let expired = receipt(&db, &root, "expired", old, false);

        let failures = clean_expired_receives(&db).unwrap();
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(!expired.exists(), "ほかの期限切れは片付く");
        assert_eq!(rows(&db), 1, "消せなかった分だけ記録を残して次回に回す");
        assert_eq!(active_receives(&db).unwrap(), 0, "期限切れは上限に数えない");

        fs::set_permissions(stuck.join("entry"), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(clean_expired_receives(&db).unwrap().is_empty());
        assert_eq!(rows(&db), 0);
        drop(db);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn finished_receives_do_not_count_and_are_cleared_after_a_while() {
        let root = std::env::temp_dir().join(format!("lisa-receive-limit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let db = Db::open(&root.join("lisa.db")).unwrap();
        let now = crate::db::now_ms();
        for n in 0..MAX_RECEIVES {
            receipt(&db, &root, &format!("done-{n}"), now, true);
        }
        receipt(&db, &root, "working", now, false);
        assert_eq!(active_receives(&db).unwrap(), 1, "戻し終えたものは数えない");

        let old_done = receipt(&db, &root, "old-done", now - RETIRED_KEEP_MS - 1000, true);
        assert!(clean_expired_receives(&db).unwrap().is_empty());
        assert!(!old_done.exists());
        assert_eq!(
            rows(&db),
            MAX_RECEIVES + 1,
            "最近戻し終えたものは再送に備えて残す"
        );
        drop(db);
        fs::remove_dir_all(&root).unwrap();
    }
}
