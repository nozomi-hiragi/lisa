use super::*;
use crate::db::{HASH_CHANGED, HASH_SCHEDULED, HASH_VISIBLE, HashQueueSeed};
use crate::listing::{Listing, read_dir};
use crate::volumes::Volume;
use std::fs;

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("lisa-hash-queue-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn add_drive(db: &mut Db, id: &str, mount: &Path) -> i64 {
    let device = db
        .this_device("test", "test", std::env::consts::OS)
        .unwrap();
    db.upsert_drive(
        &device,
        &Volume {
            id: Some(id.into()),
            label: id.into(),
            fs: String::new(),
            mount_path: mount.to_path_buf(),
            capacity: None,
            available: None,
            removable: false,
            network: false,
            read_only: false,
            case_sensitive: None,
            hidden: false,
        },
    )
    .unwrap();
    db.drive_root(id).unwrap().unwrap().id
}

fn save(db: &mut Db, drive: &str, mount: &Path, relative: &str) {
    let path = mount.join(relative);
    let listing = read_dir(&path).map_err(|error| error.to_string());
    db.save_listing(drive, mount, &path, &listing).unwrap();
}

fn drain(db: &mut Db, mount: &Path) -> Vec<PathBuf> {
    let mut folders = Vec::new();
    for _ in 0..20 {
        let batch = hash_queue_batch(db, &[("test", mount)], HASH_SCHEDULED, &Pace::FREE).unwrap();
        folders.extend(batch.folders);
        if batch.picked == 0 {
            assert_eq!(
                db.hash_queue_counts(&["test"], db::now_ms()).unwrap().total,
                0
            );
            return folders;
        }
    }
    panic!("キューが完了しない");
}

fn one_file() -> (TestDir, Db, i64) {
    let temp = TestDir::new();
    fs::write(temp.0.join("file"), "contents").unwrap();
    let mut db = Db::open_in_memory().unwrap();
    let root = add_drive(&mut db, "test", &temp.0);
    let listing = Ok(read_dir(&temp.0).unwrap());
    assert_eq!(
        db.save_initial_listings(
            "test",
            &temp.0,
            db.listing_version().unwrap(),
            [(temp.0.as_path(), &listing)],
            Duration::from_millis(20)
        )
        .unwrap()
        .len(),
        1
    );
    (temp, db, root)
}

#[test]
fn failure_classification() {
    use io::ErrorKind::*;
    for kind in [
        WouldBlock,
        ResourceBusy,
        TimedOut,
        ConnectionReset,
        ConnectionAborted,
        NotConnected,
        BrokenPipe,
        HostUnreachable,
        NetworkUnreachable,
        NetworkDown,
    ] {
        assert!(
            matches!(
                HashFailure::of(&io::Error::from(kind)),
                HashFailure::Temporary(_)
            ),
            "{kind:?}"
        );
    }
    #[cfg(unix)]
    for code in [
        libc::EAGAIN,
        libc::EBUSY,
        libc::ESTALE,
        libc::EHOSTDOWN,
        libc::ETIMEDOUT,
        libc::ENOTCONN,
        libc::EIO,
    ] {
        assert!(
            matches!(
                HashFailure::of(&io::Error::from_raw_os_error(code)),
                HashFailure::Temporary(_)
            ),
            "{code}"
        );
    }
    #[cfg(windows)]
    for code in [32, 33, 53, 64, 121, 55] {
        assert!(
            matches!(
                HashFailure::of(&io::Error::from_raw_os_error(code)),
                HashFailure::Temporary(_)
            ),
            "{code}"
        );
    }
    assert_eq!(
        HashFailure::of(&io::Error::from(NotFound)),
        HashFailure::Missing
    );
    assert_eq!(
        HashFailure::of(&io::Error::from(Interrupted)),
        HashFailure::Interrupted
    );
    assert_eq!(
        HashFailure::of(&io::Error::from(PermissionDenied)),
        HashFailure::Permanent("Permission".into())
    );
}

fn fail(db: &mut Db, mount: &Path, now: i64, failure: HashFailure) -> i64 {
    let item = db
        .pick_hash_queue(&["test"], now, 32, HASH_SCHEDULED)
        .unwrap()
        .into_iter()
        .find(|item| db.entry(item.entry_id).unwrap().unwrap().kind == Kind::File)
        .unwrap();
    let id = item.entry_id;
    let job = Job {
        item,
        entry: db.entry(id).unwrap().unwrap(),
        path: mount.join("file"),
        mount: mount.to_path_buf(),
    };
    db.ensure_hash_pending_index().unwrap();
    db.begin().unwrap();
    finish_jobs(db, &[job], &[(0, Outcome::File(Err(failure)))], now).unwrap();
    db.commit().unwrap();
    id
}

#[test]
fn retries_wait_and_eighth_failure_aggregates_parent() {
    let (temp, mut db, root) = one_file();
    let mut now = db::now_ms();
    for (attempt, delay) in [
        60_000, 300_000, 1_800_000, 7_200_000, 21_600_000, 86_400_000, 86_400_000,
    ]
    .into_iter()
    .enumerate()
    {
        let id = fail(
            &mut db,
            &temp.0,
            now,
            HashFailure::Temporary("locked".into()),
        );
        assert_eq!(db.entry(id).unwrap().unwrap().error, None);
        assert_eq!(
            db.next_hash_due_at(&["test"], now).unwrap(),
            Some(now + delay)
        );
        assert_eq!(db.hash_queue_counts(&["test"], now).unwrap().waiting, 1);
        assert!(
            !db.pick_hash_queue(&["test"], now + delay - 1, 32, HASH_SCHEDULED)
                .unwrap()
                .iter()
                .any(|job| job.entry_id == id)
        );
        now += delay;
        let job = db
            .pick_hash_queue(&["test"], now, 32, HASH_SCHEDULED)
            .unwrap()
            .into_iter()
            .find(|job| job.entry_id == id)
            .unwrap();
        assert_eq!(
            (job.attempts, job.delay_ms, job.reason.as_str()),
            (attempt as u32 + 1, delay, "retry")
        );
        assert!(db.drive_root("test").unwrap().unwrap().hash.is_none());
    }
    let id = fail(
        &mut db,
        &temp.0,
        now,
        HashFailure::Temporary("locked".into()),
    );
    assert_eq!(
        db.entry(id).unwrap().unwrap().error.as_deref(),
        Some("locked")
    );
    assert!(db.entry(root).unwrap().unwrap().hash.is_some());
    assert_eq!(db.hash_queue_counts(&["test"], now).unwrap().total, 0);
}

#[test]
fn interruption_preserves_queue_and_missing_returns_hint() {
    let (temp, mut db, _) = one_file();
    let before = db
        .pick_hash_queue(&["test"], i64::MAX, 32, HASH_SCHEDULED)
        .unwrap();
    let pace = Pace {
        step: &|_| false,
        ..Pace::FREE
    };
    let batch = hash_queue_batch(&mut db, &[("test", &temp.0)], HASH_SCHEDULED, &pace).unwrap();
    assert!(batch.interrupted);
    assert_eq!(
        before,
        db.pick_hash_queue(&["test"], i64::MAX, 32, HASH_SCHEDULED)
            .unwrap()
    );

    fs::remove_file(temp.0.join("file")).unwrap();
    let batch =
        hash_queue_batch(&mut db, &[("test", &temp.0)], HASH_SCHEDULED, &Pace::FREE).unwrap();
    assert_eq!(batch.hints, [temp.0.clone()]);
    assert_eq!(
        db.hash_queue_counts(&["test"], db::now_ms())
            .unwrap()
            .waiting,
        1
    );
    save(&mut db, "test", &temp.0, "");
    assert_eq!(
        db.hash_queue_counts(&["test"], db::now_ms()).unwrap().total,
        1
    );
    assert_eq!(drain(&mut db, &temp.0), [temp.0.clone()]);
}

#[test]
fn visible_changed_and_scheduled_keep_or_reset_backoff() {
    let (temp, mut db, root) = one_file();
    let now = db::now_ms();
    let id = fail(
        &mut db,
        &temp.0,
        now,
        HashFailure::Temporary("locked".into()),
    );
    let jobs = |db: &Db| {
        db.pick_hash_queue(&["test"], i64::MAX, 32, HASH_SCHEDULED)
            .unwrap()
    };
    let before = jobs(&db);
    let mut seed = HashQueueSeed::default();
    assert!(db.seed_hash_queue("test", &mut seed).unwrap());
    assert_eq!(before, jobs(&db));

    db.promote_hash_queue(root).unwrap();
    let items = jobs(&db);
    let visible = &items[0];
    assert_eq!(
        (visible.entry_id, visible.priority, visible.reason.as_str()),
        (id, HASH_VISIBLE, "visible")
    );
    assert_eq!(
        (
            visible.attempts,
            visible.delay_ms,
            visible.due_at,
            &visible.last_error
        ),
        (0, 0, 0, &None)
    );
    fail(
        &mut db,
        &temp.0,
        now,
        HashFailure::Temporary("locked".into()),
    );
    db.promote_hash_queue(root).unwrap();
    assert_eq!(jobs(&db)[0].reason, "visible");
    fail(
        &mut db,
        &temp.0,
        now,
        HashFailure::Temporary("locked".into()),
    );

    fs::write(temp.0.join("file"), "different length").unwrap();
    save(&mut db, "test", &temp.0, "");
    let items = jobs(&db);
    let changed = &items[0];
    assert_eq!(
        (
            changed.entry_id,
            changed.priority,
            changed.attempts,
            changed.delay_ms,
            changed.due_at,
            &changed.last_error
        ),
        (id, HASH_VISIBLE, 0, 0, 0, &None)
    );
    fs::write(temp.0.join("new"), "new").unwrap();
    save(&mut db, "test", &temp.0, "");
    assert!(
        jobs(&db)
            .iter()
            .any(|job| job.priority == HASH_CHANGED && job.reason == "changed")
    );
    drain(&mut db, &temp.0);
}

#[test]
fn seed_old_database_in_chunks_and_resume_after_reopen() {
    let temp = TestDir::new();
    let path = temp.0.join("test.db");
    {
        let mut db = Db::open(&path).unwrap();
        let root = add_drive(&mut db, "test", &temp.0);
        db.conn().execute(
            "WITH RECURSIVE ids(i) AS (VALUES(1) UNION ALL SELECT i + 1 FROM ids WHERE i < 5001)
             INSERT INTO entry(drive_id,parent_id,pos,name,name_nfc,kind,size,cloud_only,other_volume,status,listed,hash,error)
             SELECT 'test',?1,i,CAST(i AS BLOB),i,'file',0,0,0,'unloaded',0,
                 CASE WHEN i = 1 THEN zeroblob(32) END,
                 CASE WHEN i = 2 THEN 'Io' END FROM ids", [root],
        ).unwrap();
        db.conn().execute_batch("DROP TABLE hash_queue").unwrap();
    }
    let mut db = Db::open(&path).unwrap();
    assert_eq!(db.hash_queue_counts(&["test"], 0).unwrap().total, 0);
    let mut cursor = HashQueueSeed::default();
    assert!(!db.seed_hash_queue("test", &mut cursor).unwrap());
    assert!(db.hash_queue_counts(&["test"], 0).unwrap().total > 0);
    while !db.seed_hash_queue("test", &mut cursor).unwrap() {}
    assert_eq!(db.hash_queue_counts(&["test"], 0).unwrap().total, 4999);
    drop(db);
    let mut db = Db::open(&path).unwrap();
    let first = db.pick_hash_queue(&["test"], 0, 1, HASH_SCHEDULED).unwrap();
    db.remove_hash_job(first[0].entry_id).unwrap();
    assert!(
        db.seed_hash_queue("test", &mut HashQueueSeed::default())
            .unwrap()
    );
    assert_eq!(db.hash_queue_counts(&["test"], 0).unwrap().total, 4998);
}

#[test]
fn lock_retries_three_times_and_recovers_after_release() {
    let temp = TestDir::new();
    let path = temp.0.join("test.db");
    let locker = Db::open(&path).unwrap();
    let mut db = Db::open(&path).unwrap();
    let root = add_drive(&mut db, "test", &temp.0);
    db.conn().busy_timeout(Duration::ZERO).unwrap();
    locker.conn().execute_batch("BEGIN IMMEDIATE").unwrap();
    let mut calls = 0;
    let result = db::retry_busy(|| {
        calls += 1;
        db.conn().execute("DELETE FROM hash_queue", [])
    });
    assert_eq!(
        result.unwrap_err().sqlite_error_code(),
        Some(db::ErrorCode::DatabaseBusy)
    );
    assert_eq!(calls, 4);
    assert!(db.conn().is_autocommit());
    locker.conn().execute_batch("ROLLBACK").unwrap();
    db.promote_hash_queue(root).unwrap();
}

#[test]
fn batch_matches_walk_across_tree_updates_and_drive_disconnect() {
    let temp = TestDir::new();
    let mount = &temp.0;
    fs::create_dir_all(mount.join("a/deep")).unwrap();
    fs::create_dir_all(mount.join("empty")).unwrap();
    fs::create_dir_all(mount.join("special")).unwrap();
    fs::write(mount.join("a/deep/file"), "nested").unwrap();
    fs::write(mount.join("file"), "root").unwrap();
    fs::write(mount.join("special/cloud"), "placeholder").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("cloud", mount.join("special/link")).unwrap();
    let special = || -> Result<Listing, String> {
        let mut listing = read_dir(&mount.join("special")).unwrap();
        for entry in &mut listing.entries {
            if entry.kind == Kind::File {
                entry.cloud_only = true;
            }
        }
        Ok(listing)
    };
    let mut queued = Db::open_in_memory().unwrap();
    let mut walked = Db::open_in_memory().unwrap();
    let other = TestDir::new();
    add_drive(&mut queued, "other", &other.0);
    fs::write(other.0.join("file"), "external").unwrap();
    save(&mut queued, "other", &other.0, "");
    for db in [&mut queued, &mut walked] {
        add_drive(db, "test", mount);
        for relative in ["", "a", "a/deep", "empty"] {
            save(db, "test", mount, relative);
        }
        db.save_listing("test", mount, &mount.join("special"), &special())
            .unwrap();
    }
    let compare = |queued: &mut Db, walked: &mut Db| {
        let folders = drain(queued, mount);
        hash_drive(
            walked,
            "test",
            mount,
            HASH_THREADS,
            &mut |_| {},
            &Pace::FREE,
        )
        .unwrap();
        let queued_hash = queued.drive_root("test").unwrap().unwrap().hash.unwrap();
        let walked_hash = walked.drive_root("test").unwrap().unwrap().hash.unwrap();
        assert_eq!(queued_hash, walked_hash);
        assert_eq!(
            queued.tree(&queued_hash).unwrap(),
            walked.tree(&walked_hash).unwrap()
        );
        assert_eq!(walked.hash_queue_counts(&["test"], 0).unwrap().total, 0);
        folders
    };
    let folders = compare(&mut queued, &mut walked);
    for relative in ["a/deep", "a", "empty", "special"] {
        assert!(folders.contains(&mount.join(relative)));
    }
    assert_eq!(folders.last(), Some(mount));
    assert_eq!(queued.hash_queue_counts(&["other"], 0).unwrap().total, 2);
    let batch = hash_queue_batch(
        &mut queued,
        &[("other", &other.0)],
        HASH_SCHEDULED,
        &Pace::FREE,
    )
    .unwrap();
    assert_eq!(batch.folders, [other.0.clone()]);

    fs::write(mount.join("a/deep/file"), "updated size").unwrap();
    for db in [&mut queued, &mut walked] {
        save(db, "test", mount, "a/deep");
    }
    compare(&mut queued, &mut walked);

    fs::remove_file(mount.join("a/deep/file")).unwrap();
    for db in [&mut queued, &mut walked] {
        save(db, "test", mount, "a/deep");
    }
    compare(&mut queued, &mut walked);

    fs::rename(mount.join("file"), mount.join("renamed")).unwrap();
    fs::remove_dir_all(mount.join("a")).unwrap();
    for db in [&mut queued, &mut walked] {
        save(db, "test", mount, "");
    }
    compare(&mut queued, &mut walked);
}

#[test]
fn changed_fingerprint_is_reconciled_before_retry() {
    let (temp, mut db, root) = one_file();
    let file = temp.0.join("file");
    let step = |bytes| {
        if bytes != 0 {
            fs::write(&file, "changed during read").unwrap();
        }
        true
    };
    let pace = Pace {
        step: &step,
        ..Pace::FREE
    };
    let batch = hash_queue_batch(&mut db, &[("test", &temp.0)], HASH_SCHEDULED, &pace).unwrap();
    assert_eq!(batch.hints, [temp.0.clone()]);
    let child = &db.children(root).unwrap()[0];
    assert_eq!((child.hash, &child.error), (None, &None));
    assert_eq!(
        db.hash_queue_counts(&["test"], db::now_ms())
            .unwrap()
            .waiting,
        1
    );
    save(&mut db, "test", &temp.0, "");
    drain(&mut db, &temp.0);
    assert_eq!(
        db.children(root).unwrap()[0].hash,
        Some(hash_file(&file).unwrap())
    );
}

#[test]
fn manual_retry_reopens_after_a_permanent_failure() {
    let (temp, mut db, root) = one_file();
    let id = fail(
        &mut db,
        &temp.0,
        db::now_ms(),
        HashFailure::Permanent("Permission".into()),
    );
    assert_eq!(
        db.children(root).unwrap()[0].error.as_deref(),
        Some("Permission")
    );
    let now = db::now_ms();
    let version = db.listing_version().unwrap();
    assert!(
        db.defer_refreshed_hash(id, version, "locked", now - 60_000)
            .unwrap()
    );
    assert!(db.entry(root).unwrap().unwrap().hash.is_none());
    let jobs = db
        .pick_hash_queue(&["test"], now, 32, HASH_SCHEDULED)
        .unwrap();
    assert_eq!(
        (jobs[0].due_at, jobs[0].attempts, jobs[0].reason.as_str()),
        (now, 1, "retry")
    );
    drain(&mut db, &temp.0);
    assert!(db.children(root).unwrap()[0].hash.is_some());
    assert!(db.entry(root).unwrap().unwrap().hash.is_some());
}

#[test]
fn manual_success_clears_file_and_folder_jobs() {
    let (temp, mut db, root) = one_file();
    let file = &db.children(root).unwrap()[0];
    let hash = hash_file(&temp.0.join("file")).unwrap();
    assert!(
        db.save_refreshed_hash(file, db.listing_version().unwrap(), Ok(&hash))
            .unwrap()
    );
    assert_eq!(db.hash_queue_counts(&["test"], 0).unwrap().total, 1);
    assert!(db.aggregate_refreshed_folder(root).unwrap());
    assert_eq!(db.hash_queue_counts(&["test"], 0).unwrap().total, 0);
}

#[test]
fn replacing_mount_during_batch_preserves_missing_files() {
    let (temp, mut db, _) = one_file();
    let path = temp.0.join("mount");
    fs::create_dir(&path).unwrap();
    fs::rename(temp.0.join("file"), path.join("file")).unwrap();
    let before = db
        .pick_hash_queue(&["test"], i64::MAX, 32, HASH_SCHEDULED)
        .unwrap();
    let changed = std::sync::Once::new();
    let step = |_| {
        changed.call_once(|| {
            fs::rename(&path, temp.0.join("old")).unwrap();
            fs::create_dir(&path).unwrap();
        });
        true
    };
    let pace = Pace {
        step: &step,
        ..Pace::FREE
    };
    let result = hash_queue_batch(&mut db, &[("test", &path)], HASH_SCHEDULED, &pace).unwrap();
    assert!(result.interrupted);
    assert!(result.hints.is_empty());
    assert_eq!(
        db.pick_hash_queue(&["test"], i64::MAX, 32, HASH_SCHEDULED)
            .unwrap(),
        before
    );
}

#[test]
fn mount_identity_rejects_a_different_volume_serial() {
    let temp = TestDir::new();
    let (volume, file) = mount_identity(&temp.0).unwrap();
    assert!(mount_matches(&temp.0, Some((volume, file))));
    assert!(!mount_matches(&temp.0, Some((volume ^ 1, file))));
    assert!(!mount_matches(&temp.0, None));
}

#[test]
fn missing_file_returns_with_same_attributes_after_restart() {
    let temp = TestDir::new();
    let mount = temp.0.join("mount");
    fs::create_dir(&mount).unwrap();
    let path = temp.0.join("db");
    let mut db = Db::open(&path).unwrap();
    let root = add_drive(&mut db, "test", &mount);
    fs::write(mount.join("file"), b"same").unwrap();
    save(&mut db, "test", &mount, "");
    fs::rename(mount.join("file"), temp.0.join("file")).unwrap();
    let batch =
        hash_queue_batch(&mut db, &[("test", &mount)], HASH_SCHEDULED, &Pace::FREE).unwrap();
    assert_eq!(batch.hints, [mount.clone()]);
    drop(db);
    fs::rename(temp.0.join("file"), mount.join("file")).unwrap();
    let mut db = Db::open(&path).unwrap();
    assert_eq!(
        db.hash_queue_counts(&["test"], db::now_ms())
            .unwrap()
            .waiting,
        1
    );
    let listing = Ok(read_dir(&mount).unwrap());
    assert!(matches!(
        db.save_listing_at("test", &mount, &mount, &listing, None)
            .unwrap(),
        db::ListingSave::Unchanged
    ));
    drain(&mut db, &mount);
    assert!(db.entry(root).unwrap().unwrap().hash.is_some());
}

#[test]
fn incomplete_large_folder_does_not_read_all_children() {
    let temp = TestDir::new();
    let mut db = Db::open_in_memory().unwrap();
    let root = add_drive(&mut db, "test", &temp.0);
    for n in 0..256 {
        fs::write(temp.0.join(format!("{n:03}")), b"content").unwrap();
    }
    save(&mut db, "test", &temp.0, "");
    // 後続バッチの子を全件読もうとすると、name_nfc の型が合わずに失敗する。
    db.conn()
        .execute(
            "UPDATE entry SET name_nfc = X'323535' WHERE name_nfc = '255'",
            [],
        )
        .unwrap();
    let batch =
        hash_queue_batch(&mut db, &[("test", &temp.0)], HASH_SCHEDULED, &Pace::FREE).unwrap();
    assert!(batch.files.len() >= 31);
    assert!(batch.folders.is_empty());
    assert!(db.entry(root).unwrap().unwrap().hash.is_none());
    db.conn()
        .execute(
            "UPDATE entry SET name_nfc = '255' WHERE name_nfc = X'323535'",
            [],
        )
        .unwrap();
    assert_eq!(drain(&mut db, &temp.0), [temp.0.clone()]);
}
