//! 隔離フォルダ（消す前の退避先）と、期限切れを OS のゴミ箱へ送る処理。
//!
//! - 置き場は、消すものと同じボリュームの中に取る（`rename` で移せるように）。
//!   1. データのフォルダの `trash/` が同じボリュームなら、そこ。
//!   2. 違えば、ドライブの一番上の `.lisa-trash/`。
//!   3. どちらも書けなければ `None`（その 1 件は `skipped`）。
//! - 中は `<置き場>/<plan_id>/<ドライブの一番上から見たパス>` で、`<置き場>/<plan_id>/manifest.json` に
//!   元の場所を残す（DB が無くても手で戻せるように）。
//! - 期限を過ぎた計画の隔離フォルダは `trash` クレートで OS のゴミ箱へ送る。

use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::db::{self, Db};
use crate::name;
use crate::query;

use super::plan::{self, OpKind, PlanState};

/// 隔離フォルダの中の `manifest.json` のファイル名。
pub const MANIFEST_FILE: &str = "manifest.json";

/// `manifest.json` の中身。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub plan_id: i64,
    pub updated_at: i64,
    pub entries: Vec<ManifestEntry>,
}

/// `manifest.json` の 1 件。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManifestEntry {
    pub seq: i64,
    pub op: OpKind,
    pub src: PathBuf,
    pub rel_path: String,
    pub quarantined_path: PathBuf,
    pub quarantined_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_marker_path: Option<PathBuf>,
}

/// OS のゴミ箱へ送った結果。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PurgeReport {
    pub purged_plans: Vec<i64>,
    pub trashed_dirs: Vec<PathBuf>,
    pub errors: Vec<(i64, String)>,
}

/// `a` と `b` が同じボリュームにあるか（存在しなければ最も近い存在する親で比べる）。
pub fn same_volume(a: &Path, b: &Path) -> bool {
    let Some(ea) = nearest_existing(a) else {
        return false;
    };
    let Some(eb) = nearest_existing(b) else {
        return false;
    };
    same_volume_existing(&ea, &eb)
}

fn nearest_existing(path: &Path) -> Option<PathBuf> {
    let mut cur = Some(path);
    while let Some(p) = cur {
        if fs::symlink_metadata(p).is_ok() {
            return Some(p.to_path_buf());
        }
        cur = p.parent();
    }
    None
}

#[cfg(unix)]
fn same_volume_existing(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(ma) = fs::symlink_metadata(a) else {
        return false;
    };
    let Ok(mb) = fs::symlink_metadata(b) else {
        return false;
    };
    ma.dev() == mb.dev()
}

#[cfg(windows)]
fn same_volume_existing(a: &Path, b: &Path) -> bool {
    // プレフィックス（ドライブレター・UNC）を大文字小文字無視で比べる。
    let prefix_of = |p: &Path| {
        p.components().next().and_then(|c| match c {
            Component::Prefix(pre) => Some(pre.as_os_str().to_string_lossy().to_ascii_lowercase()),
            _ => None,
        })
    };
    match (prefix_of(a), prefix_of(b)) {
        (Some(pa), Some(pb)) => pa == pb,
        _ => false,
    }
}

#[cfg(not(any(unix, windows)))]
fn same_volume_existing(_a: &Path, _b: &Path) -> bool {
    false
}

/// 隔離フォルダの置き場（`<data_dir>/trash` または `<mount>/.lisa-trash`）を選ぶ。
/// 同じボリュームに書ける置き場が無ければ `None`。
pub fn choose_quarantine_base(
    data_dir: &Path,
    drive_mount: Option<&Path>,
    src: &Path,
) -> Option<PathBuf> {
    let data_trash = data_dir.join("trash");
    if same_volume(src, data_dir) && ensure_writable_dir(&data_trash) {
        return Some(data_trash);
    }
    if let Some(mount) = drive_mount {
        let drive_trash = mount.join(name::QUARANTINE_DIR_NAME);
        if same_volume(src, mount) && ensure_writable_dir(&drive_trash) {
            return Some(drive_trash);
        }
    }
    None
}

fn ensure_writable_dir(dir: &Path) -> bool {
    if fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join(format!(".lisa-probe-{}", std::process::id()));
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// `src` のドライブの一番上から見た相対パス。取れなければファイル名。
pub(crate) fn relative_for_quarantine(src: &Path, drive_mount: Option<&Path>) -> PathBuf {
    if let Some(mount) = drive_mount
        && let Ok(rel) = src.strip_prefix(mount)
        && !rel.as_os_str().is_empty()
    {
        return rel.to_path_buf();
    }
    src.file_name()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("item"))
}

/// `<base>/<plan_id>/<rel>` の形を作る。
pub(crate) fn quarantine_dest(base: &Path, plan_id: i64, rel: &Path) -> PathBuf {
    let mut out = base.join(plan_id.to_string());
    for comp in rel.components() {
        if let Component::Normal(s) = comp {
            out.push(s);
        }
    }
    out
}

/// `src` を同じボリュームの隔離フォルダへ `rename` で移し、`manifest.json` に記録する。
pub(crate) fn move_to_quarantine(
    plan_id: i64,
    seq: i64,
    op_kind: OpKind,
    src: &Path,
    drive_mount: Option<&Path>,
    data_dir: &Path,
    rename: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
) -> Result<PathBuf, String> {
    let base = choose_quarantine_base(data_dir, drive_mount, src)
        .ok_or_else(|| "隔離できない".to_string())?;
    let rel = relative_for_quarantine(src, drive_mount);
    let plan_dir = base.join(plan_id.to_string());
    let dest = quarantine_dest(&base, plan_id, &rel);
    if fs::symlink_metadata(&dest).is_ok() {
        return Err(format!(
            "隔離先にすでに同じ名前のものがあります: {}",
            dest.display()
        ));
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("隔離先を作れません: {e}"))?;
    }
    rename(src, &dest).map_err(|e| format!("隔離フォルダへ移せませんでした: {e}"))?;

    // manifest.json を更新する（失敗してもファイル自体は移り終えているので、ログ扱いで進めずエラーにしない手もあるが、
    // 通常は同じボリュームの自分のフォルダなので書ける）。
    let rel_str = rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/");
    let _ = append_manifest(
        &plan_dir,
        ManifestEntry {
            seq,
            op: op_kind,
            src: src.to_path_buf(),
            rel_path: rel_str,
            quarantined_path: dest.clone(),
            quarantined_at: db::now_ms(),
            merge_marker_path: None,
        },
        plan_id,
    );

    Ok(dest)
}

/// 落ちたあとの再開用: すでに隔離フォルダへ移り終えているか探す。
pub(crate) fn find_existing_quarantine_dest(
    plan_id: i64,
    src: &Path,
    drive_mount: Option<&Path>,
    data_dir: &Path,
) -> Option<PathBuf> {
    let rel = relative_for_quarantine(src, drive_mount);
    let mut candidates = vec![data_dir.join("trash")];
    if let Some(mount) = drive_mount {
        candidates.push(mount.join(name::QUARANTINE_DIR_NAME));
    }
    for base in candidates {
        let dest = quarantine_dest(&base, plan_id, &rel);
        if fs::symlink_metadata(&dest).is_ok() {
            return Some(dest);
        }
    }
    None
}

fn append_manifest(plan_dir: &Path, entry: ManifestEntry, plan_id: i64) -> Result<(), String> {
    fs::create_dir_all(plan_dir).map_err(|e| e.to_string())?;
    let path = plan_dir.join(MANIFEST_FILE);
    let mut manifest = fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Manifest>(&b).ok())
        .unwrap_or(Manifest {
            plan_id,
            updated_at: 0,
            entries: Vec::new(),
        });
    manifest.plan_id = plan_id;
    manifest.updated_at = entry.quarantined_at;
    if !manifest.entries.iter().any(|e| e.seq == entry.seq) {
        manifest.entries.push(entry);
    }
    save_manifest(plan_dir, &manifest)
}

pub(super) fn record_merge_marker(
    quarantined: &Path,
    plan_id: i64,
    seq: i64,
    marker_path: &Path,
) -> Result<(), String> {
    let plan_dir = find_plan_dir_ancestor(quarantined, &plan_id.to_string())
        .ok_or("隔離計画のフォルダが見つかりません")?;
    let bytes = fs::read(plan_dir.join(MANIFEST_FILE)).map_err(|error| error.to_string())?;
    let mut manifest: Manifest =
        serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    let entry = manifest
        .entries
        .iter_mut()
        .find(|entry| entry.seq == seq && entry.quarantined_path == quarantined)
        .ok_or("隔離マニフェストの項目が見つかりません")?;
    entry.merge_marker_path = Some(marker_path.to_path_buf());
    save_manifest(&plan_dir, &manifest)
}

fn save_manifest(plan_dir: &Path, manifest: &Manifest) -> Result<(), String> {
    let path = plan_dir.join(MANIFEST_FILE);
    let tmp = plan_dir.join(format!(".{MANIFEST_FILE}.tmp.{}", std::process::id()));
    let bytes = serde_json::to_vec_pretty(manifest).map_err(|e| e.to_string())?;
    fs::write(&tmp, &bytes).map_err(|e| e.to_string())?;
    fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    Ok(())
}

/// 計画 `plan_id` に属する隔離フォルダ（`<base>/<plan_id>`）の候補をすべて集める。
pub(crate) fn plan_quarantine_dirs(
    db: &Db,
    data_dir: &Path,
    plan_id: i64,
) -> Result<Vec<PathBuf>, String> {
    let id_str = plan_id.to_string();
    let mut dirs = BTreeSet::new();

    let data_plan = data_dir.join("trash").join(&id_str);
    if data_plan.exists() {
        dirs.insert(data_plan);
    }

    for drive in query::drives(db).map_err(|e| e.to_string())? {
        if let Some(mount) = drive.mount {
            let p = mount.join(name::QUARANTINE_DIR_NAME).join(&id_str);
            if p.exists() {
                dirs.insert(p);
            }
        }
    }

    // plan_op.dst に記録された隔離先の親からも `<base>/<plan_id>` をたどる。
    if let Ok(ops) = plan::plan_ops(db, plan_id) {
        for op in ops {
            if op.op == OpKind::Trash
                && let Some(dst) = op.dst
                && let Some(found) = find_plan_dir_ancestor(&dst, &id_str)
                && found.exists()
            {
                dirs.insert(found);
            }
        }
    }

    Ok(dirs.into_iter().collect())
}

/// 処分と同じ候補を確認し、アクセスできない隔離先も履歴に残す。
pub(crate) fn history_quarantine_clear<'a>(
    db: &Db,
    data_dir: &Path,
    plan_id: i64,
    ops: impl Iterator<Item = &'a plan::PlanOp>,
) -> bool {
    fn absent(path: &Path) -> bool {
        matches!(fs::symlink_metadata(path), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
    }

    let id = plan_id.to_string();
    if !history_plan_dir_clear(&data_dir.join("trash").join(&id), plan_id) {
        return false;
    }
    let Ok(drives) = query::drives(db) else {
        return false;
    };
    if drives.iter().filter_map(|d| d.mount.as_ref()).any(|mount| {
        !history_plan_dir_clear(&mount.join(name::QUARANTINE_DIR_NAME).join(&id), plan_id)
    }) {
        return false;
    }
    for op in ops.filter(|op| matches!(op.op, OpKind::Move | OpKind::Trash)) {
        // 別ボリュームへの移動でも元ファイルを隔離することがある。
        if let Some(drive_id) = &op.drive_id
            && !drives.iter().any(|drive| {
                &drive.id == drive_id && drive.mount.as_ref().is_some_and(|mount| mount.is_dir())
            })
        {
            return false;
        }
        if op.op == OpKind::Trash {
            match &op.dst {
                Some(dst)
                    if dst.is_absolute()
                        && absent(dst)
                        && find_plan_dir_ancestor(dst, &id)
                            .is_some_and(|dir| history_plan_dir_clear(&dir, plan_id)) => {}
                None if matches!(op.state, plan::OpState::Skipped | plan::OpState::Failed) => {}
                _ => return false,
            }
        }
    }
    true
}

/// 取り消しは隔離した項目だけを戻す。空の親フォルダとマニフェストは残る。
fn history_plan_dir_clear(dir: &Path, plan_id: i64) -> bool {
    match fs::symlink_metadata(dir) {
        Err(error) => return error.kind() == std::io::ErrorKind::NotFound,
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return false,
    }

    let manifest_path = dir.join(MANIFEST_FILE);
    let mut parent_dirs = BTreeSet::new();
    match fs::symlink_metadata(&manifest_path) {
        Ok(meta) if meta.is_file() => {
            let Some(manifest) = fs::read(&manifest_path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Manifest>(&bytes).ok())
                .filter(|manifest| manifest.plan_id == plan_id)
            else {
                return false;
            };
            for entry in manifest.entries {
                let relative = Path::new(&entry.rel_path);
                if relative.as_os_str().is_empty()
                    || relative
                        .components()
                        .any(|part| !matches!(part, Component::Normal(_)))
                {
                    return false;
                }
                let payload = dir.join(relative);
                if !matches!(fs::symlink_metadata(&payload), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
                {
                    return false;
                }
                for parent in payload
                    .ancestors()
                    .skip(1)
                    .take_while(|parent| *parent != dir)
                {
                    parent_dirs.insert(parent.to_path_buf());
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        _ => return false,
    }

    let mut pending = vec![dir.to_path_buf()];
    while let Some(parent) = pending.pop() {
        let Ok(entries) = fs::read_dir(parent) else {
            return false;
        };
        for entry in entries {
            let Ok(entry) = entry else {
                return false;
            };
            let path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_file() && path == manifest_path => {}
                Ok(kind) if kind.is_dir() && parent_dirs.contains(&path) => pending.push(path),
                _ => return false,
            }
        }
    }
    true
}

fn find_plan_dir_ancestor(path: &Path, plan_id_str: &str) -> Option<PathBuf> {
    let mut cur = path.parent();
    while let Some(p) = cur {
        if p.file_name().and_then(|s| s.to_str()) == Some(plan_id_str)
            && p.parent()
                .and_then(|pp| pp.file_name())
                .and_then(|s| s.to_str())
                .is_some_and(|n| n == "trash" || n == name::QUARANTINE_DIR_NAME)
        {
            return Some(p.to_path_buf());
        }
        cur = p.parent();
    }
    None
}

/// 期限（`days` 日）を過ぎた計画の隔離フォルダを OS のゴミ箱へ送る（`trash` クレート）。
pub fn purge_expired(db: &mut Db, data_dir: &Path, days: u64) -> Result<PurgeReport, String> {
    purge_expired_at_with(db, data_dir, days, db::now_ms(), |p| {
        trash::delete(p).map_err(|e| e.to_string())
    })
}

/// 指定した 1 つの計画の隔離フォルダを今すぐ OS のゴミ箱へ送る。
pub fn purge_plan(db: &mut Db, data_dir: &Path, plan_id: i64) -> Result<PurgeReport, String> {
    purge_plan_with(db, data_dir, plan_id, db::now_ms(), |p| {
        trash::delete(p).map_err(|e| e.to_string())
    })
}

/// テスト用にも使える、日時とゴミ箱送り関数を差し替えられる版。
pub(crate) fn purge_expired_at_with(
    db: &mut Db,
    data_dir: &Path,
    days: u64,
    now_ms: i64,
    mut delete_fn: impl FnMut(&Path) -> Result<(), String>,
) -> Result<PurgeReport, String> {
    let cutoff = now_ms.saturating_sub((days as i64).saturating_mul(86_400_000));
    let ids: Vec<i64> = {
        let mut stmt = db
            .conn()
            .prepare(
                "SELECT id FROM plan
                 WHERE purged_at IS NULL
                   AND finished_at IS NOT NULL
                   AND finished_at <= ?1
                   AND state IN ('done', 'partial', 'undone')
                 ORDER BY id",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([cutoff], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<_, _>>().map_err(|e| e.to_string())?
    };

    let mut report = PurgeReport::default();
    for id in ids {
        let sub = purge_plan_with(db, data_dir, id, now_ms, &mut delete_fn)?;
        report.purged_plans.extend(sub.purged_plans);
        report.trashed_dirs.extend(sub.trashed_dirs);
        report.errors.extend(sub.errors);
    }
    Ok(report)
}

pub(crate) fn purge_plan_with(
    db: &mut Db,
    data_dir: &Path,
    plan_id: i64,
    now_ms: i64,
    mut delete_fn: impl FnMut(&Path) -> Result<(), String>,
) -> Result<PurgeReport, String> {
    let _lock = super::apply::lock_plans(data_dir, plan_id)?;
    let plan = plan::load_plan_required(db, plan_id)?;
    if !matches!(
        plan.state,
        PlanState::Done | PlanState::Partial | PlanState::Undone
    ) {
        return Err(format!(
            "計画 #{plan_id} は実行が済んでいません（今の状態: {}）",
            plan.state.as_str()
        ));
    }
    let mut report = PurgeReport::default();
    if plan.purged_at.is_some() {
        return Ok(report);
    }

    let dirs = plan_quarantine_dirs(db, data_dir, plan_id)?;
    let mut failed = false;
    for dir in dirs {
        match delete_fn(&dir) {
            Ok(()) => report.trashed_dirs.push(dir),
            Err(e) => {
                failed = true;
                report
                    .errors
                    .push((plan_id, format!("{}: {e}", dir.display())));
            }
        }
    }
    if !failed {
        plan::set_plan_purged(db, plan_id, now_ms).map_err(|e| e.to_string())?;
        report.purged_plans.push(plan_id);
    }
    Ok(report)
}
