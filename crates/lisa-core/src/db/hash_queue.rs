use super::*;

pub const HASH_VISIBLE: i64 = 10;
pub const HASH_CHANGED: i64 = 20;
pub const HASH_SCHEDULED: i64 = 30;

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS hash_queue (
    entry_id INTEGER PRIMARY KEY REFERENCES entry(id) ON DELETE CASCADE,
    drive_id TEXT NOT NULL,
    priority INTEGER NOT NULL,
    reason TEXT NOT NULL,
    due_at INTEGER NOT NULL DEFAULT 0,
    delay_ms INTEGER NOT NULL DEFAULT 0,
    attempts INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    updated_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS hash_queue_pick ON hash_queue(drive_id, due_at, priority, entry_id);
CREATE INDEX IF NOT EXISTS hash_queue_due ON hash_queue(due_at, priority, entry_id);
CREATE INDEX IF NOT EXISTS hash_queue_priority ON hash_queue(drive_id, priority, due_at, entry_id);
CREATE INDEX IF NOT EXISTS hash_queue_missing ON hash_queue(entry_id) WHERE last_error = 'NotFound';
";

// ready_tree_bytes と同じ条件。
const UNFINISHED_CHILD: &str = "hash IS NULL AND error IS NULL AND cloud_only = 0
    AND merged_verified = 0 AND (kind = 'file' OR (kind = 'dir' AND other_volume = 0))";

const ELIGIBLE: &str = "hash IS NULL AND ((kind = 'file' AND cloud_only = 0
    AND merged_verified = 0) OR (kind = 'dir' AND listed = 1 AND error IS NULL
    AND cloud_only = 0 AND other_volume = 0))";

const UPSERT: &str = "
ON CONFLICT(entry_id) DO UPDATE SET
  priority = min(hash_queue.priority, excluded.priority),
  reason = CASE WHEN excluded.priority < hash_queue.priority OR excluded.priority = 10
                  THEN excluded.reason
                WHEN excluded.reason = 'changed' AND hash_queue.reason = 'retry'
                  THEN CASE WHEN hash_queue.priority = 10 THEN 'visible' ELSE 'changed' END
                ELSE hash_queue.reason END,
  due_at = CASE WHEN excluded.priority < 30 THEN 0 ELSE hash_queue.due_at END,
  delay_ms = CASE WHEN excluded.priority < 30 THEN 0 ELSE hash_queue.delay_ms END,
  attempts = CASE WHEN excluded.priority < 30 THEN 0 ELSE hash_queue.attempts END,
  last_error = CASE WHEN excluded.priority < 30 THEN NULL ELSE hash_queue.last_error END,
  updated_at = max(hash_queue.updated_at + 1, excluded.updated_at)
WHERE excluded.priority < 30 OR excluded.priority < hash_queue.priority";

pub(super) fn enqueue(conn: &Connection, id: i64, priority: i64, now: i64) -> Result<()> {
    let reason = if priority == HASH_CHANGED {
        "changed"
    } else {
        "scheduled"
    };
    conn.prepare_cached(&format!(
        "INSERT INTO hash_queue(entry_id, drive_id, priority, reason, updated_at)
         SELECT id, drive_id, ?2, ?3, ?4 FROM entry WHERE id = ?1 AND {ELIGIBLE}
         AND error IS NULL {UPSERT}"
    ))?
    .execute(params![id, priority, reason, now])?;
    Ok(())
}

pub(super) fn has_missing_child(conn: &Connection, folder: i64) -> Result<bool> {
    conn.prepare_cached(
        "SELECT 1 FROM hash_queue q INDEXED BY hash_queue_missing
         JOIN entry e ON e.id = q.entry_id WHERE q.last_error = 'NotFound' AND e.parent_id = ?1 LIMIT 1",
    )?.exists([folder])
}

/// 一覧に残っていたファイルはすぐハッシュを再試行できるようにする。
pub(super) fn reconcile_missing_children(conn: &Connection, folder: i64, now: i64) -> Result<()> {
    conn.prepare_cached(
        "UPDATE hash_queue SET reason = CASE priority WHEN 10 THEN 'visible' ELSE 'changed' END,
         priority = min(priority, 20), due_at = 0, delay_ms = 0, last_error = NULL,
         updated_at = max(updated_at + 1, ?2)
         WHERE entry_id IN (SELECT q.entry_id FROM hash_queue q INDEXED BY hash_queue_missing
             JOIN entry e ON e.id = q.entry_id WHERE q.last_error = 'NotFound' AND e.parent_id = ?1)",
    )?.execute(params![folder, now])?;
    Ok(())
}

pub(crate) fn retry_busy<T>(mut run: impl FnMut() -> Result<T>) -> Result<T> {
    let delays = [50, 200, 500];
    for delay in delays {
        match run() {
            Err(error) if busy(&error) => std::thread::sleep(Duration::from_millis(delay)),
            result => return result,
        }
    }
    let result = run();
    if let Err(error) = &result
        && busy(error)
    {
        eprintln!("WARN hash_queue: {error}");
    }
    result
}

fn busy(error: &Error) -> bool {
    matches!(
        error.sqlite_error_code(),
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    )
}

#[derive(Debug, PartialEq, Eq)]
pub struct HashQueueItem {
    pub entry_id: i64,
    pub drive_id: String,
    pub priority: i64,
    pub reason: String,
    pub due_at: i64,
    pub delay_ms: i64,
    pub attempts: u32,
    pub last_error: Option<String>,
    pub updated_at: i64,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct HashQueueCounts {
    pub total: u64,
    pub waiting: u64,
}

/// 次のチャンクに渡すカーソル。起動をまたいだ途中再開では先頭から補完する。
#[derive(Default)]
pub struct HashQueueSeed {
    after: i64,
}

impl Db {
    /// 既存 DB を最大 5,000 行ずつ補完する。完了したら true を返す。
    pub fn seed_hash_queue(&mut self, drive_id: &str, cursor: &mut HashQueueSeed) -> Result<bool> {
        let key = format!("hash_queue_seeded_v1:{drive_id}");
        let (after, done) = retry_busy(|| {
            if self
                .conn
                .query_row("SELECT 1 FROM meta WHERE key = ?1", [&key], |_| Ok(()))
                .optional()?
                .is_some()
            {
                return Ok((cursor.after, true));
            }
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let started = Instant::now();
            let mut after = cursor.after;
            let now = now_ms();
            let mut done = false;
            for _ in 0..20 {
                let ids = tx
                    .prepare_cached("SELECT id FROM entry WHERE id > ?1 AND drive_id = ?2 ORDER BY id LIMIT 250")?
                    .query_map(params![after, drive_id], |r| r.get::<_, i64>(0))?
                    .collect::<Result<Vec<_>>>()?;
                let Some(&end) = ids.last() else {
                    done = true;
                    break;
                };
                tx.prepare_cached(&format!(
                    "INSERT INTO hash_queue(entry_id, drive_id, priority, reason, updated_at)
                     SELECT id, drive_id, 30, 'scheduled', ?4 FROM entry
                     WHERE id > ?1 AND id <= ?2 AND drive_id = ?3 AND {ELIGIBLE}
                     AND error IS NULL ON CONFLICT(entry_id) DO NOTHING"
                ))?
                .execute(params![after, end, drive_id, now])?;
                after = end;
                if started.elapsed() >= Duration::from_millis(20) {
                    break;
                }
            }
            if done {
                tx.execute(
                    "INSERT OR IGNORE INTO meta(key, value) VALUES (?1, '1')",
                    [&key],
                )?;
            }
            tx.commit()?;
            Ok((after, done))
        })?;
        cursor.after = after;
        Ok(done)
    }

    /// 表示フォルダ直下のファイルと子フォルダを昇格する。
    pub fn promote_hash_queue(&mut self, folder: i64) -> Result<()> {
        retry_busy(|| {
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            // 再表示時には永続失敗だったファイルも試し直す。
            tx.prepare_cached(
                 "UPDATE entry SET error = NULL WHERE parent_id = ?1 AND kind = 'file'
                  AND cloud_only = 0 AND merged_verified = 0 AND hash IS NULL AND error IS NOT NULL",
            )?.execute([folder])?;
            if tx.changes() > 0 {
                super::clear_hash_up(&tx, folder)?;
            }
            tx.prepare_cached(&format!(
                "INSERT INTO hash_queue(entry_id, drive_id, priority, reason, updated_at)
                 SELECT id, drive_id, 10, 'visible', ?2 FROM entry
                 WHERE parent_id = ?1 AND {ELIGIBLE} {UPSERT}"
            ))?
            .execute(params![folder, now_ms()])?;
            tx.commit()
        })
    }

    /// 実行可能な仕事を優先順で取得する。表示中は max_priority に HASH_VISIBLE を渡す。
    pub fn pick_hash_queue(
        &self,
        drive_ids: &[&str],
        now: i64,
        limit: usize,
        max_priority: i64,
    ) -> Result<Vec<HashQueueItem>> {
        retry_busy(|| {
            let mut statement = self.conn.prepare_cached(
                "SELECT q.entry_id, q.drive_id, q.priority, q.reason, q.due_at, q.delay_ms,
                        q.attempts, q.last_error, q.updated_at
                 FROM hash_queue q INDEXED BY hash_queue_priority
                 JOIN entry e ON e.id = q.entry_id AND e.drive_id = q.drive_id
                 WHERE q.drive_id = ?1 AND q.priority = ?2 AND q.due_at <= ?3
                 ORDER BY q.due_at, q.entry_id LIMIT ?4",
            )?;
            let mut items = Vec::new();
            for priority in [HASH_VISIBLE, HASH_CHANGED, HASH_SCHEDULED] {
                if priority > max_priority || items.len() >= limit {
                    break;
                }
                let remaining = limit - items.len();
                let mut candidates = Vec::new();
                for drive in drive_ids {
                    for row in statement.query_map(
                        params![
                            drive,
                            priority,
                            now,
                            remaining.min(i64::MAX as usize) as i64
                        ],
                        |r| {
                            Ok(HashQueueItem {
                                entry_id: r.get(0)?,
                                drive_id: r.get(1)?,
                                priority: r.get(2)?,
                                reason: r.get(3)?,
                                due_at: r.get(4)?,
                                delay_ms: r.get(5)?,
                                attempts: r.get(6)?,
                                last_error: r.get(7)?,
                                updated_at: r.get(8)?,
                            })
                        },
                    )? {
                        candidates.push(row?);
                    }
                }
                candidates.sort_unstable_by_key(|item| (item.due_at, item.entry_id));
                items.extend(candidates.into_iter().take(remaining));
            }
            Ok(items)
        })
    }

    pub fn next_hash_due_at(&self, drive_ids: &[&str], now: i64) -> Result<Option<i64>> {
        with_drives(drive_ids, |drives| {
            self.conn
                .prepare_cached(
                    "SELECT min(due_at) FROM hash_queue
             WHERE drive_id IN (SELECT value FROM json_each(?1)) AND due_at > ?2",
                )?
                .query_row(params![drives, now], |r| r.get(0))
        })
    }

    pub fn hash_queue_counts(&self, drive_ids: &[&str], now: i64) -> Result<HashQueueCounts> {
        with_drives(drive_ids, |drives| {
            self.conn
                .prepare_cached(
                    "SELECT count(*), count(CASE WHEN due_at > ?2 THEN 1 END) FROM hash_queue
             WHERE drive_id IN (SELECT value FROM json_each(?1))",
                )?
                .query_row(params![drives, now], |r| {
                    Ok(HashQueueCounts {
                        total: r.get::<_, i64>(0)? as u64,
                        waiting: r.get::<_, i64>(1)? as u64,
                    })
                })
        })
    }

    /// 手動更新が一時失敗したファイルを、初回バックオフの状態で登録する。
    /// 一覧の世代が変わった場合や対象外になった場合は false を返す。
    pub fn defer_refreshed_hash(
        &mut self,
        entry_id: i64,
        version: i64,
        error: &str,
        now: i64,
    ) -> Result<bool> {
        retry_busy(|| {
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            if super::listing_version(&tx)? != version {
                return Ok(false);
            }
            let delay = crate::hash::hash_retry_delay_ms(1).expect("初回の待機時間");
            let inserted = tx
                .prepare_cached(
                    "INSERT INTO hash_queue(entry_id, drive_id, priority, reason, due_at, delay_ms,
                                        attempts, last_error, updated_at)
                 SELECT id, drive_id, 20, 'retry', ?2, ?3, 1, ?4, ?5 FROM entry
                 WHERE id = ?1 AND kind = 'file' AND cloud_only = 0 AND merged_verified = 0
                 ON CONFLICT(entry_id) DO UPDATE SET
                   priority = min(hash_queue.priority, excluded.priority),
                   reason = excluded.reason, due_at = excluded.due_at, delay_ms = excluded.delay_ms,
                   attempts = excluded.attempts, last_error = excluded.last_error,
                   updated_at = max(hash_queue.updated_at + 1, excluded.updated_at)",
                )?
                .execute(params![
                    entry_id,
                    now.saturating_add(delay),
                    delay,
                    error,
                    now
                ])?;
            if inserted == 0 {
                return Ok(false);
            }
            super::clear_hash_up(&tx, entry_id)?;
            tx.prepare_cached("UPDATE entry SET error = NULL WHERE id = ?1")?
                .execute([entry_id])?;
            tx.commit()?;
            Ok(true)
        })
    }

    /// 既存 DB の索引作成は初回のハッシュ処理まで待つ。
    pub(crate) fn ensure_hash_pending_index(&self) -> Result<()> {
        retry_busy(|| {
            let exists = self
                .conn
                .prepare_cached("SELECT 1 FROM sqlite_schema WHERE name = 'entry_hash_pending'")?
                .exists([])?;
            if !exists {
                self.conn.execute_batch(&format!(
                    "CREATE INDEX IF NOT EXISTS entry_hash_pending ON entry(parent_id) WHERE {UNFINISHED_CHILD}"
                ))?;
            }
            Ok(())
        })
    }

    pub(crate) fn has_unfinished_hash_children(&self, parent: i64) -> Result<bool> {
        self.conn
            .prepare_cached(&format!(
                "SELECT 1 FROM entry INDEXED BY entry_hash_pending WHERE parent_id = ?1
             AND {UNFINISHED_CHILD} LIMIT 1",
            ))?
            .exists([parent])
    }

    pub(crate) fn remove_hash_job(&self, id: i64) -> Result<()> {
        remove_job(&self.conn, id)
    }

    pub(super) fn finish_hash_job(
        &mut self,
        id: i64,
        update: impl Fn(&Connection) -> Result<()>,
    ) -> Result<()> {
        if !self.conn.is_autocommit() {
            update(&self.conn)?;
            return self.remove_hash_job(id);
        }
        retry_busy(|| {
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            update(&tx)?;
            remove_job(&tx, id)?;
            tx.commit()
        })
    }
}

pub(super) fn remove_job(conn: &Connection, id: i64) -> Result<()> {
    conn.prepare_cached("DELETE FROM hash_queue WHERE entry_id = ?1")?
        .execute([id])?;
    Ok(())
}

fn with_drives<T>(drive_ids: &[&str], mut run: impl FnMut(&str) -> Result<T>) -> Result<T> {
    let drives = serde_json::to_string(drive_ids).expect("ドライブ ID を JSON に変換");
    retry_busy(|| run(&drives))
}
