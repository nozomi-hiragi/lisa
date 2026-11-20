//! 整理（ファイルを動かす・フォルダを作る・隔離フォルダへ移す・取り消す）の芯（`docs/organize.md`）。
//!
//! - 計画を作って DB（`plan`、`plan_op`）に残すところまでは、ファイルに一切触らない。
//! - 人が許可した計画だけを `mkdir` → `move` → `trash` の順に 1 件ずつ記録しながら実行する。
//! - 消す操作はまず同じボリュームの隔離フォルダ（`<data_dir>/trash` または `<ドライブ>/.lisa-trash`）へ移す。
//! - `entry` の表には直接書かず、触ったフォルダの一覧（`touched_folders`）を返す。

mod apply;
mod check;
mod plan;
mod quarantine;

use std::path::PathBuf;

use crate::catalog::{self, CatalogSet, CatalogSettings};

pub use apply::{
    ApplyReport, UndoReport, UndoSkipped, execute_plan, recover_running_plans, touched_folders,
    undo_plan,
};
pub use plan::{
    CheckStatus, CreateResult, OpCheck, OpInput, OpKind, OpState, Plan, PlanOp, PlanState,
    PlanSummary, approve_plan, cancel_plan, create_plan, expire_stale_plans, expire_stale_plans_at,
    get_plan, list_plans, plan_ops, reject_plan,
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
}

impl Context {
    pub fn new(data_dir: PathBuf, catalogs: CatalogSet) -> Self {
        Self {
            data_dir,
            catalogs,
            home_dir: None,
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
    use std::path::{Component, Path};

    if new_name.is_empty() {
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
    let parent = from
        .parent()
        .filter(|p| from.is_absolute() && p.is_absolute())
        .ok_or("対象の絶対パスと親フォルダが必要です")?;
    Ok(parent.join(new_name))
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

#[cfg(unix)]
fn same_file_identity(from: &std::path::Path, to: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let (Ok(from), Ok(to)) = (
        std::fs::symlink_metadata(from),
        std::fs::symlink_metadata(to),
    ) else {
        return false;
    };
    (from.dev(), from.ino()) == (to.dev(), to.ino()) && from.ino() != 0
}

#[cfg(windows)]
fn same_file_identity(from: &std::path::Path, to: &std::path::Path) -> bool {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO, FileIdInfo,
        GetFileInformationByHandleEx,
    };

    let identity = |path| {
        let file = std::fs::OpenOptions::new()
            .access_mode(0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
            .ok()?;
        // SAFETY: 開いたハンドルと FILE_ID_INFO 用の書き込み先は呼び出し中有効。
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
    };
    identity(from).is_some_and(|id| Some(id) == identity(to))
}

#[cfg(not(any(unix, windows)))]
fn same_file_identity(_from: &std::path::Path, _to: &std::path::Path) -> bool {
    false
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
            h.scan(true);
            let report = undo_plan(&mut h.db, &h.ctx, id).unwrap();
            assert_eq!(report.undone, 1, "{report:?}");
            assert_names("a.txt");
            assert_eq!(fs::read(&lower).unwrap(), b"original");
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
        assert_eq!(saved_op.reason.as_deref(), Some(undo.skipped[0].reason.as_str()));
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

        let q = apply::copy_verify_and_quarantine(99, 0, &src, &dst, Some(&h.drive), &h.data_dir)
            .unwrap();
        assert!(!src.exists());
        assert_eq!(fs::read(&dst).unwrap(), b"verify-me-with-blake3");
        assert_eq!(fs::read(&q).unwrap(), b"verify-me-with-blake3");
    }
}
