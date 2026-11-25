//! 整理（ファイルを動かす・フォルダを作る・隔離フォルダへ移す・取り消す）の芯（`docs/organize.md`）。
//!
//! - 計画を作って DB（`plan`、`plan_op`）に残すところまでは、ファイルに一切触らない。
//! - 人が許可した計画だけを `mkdir` → `move` → `trash` の順に 1 件ずつ記録しながら実行する。
//! - 消す操作はまず同じボリュームの隔離フォルダ（`<data_dir>/trash` または `<ドライブ>/.lisa-trash`）へ移す。
//! - 同じドライブ内の移動は `entry` を引き継ぎ、再走査するフォルダの一覧を返す。

mod apply;
#[cfg(test)]
pub(crate) use apply::rename_without_replace;
mod check;
mod linked_delete;
mod linked_move;
pub(crate) mod markers;
mod plan;
mod quarantine;
pub mod remote_keep;

use std::path::PathBuf;

use crate::catalog::{self, CatalogSet, CatalogSettings};

pub use apply::{
    ApplyReport, UndoReport, UndoSkipped, execute_plan, recover_running_plans, touched_folders,
    undo_plan,
};
pub use plan::{
    CheckStatus, CreateResult, OpCheck, OpInput, OpKind, OpState, Plan, PlanOp, PlanState,
    PlanSummary, approve_plan, approve_plan_with_context, can_dismiss_history, cancel_plan,
    create_peer_plan, create_plan, create_remote_keep_plan, dismiss_history, expire_stale_plans,
    expire_stale_plans_at, get_plan, list_history, list_plans, peer_managed_plan, peer_plan_id,
    plan_ops, reject_plan, request_peer_restore,
};
pub use quarantine::{
    MANIFEST_FILE, Manifest, ManifestEntry, PurgeReport, choose_quarantine_base, purge_expired,
    purge_plan, same_volume,
};

/// 1 つの計画に入れられる操作の上限。
pub const MAX_OPS: usize = 5000;

/// 許可待ち（`pending`）が期限切れ（`expired`）になるまでの時間（24 時間、ミリ秒）。
pub const PENDING_EXPIRE_MS: i64 = 24 * 60 * 60 * 1000;

/// 隔離フォルダの既定の保管日数（設定 `trash_days` の既定）。
pub const DEFAULT_TRASH_DAYS: u64 = 30;

/// 整理の確かめ・実行に使う環境情報。
#[derive(Debug, Clone)]
pub struct Context {
    /// Lisa のデータのフォルダ（`lisa.db` や `trash/` の置き場）。
    pub data_dir: PathBuf,
    /// カタログの集まり（`data`・`cache`・`install`・`log` の役割の判定に使う）。
    pub catalogs: CatalogSet,
    /// 守る場所の判定で使うホームディレクトリ（`None` なら環境変数 `HOME` / `USERPROFILE`）。
    pub home_dir: Option<PathBuf>,
    pub keep_client: Option<remote_keep::relay::Client>,
    pub link_client: Option<LinkClient>,
    pub relocation_client: Option<RelocationClient>,
}

pub type LinkClient = fn(
    &std::path::Path,
    &remote_keep::relay::Route,
    &crate::merged::links::Update,
) -> Result<(), String>;

pub type RelocationClient =
    fn(&std::path::Path, &str, &TransferRequest) -> Result<TransferReply, String>;

pub use linked_delete::{
    DeleteChoice, DeleteRequest, TransferAction, TransferManifest, TransferReply, TransferRequest,
    clean_expired_receives, linked_delete_sources, receive_transfer,
};

pub use markers::flush_links;

impl Context {
    pub fn new(data_dir: PathBuf, catalogs: CatalogSet) -> Self {
        Self {
            data_dir,
            catalogs,
            home_dir: None,
            keep_client: None,
            link_client: None,
            relocation_client: None,
        }
    }

    /// データのフォルダと `settings.json`（あれば）からカタログを読み込んで作る。
    pub fn load(data_dir: PathBuf, settings_path: Option<&std::path::Path>) -> Self {
        let settings = settings_path
            .map(CatalogSettings::from_file)
            .unwrap_or_default();
        let dirs = catalog::dirs(&data_dir, &settings);
        let catalogs = CatalogSet::load_with_builtin(&dirs);
        Self::new(data_dir, catalogs)
    }

    /// 組み込みのカタログだけで作る（テストや簡易利用向け）。
    pub fn with_builtin(data_dir: PathBuf) -> Self {
        Self::new(data_dir, CatalogSet::load_with_builtin(&[]))
    }
}

/// 親フォルダを変えずに名前だけを変更する行き先を求める。
pub fn rename_destination(from: &std::path::Path, new_name: &str) -> Result<PathBuf, String> {
    validate_entry_name(new_name)?;
    let parent = from
        .parent()
        .filter(|p| from.is_absolute() && p.is_absolute())
        .ok_or("対象の絶対パスと親フォルダが必要です")?;
    Ok(parent.join(new_name))
}

/// ファイルシステムの名前を単一のパス成分に限定する。
pub fn validate_entry_name(new_name: &str) -> Result<(), String> {
    use std::path::{Component, Path};

    if new_name.trim().is_empty() {
        return Err("新しい名前を入力してください".into());
    }
    let mut components = Path::new(new_name).components();
    if new_name.contains(['/', '\\', '\0'])
        || !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
    {
        return Err("名前に /、\\、.、.. などは使えません".into());
    }
    #[cfg(windows)]
    if new_name.contains([':', '<', '>', '"', '|', '?', '*'])
        || new_name.ends_with([' ', '.'])
        || new_name.chars().any(|c| c < ' ')
    {
        return Err("Windows の名前に使えない文字が含まれています".into());
    }
    Ok(())
}

/// 行き先が別のディレクトリエントリで占有されているか。
pub fn move_destination_exists(
    from: &std::path::Path,
    to: &std::path::Path,
) -> std::io::Result<bool> {
    match std::fs::symlink_metadata(to) {
        Ok(_) => Ok(!is_name_alias(from, to)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// 大小無視などで別の名前が同じ項目を指すか。ハードリンクは別項目として扱う。
pub(crate) fn is_name_alias(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<bool> {
    let (Some(parent), Some(from_name), Some(to_name)) =
        (from.parent(), from.file_name(), to.file_name())
    else {
        return Ok(false);
    };
    // 同じ inode を持つ別名のハードリンクが大小無視で引ける場合も拒否する。
    if from_name == to_name
        || Some(parent) != to.parent()
        || from_name.to_str().is_none()
        || to_name.to_str().is_none()
        || crate::name::nfc(from_name).to_lowercase() != crate::name::nfc(to_name).to_lowercase()
        || !same_file_identity(from, to)
    {
        return Ok(false);
    }
    let mut found_from = false;
    for entry in std::fs::read_dir(parent)? {
        let name = entry?.file_name();
        if name == to_name {
            return Ok(false);
        }
        found_from |= name == from_name;
    }
    Ok(found_from)
}

fn same_file_identity(from: &std::path::Path, to: &std::path::Path) -> bool {
    file_identity(from).is_some_and(|id| Some(id) == file_identity(to))
}

#[cfg(unix)]
pub(crate) fn file_identity(path: &std::path::Path) -> Option<(u64, [u8; 16])> {
    use std::os::unix::fs::MetadataExt;

    let metadata = std::fs::symlink_metadata(path).ok()?;
    (metadata.ino() != 0).then_some((
        metadata.dev() as u64,
        u128::from(metadata.ino()).to_le_bytes(),
    ))
}

#[cfg(windows)]
pub(crate) fn file_identity(path: &std::path::Path) -> Option<(u64, [u8; 16])> {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO, FileIdInfo,
        GetFileInformationByHandleEx,
    };

    let file = std::fs::OpenOptions::new()
        .access_mode(0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .ok()?;
    // SAFETY: ハンドルと書き込み先は呼び出し中有効。
    unsafe {
        let mut info: FILE_ID_INFO = std::mem::zeroed();
        let ok = GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            (&mut info as *mut FILE_ID_INFO).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        );
        (ok != 0).then_some((info.VolumeSerialNumber, info.FileId.Identifier))
    }
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn file_identity(_path: &std::path::Path) -> Option<(u64, [u8; 16])> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::db::Db;
    use crate::hash::{self, Pace};
    use crate::listing::{Kind, read_dir};
    use crate::query;
    use crate::volumes::Volume;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "lisa-organize-{name}-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct Harness {
        _tmp: TempDir,
        drive: PathBuf,
        data_dir: PathBuf,
        db: Db,
        ctx: Context,
    }

    impl Harness {
        fn new(name: &str) -> Self {
            let tmp = TempDir::new(name);
            let drive = tmp.0.join("drive");
            let data_dir = tmp.0.join("lisa-data");
            fs::create_dir_all(&drive).unwrap();
            fs::create_dir_all(&data_dir).unwrap();
            let mut db = Db::open(&data_dir.join("lisa.db")).unwrap();
            let dev = db.this_device("mac", "mac.local", "macos").unwrap();
            db.upsert_drive(
                &dev,
                &Volume {
                    id: Some("test:vol1".into()),
                    label: "Test".into(),
                    fs: "apfs".into(),
                    mount_path: drive.clone(),
                    capacity: Some(1_000_000),
                    available: Some(500_000),
                    removable: false,
                    network: false,
                    read_only: false,
                    case_sensitive: Some(false),
                    hidden: false,
                },
            )
            .unwrap();
            let mut ctx = Context::with_builtin(data_dir.clone());
            // テスト環境の本物の HOME に依存しないよう仮のホームを置く。
            ctx.home_dir = Some(tmp.0.join("fake-home"));
            Self {
                _tmp: tmp,
                drive,
                data_dir,
                db,
                ctx,
            }
        }

        fn scan(&mut self, do_hash: bool) {
            fn walk(db: &mut Db, mount: &Path, dir: &Path) {
                let res = read_dir(dir).map_err(|e| e.to_string());
                db.save_listing("test:vol1", mount, dir, &res).unwrap();
                if let Ok(listing) = res {
                    for e in listing.entries {
                        if e.kind == Kind::Dir && !e.other_volume && !e.cloud_only {
                            walk(db, mount, &dir.join(&e.name));
                        }
                    }
                }
            }
            walk(&mut self.db, &self.drive, &self.drive);
            if do_hash {
                hash::hash_drive(
                    &mut self.db,
                    "test:vol1",
                    &self.drive,
                    2,
                    &mut |_| {},
                    &Pace::FREE,
                )
                .unwrap();
            }
        }
    }

    fn entry_at(h: &Harness, path: &Path) -> crate::db::Stored {
        let loc = query::locate(&h.db, path).unwrap().unwrap();
        h.db.entry(*loc.chain.last().unwrap()).unwrap().unwrap()
    }

    fn assert_no_file_hashes(h: &mut Harness) {
        let mut count = 0;
        hash::hash_drive(
            &mut h.db,
            "test:vol1",
            &h.drive,
            2,
            &mut |event| count += usize::from(matches!(event, hash::Event::File { .. })),
            &Pace::FREE,
        )
        .unwrap();
        assert_eq!(count, 0);
    }

    fn approve_and_move(h: &mut Harness, from: &Path, to: &Path) -> i64 {
        let created = create_plan(
            &mut h.db,
            &h.ctx,
            "test",
            None,
            &[OpInput::Move {
                from: from.to_owned(),
                to: to.to_owned(),
            }],
            false,
        )
        .unwrap();
        let id = created.id.unwrap_or_else(|| panic!("{:?}", created.checks));
        approve_plan(&mut h.db, id, false).unwrap();
        assert_eq!(execute_plan(&mut h.db, &h.ctx, id).unwrap().done, 1);
        id
    }

    #[test]
    fn moving_hashed_subtrees_updates_only_ancestor_hashes() {
        let mut h = Harness::new("move-merkle");
        let source = h.drive.join("from");
        let destination = h.drive.join("to");
        let tree = source.join("tree");
        let moved_tree = destination.join("moved");
        let leaf = tree.join("nested/item.txt");
        let moved_leaf = moved_tree.join("nested/item.txt");
        let renamed_leaf = source.join("renamed.txt");
        fs::create_dir_all(leaf.parent().unwrap()).unwrap();
        fs::create_dir(&destination).unwrap();
        fs::write(&leaf, b"preserved").unwrap();
        fs::write(destination.join("z.txt"), b"sort order").unwrap();
        h.scan(true);
        let entry = entry_at(&h, &leaf);
        let subtree = entry_at(&h, &tree);
        let before = [&source, &destination, &h.drive].map(|path| entry_at(&h, path).hash);
        let folder_move = approve_and_move(&mut h, &tree, &moved_tree);
        assert_eq!(entry_at(&h, &moved_leaf).id, entry.id);
        assert_eq!(entry_at(&h, &moved_leaf).hash, entry.hash);
        assert_eq!(entry_at(&h, &moved_tree).hash, subtree.hash);
        // 一覧の再走査やキューの再構築を待たず、変更された祖先を集約できる。
        let mut hashed = std::collections::BTreeSet::new();
        loop {
            let batch = hash::hash_queue_batch_checked(
                &mut h.db,
                &[("test:vol1", &h.drive)],
                crate::db::HASH_CHANGED,
                &Pace::FREE,
                |_, _| true,
            )
            .unwrap();
            assert!(batch.files.is_empty());
            if batch.picked == 0 {
                break;
            }
            hashed.extend(batch.folders);
        }
        assert_eq!(
            hashed,
            [&source, &destination, &h.drive]
                .into_iter()
                .cloned()
                .collect()
        );
        assert_no_file_hashes(&mut h);
        for (path, old) in [&source, &destination, &h.drive].into_iter().zip(before) {
            let updated = entry_at(&h, path).hash;
            assert!(updated.is_some());
            assert_ne!(updated, old);
        }

        let file_move = approve_and_move(&mut h, &moved_leaf, &renamed_leaf);
        assert_eq!(entry_at(&h, &renamed_leaf).hash, entry.hash);
        assert_no_file_hashes(&mut h);
        assert_eq!(undo_plan(&mut h.db, &h.ctx, file_move).unwrap().undone, 1);
        assert_eq!(undo_plan(&mut h.db, &h.ctx, folder_move).unwrap().undone, 1);
        h.scan(false);
        assert_no_file_hashes(&mut h);
        assert_eq!(entry_at(&h, &leaf).id, entry.id);
        assert_eq!(entry_at(&h, &h.drive).hash, before[2]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn case_only_rename_and_undo_on_case_insensitive_volume() {
        let mut h = Harness::new("rename-case");
        let lower = h.drive.join("a.txt");
        let upper = h.drive.join("A.txt");
        fs::write(&lower, b"original").unwrap();
        if !upper.exists() {
            // 大小を区別するボリュームでは通常の Move が対象。
            return;
        }
        let assert_names = |expected: &str| {
            let names: Vec<_> = fs::read_dir(lower.parent().unwrap())
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            assert_eq!(names, [std::ffi::OsString::from(expected)]);
        };
        for crashing in [false, true] {
            h.scan(true);
            let id = create_plan(
                &mut h.db,
                &h.ctx,
                "test",
                None,
                &[OpInput::Move {
                    from: lower.clone(),
                    to: upper.clone(),
                }],
                false,
            )
            .unwrap()
            .id
            .expect("大小だけの改名を許可する");
            approve_plan(&mut h.db, id, false).unwrap();
            if crashing {
                // ファイル名を書き換えた直後に落ちても、同じファイルを隔離しない。
                fs::rename(&lower, &upper).unwrap();
                plan::set_plan_state(&mut h.db, id, PlanState::Running, None).unwrap();
                plan::set_op_state(&mut h.db, id, 0, OpState::Running, None, None, None).unwrap();
                let report = recover_running_plans(&mut h.db, &h.ctx).unwrap();
                assert_eq!(report[0].done, 1);
            } else {
                let report = execute_plan(&mut h.db, &h.ctx, id).unwrap();
                assert_eq!(report.done, 1, "{report:?}");
            }
            assert_names("A.txt");
            assert_eq!(fs::read(&upper).unwrap(), b"original");
            assert!(entry_at(&h, &upper).hash.is_some());
            assert_no_file_hashes(&mut h);
            let report = undo_plan(&mut h.db, &h.ctx, id).unwrap();
            assert_eq!(report.undone, 1, "{report:?}");
            assert_names("a.txt");
            assert_eq!(fs::read(&lower).unwrap(), b"original");
            assert_no_file_hashes(&mut h);
        }
    }

    #[test]
    fn rename_succeeds_while_another_connection_is_hashing() {
        let mut h = Harness::new("rename-while-hashing");
        fs::create_dir_all(h.drive.join("sub")).unwrap();
        fs::write(h.drive.join("sub/a.txt"), b"rename me").unwrap();
        fs::write(h.drive.join("slow.bin"), b"read slowly").unwrap();
        h.scan(false);
        // 待ちが長いと、鍵を握られていたときに busy_timeout まで待ってしまうので短くする。
        h.db.conn()
            .execute_batch("PRAGMA busy_timeout = 300")
            .unwrap();

        // 別のつなぎでハッシュし、sub を書いたあと slow.bin を読んでいる途中で止めておく。
        let (reading_tx, reading_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel::<()>();
        let resume_rx = std::sync::Mutex::new(resume_rx);
        let db_path = h.data_dir.join("lisa.db");
        let drive = h.drive.clone();
        let hasher = std::thread::spawn(move || {
            let mut db = Db::open(&db_path).unwrap();
            let step = |bytes: u64| {
                if bytes > 0 {
                    let _ = reading_tx.send(());
                    let _ = resume_rx.lock().unwrap().recv();
                }
                true
            };
            let pace = Pace {
                background: true,
                priority: None,
                step: &step,
            };
            hash::hash_drive(&mut db, "test:vol1", &drive, 1, &mut |_| {}, &pace)
        });
        // sub/a.txt を読んだところ（まだ止めない）。
        reading_rx.recv().unwrap();
        resume_tx.send(()).unwrap();
        // slow.bin を読んでいるところで止まっている。
        reading_rx.recv().unwrap();

        let from = h.drive.join("sub/a.txt");
        let to = rename_destination(&from, "b.txt").unwrap();
        let result = create_plan(
            &mut h.db,
            &h.ctx,
            "test",
            None,
            &[OpInput::Move {
                from,
                to: to.clone(),
            }],
            false,
        )
        .and_then(|res| res.id.ok_or_else(|| format!("{:?}", res.checks)))
        .and_then(|id| approve_plan(&mut h.db, id, false).map(|_| id))
        .and_then(|id| execute_plan(&mut h.db, &h.ctx, id));
        resume_tx.send(()).unwrap();
        hasher.join().unwrap().unwrap();
        let report = result.expect("ハッシュ中でも名前を変更できる");
        assert_eq!(report.done, 1, "{report:?}");
        assert_eq!(fs::read(&to).unwrap(), b"rename me");
        assert!(entry_at(&h, &to).hash.is_some());
        // 古い一覧から集めた名前でルートハッシュを確定していないこと。
        assert!(entry_at(&h, &h.drive).hash.is_none());
        h.scan(true);
        assert!(entry_at(&h, &h.drive).hash.is_some());
    }

    #[cfg(unix)]
    #[test]
    fn hard_links_are_distinct_destinations() {
        let h = Harness::new("rename-hard-link");
        let from = h.drive.join("from.txt");
        let to = h.drive.join("to.txt");
        fs::write(&from, b"original").unwrap();
        fs::hard_link(&from, &to).unwrap();
        assert!(move_destination_exists(&from, &to).unwrap());
        let alias = h.drive.join("TO.txt");
        if alias.exists() {
            assert!(move_destination_exists(&from, &alias).unwrap());
        }
    }

    #[test]
    fn move_trash_and_undo_on_same_volume() {
        let mut h = Harness::new("move-trash-undo");
        let a = h.drive.join("a.txt");
        let dup1 = h.drive.join("dup1.txt");
        let dup2 = h.drive.join("dup2.txt");
        fs::write(&a, b"hello move").unwrap();
        fs::write(&dup1, b"duplicate body").unwrap();
        fs::write(&dup2, b"duplicate body").unwrap();
        h.scan(true);

        let moved = h.drive.join("a-moved.txt");
        let created = create_plan(
            &mut h.db,
            &h.ctx,
            "cli",
            Some("移動と重複削除"),
            &[
                OpInput::Move {
                    from: a.clone(),
                    to: moved.clone(),
                },
                OpInput::Trash {
                    path: dup1.clone(),
                    keep: Some(dup2.clone()),
                },
            ],
            false,
        )
        .unwrap();
        assert!(created.created);
        let plan_id = created.id.unwrap();
        assert_eq!(created.summary.ok, 2);
        assert_eq!(created.summary.risky, 0);
        assert_eq!(created.summary.move_bytes, 10);
        assert_eq!(created.summary.free_bytes, 14);
        // create 時点ではファイルは動かない。
        assert!(a.exists() && !moved.exists() && dup1.exists());

        // DB の entry の数は実行前後で変わらない（entry には書かない）。
        let entry_count_before: i64 =
            h.db.conn()
                .query_row("SELECT count(*) FROM entry", [], |r| r.get(0))
                .unwrap();

        approve_plan(&mut h.db, plan_id, false).unwrap();
        let report = execute_plan(&mut h.db, &h.ctx, plan_id).unwrap();
        assert_eq!(report.state, PlanState::Done);
        assert_eq!(report.done, 2);
        assert_eq!(report.skipped, 0);
        assert_eq!(report.failed, 0);
        assert!(!a.exists() && moved.exists());
        assert!(!dup1.exists() && dup2.exists());
        assert!(report.touched_folders.contains(&h.drive));

        let entry_count_after: i64 =
            h.db.conn()
                .query_row("SELECT count(*) FROM entry", [], |r| r.get(0))
                .unwrap();
        assert_eq!(entry_count_before, entry_count_after);

        // manifest.json が隔離フォルダに書かれている。
        let (_, ops) = get_plan(&h.db, plan_id).unwrap().unwrap();
        let q_path = ops[1].dst.as_ref().unwrap();
        assert!(q_path.exists());
        let manifest_path = h
            .data_dir
            .join("trash")
            .join(plan_id.to_string())
            .join(MANIFEST_FILE);
        let manifest: Manifest =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        assert_eq!(manifest.plan_id, plan_id);
        assert_eq!(manifest.entries.len(), 1);
        assert_eq!(manifest.entries[0].src, dup1);

        // 取り消しで元に戻る。
        let undo = undo_plan(&mut h.db, &h.ctx, plan_id).unwrap();
        assert_eq!(undo.state, PlanState::Undone);
        assert_eq!(undo.undone, 2);
        assert!(undo.skipped.is_empty());
        assert!(a.exists() && !moved.exists());
        assert!(dup1.exists() && !q_path.exists());
        assert_eq!(fs::read(&a).unwrap(), b"hello move");
        assert_eq!(fs::read(&dup1).unwrap(), b"duplicate body");
    }

    #[test]
    fn history_can_dismiss_after_undo_with_only_quarantine_metadata() {
        let mut h = Harness::new("history-trash-undo");
        let nested = h.drive.join("nested");
        fs::create_dir(&nested).unwrap();
        let source = nested.join("empty");
        fs::create_dir(&source).unwrap();
        h.scan(true);

        let id = create_plan(
            &mut h.db,
            &h.ctx,
            "test",
            None,
            &[OpInput::Trash {
                path: source.clone(),
                keep: None,
            }],
            false,
        )
        .unwrap()
        .id
        .unwrap();
        approve_plan(&mut h.db, id, false).unwrap();
        assert_eq!(execute_plan(&mut h.db, &h.ctx, id).unwrap().done, 1);
        let (_, ops) = get_plan(&h.db, id).unwrap().unwrap();
        let payload = ops[0].dst.as_ref().unwrap();
        let plan_dir = h.data_dir.join("trash").join(id.to_string());
        let manifest = plan_dir.join(MANIFEST_FILE);
        assert!(payload.is_dir() && manifest.is_file());

        assert_eq!(
            undo_plan(&mut h.db, &h.ctx, id).unwrap().state,
            PlanState::Undone
        );
        let (plan, _) = get_plan(&h.db, id).unwrap().unwrap();
        assert!(source.is_dir() && payload.parent().unwrap().is_dir());
        assert!(can_dismiss_history(&h.db, &h.data_dir, &plan));

        // 空でも、隔離したフォルダそのものや、用途不明のフォルダは対象外。
        fs::create_dir(payload).unwrap();
        assert!(!can_dismiss_history(&h.db, &h.data_dir, &plan));
        fs::remove_dir(payload).unwrap();
        let unknown = plan_dir.join("unknown");
        fs::create_dir(&unknown).unwrap();
        assert!(!can_dismiss_history(&h.db, &h.data_dir, &plan));
        fs::remove_dir(unknown).unwrap();

        let contents = fs::read(&manifest).unwrap();
        dismiss_history(&h.db, &h.data_dir, id).unwrap();
        assert!(list_history(&h.db, 50).unwrap().is_empty());
        assert_eq!(fs::read(manifest).unwrap(), contents);
        assert!(source.is_dir() && payload.parent().unwrap().is_dir());
    }

    #[test]
    fn mkdir_runs_before_move_and_undo_reverses() {
        let mut h = Harness::new("mkdir-move-order");
        let src = h.drive.join("doc.txt");
        let dup1 = h.drive.join("d1.txt");
        let dup2 = h.drive.join("d2.txt");
        fs::write(&src, b"doc").unwrap();
        fs::write(&dup1, b"same").unwrap();
        fs::write(&dup2, b"same").unwrap();
        h.scan(true);

        let sub = h.drive.join("archive/2026");
        let dst = sub.join("doc.txt");
        // あえて trash → move → mkdir の逆順で渡す。
        let created = create_plan(
            &mut h.db,
            &h.ctx,
            "cli",
            None,
            &[
                OpInput::Trash {
                    path: dup1.clone(),
                    keep: None,
                },
                OpInput::Move {
                    from: src.clone(),
                    to: dst.clone(),
                },
                OpInput::Mkdir { path: sub.clone() },
            ],
            false,
        )
        .unwrap();
        assert!(created.created, "{created:#?}");
        let id = created.id.unwrap();

        approve_plan(&mut h.db, id, false).unwrap();
        let report = execute_plan(&mut h.db, &h.ctx, id).unwrap();
        assert_eq!(report.state, PlanState::Done);
        assert_eq!(report.done, 3);
        assert!(dst.exists() && !src.exists() && !dup1.exists());
        assert!(entry_at(&h, &dst).hash.is_some());
        h.scan(false);
        assert_no_file_hashes(&mut h);

        // 取り消しは trash → move → mkdir の順なので、sub も archive も空になって消える。
        let undo = undo_plan(&mut h.db, &h.ctx, id).unwrap();
        assert_eq!(undo.state, PlanState::Undone);
        assert_eq!(undo.undone, 3);
        assert!(src.exists() && dup1.exists());
        assert!(!sub.exists() && !h.drive.join("archive").exists());
    }

    #[test]
    fn never_overwrites_destination() {
        let mut h = Harness::new("no-overwrite");
        let a = h.drive.join("a.txt");
        let b = h.drive.join("b.txt");
        let c = h.drive.join("c.txt");
        fs::write(&a, b"aaa").unwrap();
        fs::write(&b, b"bbb").unwrap();
        h.scan(true);

        // 計画作成時に行き先がある → refused（計画は作られない）。
        let res = create_plan(
            &mut h.db,
            &h.ctx,
            "cli",
            None,
            &[OpInput::Move {
                from: a.clone(),
                to: b.clone(),
            }],
            false,
        )
        .unwrap();
        assert!(!res.created);
        assert_eq!(res.summary.refused, 1);
        assert!(res.checks[0].check.starts_with("refused:"));

        // 計画作成時は空いていたが、実行前に埋まった → skipped で上書きしない。
        let res2 = create_plan(
            &mut h.db,
            &h.ctx,
            "cli",
            None,
            &[OpInput::Move {
                from: a.clone(),
                to: c.clone(),
            }],
            false,
        )
        .unwrap();
        let id = res2.id.unwrap();
        approve_plan(&mut h.db, id, false).unwrap();
        fs::write(&c, b"existing").unwrap();

        let report = execute_plan(&mut h.db, &h.ctx, id).unwrap();
        assert_eq!(report.state, PlanState::Partial);
        assert_eq!(report.skipped, 1);
        assert_eq!(fs::read(&c).unwrap(), b"existing");
        assert_eq!(fs::read(&a).unwrap(), b"aaa");
    }

    #[test]
    fn last_copy_and_roles_and_skip_risky() {
        let mut h = Harness::new("risky-rules");
        let unique = h.drive.join("only-one.txt");
        let dup1 = h.drive.join("d1.txt");
        let dup2 = h.drive.join("d2.txt");
        let proj = h.drive.join("MyGame");
        fs::create_dir_all(proj.join("Assets")).unwrap();
        fs::create_dir_all(proj.join("Library")).unwrap();
        fs::create_dir_all(proj.join("ProjectSettings")).unwrap();
        fs::write(
            proj.join("ProjectSettings/ProjectVersion.txt"),
            b"m_EditorVersion: 6000.0.1f1\n",
        )
        .unwrap();
        let asset = proj.join("Assets/hero.bin");
        let cache = proj.join("Library/temp.bin");
        fs::write(&unique, b"unique-bytes").unwrap();
        fs::write(&dup1, b"two-copies").unwrap();
        fs::write(&dup2, b"two-copies").unwrap();
        fs::write(&asset, b"two-copies").unwrap(); // data の中（重複でも risky）
        fs::write(&cache, b"only-cache").unwrap(); // cache の中（最後の 1 つでも ok）
        h.scan(true);

        // カタログを当てて catalog_match を保存。
        let (found, _) = crate::catalog::db::find(&h.db, &h.ctx.catalogs, None, false, &|| true)
            .unwrap()
            .unwrap();
        crate::catalog::db::save(&mut h.db, &h.ctx.catalogs, &found, None).unwrap();

        let res = create_plan(
            &mut h.db,
            &h.ctx,
            "cli",
            None,
            &[
                OpInput::Trash {
                    path: unique.clone(),
                    keep: None,
                },
                OpInput::Trash {
                    path: dup1.clone(),
                    keep: Some(dup2.clone()),
                },
                OpInput::Trash {
                    path: asset.clone(),
                    keep: Some(dup2.clone()),
                },
                OpInput::Trash {
                    path: cache.clone(),
                    keep: None,
                },
            ],
            false,
        )
        .unwrap();
        assert!(res.created, "{res:#?}");
        assert!(
            res.checks[0].check.starts_with("risky:"),
            "{:?}",
            res.checks[0]
        );
        assert_eq!(res.checks[1].check, "ok");
        assert!(
            res.checks[2].check.starts_with("risky:"),
            "{:?}",
            res.checks[2]
        );
        assert_eq!(res.checks[3].check, "ok");
        assert_eq!(res.summary.risky, 2);
        assert_eq!(res.summary.ok, 2);

        // skip_risky = true で許可すると、ok の 2 件だけ実行され、risky の 2 件は飛ばされる。
        let id = res.id.unwrap();
        approve_plan(&mut h.db, id, true).unwrap();
        let report = execute_plan(&mut h.db, &h.ctx, id).unwrap();
        assert_eq!(report.state, PlanState::Partial);
        assert_eq!(report.done, 2);
        assert_eq!(report.skipped, 2);
        assert!(unique.exists());
        assert!(asset.exists());
        assert!(!dup1.exists());
        assert!(!cache.exists());

        // 同じ計画で 2 つとも trash する（残り 0 になる）場合も risky。
        let both1 = h.drive.join("both1.txt");
        let both2 = h.drive.join("both2.txt");
        fs::write(&both1, b"pair-only").unwrap();
        fs::write(&both2, b"pair-only").unwrap();
        h.scan(true);
        let pair_res = create_plan(
            &mut h.db,
            &h.ctx,
            "cli",
            None,
            &[
                OpInput::Trash {
                    path: both1.clone(),
                    keep: None,
                },
                OpInput::Trash {
                    path: both2.clone(),
                    keep: None,
                },
            ],
            true,
        )
        .unwrap();
        assert_eq!(pair_res.summary.risky, 2);

        // 同じ中身の 2 つ（ほかに残り無し）を両方 trash し、互いを keep に指定しても risky になる。
        let mutual_keep_res = create_plan(
            &mut h.db,
            &h.ctx,
            "cli",
            None,
            &[
                OpInput::Trash {
                    path: both1,
                    keep: Some(both2.clone()),
                },
                OpInput::Trash {
                    path: both2,
                    keep: Some(h.drive.join("both1.txt")),
                },
            ],
            true,
        )
        .unwrap();
        assert!(
            mutual_keep_res.summary.risky >= 1,
            "mutual keep of last two copies must be risky: {mutual_keep_res:#?}"
        );
        assert_eq!(mutual_keep_res.summary.risky, 2);

        // data フォルダごとの move も risky。
        let move_data = create_plan(
            &mut h.db,
            &h.ctx,
            "cli",
            None,
            &[OpInput::Move {
                from: proj.join("Assets"),
                to: h.drive.join("Assets-moved"),
            }],
            true,
        )
        .unwrap();
        assert_eq!(move_data.summary.risky, 1);
    }

    #[test]
    fn unhashed_file_trash_is_risky() {
        let mut h = Harness::new("unhashed");
        let f = h.drive.join("new.txt");
        fs::write(&f, b"no hash yet").unwrap();
        h.scan(false); // ハッシュしない。

        let res = create_plan(
            &mut h.db,
            &h.ctx,
            "cli",
            None,
            &[OpInput::Trash {
                path: f,
                keep: None,
            }],
            true,
        )
        .unwrap();
        assert_eq!(res.summary.risky, 1);
        assert!(res.checks[0].check.starts_with("risky:"));
    }

    #[test]
    fn changed_source_is_skipped_before_apply() {
        let mut h = Harness::new("changed-source");
        let m = h.drive.join("move-me.txt");
        let t = h.drive.join("trash-me.txt");
        let k = h.drive.join("keep-me.txt");
        fs::write(&m, b"original-1").unwrap();
        fs::write(&t, b"same-bytes").unwrap();
        fs::write(&k, b"same-bytes").unwrap();
        h.scan(true);

        let created = create_plan(
            &mut h.db,
            &h.ctx,
            "cli",
            None,
            &[
                OpInput::Move {
                    from: m.clone(),
                    to: h.drive.join("moved.txt"),
                },
                OpInput::Trash {
                    path: t.clone(),
                    keep: Some(k.clone()),
                },
            ],
            false,
        )
        .unwrap();
        let id = created.id.unwrap();
        approve_plan(&mut h.db, id, false).unwrap();

        // 実行直前に m の大きさを変え、t は同じ大きさ・同じ更新日時で中身だけ書き換える（ハッシュ検証で弾く）。
        fs::write(&m, b"changed-longer-content").unwrap();
        let orig_mtime = fs::metadata(&t).unwrap().modified().unwrap();
        fs::write(&t, b"DIFF-BYTES").unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&t)
            .unwrap()
            .set_modified(orig_mtime)
            .unwrap();

        let report = execute_plan(&mut h.db, &h.ctx, id).unwrap();
        assert_eq!(report.state, PlanState::Partial);
        assert_eq!(report.done, 0);
        assert_eq!(report.skipped, 2);
        assert!(m.exists() && t.exists());
    }

    #[test]
    fn recovers_after_crash_in_running_state() {
        let mut h = Harness::new("crash-recovery");
        let a = h.drive.join("a.txt");
        let b = h.drive.join("b.txt");
        let c = h.drive.join("c.txt");
        fs::write(&a, b"aaa").unwrap();
        fs::write(&b, b"bbb").unwrap();
        fs::write(&c, b"ccc").unwrap();
        h.scan(true);

        let a2 = h.drive.join("a2.txt");
        let b2 = h.drive.join("b2.txt");
        let c2 = h.drive.join("c2.txt");
        let created = create_plan(
            &mut h.db,
            &h.ctx,
            "cli",
            None,
            &[
                OpInput::Move {
                    from: a.clone(),
                    to: a2.clone(),
                },
                OpInput::Move {
                    from: b.clone(),
                    to: b2.clone(),
                },
                OpInput::Move {
                    from: c.clone(),
                    to: c2.clone(),
                },
            ],
            false,
        )
        .unwrap();
        let id = created.id.unwrap();
        approve_plan(&mut h.db, id, false).unwrap();

        // 落ちた状態を再現:
        // - plan は running
        // - seq 0 はディスク上ではすでに move 完了しているのに DB は running のまま
        // - seq 1 はまだ move していないのに DB は running のまま
        // - seq 2 は pending
        fs::rename(&a, &a2).unwrap();
        plan::set_plan_state(&mut h.db, id, PlanState::Running, None).unwrap();
        plan::set_op_state(&mut h.db, id, 0, OpState::Running, None, None, None).unwrap();
        plan::set_op_state(&mut h.db, id, 1, OpState::Running, None, None, None).unwrap();

        let reports = recover_running_plans(&mut h.db, &h.ctx).unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].state, PlanState::Done);
        assert_eq!(reports[0].done, 3);
        assert!(!a.exists() && a2.exists());
        assert!(!b.exists() && b2.exists());
        assert!(!c.exists() && c2.exists());
        for path in [&a2, &b2, &c2] {
            assert!(entry_at(&h, path).hash.is_some());
        }
        assert_no_file_hashes(&mut h);
    }

    fn execute_moves_for_undo(h: &mut Harness) -> i64 {
        let ops = ["a", "b"].map(|name| {
            let from = h.drive.join(name);
            fs::write(&from, name).unwrap();
            OpInput::Move {
                from,
                to: h.drive.join(format!("{name}-moved")),
            }
        });
        h.scan(true);
        let id = create_plan(&mut h.db, &h.ctx, "cli", None, &ops, false)
            .unwrap()
            .id
            .unwrap();
        approve_plan(&mut h.db, id, false).unwrap();
        assert_eq!(execute_plan(&mut h.db, &h.ctx, id).unwrap().done, 2);
        id
    }

    #[test]
    fn concurrent_undo_only_runs_once() {
        let mut h = Harness::new("concurrent-undo");
        let id = execute_moves_for_undo(&mut h);
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        let ctx = h.ctx.clone();
        let first = std::thread::spawn(move || {
            let mut db = Db::open(&ctx.data_dir.join("lisa.db")).unwrap();
            apply::plan_start_hook::set(move || {
                locked_tx.send(()).unwrap();
                resume_rx.recv().unwrap();
            });
            undo_plan(&mut db, &ctx, id)
        });

        locked_rx.recv().unwrap();
        let ctx = h.ctx.clone();
        let second = std::thread::spawn(move || {
            let mut db = Db::open(&ctx.data_dir.join("lisa.db")).unwrap();
            undo_plan(&mut db, &ctx, id)
        });
        resume_tx.send(()).unwrap();
        let first = first.join().unwrap().unwrap();
        assert_eq!(first.undone, 2);
        assert!(first.skipped.is_empty());
        assert!(
            second
                .join()
                .unwrap()
                .unwrap_err()
                .contains("すでに取り消しています")
        );
    }

    #[test]
    fn concurrent_plans_wait_and_both_execute() {
        let mut h = Harness::new("concurrent-plans");
        let ops = ["first", "second"].map(|name| {
            let from = h.drive.join(name);
            fs::write(&from, name).unwrap();
            OpInput::Move {
                from,
                to: h.drive.join(format!("{name}-moved")),
            }
        });
        h.scan(true);
        let [first_id, second_id] = ops.map(|op| {
            let id = create_plan(&mut h.db, &h.ctx, "cli", None, &[op], false)
                .unwrap()
                .id
                .unwrap();
            approve_plan(&mut h.db, id, false).unwrap();
            id
        });

        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let ctx = h.ctx.clone();
        let first = std::thread::spawn(move || {
            let mut db = Db::open(&ctx.data_dir.join("lisa.db")).unwrap();
            apply::plan_start_hook::set(move || {
                locked_tx.send(()).unwrap();
                resume_rx.recv().unwrap();
            });
            execute_plan(&mut db, &ctx, first_id)
        });

        locked_rx.recv().unwrap();
        let ctx = h.ctx.clone();
        let second = std::thread::spawn(move || {
            let mut db = Db::open(&ctx.data_dir.join("lisa.db")).unwrap();
            started_tx.send(()).unwrap();
            result_tx
                .send(execute_plan(&mut db, &ctx, second_id))
                .unwrap();
        });
        started_rx.recv().unwrap();
        let early = result_rx.recv_timeout(std::time::Duration::from_millis(100));
        resume_tx.send(()).unwrap();

        assert_eq!(first.join().unwrap().unwrap().done, 1);
        second.join().unwrap();
        assert!(matches!(
            early,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        assert_eq!(result_rx.recv().unwrap().unwrap().done, 1);
        assert_eq!(fs::read(h.drive.join("first-moved")).unwrap(), b"first");
        assert_eq!(fs::read(h.drive.join("second-moved")).unwrap(), b"second");
    }

    #[test]
    fn undo_refuses_lock_held_by_another_open_file() {
        let mut h = Harness::new("external-plan-lock");
        let id = execute_moves_for_undo(&mut h);
        // プロセス内 Mutex を共有しない相手と同様に、別のハンドルで OS ロックを取る。
        let external_lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(h.data_dir.join("organize.lock"))
            .unwrap();
        external_lock.try_lock().unwrap();
        let mut other_db = Db::open(&h.data_dir.join("lisa.db")).unwrap();
        assert!(
            undo_plan(&mut other_db, &h.ctx, id)
                .unwrap_err()
                .contains("取り消し中")
        );
        drop(external_lock);
        assert_eq!(undo_plan(&mut other_db, &h.ctx, id).unwrap().undone, 2);
    }

    #[test]
    fn interrupted_undo_can_resume_remaining_operations() {
        let mut h = Harness::new("interrupted-undo");
        let id = execute_moves_for_undo(&mut h);
        // 逆順の最初の操作を戻して記録したところで止まった状態。
        let last_op = plan::plan_ops(&h.db, id).unwrap().pop().unwrap();
        fs::rename(last_op.dst.unwrap(), last_op.src).unwrap();
        plan::set_op_state(
            &mut h.db,
            id,
            last_op.seq,
            OpState::Undone,
            None,
            None,
            None,
        )
        .unwrap();

        assert!(recover_running_plans(&mut h.db, &h.ctx).unwrap().is_empty());
        let report = undo_plan(&mut h.db, &h.ctx, id).unwrap();
        assert_eq!(report.state, PlanState::Undone);
        assert_eq!(report.undone, 1);
        assert!(report.skipped.is_empty());
        assert_eq!(fs::read(h.drive.join("a")).unwrap(), b"a");
        assert_eq!(fs::read(h.drive.join("b")).unwrap(), b"b");
    }

    #[test]
    fn undo_skips_when_original_path_is_occupied() {
        let mut h = Harness::new("undo-occupied");
        let a = h.drive.join("a.txt");
        let b = h.drive.join("b.txt");
        fs::write(&a, b"first").unwrap();
        h.scan(true);

        let created = create_plan(
            &mut h.db,
            &h.ctx,
            "cli",
            None,
            &[OpInput::Move {
                from: a.clone(),
                to: b.clone(),
            }],
            false,
        )
        .unwrap();
        let id = created.id.unwrap();
        approve_plan(&mut h.db, id, false).unwrap();
        execute_plan(&mut h.db, &h.ctx, id).unwrap();

        // 元の場所 a.txt を別ファイルで埋める。
        fs::write(&a, b"blocker").unwrap();

        let undo = undo_plan(&mut h.db, &h.ctx, id).unwrap();
        assert_eq!(undo.undone, 0);
        assert_eq!(undo.skipped.len(), 1);
        assert_eq!(undo.state, PlanState::Undone);
        let saved_plan = plan::load_plan_required(&mut h.db, id).unwrap();
        assert_eq!(saved_plan.state, PlanState::Undone);
        let saved_op = plan::plan_ops(&mut h.db, id).unwrap().remove(0);
        assert_eq!(saved_op.state, OpState::Skipped);
        assert_eq!(
            saved_op.reason.as_deref(),
            Some(undo.skipped[0].reason.as_str())
        );
        assert_eq!(fs::read(&a).unwrap(), b"blocker");
        assert_eq!(fs::read(&b).unwrap(), b"first");
    }

    #[test]
    fn protected_paths_and_flags_are_refused() {
        let mut h = Harness::new("protected");
        let ok_file = h.drive.join("ok.txt");
        let cloud_file = h.drive.join("cloud.txt");
        let other_dir = h.drive.join("mounted");
        fs::write(&ok_file, b"ok").unwrap();
        fs::write(&cloud_file, b"cloud").unwrap();
        fs::create_dir_all(&other_dir).unwrap();
        h.scan(true);

        // cloud_only と other_volume の印を DB に立てる。
        let cloud_id = query::locate(&h.db, &cloud_file)
            .unwrap()
            .unwrap()
            .chain
            .pop()
            .unwrap();
        let other_id = query::locate(&h.db, &other_dir)
            .unwrap()
            .unwrap()
            .chain
            .pop()
            .unwrap();
        h.db.conn()
            .execute("UPDATE entry SET cloud_only = 1 WHERE id = ?1", [cloud_id])
            .unwrap();
        h.db.conn()
            .execute(
                "UPDATE entry SET other_volume = 1 WHERE id = ?1",
                [other_id],
            )
            .unwrap();

        // ドライブの一番上、cloud_only、other_volume、.lisa-trash はすべて refused。
        for op in [
            OpInput::Trash {
                path: h.drive.clone(),
                keep: None,
            },
            OpInput::Trash {
                path: cloud_file,
                keep: None,
            },
            OpInput::Trash {
                path: other_dir,
                keep: None,
            },
            OpInput::Mkdir {
                path: h.drive.join(".lisa-trash/sub"),
            },
        ] {
            let r = create_plan(&mut h.db, &h.ctx, "cli", None, &[op], false).unwrap();
            assert!(!r.created);
            assert_eq!(r.summary.refused, 1, "{r:#?}");
        }

        // OS とアプリ・データのフォルダの保護判定を直接確かめる（Mac・Windows・firmlink）。
        let home_mac = Path::new("/Users/alice");
        let data = Path::new("/Users/alice/Library/Application Support/Lisa");
        for p in [
            "/System/Library/CoreServices",
            "/System/Volumes/Data/System/Foo",
            "/Library/Preferences",
            "/Applications/Safari.app",
            "/Users/alice/Library/Caches",
            "/System/Volumes/Data/Users/alice/Library/Application Support/Lisa/lisa.db",
            "/Users/alice",
            "C:\\Windows\\System32",
            "C:\\Program Files\\App",
            "C:\\Program Files (x86)\\Old",
            "C:\\ProgramData\\Foo",
            "C:\\Users\\alice\\AppData\\Local",
        ] {
            assert!(
                check::is_protected_path(Path::new(p), None, data, Some(home_mac), true).is_some(),
                "should be protected: {p}"
            );
        }
        // 普通のユーザーファイル（/System/Volumes/Data/Users/alice/Documents/...）は保護されない。
        assert!(
            check::is_protected_path(
                Path::new("/System/Volumes/Data/Users/alice/Documents/a.txt"),
                Some(Path::new("/System/Volumes/Data")),
                data,
                Some(home_mac),
                true,
            )
            .is_none()
        );
    }

    #[test]
    fn conflicts_expire_cancel_and_purge_work() {
        let mut h = Harness::new("lifecycle");
        let a = h.drive.join("a.txt");
        let b = h.drive.join("b.txt");
        let dir = h.drive.join("dir");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&a, b"dup").unwrap();
        fs::write(&b, b"dup").unwrap();
        h.scan(true);

        // 同じパスを 2 回触る・to が他の from の中に入る → refused。
        let dup_touch = create_plan(
            &mut h.db,
            &h.ctx,
            "cli",
            None,
            &[
                OpInput::Move {
                    from: a.clone(),
                    to: h.drive.join("x.txt"),
                },
                OpInput::Trash {
                    path: a.clone(),
                    keep: None,
                },
            ],
            false,
        )
        .unwrap();
        assert!(!dup_touch.created);
        assert_eq!(dup_touch.summary.refused, 2);

        let inside_from = create_plan(
            &mut h.db,
            &h.ctx,
            "cli",
            None,
            &[
                OpInput::Move {
                    from: dir.clone(),
                    to: h.drive.join("dir2"),
                },
                OpInput::Move {
                    from: a.clone(),
                    to: dir.join("a.txt"),
                },
            ],
            false,
        )
        .unwrap();
        assert!(!inside_from.created);
        assert!(inside_from.summary.refused >= 1);

        // 取り下げ（cancel）と期限切れ（expired）。
        let p1 = create_plan(
            &mut h.db,
            &h.ctx,
            "mcp",
            None,
            &[OpInput::Trash {
                path: a.clone(),
                keep: Some(b.clone()),
            }],
            false,
        )
        .unwrap()
        .id
        .unwrap();
        assert_eq!(
            cancel_plan(&mut h.db, p1).unwrap().state,
            PlanState::Cancelled
        );

        let p2 = create_plan(
            &mut h.db,
            &h.ctx,
            "mcp",
            None,
            &[OpInput::Trash {
                path: a.clone(),
                keep: Some(b.clone()),
            }],
            false,
        )
        .unwrap()
        .id
        .unwrap();
        let future = crate::db::now_ms() + PENDING_EXPIRE_MS + 1000;
        assert_eq!(expire_stale_plans_at(&mut h.db, future).unwrap(), 1);
        assert_eq!(
            get_plan(&h.db, p2).unwrap().unwrap().0.state,
            PlanState::Expired
        );

        // 実行 → 期限切れの隔離フォルダをゴミ箱送り（モック削除関数で本物のゴミ箱には触らない）。
        let p3 = create_plan(
            &mut h.db,
            &h.ctx,
            "cli",
            None,
            &[OpInput::Trash {
                path: a.clone(),
                keep: Some(b.clone()),
            }],
            false,
        )
        .unwrap()
        .id
        .unwrap();
        approve_plan(&mut h.db, p3, false).unwrap();
        execute_plan(&mut h.db, &h.ctx, p3).unwrap();

        let mut trashed = Vec::new();
        let after_31d = crate::db::now_ms() + 31 * 86_400_000;
        let purge = quarantine::purge_expired_at_with(
            &mut h.db,
            &h.data_dir,
            DEFAULT_TRASH_DAYS,
            after_31d,
            |p| {
                trashed.push(p.to_path_buf());
                fs::remove_dir_all(p).map_err(|e| e.to_string())
            },
        )
        .unwrap();
        assert_eq!(purge.purged_plans, vec![p3]);
        assert_eq!(trashed.len(), 1);

        // ゴミ箱送り済みの計画の trash は取り消せない（skipped）。
        let undo = undo_plan(&mut h.db, &h.ctx, p3).unwrap();
        assert_eq!(undo.undone, 0);
        assert_eq!(undo.skipped.len(), 1);
    }

    #[test]
    fn merged_file_survives_name_collision_and_preserves_ancestor_hash() {
        let mut h = Harness::new("merge-marker");
        let source_parent = h.drive.join("a").join("b");
        fs::create_dir_all(&source_parent).unwrap();
        let keep = h.drive.join("keep.txt");
        let source = source_parent.join("海 写真.txt");
        let collision = source_parent.join("海 写真.txt.lmf");
        fs::write(&collision, b"same").unwrap();
        fs::write(&source, b"same").unwrap();
        fs::write(&keep, b"same").unwrap();
        h.scan(true);
        let before = h.db.drive_root("test:vol1").unwrap().unwrap().hash;
        let signature = crate::dups::signature(&h.db).unwrap();

        let id = create_plan(
            &mut h.db,
            &h.ctx,
            "cli",
            None,
            &[OpInput::Trash {
                path: source.clone(),
                keep: Some(keep),
            }],
            false,
        )
        .unwrap()
        .id
        .unwrap();
        approve_plan(&mut h.db, id, false).unwrap();
        assert_eq!(execute_plan(&mut h.db, &h.ctx, id).unwrap().done, 1);
        assert!(!source.exists());
        assert_eq!(fs::read(&collision).unwrap(), b"same");
        let marker_path = source_parent.join("海 写真.txt (2).lmf");
        let marker = crate::merged::read(&marker_path).unwrap();
        assert_eq!(crate::merged::inspect(&marker_path).unwrap(), marker);
        assert!(
            crate::merged::inspect(&collision)
                .unwrap_err()
                .contains("形式ではありません")
        );
        let renamed = source_parent.join("種類違い.lmd");
        fs::copy(&marker_path, &renamed).unwrap();
        assert!(
            crate::merged::inspect(&renamed)
                .unwrap_err()
                .contains("種類が一致しません")
        );
        fs::remove_file(&renamed).unwrap();
        assert!(
            crate::merged::inspect(&source_parent.join("無い.lmf"))
                .unwrap_err()
                .contains("開けません")
        );
        assert_eq!(marker.source.relative_path.last().unwrap(), "海 写真.txt");
        assert_eq!(marker.target.relative_path, ["keep.txt"]);
        assert_eq!(marker.kind, Kind::File);
        h.scan(true);
        assert_eq!(h.db.drive_root("test:vol1").unwrap().unwrap().hash, before);
        assert_ne!(crate::dups::signature(&h.db).unwrap(), signature);
        let groups = crate::dups::groups(&h.db, 5).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].copies.len(), 2);
        assert!(groups[0].copies.iter().any(|copy| copy.path == collision));

        assert_eq!(undo_plan(&mut h.db, &h.ctx, id).unwrap().undone, 1);
        assert_eq!(fs::read(source).unwrap(), b"same");
        assert!(!marker_path.exists());
    }

    #[test]
    fn opening_a_marker_resolves_only_the_recorded_unchanged_target() {
        let mut h = Harness::new("merge-target");
        let keep = h.drive.join("keep.txt");
        let source = h.drive.join("copy.txt");
        for path in [&source, &keep] {
            fs::write(path, b"same").unwrap();
        }
        h.scan(true);
        let id = create_plan(
            &mut h.db,
            &h.ctx,
            "cli",
            None,
            &[OpInput::Trash {
                path: source,
                keep: Some(keep.clone()),
            }],
            false,
        )
        .unwrap()
        .id
        .unwrap();
        approve_plan(&mut h.db, id, false).unwrap();
        assert_eq!(execute_plan(&mut h.db, &h.ctx, id).unwrap().done, 1);
        let marker = crate::merged::read(&h.drive.join("copy.txt.lmf")).unwrap();
        let expected = query::locate(&h.db, &keep).unwrap().unwrap().chain;
        assert_eq!(
            crate::merged::resolve_target(&h.db, &marker).unwrap(),
            expected
        );
        fs::write(&keep, b"else").unwrap();
        assert!(
            crate::merged::resolve_target(&h.db, &marker)
                .unwrap_err()
                .contains("内容が変わっています")
        );
        fs::write(&keep, b"same").unwrap();
        fs::rename(&keep, h.drive.join("renamed.txt")).unwrap();
        assert!(
            crate::merged::resolve_target(&h.db, &marker)
                .unwrap_err()
                .contains("移動・改名")
        );
    }

    fn publish_marker(h: &mut Harness, source: &Path, keep: &Path) -> i64 {
        let created = create_plan(
            &mut h.db,
            &h.ctx,
            "test",
            None,
            &[OpInput::Trash {
                path: source.to_owned(),
                keep: Some(keep.to_owned()),
            }],
            false,
        )
        .unwrap();
        let id = created.id.unwrap_or_else(|| panic!("{:?}", created.checks));
        approve_plan(&mut h.db, id, false).unwrap();
        assert_eq!(execute_plan(&mut h.db, &h.ctx, id).unwrap().done, 1);
        h.scan(true);
        id
    }

    #[test]
    fn undo_discovers_new_links_and_recovers_after_rename() {
        for interrupted in [false, true] {
            let mut h = Harness::new("undo-new-links");
            let original = h.drive.join("old");
            let moved = h.drive.join("moved");
            let copy = h.drive.join("copy");
            for path in [&original, &copy] {
                fs::write(path, b"same").unwrap();
            }
            h.scan(true);
            let id = approve_and_move(&mut h, &original, &moved);
            h.scan(true);
            publish_marker(&mut h, &copy, &moved);
            let path = h.drive.join("copy.lmf");
            let marker = crate::merged::read(&path).unwrap();
            h.db.conn()
                .execute(
                    "UPDATE merged_link SET sync_state='pending' WHERE link_id=?1",
                    [&marker.link_id],
                )
                .unwrap();
            let postponed = undo_plan(&mut h.db, &h.ctx, id).unwrap();
            assert_eq!(postponed.undone, 0);
            assert_eq!(postponed.state, PlanState::Done);
            assert!(moved.exists());
            h.db.conn()
                .execute(
                    "UPDATE merged_link SET sync_state='confirmed' WHERE link_id=?1",
                    [&marker.link_id],
                )
                .unwrap();
            if interrupted {
                let op = plan_ops(&h.db, id).unwrap().remove(0);
                super::linked_move::prepare_undo(&h.db, &op, &moved).unwrap();
                fs::rename(&moved, &original).unwrap();
            }
            assert_eq!(undo_plan(&mut h.db, &h.ctx, id).unwrap().undone, 1);
            let marker = crate::merged::read(&path).unwrap();
            assert_eq!(marker.target.relative_path, ["old"]);
            assert!(crate::merged::resolve_target(&h.db, &marker).is_ok());
            assert_no_file_hashes(&mut h);
        }
    }

    #[test]
    fn periodic_flush_retries_a_local_rewrite_after_an_io_failure() {
        let mut h = Harness::new("move-rewrite-retry");
        let old = h.drive.join("old");
        let new = h.drive.join("moved");
        let copy = h.drive.join("copy");
        for path in [&old, &copy] {
            fs::write(path, b"same").unwrap();
        }
        h.scan(true);
        let merge = publish_marker(&mut h, &copy, &old);
        let path = h.drive.join("copy.lmf");
        let original = crate::merged::read(&path).unwrap();
        let blocked = h.drive.join(format!(
            ".lisa-restore-move-{}-0-0-{}.tmp",
            merge + 1,
            original.link_id
        ));
        fs::write(&blocked, b"external").unwrap();
        let id = approve_and_move(&mut h, &old, &new);
        assert_eq!(id, merge + 1);
        assert_eq!(crate::merged::read(&path).unwrap(), original);
        flush_links(&mut h.db, &h.ctx).unwrap();
        fs::remove_file(blocked).unwrap();
        // 別接続で再開しても、永続化した書き換えを続けられる。
        let mut reopened = Db::open(&h.data_dir.join("lisa.db")).unwrap();
        flush_links(&mut reopened, &h.ctx).unwrap();
        let updated = crate::merged::read(&path).unwrap();
        assert_eq!(updated.target.relative_path, ["moved"]);
        assert!(crate::merged::resolve_target(&reopened, &updated).is_ok());
    }

    #[test]
    fn deletion_check_preserves_a_remote_rewrite_between_backup_and_publish() {
        let mut h = Harness::new("remote-rewrite-recovery");
        let old = h.drive.join("old");
        let new = h.drive.join("moved");
        let copy = h.drive.join("copy");
        for path in [&old, &copy] {
            fs::write(path, b"same").unwrap();
        }
        h.scan(true);
        publish_marker(&mut h, &copy, &old);
        let path = h.drive.join("copy.lmf");
        let original = crate::merged::read(&path).unwrap();
        h.db.conn()
            .execute(
                "UPDATE merged_link SET relay_device='relay' WHERE link_id=?1",
                [&original.link_id],
            )
            .unwrap();
        let (link, _) = crate::merged::links::active_link(&h.db, &original.link_id)
            .unwrap()
            .unwrap();
        let target = crate::merged::Location::local_for_path(&h.db, &new).unwrap();
        let updated = crate::merged::links::retargeted_marker(&original, &target).unwrap();
        let operation = uuid::Uuid::new_v4().to_string();
        let staged = path.with_file_name(format!(
            ".lisa-restore-{operation}-{}.tmp",
            original.link_id
        ));
        let backup = path.with_file_name(format!(
            ".lisa-restore-{operation}-{}.old",
            original.link_id
        ));
        fs::write(&staged, serde_json::to_vec_pretty(&updated).unwrap()).unwrap();
        let progress = serde_json::json!({
            "links": [], "destination": null, "chosen_removed": false,
            "rewrites": [{"link_id": original.link_id, "identity": file_identity(&staged).unwrap(),
                          "old_identity": file_identity(&path).unwrap()}]
        });
        h.db.conn()
            .execute(
                "INSERT INTO merged_link_rewrite VALUES (?1,?2,?3)",
                rusqlite::params![operation, original.link_id, progress.to_string()],
            )
            .unwrap();
        fs::rename(&path, &backup).unwrap();
        flush_links(&mut h.db, &h.ctx).unwrap();
        assert!(
            crate::merged::links::active_link(&h.db, &original.link_id)
                .unwrap()
                .is_some()
        );
        let request = TransferRequest {
            operation_id: operation,
            marker: original,
            generation: link.generation,
            expires_at: crate::db::now_ms() + 60_000,
            action: TransferAction::Retarget { target },
        };
        let (reply, _) = receive_transfer(&mut h.db, &h.data_dir, "relay", &request).unwrap();
        assert!(reply.published);
        assert_eq!(crate::merged::read(&path).unwrap(), updated);
        assert!(!backup.exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn retarget_uses_the_stored_names_of_destination_ancestors() {
        let mut h = Harness::new("normalized-destination");
        let old = h.drive.join("old");
        let copy = h.drive.join("copy");
        let parent = h.drive.join("Café").join("Child");
        fs::create_dir_all(&parent).unwrap();
        for path in [&old, &copy] {
            fs::write(path, b"same").unwrap();
        }
        let alias = h.drive.join("CAFÉ").join("child");
        if !alias.is_dir() {
            return;
        }
        h.scan(true);
        publish_marker(&mut h, &copy, &old);
        let moved = alias.join("moved");
        let id = approve_and_move(&mut h, &old, &moved);
        let marker = crate::merged::read(&h.drive.join("copy.lmf")).unwrap();
        assert_eq!(
            marker.target.display_path,
            parent.join("moved").to_str().unwrap()
        );
        assert!(crate::merged::resolve_target(&h.db, &marker).is_ok());
        assert_eq!(undo_plan(&mut h.db, &h.ctx, id).unwrap().undone, 1);
    }

    #[test]
    fn moving_targets_updates_the_marker_and_undo_reverses_it() {
        for (directory, parent) in [(false, false), (true, false), (false, true)] {
            let mut h = Harness::new("move-target");
            let old = h.drive.join("old");
            let new = h.drive.join("new");
            let source = h.drive.join("copy");
            let keep = if parent {
                old.join("keep")
            } else {
                old.clone()
            };
            if directory {
                for path in [&source, &keep] {
                    fs::create_dir(path).unwrap();
                    fs::write(path.join("item"), b"same").unwrap();
                }
            } else {
                if parent {
                    fs::create_dir(&old).unwrap();
                }
                for path in [&source, &keep] {
                    fs::write(path, b"same").unwrap();
                }
            }
            h.scan(true);
            publish_marker(&mut h, &source, &keep);
            let marker_path = h
                .drive
                .join(if directory { "copy.lmd" } else { "copy.lmf" });
            let id = approve_and_move(&mut h, &old, &new);
            let marker = crate::merged::read(&marker_path).unwrap();
            assert_eq!(
                marker.target.relative_path,
                if parent {
                    vec!["new", "keep"]
                } else {
                    vec!["new"]
                }
            );
            assert!(crate::merged::resolve_target(&h.db, &marker).is_ok());
            assert!(entry_at(&h, &marker_path).merged_verified);
            assert_eq!(undo_plan(&mut h.db, &h.ctx, id).unwrap().undone, 1);
            let restored = crate::merged::read(&marker_path).unwrap();
            assert_eq!(restored.target.relative_path[0], "old");
            assert!(crate::merged::resolve_target(&h.db, &restored).is_ok());
        }
    }

    #[test]
    fn recovery_retargets_the_marker_after_filesystem_move() {
        let mut h = Harness::new("recover-linked-move");
        let keep = h.drive.join("keep");
        let source = h.drive.join("copy");
        let moved = h.drive.join("moved");
        for path in [&keep, &source] {
            fs::write(path, b"same").unwrap();
        }
        h.scan(true);
        publish_marker(&mut h, &source, &keep);
        let id = create_plan(
            &mut h.db,
            &h.ctx,
            "test",
            None,
            &[OpInput::Move {
                from: keep.clone(),
                to: moved.clone(),
            }],
            false,
        )
        .unwrap()
        .id
        .unwrap();
        approve_plan(&mut h.db, id, false).unwrap();
        let op = plan_ops(&h.db, id).unwrap().remove(0);
        super::linked_move::prepare(&h.db, &op).unwrap();
        plan::set_plan_state(&mut h.db, id, PlanState::Running, None).unwrap();
        plan::set_op_state(&mut h.db, id, 0, OpState::Running, None, None, None).unwrap();
        fs::rename(&keep, &moved).unwrap();

        let reports = recover_running_plans(&mut h.db, &h.ctx).unwrap();
        assert_eq!(reports[0].done, 1);
        let path = h.drive.join("copy.lmf");
        let marker = crate::merged::read(&path).unwrap();
        assert_eq!(marker.target.relative_path, ["moved"]);
        assert!(crate::merged::resolve_target(&h.db, &marker).is_ok());
        assert!(reports[0].touched_folders.contains(&h.drive));
        assert_no_file_hashes(&mut h);
    }

    #[test]
    fn a_marker_moved_before_a_crash_is_recovered_without_being_removed() {
        let mut h = Harness::new("recover-marker-move");
        let keep = h.drive.join("keep");
        let source = h.drive.join("copy");
        for path in [&keep, &source] {
            fs::write(path, b"same").unwrap();
        }
        h.scan(true);
        publish_marker(&mut h, &source, &keep);
        let original = h.drive.join("copy.lmf");
        let moved = h.drive.join("renamed.lmf");
        let created = create_plan(
            &mut h.db,
            &h.ctx,
            "test",
            None,
            &[OpInput::Move {
                from: original.clone(),
                to: moved.clone(),
            }],
            false,
        )
        .unwrap();
        let id = created.id.unwrap();
        approve_plan(&mut h.db, id, false).unwrap();
        let op = plan_ops(&h.db, id).unwrap().remove(0);
        super::linked_move::prepare(&h.db, &op).unwrap();
        plan::set_plan_state(&mut h.db, id, PlanState::Running, None).unwrap();
        plan::set_op_state(&mut h.db, id, 0, OpState::Running, None, None, None).unwrap();
        fs::rename(original, &moved).unwrap();

        flush_links(&mut h.db, &h.ctx).unwrap();
        assert_eq!(recover_running_plans(&mut h.db, &h.ctx).unwrap()[0].done, 1);
        let recovered = crate::merged::read(&moved).unwrap();
        assert_eq!(recovered.source.relative_path, ["renamed"]);
        assert!(entry_at(&h, &moved).merged_verified);
    }

    #[test]
    fn moving_verified_markers_updates_their_source_and_original_undo() {
        let mut h = Harness::new("move-marker");
        let keep = h.drive.join("keep");
        let source = h.drive.join("copy");
        let folder = h.drive.join("folder");
        let relocated = h.drive.join("relocated");
        fs::create_dir(&folder).unwrap();
        for path in [&source, &keep] {
            fs::write(path, b"same").unwrap();
        }
        h.scan(true);
        let trash = publish_marker(&mut h, &source, &keep);
        // アプリではカタログの再評価トリガーも同じ DB で動く。
        crate::catalog::db::IncrementalMatches::default()
            .refresh(&h.db, &h.ctx.catalogs, true)
            .unwrap();
        let marker = h.drive.join("copy.lmf");
        let within = folder.join("copy.lmf");
        let move_id = approve_and_move(&mut h, &marker, &within);
        let renamed = folder.join("renamed.lmf");
        let rename_id = approve_and_move(&mut h, &within, &renamed);
        h.scan(false);
        let folder_id = approve_and_move(&mut h, &folder, &relocated);
        let final_marker = relocated.join("renamed.lmf");
        assert!(entry_at(&h, &final_marker).merged_verified);
        let current = crate::merged::read(&final_marker).unwrap();
        assert_eq!(current.source.relative_path, ["relocated", "renamed"]);
        flush_links(&mut h.db, &h.ctx).unwrap();
        assert_eq!(
            crate::merged::links::sources_for_entry(&h.db, entry_at(&h, &keep).id)
                .unwrap()
                .len(),
            1
        );
        let refused = create_plan(
            &mut h.db,
            &h.ctx,
            "test",
            None,
            &[OpInput::Move {
                from: final_marker.clone(),
                to: relocated.join("renamed.txt"),
            }],
            false,
        )
        .unwrap();
        assert!(!refused.created);
        assert_eq!(undo_plan(&mut h.db, &h.ctx, folder_id).unwrap().undone, 1);
        assert_eq!(undo_plan(&mut h.db, &h.ctx, rename_id).unwrap().undone, 1);
        assert_eq!(undo_plan(&mut h.db, &h.ctx, move_id).unwrap().undone, 1);
        assert!(entry_at(&h, &marker).merged_verified);
        approve_and_move(&mut h, &marker, &within);
        assert_eq!(undo_plan(&mut h.db, &h.ctx, trash).unwrap().undone, 1);
        assert!(!within.exists());
        assert_eq!(fs::read(source).unwrap(), b"same");
    }

    #[test]
    fn folder_target_remains_verifiable_after_a_child_becomes_a_link() {
        let mut h = Harness::new("merge-nested-marker");
        let retained = h.drive.join("retained");
        let source = h.drive.join("source");
        let kept_child = h.drive.join("child.txt");
        fs::write(&kept_child, b"same").unwrap();
        for folder in [&retained, &source] {
            fs::create_dir(folder).unwrap();
            fs::write(folder.join("child.txt"), b"same").unwrap();
        }
        h.scan(true);

        for (copy, keep) in [
            (source, retained.clone()),
            (retained.join("child.txt"), kept_child),
        ] {
            let id = create_plan(
                &mut h.db,
                &h.ctx,
                "cli",
                None,
                &[OpInput::Trash {
                    path: copy,
                    keep: Some(keep),
                }],
                false,
            )
            .unwrap()
            .id
            .unwrap();
            approve_plan(&mut h.db, id, false).unwrap();
            assert_eq!(execute_plan(&mut h.db, &h.ctx, id).unwrap().done, 1);
            h.scan(true);
        }
        let marker = crate::merged::read(&h.drive.join("source.lmd")).unwrap();
        let expected = query::locate(&h.db, &retained).unwrap().unwrap().chain;
        assert_eq!(
            crate::merged::resolve_target(&h.db, &marker).unwrap(),
            expected
        );
    }

    #[test]
    fn undo_keeps_a_marker_whose_text_was_edited() {
        use std::io::Write;

        let mut h = Harness::new("merge-undo-edit");
        let keep = h.drive.join("keep.txt");
        let source = h.drive.join("copy.txt");
        for path in [&source, &keep] {
            fs::write(path, b"same").unwrap();
        }
        h.scan(true);
        let id = create_plan(
            &mut h.db,
            &h.ctx,
            "cli",
            None,
            &[OpInput::Trash {
                path: source.clone(),
                keep: Some(keep),
            }],
            false,
        )
        .unwrap()
        .id
        .unwrap();
        approve_plan(&mut h.db, id, false).unwrap();
        assert_eq!(execute_plan(&mut h.db, &h.ctx, id).unwrap().done, 1);
        let marker = h.drive.join("copy.txt.lmf");
        fs::OpenOptions::new()
            .append(true)
            .open(&marker)
            .unwrap()
            .write_all(b"\n")
            .unwrap();
        assert_eq!(undo_plan(&mut h.db, &h.ctx, id).unwrap().undone, 1);
        assert!(source.exists());
        assert!(marker.exists());
        let op = plan_ops(&h.db, id).unwrap().remove(0);
        assert!(op.reason.unwrap().contains("目印が残っています"));
    }

    #[test]
    fn recovery_creates_folder_marker_once_and_does_not_restore_a_deleted_marker() {
        for already_published in [false, true] {
            let mut h = Harness::new("merge-recovery");
            let keep = h.drive.join("keep");
            let source = h.drive.join("copy");
            for path in [&source, &keep] {
                fs::create_dir_all(path).unwrap();
                fs::write(path.join("child"), b"same").unwrap();
            }
            h.scan(true);
            let before = h.db.drive_root("test:vol1").unwrap().unwrap().hash;
            let id = create_plan(
                &mut h.db,
                &h.ctx,
                "cli",
                None,
                &[OpInput::Trash {
                    path: source.clone(),
                    keep: Some(keep.clone()),
                }],
                false,
            )
            .unwrap()
            .id
            .unwrap();
            approve_plan(&mut h.db, id, false).unwrap();
            plan::set_plan_state(&mut h.db, id, PlanState::Running, None).unwrap();
            plan::set_op_state(&mut h.db, id, 0, OpState::Running, None, None, None).unwrap();
            let op = plan_ops(&h.db, id).unwrap().remove(0);
            markers::prepare(&mut h.db, &op, None, None).unwrap();
            let quarantined = quarantine::move_to_quarantine(
                id,
                0,
                OpKind::Trash,
                &source,
                Some(&h.drive),
                &h.data_dir,
                |from, to| fs::rename(from, to),
            )
            .unwrap();
            let path = h.drive.join("copy.lmd");
            if already_published {
                markers::publish(&mut h.db, &op, Some(&quarantined)).unwrap();
                fs::remove_file(&path).unwrap();
            }
            assert_eq!(recover_running_plans(&mut h.db, &h.ctx).unwrap()[0].done, 1);
            assert_eq!(path.exists(), !already_published);
            if !already_published {
                let marker = crate::merged::read(&path).unwrap();
                assert_eq!(marker.kind, Kind::Dir);
                assert_eq!(marker.size, 4);
                h.scan(true);
                assert_eq!(h.db.drive_root("test:vol1").unwrap().unwrap().hash, before);
                assert!(crate::dups::groups(&h.db, 5).unwrap().is_empty());
            }
            assert_eq!(undo_plan(&mut h.db, &h.ctx, id).unwrap().undone, 1);
            assert_eq!(fs::read(source.join("child")).unwrap(), b"same");
            assert!(!path.exists());
        }
    }

    #[test]
    fn deleting_linked_actual_requires_a_choice_and_retargets_other_local_links() {
        let mut h = Harness::new("linked-delete");
        let a = h.drive.join("keeper.txt");
        let b = h.drive.join("restore.txt");
        let c = h.drive.join("other.txt");
        for path in [&a, &b, &c] {
            fs::write(path, b"same").unwrap();
        }
        h.scan(true);
        let merge = create_plan(
            &mut h.db,
            &h.ctx,
            "cli",
            None,
            &[
                OpInput::Trash {
                    path: b.clone(),
                    keep: Some(a.clone()),
                },
                OpInput::Trash {
                    path: c.clone(),
                    keep: Some(a.clone()),
                },
            ],
            false,
        )
        .unwrap()
        .id
        .unwrap();
        approve_plan(&mut h.db, merge, false).unwrap();
        assert_eq!(execute_plan(&mut h.db, &h.ctx, merge).unwrap().done, 2);
        h.scan(true);

        let pending = create_plan(
            &mut h.db,
            &h.ctx,
            "cli",
            None,
            &[OpInput::Trash {
                path: a.clone(),
                keep: None,
            }],
            false,
        )
        .unwrap();
        assert!(!pending.created);
        assert!(pending.checks[0].check.contains("確認待ち"));
        let sources = linked_delete_sources(&h.db, &a).unwrap();
        assert_eq!(sources.len(), 2);
        let marker_b = crate::merged::read(&h.drive.join("restore.txt.lmf")).unwrap();
        let result = create_plan(
            &mut h.db,
            &h.ctx,
            "cli",
            None,
            &[OpInput::TrashLinked {
                path: a.clone(),
                decision: DeleteRequest {
                    choice: DeleteChoice::Restore {
                        link_id: marker_b.link_id.clone(),
                    },
                    links: sources
                        .into_iter()
                        .map(|link| link.marker.link_id)
                        .collect(),
                },
            }],
            false,
        )
        .unwrap();
        let id = result.id.unwrap_or_else(|| panic!("{:?}", result.checks));
        assert_eq!(result.summary.risky, 0, "戻し先があれば最後の 1 つではない");
        approve_plan(&mut h.db, id, false).unwrap();
        assert_eq!(execute_plan(&mut h.db, &h.ctx, id).unwrap().done, 1);
        assert!(!a.exists());
        assert_eq!(fs::read(&b).unwrap(), b"same");
        assert!(!h.drive.join("restore.txt.lmf").exists());
        let marker_c = crate::merged::read(&h.drive.join("other.txt.lmf")).unwrap();
        assert_eq!(marker_c.target, marker_b.source);
        h.scan(true);
        let expected = query::locate(&h.db, &b).unwrap().unwrap().chain;
        assert_eq!(
            crate::merged::resolve_target(&h.db, &marker_c).unwrap(),
            expected
        );
    }

    #[test]
    fn changed_keep_hash_after_approval_blocks_file_quarantine() {
        let mut h = Harness::new("changed-keep");
        let keep = h.drive.join("keep.txt");
        let copy = h.drive.join("copy.txt");
        for path in [&keep, &copy] {
            fs::write(path, b"same").unwrap();
        }
        h.scan(true);
        let created = create_plan(
            &mut h.db,
            &h.ctx,
            "cli",
            None,
            &[OpInput::Trash {
                path: copy.clone(),
                keep: Some(keep.clone()),
            }],
            false,
        )
        .unwrap();
        let id = created.id.unwrap();
        approve_plan(&mut h.db, id, false).unwrap();
        fs::write(&keep, b"else").unwrap();
        let report = execute_plan(&mut h.db, &h.ctx, id).unwrap();
        assert_eq!(report.done, 0);
        assert_eq!(fs::read(&copy).unwrap(), b"same");
        let op = plan_ops(&h.db, id).unwrap().remove(0);
        assert_eq!(op.state, OpState::Skipped);
        assert_eq!(
            op.reason.as_deref(),
            Some("残すファイルの中身が変わっています")
        );
    }

    #[test]
    fn move_and_trash_with_moved_keep_in_same_plan() {
        let mut h = Harness::new("moved-keep");
        let keep_src = h.drive.join("keep.txt");
        let keep_dst = h.drive.join("keep-moved.txt");
        let dup = h.drive.join("dup.txt");
        fs::write(&keep_src, b"same-body").unwrap();
        fs::write(&dup, b"same-body").unwrap();
        h.scan(true);

        let created = create_plan(
            &mut h.db,
            &h.ctx,
            "cli",
            None,
            &[
                OpInput::Move {
                    from: keep_src.clone(),
                    to: keep_dst.clone(),
                },
                OpInput::Trash {
                    path: dup.clone(),
                    keep: Some(keep_src.clone()),
                },
            ],
            false,
        )
        .unwrap();
        assert!(created.created);
        assert_eq!(created.summary.ok, 2);
        let id = created.id.unwrap();

        approve_plan(&mut h.db, id, false).unwrap();
        let report = execute_plan(&mut h.db, &h.ctx, id).unwrap();
        assert_eq!(report.state, PlanState::Done);
        assert_eq!(report.done, 2);
        assert!(!keep_src.exists() && keep_dst.exists() && !dup.exists());
    }

    #[test]
    fn cross_volume_move_copies_verifies_and_quarantines() {
        let h = Harness::new("cross-vol");
        let src = h.drive.join("src.bin");
        let dst = h.drive.join("dst.bin");
        fs::write(&src, b"verify-me-with-blake3").unwrap();

        let (q, warning) =
            apply::copy_verify_and_quarantine(99, 0, &src, &dst, Some(&h.drive), &h.data_dir)
                .unwrap();
        assert_eq!(warning, None);
        assert!(!src.exists());
        assert_eq!(fs::read(&dst).unwrap(), b"verify-me-with-blake3");
        assert_eq!(fs::read(&q).unwrap(), b"verify-me-with-blake3");
    }

    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    mod move_times {
        use super::*;
        use apply::timestamp_test::{FAIL_STAGING_CLEANUP, FAULT, FORCE_COPY};
        use std::time::{Duration, SystemTime, UNIX_EPOCH};

        #[test]
        fn copied_target_and_undo_retarget_the_marker() {
            let mut h = Harness::new("copy-linked-target");
            let keep = h.drive.join("keep");
            let copy = h.drive.join("copy");
            let folder = h.drive.join("folder");
            let moved = folder.join("new");
            fs::create_dir(&folder).unwrap();
            for path in [&keep, &copy] {
                fs::write(path, b"same").unwrap();
            }
            h.scan(true);
            publish_marker(&mut h, &copy, &keep);
            FORCE_COPY.set(true);
            let id = approve_and_move(&mut h, &keep, &moved);
            assert!(query::locate(&h.db, &moved).unwrap().is_none());
            h.scan(true);
            let marker_path = h.drive.join("copy.lmf");
            let marker = crate::merged::read(&marker_path).unwrap();
            assert_eq!(marker.target.relative_path, ["folder", "new"]);
            assert!(crate::merged::resolve_target(&h.db, &marker).is_ok());
            assert_eq!(undo_plan(&mut h.db, &h.ctx, id).unwrap().undone, 1);
            FORCE_COPY.set(false);
            h.scan(true);
            let marker = crate::merged::read(&marker_path).unwrap();
            assert_eq!(marker.target.relative_path, ["keep"]);
            assert!(crate::merged::resolve_target(&h.db, &marker).is_ok());
        }

        #[test]
        fn interrupted_copy_undo_removes_only_the_verified_original() {
            let mut h = Harness::new("undo-copy-recovery");
            let original = h.drive.join("original");
            let folder = h.drive.join("folder");
            let moved = folder.join("moved");
            fs::create_dir(&folder).unwrap();
            fs::write(&original, b"contents").unwrap();
            h.scan(true);
            FORCE_COPY.set(true);
            let id = approve_and_move(&mut h, &original, &moved);
            h.scan(true);

            let op = plan_ops(&h.db, id).unwrap().remove(0);
            super::super::linked_move::prepare_undo(&h.db, &op, &moved).unwrap();
            // コピーを正式名に置いた直後、古いコピーを取り除く前で停止。
            apply::copy_verify_and_publish(id, op.seq, &moved, &original).unwrap();
            super::super::linked_move::record_undo_copy(&h.db, &op, &original).unwrap();

            assert_eq!(undo_plan(&mut h.db, &h.ctx, id).unwrap().undone, 1);
            FORCE_COPY.set(false);
            assert_eq!(fs::read(&original).unwrap(), b"contents");
            assert!(!moved.exists());
        }

        fn old_time(index: u64) -> SystemTime {
            UNIX_EPOCH + Duration::new(1_500_000_000 + index, 123_456_700)
        }

        fn make_tree(root: &Path, links: bool) -> Vec<PathBuf> {
            fs::create_dir_all(root.join("nested")).unwrap();
            fs::create_dir(root.join("empty")).unwrap();
            fs::write(root.join("nested/fault.bin"), b"payload").unwrap();
            let mut paths = vec![
                PathBuf::from("nested/fault.bin"),
                PathBuf::from("nested"),
                PathBuf::from("empty"),
            ];
            #[cfg(unix)]
            if links {
                for (name, target) in [
                    ("relative", "nested/fault.bin"),
                    ("broken", "missing"),
                    ("external", "../../outside.bin"),
                ] {
                    std::os::unix::fs::symlink(target, root.join(name)).unwrap();
                    paths.push(PathBuf::from(name));
                }
            }
            #[cfg(not(unix))]
            let _ = links;
            paths.push(PathBuf::new());
            for (i, path) in paths.iter().enumerate() {
                apply::set_mtime(&root.join(path), old_time(i as u64)).unwrap();
            }
            paths
        }

        #[derive(Debug, PartialEq, Eq)]
        enum Content {
            File(Vec<u8>),
            Directory(Vec<std::ffi::OsString>),
            Link(PathBuf),
        }

        fn snapshot(root: &Path, paths: &[PathBuf]) -> Vec<(SystemTime, Content)> {
            paths
                .iter()
                .map(|path| {
                    let path = if path.as_os_str().is_empty() {
                        root.to_path_buf()
                    } else {
                        root.join(path)
                    };
                    let meta = fs::symlink_metadata(&path).unwrap();
                    let content = if meta.is_symlink() {
                        Content::Link(fs::read_link(&path).unwrap())
                    } else if meta.is_dir() {
                        let mut names: Vec<_> = fs::read_dir(&path)
                            .unwrap()
                            .map(|entry| entry.unwrap().file_name())
                            .collect();
                        names.sort();
                        Content::Directory(names)
                    } else {
                        Content::File(fs::read(&path).unwrap())
                    };
                    (meta.modified().unwrap(), content)
                })
                .collect()
        }

        fn plan_move(h: &mut Harness, src: &Path, dst: &Path) -> i64 {
            h.scan(true);
            let result = create_plan(
                &mut h.db,
                &h.ctx,
                "test",
                None,
                &[OpInput::Move {
                    from: src.into(),
                    to: dst.into(),
                }],
                false,
            )
            .unwrap();
            let id = result.id.unwrap_or_else(|| panic!("{:?}", result.checks));
            approve_plan(&mut h.db, id, false).unwrap();
            id
        }

        fn paths(h: &Harness) -> (PathBuf, PathBuf) {
            let from = h.drive.join("from");
            let to = h.drive.join("to");
            fs::create_dir(&from).unwrap();
            fs::create_dir(&to).unwrap();
            (from.join("subject"), to.join("subject"))
        }

        #[test]
        fn move_and_undo_keep_full_timestamps_and_contents() {
            for (force_copy, tree) in [(false, false), (false, true), (true, false), (true, true)] {
                let mut h = Harness::new("timestamp-round-trip");
                let (src, dst) = paths(&h);
                let external = h.drive.join("outside.bin");
                fs::write(&external, b"outside").unwrap();
                apply::set_mtime(&external, old_time(20)).unwrap();
                let external_paths = &[PathBuf::from("outside.bin")];
                let outside_before = snapshot(&h.drive, external_paths);
                let entries = if tree {
                    make_tree(&src, true)
                } else {
                    fs::write(&src, b"file").unwrap();
                    apply::set_mtime(&src, old_time(15)).unwrap();
                    vec![PathBuf::new()]
                };
                let before = snapshot(&src, &entries);
                let id = plan_move(&mut h, &src, &dst);
                FORCE_COPY.set(force_copy);
                let moved = execute_plan(&mut h.db, &h.ctx, id).unwrap();
                assert_eq!(moved.done, 1, "{:?}", get_plan(&h.db, id).unwrap());
                assert_eq!(
                    moved.touched_folders,
                    [src.parent().unwrap(), dst.parent().unwrap()]
                );
                assert!(!src.exists());
                assert_eq!(snapshot(&dst, &entries), before);
                assert_eq!(snapshot(&h.drive, external_paths), outside_before);
                let restored = undo_plan(&mut h.db, &h.ctx, id).unwrap();
                FORCE_COPY.set(false);
                assert_eq!(restored.undone, 1, "{restored:?}");
                assert_eq!(restored.touched_folders, moved.touched_folders);
                assert!(!dst.exists());
                assert_eq!(snapshot(&src, &entries), before);
                assert_eq!(snapshot(&h.drive, external_paths), outside_before);
                assert!(undo_plan(&mut h.db, &h.ctx, id).is_err());
            }
        }

        #[test]
        fn timestamp_failures_leave_source_and_record_reason_in_both_directions() {
            for undo in [false, true] {
                for step in [
                    "更新日時の取得",
                    "更新日時の設定",
                    "更新日時の再取得",
                    "丸め",
                ] {
                    let mut h = Harness::new("timestamp-failure");
                    let (src, dst) = paths(&h);
                    let entries = make_tree(&src, false);
                    let before = snapshot(&src, &entries);
                    let id = plan_move(&mut h, &src, &dst);
                    FORCE_COPY.set(true);
                    if undo {
                        assert_eq!(execute_plan(&mut h.db, &h.ctx, id).unwrap().done, 1);
                    }
                    FAULT.set(Some(step));
                    if undo {
                        let report = undo_plan(&mut h.db, &h.ctx, id).unwrap();
                        assert_eq!(report.undone, 0);
                        assert_eq!(report.skipped.len(), 1);
                    } else {
                        let report = execute_plan(&mut h.db, &h.ctx, id).unwrap();
                        assert_eq!(report.state, PlanState::Partial);
                        assert_eq!(report.failed, 1);
                    }
                    FAULT.set(None);
                    FORCE_COPY.set(false);
                    let (_, ops) = get_plan(&h.db, id).unwrap().unwrap();
                    let reason = ops[0].reason.as_deref().unwrap();
                    assert!(
                        reason.contains("nested") && reason.contains("fault.bin"),
                        "{reason}"
                    );
                    if step == "丸め" {
                        assert!(reason.contains("更新日時の照合"), "{reason}");
                        assert!(reason.contains("1500000000.123456700"), "{reason}");
                        assert!(reason.contains("1500000000.000000000"), "{reason}");
                    } else {
                        assert!(reason.contains(step) && reason.contains("I/O"), "{reason}");
                    }
                    let (original, absent) = if undo { (&dst, &src) } else { (&src, &dst) };
                    assert_eq!(snapshot(original, &entries), before);
                    assert_eq!(fs::read_dir(absent.parent().unwrap()).unwrap().count(), 0);
                }
            }
        }

        #[test]
        fn recovery_compares_child_timestamps_before_quarantine() {
            for mismatch in [false, true] {
                let mut h = Harness::new("timestamp-recovery");
                let (src, dst) = paths(&h);
                let entries = make_tree(&src, false);
                let before = snapshot(&src, &entries);
                let id = plan_move(&mut h, &src, &dst);
                plan::set_plan_state(&mut h.db, id, PlanState::Running, None).unwrap();
                plan::set_op_state(&mut h.db, id, 0, OpState::Running, None, None, None).unwrap();
                apply::copy_verify_and_publish(id, 0, &src, &dst).unwrap();
                if mismatch {
                    apply::set_mtime(&dst.join("nested/fault.bin"), old_time(30)).unwrap();
                }
                let published = snapshot(&dst, &entries);
                let reports = recover_running_plans(&mut h.db, &h.ctx).unwrap();
                assert_eq!(snapshot(&dst, &entries), published);
                if mismatch {
                    assert_eq!(reports[0].failed, 1);
                    assert_eq!(snapshot(&src, &entries), before);
                    let (_, ops) = get_plan(&h.db, id).unwrap().unwrap();
                    let reason = ops[0].reason.as_deref().unwrap();
                    assert!(
                        reason.contains("中断後") && reason.contains("fault.bin"),
                        "{reason}"
                    );
                    assert!(reason.contains("1500000030.123456700"), "{reason}");
                } else {
                    assert_eq!(reports[0].done, 1);
                    assert!(!src.exists());
                    assert_eq!(published, before);
                }
            }
        }

        #[test]
        fn published_copy_completes_both_directions_with_cleanup_warning() {
            let mut h = Harness::new("timestamp-cleanup-warning");
            let (src, dst) = paths(&h);
            fs::write(&src, b"payload").unwrap();
            let id = plan_move(&mut h, &src, &dst);
            let staging_name = format!(".lisa-moving-{id}-0-{}", std::process::id());
            let move_staging = dst.with_file_name(&staging_name);
            let undo_staging = src.with_file_name(&staging_name);
            FORCE_COPY.set(true);
            FAIL_STAGING_CLEANUP.set(true);
            let applied = execute_plan(&mut h.db, &h.ctx, id).unwrap();
            assert_eq!(applied.done, 1);
            assert!(!src.exists());
            assert_eq!(fs::read(&dst).unwrap(), b"payload");
            let op = plan::plan_ops(&h.db, id).unwrap().remove(0);
            let reason = op.reason.unwrap();
            assert!(reason.contains("警告") && reason.contains(move_staging.to_str().unwrap()));
            assert_eq!(fs::read_dir(&move_staging).unwrap().count(), 0);

            let restored = undo_plan(&mut h.db, &h.ctx, id).unwrap();
            FAIL_STAGING_CLEANUP.set(false);
            FORCE_COPY.set(false);
            assert_eq!(restored.undone, 1);
            assert!(!dst.exists());
            assert_eq!(fs::read(&src).unwrap(), b"payload");
            let op = plan::plan_ops(&h.db, id).unwrap().remove(0);
            let reason = op.reason.unwrap();
            assert!(reason.contains(move_staging.to_str().unwrap()));
            assert!(reason.contains(undo_staging.to_str().unwrap()));
            assert_eq!(fs::read_dir(&undo_staging).unwrap().count(), 0);
        }

        #[test]
        fn copy_leaves_unowned_staging_and_destination_items_untouched() {
            let h = Harness::new("timestamp-collisions");
            let (src, dst) = paths(&h);
            fs::write(&src, b"source").unwrap();
            let staging = dst
                .parent()
                .unwrap()
                .join(format!(".lisa-moving-1-0-{}", std::process::id()));
            fs::write(&staging, b"existing staging entry").unwrap();
            let reason = apply::copy_verify_and_publish(1, 0, &src, &dst).unwrap_err();
            assert!(reason.contains("一時フォルダの作成"), "{reason}");
            assert!(reason.contains(staging.to_str().unwrap()), "{reason}");
            assert!(
                reason.contains("前回の残りを確認して削除してから再実行"),
                "{reason}"
            );
            assert_eq!(fs::read(&staging).unwrap(), b"existing staging entry");
            assert!(!dst.exists());

            fs::remove_file(&staging).unwrap();
            fs::write(&dst, b"existing destination").unwrap();
            let reason = apply::copy_verify_and_publish(1, 0, &src, &dst).unwrap_err();
            assert!(reason.contains("正式名への確定"), "{reason}");
            assert_eq!(fs::read(&dst).unwrap(), b"existing destination");
            assert_eq!(fs::read(&src).unwrap(), b"source");
            assert!(!staging.exists());
        }
    }
}
