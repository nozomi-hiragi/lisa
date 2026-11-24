use std::fs::{self, Metadata};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime};

use lisa_core::db::{Db, ErrorCode, ListingSave};
use lisa_core::hash;
use lisa_core::listing::{self, Kind};

use super::{Message, RefreshRequest, Request, Worker, readable};

const RETRY_WAIT: Duration = Duration::from_millis(25);

enum Job {
    List(PathBuf),
    Hash(PathBuf),
    Aggregate(PathBuf),
}

enum Failure {
    Retry(String),
    Permanent(String),
    Superseded,
    Yield,
}

impl From<lisa_core::db::Error> for Failure {
    fn from(error: lisa_core::db::Error) -> Self {
        if matches!(
            error.sqlite_error_code(),
            Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
        ) {
            Self::Retry(error.to_string())
        } else {
            Self::Permanent(error.to_string())
        }
    }
}

fn fingerprint(meta: &Metadata) -> (u64, Option<SystemTime>, Option<(u64, u64, i64, i64)>) {
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt;
        Some((meta.dev(), meta.ino(), meta.ctime(), meta.ctime_nsec()))
    };
    #[cfg(not(unix))]
    let identity = None;
    (meta.len(), meta.modified().ok(), identity)
}

pub(in crate::scan) fn directory_identity(meta: &Metadata) -> Option<(u64, u64)> {
    if !meta.is_dir() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some((meta.dev(), meta.ino()))
    }
    #[cfg(not(unix))]
    {
        Some((0, 0))
    }
}

impl Worker {
    pub(super) fn run_manual_queue(&mut self) -> bool {
        loop {
            if self.control.closed.load(Ordering::Relaxed) {
                return false;
            }
            let request = {
                let mut queue = self.control.manual.lock().expect("手動更新");
                let request = queue.pending.pop_front();
                queue.active = request.clone();
                queue.processed = Default::default();
                self.update_pending_flags(&queue);
                request
            };
            let Some(request) = request else { return true };
            if !self.finish_refresh(request) {
                return false;
            }
        }
    }

    fn finish_refresh(&mut self, request: RefreshRequest) -> bool {
        let error = self.run_refresh(&request);
        if let Some((_, reply)) = &request.created {
            let _ = reply.send(error.clone());
        }
        let sent = self.send(Message::RefreshFinished { request, error });
        self.wake();
        sent
    }

    /// 実行中の更新を残し、作成済みの親一覧だけ先に確定する。
    fn service_created(&mut self) -> bool {
        while self.control.created_pending.load(Ordering::Relaxed) {
            let request = {
                let mut queue = self.control.manual.lock().expect("手動更新");
                let request = queue.pending.pop_front().expect("作成済みの一覧要求");
                self.update_pending_flags(&queue);
                request
            };
            if !self.finish_refresh(request) {
                return false;
            }
            if let Some(db) = &self.db {
                let _ = db.set_busy_timeout(RETRY_WAIT);
            }
        }
        true
    }

    fn update_pending_flags(&self, queue: &super::ManualQueue) {
        self.control
            .manual_pending
            .store(!queue.pending.is_empty(), Ordering::Relaxed);
        self.control.created_pending.store(
            queue
                .pending
                .front()
                .is_some_and(|request| request.created.is_some()),
            Ordering::Relaxed,
        );
    }

    fn run_refresh(&mut self, request: &RefreshRequest) -> Option<String> {
        let target = &request.target;
        let Some(index) = self.root_of(&target.path) else {
            return Some("更新対象のドライブを確認できませんでした".into());
        };
        let root = &self.roots[index];
        if root.drive_id.as_ref() != Some(&target.drive_id)
            || self.is_skipped(&target.path)
            || target
                .path
                .components()
                .any(|part| part == Component::ParentDir)
        {
            return Some("ローカルドライブの更新対象を確認できませんでした".into());
        }
        let mount = root.mount.clone();
        if root.mount_identity.is_none()
            || fs::symlink_metadata(&mount)
                .ok()
                .as_ref()
                .and_then(directory_identity)
                != root.mount_identity
        {
            return Some("ドライブの接続を確認できませんでした".into());
        }
        #[cfg(all(windows, not(test)))]
        if !crate::scan::scan_volumes().iter().any(|volume| {
            volume.mount_path == mount && volume.id.as_ref() == Some(&target.drive_id)
        }) {
            return Some("ドライブの接続を確認できませんでした".into());
        }
        if !self.send(Message::RefreshStarted(request.clone())) {
            return None;
        }
        self.wake();
        let Some(db) = &mut self.db else {
            return Some("DB を開けませんでした".into());
        };
        if let Err(error) = db.commit().and_then(|_| db.set_busy_timeout(RETRY_WAIT)) {
            return Some(error.to_string());
        }
        let error = self.refresh_jobs(request, &mount, root.mount_identity);
        if let Some(db) = &self.db {
            let _ = db.set_busy_timeout(Duration::from_secs(5));
        }
        error
    }

    fn refresh_jobs(
        &mut self,
        request: &RefreshRequest,
        mount: &Path,
        mount_identity: Option<(u64, u64)>,
    ) -> Option<String> {
        let target = &request.target;
        let first = if target.subtree {
            &target.path
        } else {
            target.path.parent()?
        };
        let mut jobs = Vec::new();
        for ancestor in first
            .ancestors()
            .skip(1)
            .take_while(|path| path.starts_with(mount))
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            if request.created.is_none() {
                jobs.push(Job::Aggregate(ancestor.to_path_buf()));
            }
        }
        jobs.push(Job::List(first.to_path_buf()));
        let mut error = None;
        while let Some(job) = jobs.pop() {
            if self.control.closed.load(Ordering::Relaxed) {
                break;
            }
            if request.created.is_none() && !self.service_created() {
                break;
            }
            if fs::symlink_metadata(mount)
                .ok()
                .as_ref()
                .and_then(directory_identity)
                != mount_identity
            {
                return Some("手動更新中にドライブの接続が変わりました".into());
            }
            self.control.apply_priority();
            for attempt in 0..=3 {
                match self.refresh_step(request, mount, &job, &mut jobs) {
                    Ok(()) | Err(Failure::Superseded) => break,
                    Err(Failure::Yield) => {
                        jobs.push(job);
                        break;
                    }
                    Err(Failure::Retry(_)) if attempt < 3 => std::thread::sleep(RETRY_WAIT),
                    Err(Failure::Permanent(reason) | Failure::Retry(reason)) => {
                        if error.is_none() {
                            error = Some(reason);
                        }
                        break;
                    }
                }
            }
            // 一覧要求はハッシュや再帰読み込みを伴わない一段だけを先に出す。
            for _ in 0..8 {
                match self.requests.try_recv() {
                    Ok(Request::Visible(path)) => {
                        self.hints.remove(&path);
                        if let Some(index) = self.root_of(&path)
                            && !self.is_skipped(&path)
                        {
                            self.store_reread(index, &path, false);
                            self.hints.insert(path);
                            self.dirty = true;
                        }
                        self.control.urgent.fetch_sub(1, Ordering::Relaxed);
                    }
                    Ok(Request::ManualWake) => {}
                    Ok(request @ (Request::Hint(_) | Request::DebugForceChanged(_))) => {
                        if !self.handle(request) {
                            return error;
                        }
                    }
                    Err(_) => break,
                }
            }
        }
        error
    }

    fn refresh_step(
        &mut self,
        request: &RefreshRequest,
        mount: &Path,
        job: &Job,
        jobs: &mut Vec<Job>,
    ) -> Result<(), Failure> {
        let target = &request.target;
        let drive = &target.drive_id;
        let path = match job {
            Job::List(path) | Job::Hash(path) | Job::Aggregate(path) => path,
        };
        let mut parent = path.parent();
        while let Some(path) = parent.filter(|path| path.starts_with(mount) && *path != mount) {
            if !fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir()) {
                return Err(Failure::Permanent(format!(
                    "親フォルダを確認できません: {}",
                    path.display()
                )));
            }
            parent = path.parent();
        }
        let db = self.db.as_mut().expect("手動更新の DB");
        let superseded = |queue: &super::ManualQueue, path: &Path| {
            request.created.is_none()
                && queue.pending.iter().any(|next| {
                    next.created.is_none()
                        && next.target.drive_id == *drive
                        && next.target.contains(path)
                })
        };
        let newer = |path: &Path| superseded(&self.control.manual.lock().expect("手動更新"), path);
        // 書込の順番は単一のワーカーが守る。先に確定枠を取ったパスへの新規依頼は
        // 別世代に積まれ、DB 書込の完了後にディスクから読み直す。
        let reserve = |path: &Path, processed: bool| {
            let mut queue = self.control.manual.lock().expect("手動更新");
            if superseded(&queue, path) || self.control.closed.load(Ordering::Relaxed) {
                return Err(Failure::Superseded);
            }
            if processed && request.created.is_none() {
                queue.processed.insert(path.to_path_buf());
            }
            Ok(())
        };
        match job {
            Job::List(path) => {
                let version = db.listing_version()?;
                let result = match fs::symlink_metadata(path) {
                    Ok(meta) if meta.is_dir() => Self::read_dir_filtered(&self.skip_dirs, path),
                    Ok(_) => Err("更新対象が実フォルダではありません".into()),
                    Err(error) => Err(error.to_string()),
                };
                let removed = if target.subtree && request.created.is_none() {
                    match &result {
                        Ok(listing) => Self::removed_folders(db, drive, mount, path, listing)?,
                        Err(_) => Vec::new(),
                    }
                } else {
                    Vec::new()
                };
                reserve(path, target.subtree)?;
                let saved = db.save_listing_at(drive, mount, path, &result, Some(version))?;
                match saved {
                    ListingSave::Retry => {
                        return Err(Failure::Retry("一覧の世代が変わりました".into()));
                    }
                    ListingSave::Missing => {
                        return Err(Failure::Permanent(format!(
                            "一覧を保存できませんでした: {}",
                            path.display()
                        )));
                    }
                    _ => {}
                }
                let mut failure = result.as_ref().err().cloned();
                if let Ok(listing) = &result {
                    if let Some((created, _)) = &request.created {
                        self.hints.insert(created.clone());
                        if !listing
                            .entries
                            .iter()
                            .any(|entry| readable(entry) && path.join(&entry.name) == *created)
                        {
                            failure = Some("作成済みのフォルダを一覧で確認できませんでした".into());
                        }
                    } else {
                        jobs.push(Job::Aggregate(path.clone()));
                        for child in listing.entries.iter().rev() {
                            let child_path = path.join(&child.name);
                            if target.subtree && readable(child) {
                                jobs.push(Job::List(child_path));
                            } else if target.contains(&child_path)
                                && child.kind == Kind::File
                                && !child.cloud_only
                                && !child.other_volume
                            {
                                jobs.push(Job::Hash(child_path));
                            }
                        }
                    }
                    if !target.subtree
                        && !listing
                            .entries
                            .iter()
                            .any(|child| path.join(&child.name) == target.path)
                    {
                        failure = Some(format!("更新対象が消えました: {}", target.path.display()));
                    }
                    if let Some((name, _, error)) = listing.failed.first() {
                        failure = Some(format!("{}: {error}", name.to_string_lossy()));
                    }
                }
                if target.subtree {
                    self.send(Message::RefreshFolder(path.clone()));
                    for removed in removed {
                        self.send(Message::RefreshFolder(removed));
                    }
                }
                if request.created.is_some() && matches!(saved, ListingSave::Changed) {
                    self.dirty = true;
                }
                if matches!(saved, ListingSave::Changed) {
                    self.send(Message::Changed { path: path.clone() });
                }
                self.send(Message::Listed {
                    path: path.clone(),
                    result,
                    stored: true,
                });
                self.send(Message::Committed);
                self.wake();
                if let Some(reason) = failure {
                    return Err(Failure::Permanent(format!("{}: {reason}", path.display())));
                }
            }
            Job::Hash(path) => {
                if newer(path) {
                    return Err(Failure::Superseded);
                }
                let version = db.listing_version()?;
                let parent = path.parent().expect("ファイルの親");
                let Some(id) = db.find_folder(drive, mount, parent)? else {
                    return Err(Failure::Permanent(format!(
                        "親が消えました: {}",
                        parent.display()
                    )));
                };
                let name = lisa_core::name::raw_bytes(path.file_name().expect("ファイル名"));
                let Some(entry) = db.child_named(id, &name)? else {
                    return Err(Failure::Permanent(format!(
                        "ファイルが消えました: {}",
                        path.display()
                    )));
                };
                if entry.merged_verified
                    || entry.cloud_only
                    || entry.other_volume
                    || entry.kind != Kind::File
                {
                    return Ok(());
                }
                let meta =
                    |error: io::Error| Failure::Permanent(format!("{}: {error}", path.display()));
                let before = fs::symlink_metadata(path);
                let changed = match &before {
                    Ok(before) => {
                        !before.is_file()
                            || listing::cloud_only(before)
                            || before.len() != entry.size
                            || listing::mtime_ms(before) != entry.mtime
                    }
                    Err(error) => error.kind() == io::ErrorKind::NotFound,
                };
                if changed {
                    let result = Self::read_dir_filtered(&self.skip_dirs, parent);
                    reserve(path, true)?;
                    let saved = db.save_listing_at(drive, mount, parent, &result, Some(version))?;
                    if matches!(saved, ListingSave::Changed) {
                        self.send(Message::Changed {
                            path: parent.to_path_buf(),
                        });
                        self.send(Message::Listed {
                            path: parent.to_path_buf(),
                            result,
                            stored: true,
                        });
                        self.send(Message::Committed);
                        self.wake();
                    }
                    return Err(Failure::Retry(format!(
                        "ハッシュ前に属性が変わりました: {}",
                        path.display()
                    )));
                }
                let before_fingerprint = fingerprint(&before.map_err(meta)?);
                let hash = hash::hash_regular_file_paced(
                    path,
                    |meta| fingerprint(meta) == before_fingerprint && !listing::cloud_only(meta),
                    &|_| {
                        !self.control.closed.load(Ordering::Relaxed)
                            && self.control.urgent.load(Ordering::Relaxed) == 0
                            && !self.control.created_pending.load(Ordering::Relaxed)
                            && !newer(path)
                    },
                );
                let after = fs::symlink_metadata(path)
                    .map_err(|error| Failure::Retry(format!("{}: {error}", path.display())))?;
                if hash
                    .as_ref()
                    .is_err_and(|error| error.kind() == io::ErrorKind::WouldBlock)
                    || before_fingerprint != fingerprint(&after)
                    || !after.is_file()
                    || listing::cloud_only(&after)
                {
                    return Err(Failure::Retry(format!(
                        "読込中にファイルが変わりました: {}",
                        path.display()
                    )));
                }
                if newer(path) || self.control.closed.load(Ordering::Relaxed) {
                    return Err(Failure::Superseded);
                }
                if hash
                    .as_ref()
                    .is_err_and(|error| error.kind() == io::ErrorKind::Interrupted)
                {
                    return Err(Failure::Yield);
                }
                reserve(path, true)?;
                let hash = hash.map_err(|error| format!("{}: {error}", path.display()));
                let result = hash.as_ref().map_err(String::as_str);
                if !db.save_refreshed_hash(&entry, version, result)? {
                    return Err(Failure::Retry(format!(
                        "ハッシュの保存前に一覧が変わりました: {}",
                        path.display()
                    )));
                }
                if let Err(error) = hash {
                    return Err(Failure::Permanent(error));
                }
            }
            Job::Aggregate(path) => {
                if Self::aggregate_refresh_folder(db, drive, mount, path)? {
                    self.send(Message::Hashed { path: path.clone() });
                    self.send(Message::Committed);
                    self.wake();
                }
            }
        }
        Ok(())
    }

    /// 差分保存で連鎖削除される子も、以前のディスク画像を消す対象にする。
    fn removed_folders(
        db: &Db,
        drive: &str,
        mount: &Path,
        path: &Path,
        listing: &listing::Listing,
    ) -> Result<Vec<PathBuf>, Failure> {
        let Some(folder) = db.find_folder_ref(drive, mount, path)? else {
            return Ok(Vec::new());
        };
        let retained: std::collections::HashSet<&[u8]> = listing
            .entries
            .iter()
            .filter(|entry| readable(entry))
            .map(|entry| entry.name_raw.as_slice())
            .collect();
        let known_children = |id, parent: &Path| -> Result<Vec<_>, Failure> {
            Ok(db
                .children(id)?
                .into_iter()
                .filter(|child| child.kind == Kind::Dir && !child.cloud_only && !child.other_volume)
                .map(|child| {
                    (
                        child.id,
                        child.name_raw.clone(),
                        parent.join(lisa_core::name::from_raw_bytes(&child.name_raw)),
                    )
                })
                .collect())
        };
        let mut stack = known_children(folder, path)?;
        stack.retain(|(_, name, _)| !retained.contains(name.as_slice()));
        let mut removed = Vec::new();
        while let Some((id, _, path)) = stack.pop() {
            stack.extend(known_children(id, &path)?);
            removed.push(path);
        }
        Ok(removed)
    }

    fn aggregate_refresh_folder(
        db: &mut Db,
        drive: &str,
        mount: &Path,
        path: &Path,
    ) -> Result<bool, Failure> {
        let Some(id) = db.find_folder(drive, mount, path)? else {
            return Ok(false);
        };
        Ok(db.aggregate_refreshed_folder(id)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{RefreshTarget, Root};
    use lisa_core::hash::Pace;
    use std::sync::{Arc, Mutex, mpsc::channel};

    struct Fixture(PathBuf);

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn 更新は同サイズ同時刻の単体と既読階層を再計算する() {
        let fixture =
            Fixture(std::env::temp_dir().join(format!("lisa-manual-{}", std::process::id())));
        let mount = &fixture.0.join("volume");
        let db_path = fixture.0.join("test.db");
        let branch = mount.join("branch");
        let nested = branch.join("nested");
        let normal_dir = mount.join("normal");
        fs::create_dir_all(&normal_dir).unwrap();
        let normal_file = normal_dir.join("file");
        fs::write(&normal_file, b"before").unwrap();
        fs::create_dir_all(nested.join("removed/deep")).unwrap();
        let first = nested.join("first");
        let sibling = nested.join("sibling");
        fs::write(&first, b"aaa").unwrap();
        fs::write(&sibling, b"bbb").unwrap();
        fs::write(nested.join("rename"), b"ccc").unwrap();
        fs::write(nested.join("removed/deep/file"), b"ddd").unwrap();
        fs::write(nested.join("move"), b"mmm").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&first, nested.join("link")).unwrap();

        let mut db = Db::open(&db_path).unwrap();
        let drive = "test:refresh";
        let device = db.this_device("test", "test", "test").unwrap();
        db.upsert_drive(
            &device,
            &lisa_core::volumes::Volume {
                id: Some(drive.into()),
                mount_path: mount.clone(),
                ..crate::scan::root_volume()
            },
        )
        .unwrap();
        for folder in [
            mount.clone(),
            normal_dir.clone(),
            branch.clone(),
            nested.clone(),
            nested.join("removed"),
            nested.join("removed/deep"),
        ] {
            db.save_listing(
                drive,
                mount,
                &folder,
                &listing::read_dir(&folder).map_err(|error| error.to_string()),
            )
            .unwrap();
        }
        hash::hash_drive(&mut db, drive, mount, 1, &mut |_| {}, &Pace::FREE).unwrap();
        let (normal, requests) = channel();
        let (sender, messages) = channel();
        let mut worker = Worker {
            db: Some(db),
            roots: vec![Root {
                drive_id: Some(drive.into()),
                mount: mount.clone(),
                mount_identity: directory_identity(&fs::symlink_metadata(mount).unwrap()),
                node: 0,
                listed: true,
            }],
            sender,
            requests,
            proxy: None,
            progress: Arc::new(Mutex::default()),
            control: Arc::new(Default::default()),
            dirty: false,
            hints: Default::default(),
            skip_dirs: vec![],
        };

        let replace = |path: &Path, bytes| {
            let modified = fs::metadata(path).unwrap().modified().unwrap();
            fs::write(path, bytes).unwrap();
            fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(modified))
                .unwrap();
        };
        replace(&first, b"xxx");
        replace(&sibling, b"yyy");
        let item_hash = |worker: &Worker, name: &str| {
            let db = worker.db.as_ref().unwrap();
            let folder = db.find_folder_ref(drive, mount, &nested).unwrap().unwrap();
            db.children(folder)
                .unwrap()
                .into_iter()
                .find(|child| child.name_nfc == name)
                .unwrap()
                .hash
        };
        let sibling_original = item_hash(&worker, "sibling");
        let request = |path: PathBuf, subtree| RefreshRequest {
            target: RefreshTarget {
                drive_id: drive.into(),
                path,
                subtree,
            },
            generation: 1,
            created: None,
        };
        // 普通のハッシュの途中で手動要求を積み、次の読み取り確認で譲る。
        fs::write(&normal_file, vec![7_u8; 2 * 1024 * 1024]).unwrap();
        worker.store_reread(0, &normal_dir, false);
        let _ = messages.try_iter().count();
        let control = worker.control.clone();
        let background = super::super::Background::new(control.clone());
        control.rest_on_battery.store(false, Ordering::Relaxed);
        let generation = std::sync::atomic::AtomicU64::new(0);
        let target = request(first.clone(), false).target;
        let step = |bytes| {
            if bytes > 0 && generation.load(Ordering::Relaxed) == 0 {
                let queued = control.queue_refresh(target.clone(), None).unwrap();
                generation.store(queued, Ordering::Relaxed);
            }
            background.step(bytes, || false)
        };
        assert!(worker.hash(
            0,
            Some(&normal_dir),
            &Pace {
                step: &step,
                ..Pace::FREE
            }
        ));
        let generation = generation.load(Ordering::Relaxed);
        assert_ne!(generation, 0);
        let normal_hash = |worker: &Worker| {
            let db = worker.db.as_ref().unwrap();
            let id = db
                .find_folder_ref(drive, mount, &normal_dir)
                .unwrap()
                .unwrap();
            db.children(id).unwrap()[0].hash
        };
        assert_eq!(normal_hash(&worker), None);
        let _ = messages.try_iter().count();
        for _ in 0..128 {
            normal.send(Request::Hint(nested.clone())).unwrap();
        }
        worker.control.urgent.fetch_add(1, Ordering::Relaxed);
        assert!(worker.handle(Request::Visible(mount.clone())));
        assert!(
            matches!(messages.try_recv().unwrap(), Message::RefreshStarted(request) if request.generation == generation)
        );
        assert_eq!(
            item_hash(&worker, "first"),
            Some(hash::hash_file(&first).unwrap())
        );
        assert_eq!(item_hash(&worker, "sibling"), sibling_original);
        let remaining: Vec<_> = messages.try_iter().collect();
        let completed = remaining
            .iter()
            .position(|message| {
                matches!(message,
            Message::RefreshFinished { request, error: None } if request.generation == generation)
            })
            .unwrap();
        assert!(
            remaining[completed + 1..].iter().any(|message| {
                matches!(message, Message::Listed { path, .. } if path == mount)
            })
        );

        assert_eq!(
            normal_hash(&worker),
            Some(hash::hash_file(&normal_file).unwrap())
        );

        fs::rename(nested.join("rename"), nested.join("renamed")).unwrap();
        fs::remove_dir_all(nested.join("removed")).unwrap();
        fs::create_dir(nested.join("added")).unwrap();
        fs::write(nested.join("added/file"), b"eee").unwrap();
        fs::rename(nested.join("move"), nested.join("added/moved")).unwrap();
        let mut other = Db::open(&db_path).unwrap();
        other.begin().unwrap();
        other.set_meta("fixture-lock", "held").unwrap();
        worker
            .db
            .as_ref()
            .unwrap()
            .set_busy_timeout(RETRY_WAIT)
            .unwrap();
        let lock = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            other.commit().unwrap();
        });
        assert!(worker.run_refresh(&request(branch.clone(), true)).is_none());
        lock.join().unwrap();
        assert_eq!(
            item_hash(&worker, "sibling"),
            Some(hash::hash_file(&sibling).unwrap())
        );
        let db = worker.db.as_mut().unwrap();
        assert!(
            db.find_folder(drive, mount, &nested.join("added"))
                .unwrap()
                .is_some()
        );
        assert!(
            db.find_folder(drive, mount, &nested.join("removed/deep"))
                .unwrap()
                .is_none()
        );
        let folder = db.find_folder(drive, mount, &nested).unwrap().unwrap();
        let children = db.children(folder).unwrap();
        assert!(children.iter().any(|child| child.name_nfc == "renamed"));
        assert!(
            !children
                .iter()
                .any(|child| matches!(child.name_nfc.as_str(), "rename" | "move"))
        );
        #[cfg(unix)]
        assert!(
            children
                .iter()
                .any(|child| child.name_nfc == "link" && child.hash.is_none())
        );
        let refreshed = db.drive_root(drive).unwrap().unwrap().hash;
        let mut full = Db::open_in_memory().unwrap();
        let device = full.this_device("test", "test", "test").unwrap();
        full.upsert_drive(
            &device,
            &lisa_core::volumes::Volume {
                id: Some(drive.into()),
                mount_path: mount.clone(),
                ..crate::scan::root_volume()
            },
        )
        .unwrap();
        let mut directories = vec![mount.clone()];
        while let Some(path) = directories.pop() {
            let listing = listing::read_dir(&path).map_err(|error| error.to_string());
            directories.extend(
                listing
                    .as_ref()
                    .unwrap()
                    .entries
                    .iter()
                    .filter(|child| readable(child))
                    .map(|child| path.join(&child.name)),
            );
            full.save_listing(drive, mount, &path, &listing).unwrap();
        }
        hash::hash_drive(&mut full, drive, mount, 1, &mut |_| {}, &Pace::FREE).unwrap();
        assert!(refreshed.is_some());
        assert_eq!(refreshed, full.drive_root(drive).unwrap().unwrap().hash);
        assert!(
            messages
                .try_iter()
                .any(|message| matches!(message, Message::RefreshFolder(path) if path == nested))
        );

        // 読めないフォルダは既存の子を削除せず、他の要求は継続できる。
        let displaced = mount.join("displaced");
        let old_nested = request(nested.clone(), true);
        fs::rename(&nested, &displaced).unwrap();
        assert!(worker.run_refresh(&old_nested).is_some());
        assert!(item_hash(&worker, "sibling").is_some());
        fs::rename(&displaced, &nested).unwrap();

        let disconnected = fixture.0.join("disconnected");
        fs::rename(mount, &disconnected).unwrap();
        assert!(worker.run_refresh(&old_nested).is_some());
        #[cfg(unix)]
        {
            fs::create_dir(mount).unwrap();
            assert!(worker.run_refresh(&old_nested).is_some());
            fs::remove_dir(mount).unwrap();
        }
        fs::rename(disconnected, mount).unwrap();
        assert!(worker.run_refresh(&old_nested).is_none());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&first, fs::Permissions::from_mode(0o000)).unwrap();
            if fs::File::open(&first).is_err() {
                replace(&sibling, b"zzz");
                assert!(worker.run_refresh(&old_nested).is_some());
                assert_eq!(item_hash(&worker, "first"), None);
                assert_eq!(
                    item_hash(&worker, "sibling"),
                    Some(hash::hash_file(&sibling).unwrap())
                );
            }
        }
        let created = nested.join("created");
        fs::create_dir(&created).unwrap();
        let (reply, response) = channel();
        let refresh = request(branch.clone(), true);
        worker
            .control
            .queue_refresh(request(normal_dir.clone(), true).target, None)
            .unwrap();
        let generation = worker
            .control
            .queue_refresh(
                request(nested.clone(), true).target,
                Some((created.clone(), reply)),
            )
            .unwrap();
        let _ = messages.try_iter().count();
        let _ = worker.run_refresh(&refresh);
        assert_eq!(response.try_recv().unwrap(), None);
        assert!(
            matches!(messages.try_iter().find(|msg| matches!(msg, Message::RefreshFinished { .. })),
            Some(Message::RefreshFinished { request, error: None }) if request.generation == generation)
        );
        assert!(!worker.control.created_pending.load(Ordering::Relaxed));
        worker.control.closed.store(true, Ordering::Relaxed);
        assert!(!worker.run_manual_queue());
    }

    #[test]
    fn 処理前は親子をまとめ処理済みなら新しい世代を積む() {
        let control = super::super::Control::default();
        let target = |path: &str, subtree| RefreshTarget {
            path: PathBuf::from(path),
            subtree,
            drive_id: "synthetic".into(),
        };
        let parent = target("branch", true);
        let file = target("branch/file", false);
        let first = control.queue_refresh(parent.clone(), None).unwrap();
        assert_eq!(control.queue_refresh(file.clone(), None), Some(first));
        {
            let mut queue = control.manual.lock().unwrap();
            queue.active = queue.pending.pop_front();
        }
        assert_eq!(control.queue_refresh(file.clone(), None), Some(first));
        control
            .manual
            .lock()
            .unwrap()
            .processed
            .insert(file.path.clone());
        let next = control.queue_refresh(file, None).unwrap();
        let other_drive = RefreshTarget {
            drive_id: "other".into(),
            ..parent
        };
        assert!(control.queue_refresh(other_drive, None).unwrap() > next);
        assert!(next > first);
        assert!(control.manual_pending.load(Ordering::Relaxed));
        let control = Arc::new(control);
        let background = super::super::Background::new(control.clone());
        control.set_active(true);
        for boost in [false, true] {
            control.set_boost(boost);
            assert!(!background.step(0, || false));
            assert_eq!(control.boost_status().1, boost);
        }
    }
}
