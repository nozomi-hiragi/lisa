use super::*;
use std::collections::{HashMap, HashSet};
use std::fs::{Metadata, symlink_metadata};

use crate::db::HashQueueItem;
use crate::listing::{Failure, cloud_only, mtime_ms};
use crate::volumes::{mount_identity, mount_matches};
use rusqlite::{OptionalExtension, params};

pub const HASH_THREADS: usize = 4;
pub const HASH_BATCH_SIZE: usize = 32;

#[derive(Debug, PartialEq, Eq)]
pub enum HashFailure {
    Interrupted,
    Missing,
    Temporary(String),
    FingerprintChanged,
    Permanent(String),
}

impl HashFailure {
    pub fn of(error: &io::Error) -> Self {
        use io::ErrorKind::*;
        if raw_temporary(error) {
            return Self::Temporary(error.to_string());
        }
        match error.kind() {
            Interrupted => return Self::Interrupted,
            NotFound => return Self::Missing,
            PermissionDenied => return Self::Permanent("Permission".into()),
            _ => {}
        }
        if matches!(
            error.kind(),
            WouldBlock
                | ResourceBusy
                | TimedOut
                | ConnectionReset
                | ConnectionAborted
                | NotConnected
                | BrokenPipe
                | HostUnreachable
                | NetworkUnreachable
                | NetworkDown
        ) {
            Self::Temporary(error.to_string())
        } else {
            Self::Permanent(format!("{:?}", Failure::of(error)))
        }
    }
}

fn raw_temporary(error: &io::Error) -> bool {
    let Some(code) = error.raw_os_error() else {
        return false;
    };
    #[cfg(unix)]
    return matches!(
        code,
        libc::EAGAIN
            | libc::EBUSY
            | libc::ESTALE
            | libc::EHOSTDOWN
            | libc::ETIMEDOUT
            | libc::ENOTCONN
            | libc::EIO
    );
    #[cfg(windows)]
    return matches!(code, 32 | 33 | 53 | 64 | 121 | 55);
    #[cfg(not(any(unix, windows)))]
    {
        let _ = code;
        false
    }
}

/// 一時失敗した回数に応じた次回までの待機。8 回目の失敗で打ち切る。
pub fn hash_retry_delay_ms(attempts: u32) -> Option<i64> {
    match attempts {
        1 => Some(60_000),
        2 => Some(300_000),
        3 => Some(1_800_000),
        4 => Some(7_200_000),
        5 => Some(21_600_000),
        6 | 7 => Some(86_400_000),
        _ => None,
    }
}

#[derive(Default, Debug)]
pub struct HashBatch {
    pub picked: usize,
    pub files: Vec<(PathBuf, u64)>,
    pub folders: Vec<PathBuf>,
    pub hints: Vec<PathBuf>,
    pub interrupted: bool,
}

struct Job {
    item: HashQueueItem,
    entry: Stored,
    path: PathBuf,
    mount: PathBuf,
}

enum Outcome {
    Dir,
    File(Result<Hash, HashFailure>),
}

/// 接続中ドライブのキューを最大 32 件処理する。表示中と裏作業は異なる優先度上限と Pace で呼ぶ。
pub fn hash_queue_batch(
    db: &mut Db,
    mounts: &[(&str, &Path)],
    max_priority: i64,
    pace: &Pace,
) -> db::Result<HashBatch> {
    let identities: HashMap<_, _> = mounts
        .iter()
        .map(|(id, path)| (*id, mount_identity(path)))
        .collect();
    hash_queue_batch_checked(db, mounts, max_priority, pace, |id, path| {
        mount_matches(path, identities[id])
    })
}

/// 呼び出し元が保持するマウントの識別子で、確定直前に接続を確認できる版。
pub fn hash_queue_batch_checked(
    db: &mut Db,
    mounts: &[(&str, &Path)],
    max_priority: i64,
    pace: &Pace,
    connected: impl Fn(&str, &Path) -> bool,
) -> db::Result<HashBatch> {
    db.commit()?;
    let drive_ids: Vec<_> = mounts.iter().map(|(id, _)| *id).collect();
    let jobs = db::retry_busy(|| {
        let tx = db.conn().unchecked_transaction()?;
        // キュー取得とパスの復元は同じ読み取りスナップショットで行う。
        let jobs = load_jobs(db, mounts, &drive_ids, max_priority)?;
        tx.commit()?;
        Ok(jobs)
    })?;
    if jobs.is_empty() {
        return Ok(HashBatch::default());
    }
    db.ensure_hash_pending_index()?;
    let next = AtomicUsize::new(0);
    let results = Mutex::new(Vec::with_capacity(jobs.len()));
    std::thread::scope(|scope| {
        for _ in 0..HASH_THREADS.min(jobs.len()) {
            let next = &next;
            let jobs = &jobs;
            let results = &results;
            scope.spawn(move || {
                pace.apply_priority();
                let step = |bytes| {
                    if let Some(priority) = pace.priority {
                        priority();
                    }
                    (pace.step)(bytes)
                };
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(job) = jobs.get(index) else { break };
                    let outcome = if !step(0) {
                        Outcome::File(Err(HashFailure::Interrupted))
                    } else if job.entry.kind == Kind::Dir {
                        Outcome::Dir
                    } else {
                        Outcome::File(read_job(job, &step))
                    };
                    results.lock().expect("ハッシュ結果").push((index, outcome));
                }
            });
        }
    });
    let mut results = results.into_inner().expect("ハッシュ結果");
    // I/O を終えてから同じ媒体か再確認する。切断中の結果は、消失を含めて確定しない。
    let mut verified = HashMap::new();
    for (index, outcome) in &mut results {
        let job = &jobs[*index];
        if !*verified
            .entry(job.item.drive_id.as_str())
            .or_insert_with(|| connected(&job.item.drive_id, &job.mount))
        {
            *outcome = Outcome::File(Err(HashFailure::Interrupted));
        }
    }
    if results
        .iter()
        .all(|(_, outcome)| matches!(outcome, Outcome::File(Err(HashFailure::Interrupted))))
    {
        return Ok(HashBatch {
            picked: jobs.len(),
            interrupted: true,
            ..Default::default()
        });
    }
    results.sort_unstable_by_key(|(index, _)| *index);
    db::retry_busy(|| {
        db.conn().execute_batch("BEGIN IMMEDIATE")?;
        let result = finish_jobs(db, &jobs, &results, db::now_ms()).and_then(|batch| {
            db.conn().execute_batch("COMMIT")?;
            Ok(batch)
        });
        if result.is_err() && !db.conn().is_autocommit() {
            db.conn().execute_batch("ROLLBACK")?;
        }
        result
    })
}

fn load_jobs(
    db: &Db,
    mounts: &[(&str, &Path)],
    drive_ids: &[&str],
    max_priority: i64,
) -> db::Result<Vec<Job>> {
    let mut jobs = Vec::new();
    for item in db.pick_hash_queue(drive_ids, db::now_ms(), HASH_BATCH_SIZE, max_priority)? {
        let Some(entry) = db.entry(item.entry_id)? else {
            continue;
        };
        let Some((_, mount)) = mounts.iter().find(|(id, _)| *id == item.drive_id) else {
            continue;
        };
        let Some(path) = entry_location(db, entry.id, mount)?.map(|(_, path)| path) else {
            continue;
        };
        jobs.push(Job {
            item,
            entry,
            path,
            mount: mount.to_path_buf(),
        });
    }
    Ok(jobs)
}

fn entry_location(db: &Db, id: i64, mount: &Path) -> db::Result<Option<(Option<i64>, PathBuf)>> {
    let mut stmt = db
        .conn()
        .prepare_cached("SELECT parent_id, name FROM entry WHERE id = ?1")?;
    let mut current = Some(id);
    let mut chain = Vec::new();
    let mut parent = None;
    while let Some(entry_id) = current {
        let Some((up, name)): Option<(Option<i64>, Vec<u8>)> = stmt
            .query_row([entry_id], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()?
        else {
            return Ok(None);
        };
        if entry_id == id {
            parent = up;
        }
        if up.is_some() {
            chain.push(crate::name::from_raw_bytes(&name));
        }
        current = up;
    }
    let mut path = mount.to_path_buf();
    path.extend(chain.into_iter().rev());
    Ok(Some((parent, path)))
}

fn fingerprint(meta: &Metadata) -> (u64, Option<std::time::SystemTime>, Option<u64>) {
    (
        meta.len(),
        meta.modified().ok(),
        crate::listing::file_id(meta),
    )
}

fn read_job(job: &Job, step: &(dyn Fn(u64) -> bool + Sync)) -> Result<Hash, HashFailure> {
    let checked_metadata = || {
        let meta = symlink_metadata(&job.path).map_err(|error| HashFailure::of(&error))?;
        if !meta.is_file() || cloud_only(&meta) {
            return Err(HashFailure::Permanent("Io".into()));
        }
        Ok(meta)
    };
    if job.entry.kind != Kind::File
        || job.entry.cloud_only
        || job.entry.merged_verified
        || job.entry.hash.is_some()
    {
        return Err(HashFailure::Permanent("Io".into()));
    }
    let before = checked_metadata()?;
    if before.len() != job.entry.size || mtime_ms(&before) != job.entry.mtime {
        return Err(HashFailure::FingerprintChanged);
    }
    let hash = hash_regular_file_paced(
        &job.path,
        |meta| fingerprint(meta) == fingerprint(&before),
        step,
    )
    .map_err(|error| {
        if error.kind() == io::ErrorKind::WouldBlock && error.raw_os_error().is_none() {
            HashFailure::FingerprintChanged
        } else {
            HashFailure::of(&error)
        }
    })?;
    let after = checked_metadata()?;
    if fingerprint(&before) != fingerprint(&after) {
        return Err(HashFailure::FingerprintChanged);
    }
    Ok(hash)
}

fn finish_jobs(
    db: &mut Db,
    jobs: &[Job],
    results: &[(usize, Outcome)],
    now: i64,
) -> db::Result<HashBatch> {
    let mut batch = HashBatch {
        picked: jobs.len(),
        ..Default::default()
    };
    let mut dirs = Vec::new();
    let mut hints = HashSet::new();
    for (index, outcome) in results {
        let job = &jobs[*index];
        if matches!(outcome, Outcome::File(Err(HashFailure::Interrupted))) {
            batch.interrupted = true;
            continue;
        }
        let id = job.entry.id;
        let stamp: Option<i64> = db
            .conn()
            .prepare_cached("SELECT updated_at FROM hash_queue WHERE entry_id = ?1")?
            .query_row([id], |row| row.get(0))
            .optional()?;
        if stamp != Some(job.item.updated_at) || db.entry(id)?.as_ref() != Some(&job.entry) {
            continue;
        }
        let Some((parent, path)) = entry_location(db, id, &job.mount)? else {
            continue;
        };
        if path != job.path {
            continue;
        }
        if job.entry.hash.is_some() || job.entry.error.is_some() {
            db.remove_hash_job(id)?;
            continue;
        }
        match outcome {
            Outcome::File(Err(HashFailure::Interrupted)) => unreachable!(),
            Outcome::Dir => {
                // 未確定の子があれば、子の完了や次の一覧保存から再判定する。
                db.remove_hash_job(id)?;
                dirs.push((id, path));
                continue;
            }
            Outcome::File(Ok(hash)) => {
                db.set_hash(id, hash, job.entry.size, job.entry.mtime)?;
                batch.files.push((path.clone(), job.entry.size));
            }
            Outcome::File(Err(HashFailure::Missing)) => {
                // 親一覧が確定するまでは再処理できるように残す。
                // 一覧で本当に消えていれば entry とともに連鎖削除される。
                let delay = hash_retry_delay_ms(1).expect("初回の待機時間");
                db.conn().prepare_cached(
                    "UPDATE hash_queue SET reason = 'retry', due_at = ?2, delay_ms = ?3,
                     last_error = 'NotFound', updated_at = max(updated_at + 1, ?4) WHERE entry_id = ?1",
                )?.execute(params![id, now.saturating_add(delay), delay, now])?;
                if let Some(parent) = path.parent()
                    && hints.insert(parent.to_path_buf())
                {
                    batch.hints.push(parent.to_path_buf());
                }
                continue;
            }
            Outcome::File(Err(
                failure @ (HashFailure::Temporary(_) | HashFailure::FingerprintChanged),
            )) => {
                let error = match failure {
                    HashFailure::Temporary(error) => error.as_str(),
                    _ => {
                        if let Some(parent) = path.parent()
                            && hints.insert(parent.to_path_buf())
                        {
                            batch.hints.push(parent.to_path_buf());
                        }
                        "読取前後に属性が変化"
                    }
                };
                let attempts = job.item.attempts.saturating_add(1);
                if let Some(delay) = hash_retry_delay_ms(attempts) {
                    db.conn()
                        .prepare_cached(
                            "UPDATE hash_queue SET reason = 'retry', attempts = ?2, delay_ms = ?3,
                         due_at = ?4, last_error = ?5, updated_at = max(updated_at + 1, ?6)
                         WHERE entry_id = ?1",
                        )?
                        .execute(params![
                            id,
                            attempts,
                            delay,
                            now.saturating_add(delay),
                            error,
                            now
                        ])?;
                    continue;
                }
                db.set_hash_error(id, error)?;
                batch.files.push((path.clone(), job.entry.size));
            }
            Outcome::File(Err(HashFailure::Permanent(error))) => {
                db.set_hash_error(id, error)?;
                batch.files.push((path.clone(), job.entry.size));
            }
        }
        if let (Some(id), Some(parent_path)) = (parent, path.parent()) {
            dirs.push((id, parent_path.to_path_buf()));
        }
    }
    aggregate(db, dirs, &mut batch.folders)?;
    Ok(batch)
}

fn aggregate(db: &mut Db, dirs: Vec<(i64, PathBuf)>, done: &mut Vec<PathBuf>) -> db::Result<()> {
    // 深いフォルダが先なら、同じ祖先を何度も読まずに済む。
    let mut pending: HashMap<_, _> = dirs.into_iter().collect();
    while let Some(id) = pending
        .iter()
        .max_by_key(|(id, path)| (path.components().count(), **id))
        .map(|(id, _)| *id)
    {
        let path = pending.remove(&id).expect("集約対象");
        let Some(folder) = db.entry(id)? else {
            continue;
        };
        if folder.kind != Kind::Dir
            || !folder.listed
            || folder.error.is_some()
            || folder.hash.is_some()
            || folder.cloud_only
            || folder.other_volume
        {
            continue;
        }
        if db.has_unfinished_hash_children(id)? {
            continue;
        }
        let entries = super::stored_tree_bytes(&db.children(id)?);
        let hash = *blake3::hash(&entries).as_bytes();
        db.set_hash(id, &hash, 0, folder.mtime)?;
        db.save_tree(&hash, &entries)?;
        let parent: Option<i64> = db
            .conn()
            .prepare_cached("SELECT parent_id FROM entry WHERE id = ?1")?
            .query_row([id], |r| r.get(0))?;
        if let (Some(parent), Some(parent_path)) = (parent, path.parent()) {
            pending.insert(parent, parent_path.to_path_buf());
        }
        done.push(path);
    }
    Ok(())
}

#[cfg(test)]
#[path = "queue_tests.rs"]
mod tests;
