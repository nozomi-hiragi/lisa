//! 読んだ木を SQLite に残す（`docs/schema.md` の `device`、`drive`、`entry`）。
//!
//! - 端末は、初めて開いたときに UUID を作って `meta` に覚える。
//! - ドライブは OS がくれる ID（`Volume::id`）で持つ。ID の無いドライブは残さない。
//! - `entry` はドライブの一番上（名前が空、親が無い）から木の形で持つ。フォルダの大きさは
//!   残さず、読み出すときに子から足す。子を読み終えたフォルダは `listed` が立つ。
//! - 一覧はフォルダ単位で確定する。ほかの接続からも整理などの書き込みがある。

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::listing::{Entry, Kind, Listing};
use crate::volumes::Volume;

pub use rusqlite::{Error, ErrorCode};

mod hash_queue;
pub(crate) use hash_queue::retry_busy;
pub use hash_queue::{
    HASH_CHANGED, HASH_SCHEDULED, HASH_VISIBLE, HashQueueCounts, HashQueueItem, HashQueueSeed,
};

pub type Result<T> = rusqlite::Result<T>;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS device (
    id           TEXT PRIMARY KEY,
    name         TEXT NOT NULL,
    hostname     TEXT NOT NULL,
    os           TEXT NOT NULL,
    last_seen_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS drive (
    id              TEXT PRIMARY KEY,
    label           TEXT NOT NULL,
    fs              TEXT NOT NULL,
    capacity        INTEGER,
    case_sensitive  INTEGER,
    last_device_id  TEXT,
    last_mount_path TEXT,
    last_seen_at    INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS entry (
    id           INTEGER PRIMARY KEY,
    drive_id     TEXT NOT NULL REFERENCES drive(id),
    parent_id    INTEGER REFERENCES entry(id) ON DELETE CASCADE,
    pos          INTEGER NOT NULL,
    name         BLOB NOT NULL,
    name_nfc     TEXT NOT NULL,
    kind         TEXT NOT NULL,
    size         INTEGER NOT NULL,
    mtime        INTEGER,
    file_id      INTEGER,
    nlink        INTEGER,
    cloud_only   INTEGER NOT NULL,
    other_volume INTEGER NOT NULL,
    status       TEXT NOT NULL,
    error        TEXT,
    listed       INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS tree (
    hash    BLOB PRIMARY KEY,
    algo    TEXT NOT NULL,
    entries BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS snapshot (
    id        INTEGER PRIMARY KEY,
    drive_id  TEXT NOT NULL REFERENCES drive(id),
    root_hash BLOB NOT NULL,
    algo      TEXT NOT NULL,
    prev_id   INTEGER REFERENCES snapshot(id),
    taken_at  INTEGER NOT NULL,
    device_id TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS snapshot_drive ON snapshot(drive_id, id);
CREATE UNIQUE INDEX IF NOT EXISTS entry_child ON entry(parent_id, name);
CREATE UNIQUE INDEX IF NOT EXISTS entry_root ON entry(drive_id) WHERE parent_id IS NULL;
CREATE INDEX IF NOT EXISTS entry_drive ON entry(drive_id);
CREATE TABLE IF NOT EXISTS catalog_match (
    entry_id    INTEGER NOT NULL REFERENCES entry(id) ON DELETE CASCADE,
    catalog_id  TEXT NOT NULL,
    catalog_rev TEXT NOT NULL,
    state       TEXT NOT NULL,      -- 'match' / 'maybe' / 'pending'
    captures    TEXT NOT NULL,      -- JSON のオブジェクト
    matched_at  INTEGER NOT NULL,
    PRIMARY KEY (entry_id, catalog_id)
);
CREATE INDEX IF NOT EXISTS catalog_match_catalog ON catalog_match(catalog_id);
CREATE TABLE IF NOT EXISTS content_check (
    hash     BLOB NOT NULL,
    algo     TEXT NOT NULL,
    rule     BLOB NOT NULL,         -- Content::rule_key() の blake3（32 バイト）
    hit      INTEGER NOT NULL,
    captures TEXT NOT NULL,
    PRIMARY KEY (hash, algo, rule)
);
CREATE TABLE IF NOT EXISTS assignment (
    id         INTEGER PRIMARY KEY,
    drive_id   TEXT NOT NULL,
    path       TEXT NOT NULL,       -- ドライブの一番上から見たパス（NFC、区切りは /、一番上は ''）
    file_id    INTEGER,
    app        TEXT NOT NULL,
    role       TEXT,
    note       TEXT,
    by         TEXT NOT NULL,       -- 'user' / 'mcp'
    created_at INTEGER NOT NULL,
    lost_at    INTEGER
);
CREATE UNIQUE INDEX IF NOT EXISTS assignment_place ON assignment(drive_id, path);
CREATE TABLE IF NOT EXISTS plan (
    id          INTEGER PRIMARY KEY,
    state       TEXT NOT NULL,     -- pending / approved / running / done / partial / rejected / cancelled / expired / undone
    by          TEXT NOT NULL,     -- 'mcp:<クライアント名>' / 'user' / 'cli'
    note        TEXT,              -- LLM が付けた説明
    summary     TEXT NOT NULL,     -- JSON（件数、大きさ、危ないものの数など。作ったときのもの）
    skip_risky  INTEGER NOT NULL DEFAULT 0,
    created_at  INTEGER NOT NULL,
    decided_at  INTEGER,
    finished_at INTEGER,
    purged_at   INTEGER            -- 隔離から OS のゴミ箱へ送った時刻
);
CREATE INDEX IF NOT EXISTS plan_state ON plan(state, id);
CREATE TABLE IF NOT EXISTS peer_plan_request (
    device_id    TEXT NOT NULL,
    request_id   TEXT NOT NULL,
    payload_hash BLOB NOT NULL,
    plan_id      INTEGER NOT NULL REFERENCES plan(id),
    response     TEXT NOT NULL,
    PRIMARY KEY (device_id, request_id)
);
CREATE UNIQUE INDEX IF NOT EXISTS peer_plan_request_plan ON peer_plan_request(plan_id);
CREATE TABLE IF NOT EXISTS peer_restore_request (
    plan_id      INTEGER PRIMARY KEY REFERENCES peer_plan_request(plan_id),
    requested_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS remote_keep_plan (
    plan_id INTEGER PRIMARY KEY REFERENCES plan(id),
    intent  TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS keep_relay_grant (
    target_device TEXT NOT NULL,
    request_id    TEXT NOT NULL,
    secret_hash   BLOB NOT NULL,
    keep          TEXT NOT NULL,
    expires_at    INTEGER NOT NULL,
    counter       INTEGER NOT NULL,
    lease         TEXT,
    revoked       INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (target_device, request_id)
);
CREATE TABLE IF NOT EXISTS keep_reservation (
    token      TEXT PRIMARY KEY,
    path       TEXT NOT NULL,
    expires_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS dismissed_plan (
    plan_id INTEGER PRIMARY KEY REFERENCES plan(id) ON DELETE CASCADE
);
CREATE TABLE IF NOT EXISTS plan_op (
    plan_id   INTEGER NOT NULL REFERENCES plan(id) ON DELETE CASCADE,
    seq       INTEGER NOT NULL,
    op        TEXT NOT NULL,       -- mkdir / move / trash
    drive_id  TEXT,
    src       TEXT NOT NULL,       -- 元の絶対パス（mkdir は作るパス）
    dst       TEXT,                -- move の行き先、trash は隔離した場所（済んでから）
    keep      TEXT,
    size      INTEGER,             -- 作ったときの大きさ（フォルダは中の合計）
    mtime     INTEGER,
    hash      BLOB,
    risky     TEXT,                -- 危ない理由（無ければ NULL）
    state     TEXT NOT NULL,       -- pending / running / done / skipped / failed / undone
    reason    TEXT,                -- skipped・failed の理由
    done_at   INTEGER,
    PRIMARY KEY (plan_id, seq)
);
CREATE TABLE IF NOT EXISTS merge_marker (
    plan_id INTEGER NOT NULL,
    seq INTEGER NOT NULL,
    journal TEXT NOT NULL,
    PRIMARY KEY (plan_id, seq)
);
CREATE TABLE IF NOT EXISTS merged_link (
    link_id TEXT PRIMARY KEY,
    source_device TEXT NOT NULL,
    target_device TEXT NOT NULL,
    target_volume TEXT NOT NULL,
    target_path TEXT NOT NULL,
    generation INTEGER NOT NULL,
    state TEXT NOT NULL,
    marker TEXT NOT NULL,
    relay_device TEXT,
    checked_at INTEGER NOT NULL,
    sync_state TEXT NOT NULL,
    sync_reason TEXT,
    marker_checked_at INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS merged_link_target ON merged_link(target_device, target_volume, target_path);
CREATE TABLE IF NOT EXISTS merged_link_outbox (
    link_id TEXT PRIMARY KEY REFERENCES merged_link(link_id) ON DELETE CASCADE,
    generation INTEGER NOT NULL,
    queued_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS merged_link_outbox_age ON merged_link_outbox(queued_at, link_id);
CREATE TABLE IF NOT EXISTS merged_link_relay (
    link_id TEXT PRIMARY KEY,
    source_device TEXT NOT NULL,
    target_device TEXT NOT NULL,
    request_id TEXT NOT NULL,
    secret_hash BLOB NOT NULL,
    marker_hash BLOB NOT NULL,
    generation INTEGER NOT NULL,
    state TEXT NOT NULL,
    relocation_operation TEXT,
    revoked INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS merged_link_delete (
    plan_id INTEGER NOT NULL,
    seq INTEGER NOT NULL,
    operation_id TEXT NOT NULL UNIQUE,
    request TEXT NOT NULL,
    progress TEXT,
    PRIMARY KEY(plan_id, seq),
    FOREIGN KEY(plan_id, seq) REFERENCES plan_op(plan_id, seq) ON DELETE CASCADE
);
CREATE TABLE IF NOT EXISTS merged_link_receive (
    operation_id TEXT NOT NULL,
    link_id TEXT NOT NULL,
    receipt TEXT NOT NULL,
    touched_at INTEGER NOT NULL,
    PRIMARY KEY(operation_id, link_id)
);
CREATE TABLE IF NOT EXISTS merged_link_transfer_progress (
    operation_id TEXT PRIMARY KEY,
    received INTEGER NOT NULL,
    total INTEGER NOT NULL,
    touched_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS merged_link_move (
    plan_id INTEGER NOT NULL,
    seq INTEGER NOT NULL,
    movements TEXT NOT NULL,
    undo_size INTEGER,
    undo_mtime INTEGER,
    PRIMARY KEY(plan_id,seq),
    FOREIGN KEY(plan_id,seq) REFERENCES plan_op(plan_id,seq) ON DELETE CASCADE
);
CREATE TABLE IF NOT EXISTS merged_link_move_undo (
    plan_id INTEGER NOT NULL,
    seq INTEGER NOT NULL,
    movements TEXT NOT NULL,
    identity TEXT,
    copied_identity TEXT,
    PRIMARY KEY(plan_id,seq),
    FOREIGN KEY(plan_id,seq) REFERENCES plan_op(plan_id,seq) ON DELETE CASCADE
);
CREATE TABLE IF NOT EXISTS merged_link_move_retry (
    operation TEXT NOT NULL,
    link_id TEXT NOT NULL,
    movement TEXT NOT NULL,
    undo INTEGER NOT NULL,
    attempted_at INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY(operation,link_id)
);
CREATE INDEX IF NOT EXISTS merge_marker_published_path
    ON merge_marker(json_extract(journal, '$.state.Published.path'));
CREATE TABLE IF NOT EXISTS merged_link_source_outbox (
    link_id TEXT NOT NULL REFERENCES merged_link(link_id) ON DELETE CASCADE,
    generation INTEGER NOT NULL,
    update_json TEXT NOT NULL,
    queued_at INTEGER NOT NULL,
    PRIMARY KEY(link_id,generation)
);
CREATE TABLE IF NOT EXISTS merged_link_retarget_receipt (
    operation TEXT PRIMARY KEY,
    request_hash BLOB NOT NULL,
    source TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS merged_link_retarget_outbox (
    link_id TEXT NOT NULL REFERENCES merged_link(link_id) ON DELETE CASCADE,
    generation INTEGER NOT NULL,
    relay_device TEXT NOT NULL,
    request TEXT NOT NULL,
    attempted_at INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY(link_id,generation)
);
CREATE TABLE IF NOT EXISTS merged_link_rewrite (
    operation_id TEXT NOT NULL,
    link_id TEXT NOT NULL,
    progress TEXT NOT NULL,
    PRIMARY KEY(operation_id,link_id)
);
CREATE TABLE IF NOT EXISTS merged_link_relocation_target (
    operation_id TEXT PRIMARY KEY,
    marker TEXT NOT NULL,
    generation INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS merged_link_relocation_done (
    operation_id TEXT NOT NULL,
    link_id TEXT NOT NULL,
    marker_hash BLOB NOT NULL,
    PRIMARY KEY(operation_id,link_id)
);
";

/// `Stored` に読む列（`row_to_stored` と同じ順）。
const COLUMNS: &str = "id, name, name_nfc, kind, size, cloud_only, other_volume, error, listed, mtime, hash, merged, merged_verified";

fn row_to_stored(r: &rusqlite::Row) -> Result<Stored> {
    let kind: String = r.get(3)?;
    Ok(Stored {
        id: r.get(0)?,
        name_raw: r.get(1)?,
        name_nfc: r.get(2)?,
        kind: kind_from(&kind),
        size: r.get::<_, i64>(4)? as u64,
        cloud_only: r.get(5)?,
        other_volume: r.get(6)?,
        error: r.get(7)?,
        listed: r.get(8)?,
        mtime: r.get(9)?,
        hash: r
            .get::<_, Option<Vec<u8>>>(10)?
            .and_then(|h| h.try_into().ok()),
        merged: r
            .get::<_, Option<String>>(11)?
            .and_then(|json| serde_json::from_str(&json).ok()),
        merged_verified: r.get(12)?,
        children: Vec::new(),
    })
}

/// DB から読み出したフォルダかファイル（子は入れない。`children` はテストで木を組むときだけ使う）。
#[derive(Debug, Clone, PartialEq)]
pub struct Stored {
    /// `entry.id`。
    pub id: i64,
    pub name_raw: Vec<u8>,
    pub name_nfc: String,
    pub kind: Kind,
    /// ファイルの大きさ。フォルダは 0（子から足す）。
    pub size: u64,
    pub cloud_only: bool,
    pub other_volume: bool,
    /// 子を読み終えた（読めなかったときも含む）。
    pub listed: bool,
    /// 読めなかった理由（フォルダは一覧、ファイルはハッシュ）。
    pub error: Option<String>,
    /// 更新日時（UTC の Unix 時刻、ミリ秒）。
    pub mtime: Option<i64>,
    /// 中身のハッシュ（blake3）。フォルダは子から作る Merkle。まだなら None。
    pub hash: Option<crate::hash::Hash>,
    pub merged: Option<crate::merged::Marker>,
    pub merged_verified: bool,
    /// 子（読んだときの並び = `name::hash_order`）。
    pub children: Vec<Stored>,
}

/// ドライブのスナップショット。
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub id: i64,
    pub root_hash: crate::hash::Hash,
    /// 前のスナップショット（鎖のようにつなぐ）。
    pub prev_id: Option<i64>,
    /// 撮った日時（UTC の Unix 時刻、ミリ秒）。
    pub taken_at: i64,
}

/// 一覧の保存結果。
pub enum ListingSave {
    Unchanged,
    Missing,
    First,
    Changed,
    Retry,
}

pub struct Db {
    pub(crate) instance: uuid::Uuid,
    conn: Connection,
    /// フォルダのパスから `entry.id` を引く（読んだフォルダの子のフォルダを覚えておく）。
    folders: HashMap<PathBuf, i64>,
    folders_version: Option<i64>,
    in_transaction: bool,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        Self::with(Connection::open(path)?)
    }

    /// 中のつなぎ（同じ crate の読み出し用）。
    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }

    /// 中のつなぎ（同じ crate の書き込み用）。
    pub(crate) fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    /// メモリの中だけの DB（テスト用）。
    pub fn open_in_memory() -> Result<Self> {
        Self::with(Connection::open_in_memory()?)
    }

    fn with(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = ON;
             PRAGMA cache_size = -65536; PRAGMA temp_store = MEMORY; PRAGMA busy_timeout = 5000;",
        )?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        conn.execute_batch(hash_queue::SCHEMA)?;
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS entry_merged ON entry(id) WHERE merged IS NOT NULL;",
        )?;
        Ok(Self {
            instance: uuid::Uuid::new_v4(),
            conn,
            folders: HashMap::new(),
            folders_version: None,
            in_transaction: false,
        })
    }

    /// この端末の ID。初めてなら作る。名前などは毎回書き直す。
    pub fn this_device(&mut self, name: &str, hostname: &str, os: &str) -> Result<String> {
        let id: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'this_device'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        let id = match id {
            Some(id) => id,
            None => {
                let id = uuid::Uuid::new_v4().to_string();
                self.conn.execute(
                    "INSERT INTO meta (key, value) VALUES ('this_device', ?1)",
                    [&id],
                )?;
                id
            }
        };
        self.conn.execute(
            "INSERT INTO device (id, name, hostname, os, last_seen_at) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET hostname = ?3, os = ?4, last_seen_at = ?5",
            params![id, name, hostname, os, now_ms()],
        )?;
        Ok(id)
    }

    /// 覚えておく値（`meta` の表）。無ければ None。
    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        self.conn
            .prepare_cached("SELECT value FROM meta WHERE key = ?1")?
            .query_row([key], |r| r.get(0))
            .optional()
    }

    /// 値を覚えておく（`meta` の表）。
    pub fn set_meta(&mut self, key: &str, value: &str) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = ?2",
            )?
            .execute([key, value])?;
        Ok(())
    }

    /// ドライブを書く（無ければ作り、一番上の `entry` も作る）。ID の無いドライブは何もしない。
    pub fn upsert_drive(&mut self, device_id: &str, volume: &Volume) -> Result<()> {
        let Some(id) = &volume.id else {
            return Ok(());
        };
        self.conn.execute(
            "INSERT INTO drive (id, label, fs, capacity, case_sensitive, last_device_id,
                                last_mount_path, last_seen_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO UPDATE SET label = ?2, fs = ?3, capacity = ?4,
                 case_sensitive = ?5, last_device_id = ?6, last_mount_path = ?7, last_seen_at = ?8",
            params![
                id,
                volume.label,
                volume.fs,
                volume.capacity.map(|c| c as i64),
                volume.case_sensitive,
                device_id,
                volume.mount_path.to_string_lossy(),
                now_ms()
            ],
        )?;
        self.conn.execute(
            "INSERT INTO entry (drive_id, parent_id, pos, name, name_nfc, kind, size, cloud_only,
                                other_volume, status, listed)
             SELECT ?1, NULL, 0, x'', '', 'dir', 0, 0, 0, 'unloaded', 0
             WHERE NOT EXISTS (SELECT 1 FROM entry WHERE drive_id = ?1 AND parent_id IS NULL)",
            [id],
        )?;
        Ok(())
    }

    /// ドライブの一番上の行（子は入れない）。ドライブが無ければ None。
    pub fn drive_root(&self, drive_id: &str) -> Result<Option<Stored>> {
        self.conn
            .prepare_cached(&format!(
                "SELECT {COLUMNS} FROM entry WHERE drive_id = ?1 AND parent_id IS NULL"
            ))?
            .query_row([drive_id], row_to_stored)
            .optional()
    }

    /// 1 行（子は入れない）。無ければ None。
    pub fn entry(&self, id: i64) -> Result<Option<Stored>> {
        self.conn
            .prepare_cached(&format!("SELECT {COLUMNS} FROM entry WHERE id = ?1"))?
            .query_row([id], row_to_stored)
            .optional()
    }

    /// フォルダ `id` の子（読んだときの並び。子の子は入れない）。
    pub fn children(&self, id: i64) -> Result<Vec<Stored>> {
        let mut statement = self.conn.prepare_cached(&format!(
            "SELECT {COLUMNS} FROM entry WHERE parent_id = ?1 ORDER BY pos"
        ))?;
        let rows = statement.query_map([id], row_to_stored)?;
        rows.collect()
    }

    /// 親とディスク上の名前から子を 1 件取得する。
    pub fn child_named(&self, parent: i64, name: &[u8]) -> Result<Option<Stored>> {
        self.conn
            .prepare_cached(&format!(
                "SELECT {COLUMNS} FROM entry WHERE parent_id = ?1 AND name = ?2"
            ))?
            .query_row(params![parent, name], row_to_stored)
            .optional()
    }

    /// ファイルかフォルダのハッシュを書く（`size` と `mtime` はハッシュしたときの値）。
    pub fn set_hash(
        &mut self,
        id: i64,
        hash: &crate::hash::Hash,
        size: u64,
        mtime: Option<i64>,
    ) -> Result<()> {
        self.finish_hash_job(id, |conn| {
            conn.prepare_cached(
                "UPDATE entry SET hash = ?2, algo = ?3, hashed_size = ?4, hashed_mtime = ?5,
                     error = NULL WHERE id = ?1",
            )?
            .execute(params![
                id,
                &hash[..],
                crate::hash::ALGO,
                size as i64,
                mtime
            ])?;
            Ok(())
        })
    }

    /// 別の接続が一覧を変えていなければ、読み直したハッシュと祖先の無効化を一緒に確定する。
    pub fn save_refreshed_hash(
        &mut self,
        entry: &Stored,
        version: i64,
        result: std::result::Result<&crate::hash::Hash, &str>,
    ) -> Result<bool> {
        retry_busy(|| {
            let (hash, error) = match result {
                Ok(hash) => (Some(hash), None),
                Err(error) => (None, Some(error)),
            };
            let tx = self.conn.savepoint()?;
            if listing_version(&tx)? != version {
                return Ok(false);
            }
            if entry.hash.as_ref() != hash {
                clear_hash_up(&tx, entry.id)?;
            }
            tx.execute(
                "UPDATE entry SET hash = ?2, algo = ?3, hashed_size = size, hashed_mtime = mtime,
                    error = ?4 WHERE id = ?1",
                params![
                    entry.id,
                    hash.map(|hash| &hash[..]),
                    crate::hash::ALGO,
                    error
                ],
            )?;
            hash_queue::remove_job(&tx, entry.id)?;
            tx.commit()?;
            Ok(true)
        })
    }

    /// 保存済みの子だけでフォルダを集約する。別の接続の書き込みと競合した場合は再試行できる。
    pub fn aggregate_refreshed_folder(&mut self, id: i64) -> Result<bool> {
        retry_busy(|| {
            let tx = self.conn.savepoint()?;
            let Some(folder) = tx
                .prepare_cached(&format!("SELECT {COLUMNS} FROM entry WHERE id = ?1"))?
                .query_row([id], row_to_stored)
                .optional()?
            else {
                return Ok(false);
            };
            if folder.kind != Kind::Dir || !folder.listed || folder.error.is_some() {
                return Ok(false);
            }
            let mut children = {
                let mut statement = tx.prepare_cached(&format!(
                    "SELECT {COLUMNS} FROM entry WHERE parent_id = ?1 ORDER BY pos"
                ))?;
                statement
                    .query_map([id], row_to_stored)?
                    .collect::<Result<Vec<_>>>()?
            };
            let Some(entries) = crate::hash::ready_tree_bytes(&mut children) else {
                return Ok(false);
            };
            let hash = blake3::hash(&entries);
            if folder.hash.as_ref() != Some(hash.as_bytes()) {
                clear_hash_up(&tx, id)?;
            }
            tx.execute(
                "UPDATE entry SET hash = ?2, algo = ?3, hashed_size = 0, hashed_mtime = mtime
                 WHERE id = ?1",
                params![id, &hash.as_bytes()[..], crate::hash::ALGO],
            )?;
            tx.execute(
                "INSERT OR IGNORE INTO tree (hash, algo, entries) VALUES (?1, ?2, ?3)",
                params![&hash.as_bytes()[..], crate::hash::ALGO, entries],
            )?;
            hash_queue::remove_job(&tx, id)?;
            tx.commit()?;
            Ok(true)
        })
    }

    /// 手動更新では長いビジー待ちを避け、短い仕事の切れ目で再試行する。
    pub fn set_busy_timeout(&self, timeout: std::time::Duration) -> Result<()> {
        self.conn.busy_timeout(timeout)
    }

    /// フォルダの中身（`hash::tree_bytes`）を、そのハッシュをキーにして残す（同じ中身は 1 回だけ）。
    pub fn save_tree(&mut self, hash: &crate::hash::Hash, entries: &[u8]) -> Result<()> {
        self.conn
            .prepare_cached("INSERT OR IGNORE INTO tree (hash, algo, entries) VALUES (?1, ?2, ?3)")?
            .execute(params![&hash[..], crate::hash::ALGO, entries])?;
        Ok(())
    }

    /// フォルダの中身（`tree` の表）。無ければ None。
    pub fn tree(&self, hash: &crate::hash::Hash) -> Result<Option<Vec<u8>>> {
        self.conn
            .prepare_cached("SELECT entries FROM tree WHERE hash = ?1")?
            .query_row([&hash[..]], |r| r.get(0))
            .optional()
    }

    /// ハッシュはあるが `tree` の表に中身が無いフォルダ（前の版で作ったもの）の中身を、子の行から
    /// 作って残す。`step` が false を返したらやめる。（残した数）を返す。
    pub fn fill_trees(&mut self, step: &dyn Fn() -> bool) -> Result<u64> {
        let ids: Vec<i64> = self
            .conn
            .prepare(
                "SELECT id FROM entry WHERE kind = 'dir' AND hash IS NOT NULL
                   AND hash NOT IN (SELECT hash FROM tree)",
            )?
            .query_map([], |r| r.get(0))?
            .collect::<Result<_>>()?;
        let mut done = 0;
        self.begin()?;
        for (i, id) in ids.into_iter().enumerate() {
            if !step() {
                break;
            }
            let Some(folder) = self.entry(id)? else {
                continue;
            };
            let Some(hash) = folder.hash else { continue };
            let children = self.children(id)?;
            let parts: Vec<crate::hash::Child> = children
                .iter()
                .map(|c| crate::hash::Child {
                    kind: c.kind,
                    name_nfc: &c.name_nfc,
                    hash: c.hash.as_ref(),
                    size: c.size,
                })
                .collect();
            let entries = crate::hash::tree_bytes(&parts);
            // 子が変わってハッシュが合わなくなっていたら、残さない（あとでハッシュし直す）。
            if *blake3::hash(&entries).as_bytes() == hash {
                self.save_tree(&hash, &entries)?;
                done += 1;
            }
            if i % 2000 == 1999 {
                self.commit()?;
                self.begin()?;
            }
        }
        self.commit()?;
        Ok(done)
    }

    /// ドライブのスナップショットを撮る（一番上のハッシュを、前のスナップショットにつないで残す）。
    /// 一番上のハッシュがまだ無いか、前と同じなら撮らない。撮ったら番号を返す。
    pub fn take_snapshot(&mut self, drive_id: &str) -> Result<Option<i64>> {
        let Some(root) = self.drive_root(drive_id)? else {
            return Ok(None);
        };
        let Some(hash) = root.hash else {
            return Ok(None);
        };
        let last: Option<(i64, Vec<u8>)> = self
            .conn
            .query_row(
                "SELECT id, root_hash FROM snapshot WHERE drive_id = ?1 ORDER BY id DESC LIMIT 1",
                [drive_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if last.as_ref().is_some_and(|(_, h)| h[..] == hash[..]) {
            return Ok(None);
        }
        let device: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'this_device'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        let id = self.conn.query_row(
            "INSERT INTO snapshot (drive_id, root_hash, algo, prev_id, taken_at, device_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) RETURNING id",
            params![
                drive_id,
                &hash[..],
                crate::hash::ALGO,
                last.map(|(id, _)| id),
                now_ms(),
                device.unwrap_or_default()
            ],
            |r| r.get(0),
        )?;
        Ok(Some(id))
    }

    /// ドライブのスナップショット（新しい順）: (番号, 一番上のハッシュ, 前の番号, 撮った日時)。
    pub fn snapshots(&self, drive_id: &str) -> Result<Vec<Snapshot>> {
        self.conn
            .prepare(
                "SELECT id, root_hash, prev_id, taken_at FROM snapshot WHERE drive_id = ?1
                 ORDER BY id DESC",
            )?
            .query_map([drive_id], |r| {
                Ok(Snapshot {
                    id: r.get(0)?,
                    root_hash: r.get::<_, Vec<u8>>(1)?.try_into().unwrap_or([0; 32]),
                    prev_id: r.get(2)?,
                    taken_at: r.get(3)?,
                })
            })?
            .collect()
    }

    /// ファイルをハッシュできなかった理由を書く。
    pub fn set_hash_error(&mut self, id: i64, error: &str) -> Result<()> {
        self.finish_hash_job(id, |conn| {
            conn.prepare_cached("UPDATE entry SET error = ?2 WHERE id = ?1")?
                .execute(params![id, error])?;
            Ok(())
        })
    }

    /// `find_folder` と同じだが、覚えている番号を使わず、書き換えもしない（`&self` で引ける）。
    pub fn find_folder_ref(
        &self,
        drive_id: &str,
        mount: &Path,
        path: &Path,
    ) -> Result<Option<i64>> {
        folder_id_at(&self.conn, drive_id, mount, path)
    }

    /// フォルダ `path`（ドライブ `drive_id` の `mount` の下）の `entry.id`。無ければ None。
    pub fn find_folder(
        &mut self,
        drive_id: &str,
        mount: &Path,
        path: &Path,
    ) -> Result<Option<i64>> {
        self.folder_id(drive_id, mount, path)
    }

    /// まだ読んでいない（中に入って読める）フォルダのパスを、木の深さ優先の順に（読み直しの続き用）。
    pub fn unlisted_folders(&self, drive_id: &str, mount: &Path) -> Result<Vec<PathBuf>> {
        let mut statement = self.conn.prepare(
            "SELECT id FROM entry WHERE drive_id = ?1 AND listed = 0 AND kind = 'dir'
                 AND cloud_only = 0 AND other_volume = 0",
        )?;
        let ids: Vec<i64> = statement
            .query_map([drive_id], |r| r.get(0))?
            .collect::<Result<_>>()?;
        // 祖先をたどって、並びの鍵（一番上からの pos の列）とパスを作る。
        let mut up = self
            .conn
            .prepare_cached("SELECT parent_id, pos, name FROM entry WHERE id = ?1")?;
        let mut parents: HashMap<i64, (Option<i64>, i64, Vec<u8>)> = HashMap::new();
        let mut found: Vec<(Vec<i64>, PathBuf)> = Vec::new();
        for id in ids {
            let mut key = Vec::new();
            let mut names = Vec::new();
            let mut cur = Some(id);
            while let Some(i) = cur {
                let row = match parents.get(&i) {
                    Some(row) => row.clone(),
                    None => {
                        let row: (Option<i64>, i64, Vec<u8>) =
                            up.query_row([i], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
                        parents.insert(i, row.clone());
                        row
                    }
                };
                if row.0.is_some() {
                    key.push(row.1);
                    names.push(row.2);
                }
                cur = row.0;
            }
            key.reverse();
            names.reverse();
            let mut path = mount.to_path_buf();
            for name in names {
                path.push(crate::name::from_raw_bytes(&name));
            }
            found.push((key, path));
        }
        found.sort();
        Ok(found.into_iter().map(|(_, path)| path).collect())
    }

    /// まとめて書き始める。
    pub fn begin(&mut self) -> Result<()> {
        if !self.in_transaction {
            self.conn.execute_batch("BEGIN")?;
            self.in_transaction = true;
        }
        Ok(())
    }

    /// `begin` してまだ確定していないか。
    pub fn in_transaction(&self) -> bool {
        self.in_transaction
    }

    /// まとめて書いたものを確定する。
    pub fn commit(&mut self) -> Result<()> {
        if self.in_transaction {
            self.conn.execute_batch("COMMIT")?;
            self.in_transaction = false;
        }
        Ok(())
    }

    /// ディスクを読む前に取得する。他の接続が確定すると値が変わる。
    pub fn listing_version(&self) -> Result<i64> {
        listing_version(&self.conn)
    }

    /// 読んだ一覧を 1 フォルダずつ確定する。
    pub fn save_listing(
        &mut self,
        drive_id: &str,
        mount: &Path,
        path: &Path,
        result: &std::result::Result<Listing, String>,
    ) -> Result<bool> {
        Ok(matches!(
            self.save_listing_at(drive_id, mount, path, result, None)?,
            ListingSave::Changed
        ))
    }

    /// 読み始めてから別の接続の確定があった場合、必要な書き込みを見送る。
    /// `Retry` のときはディスクから読み直す。
    pub fn save_listing_at(
        &mut self,
        drive_id: &str,
        mount: &Path,
        path: &Path,
        result: &std::result::Result<Listing, String>,
        version: Option<i64>,
    ) -> Result<ListingSave> {
        retry_busy(|| self.try_save_listing_at(drive_id, mount, path, result, version))
    }

    fn try_save_listing_at(
        &mut self,
        drive_id: &str,
        mount: &Path,
        path: &Path,
        result: &std::result::Result<Listing, String>,
        version: Option<i64>,
    ) -> Result<ListingSave> {
        self.refresh_folder_cache()?;
        let tx = self.conn.savepoint()?;
        let Some(folder) = folder_id_at(&tx, drive_id, mount, path)? else {
            tx.commit()?;
            self.folders.remove(path);
            return Ok(ListingSave::Missing);
        };
        let mut folders = Vec::new();
        let saved = apply_listing(&tx, drive_id, folder, result, version, &mut folders);
        match saved {
            Ok(saved @ (ListingSave::Unchanged | ListingSave::First | ListingSave::Changed)) => {
                tx.commit()?;
                self.cache_listing_folders(path, &saved, folders);
                Ok(saved)
            }
            // 比較後に WAL が進んだ場合も、古い一覧を同じまま再試行しない。
            Err(Error::SqliteFailure(error, _))
                if version.is_some()
                    && error.extended_code == rusqlite::ffi::SQLITE_BUSY_SNAPSHOT =>
            {
                Ok(ListingSave::Retry)
            }
            other => other,
        }
    }

    /// ロックの外で読んだ初回一覧を親から順に確定する。DB 世代が変わっていれば空を返す。
    /// 時間の上限に達したら、処理済みの先頭分を確定する。フォルダの途中では区切らない。
    pub fn save_initial_listings<'a>(
        &mut self,
        drive_id: &str,
        mount: &Path,
        version: i64,
        listings: impl IntoIterator<Item = (&'a Path, &'a std::result::Result<Listing, String>)>,
        budget: Duration,
    ) -> Result<Vec<ListingSave>> {
        let mut tx = retry_busy(|| {
            rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)
        })?;
        let started = Instant::now();
        let mut saved = Vec::new();
        if listing_version(&tx)? != version {
            tx.commit()?;
            return Ok(saved);
        }
        for (path, listing) in listings {
            if !saved.is_empty() && started.elapsed() >= budget {
                break;
            }
            let folder_tx = tx.savepoint()?;
            let result = match folder_id_at(&folder_tx, drive_id, mount, path)? {
                Some(folder) => {
                    apply_listing(&folder_tx, drive_id, folder, listing, None, &mut Vec::new())?
                }
                None => ListingSave::Missing,
            };
            folder_tx.commit()?;
            saved.push(result);
        }
        tx.commit()?;
        // この接続の確定では data_version が進まないので、以前のパスキャッシュを破棄する。
        self.folders.clear();
        Ok(saved)
    }

    /// 再試行が続いたときだけ、ほかの接続の書き込み完了を待ってから一段読み直して保存する。
    pub fn save_fresh_listing(
        &mut self,
        drive_id: &str,
        mount: &Path,
        path: &Path,
        read: impl FnOnce() -> std::result::Result<Listing, String>,
    ) -> Result<(ListingSave, std::result::Result<Listing, String>)> {
        self.refresh_folder_cache()?;
        let tx = retry_busy(|| {
            rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)
        })?;
        let folder = folder_id_at(&tx, drive_id, mount, path)?;
        let listing = read();
        let mut folders = Vec::new();
        let saved = match folder {
            Some(folder) => apply_listing(&tx, drive_id, folder, &listing, None, &mut folders)?,
            None => ListingSave::Missing,
        };
        tx.commit()?;
        self.cache_listing_folders(path, &saved, folders);
        Ok((saved, listing))
    }

    fn cache_listing_folders(
        &mut self,
        path: &Path,
        saved: &ListingSave,
        folders: Vec<(&OsStr, i64)>,
    ) {
        self.folders.remove(path);
        if matches!(saved, ListingSave::Changed) {
            self.folders.retain(|cached, _| !cached.starts_with(path));
        }
        self.folders
            .extend(folders.into_iter().map(|(name, id)| (path.join(name), id)));
    }

    pub(crate) fn entry_move_version(&self) -> Result<i64> {
        Ok(self
            .meta("entry_move_version")?
            .and_then(|value| value.parse().ok())
            .unwrap_or(0))
    }

    /// 物理リネーム後に、同じドライブ内で走査済みの項目を引き継ぐ。
    pub(crate) fn move_entry(
        &mut self,
        drive_id: &str,
        mount: &Path,
        src: &Path,
        dst: &Path,
    ) -> Result<()> {
        let parents = move_parents(mount, src, dst);
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::move_entry_in(&tx, drive_id, mount, src, dst, parents)?;
        tx.commit()?;
        self.folders.clear();
        Ok(())
    }

    /// 監視が物理リネームと行の引き継ぎの間に旧行を削除しないよう、書き込み権を先に取る。
    pub(crate) fn rename_preserving_entry(
        &mut self,
        drive_id: &str,
        mount: &Path,
        src: &Path,
        dst: &Path,
        rename: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
    ) -> std::io::Result<Option<Error>> {
        let parents = move_parents(mount, src, dst);
        let result = match self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
        {
            Ok(tx) => {
                rename(src, dst)?;
                Self::move_entry_in(&tx, drive_id, mount, src, dst, parents)
                    .and_then(|()| tx.commit())
            }
            Err(error) => {
                rename(src, dst)?;
                Err(error)
            }
        };
        self.folders.clear();
        Ok(result.err())
    }

    fn move_entry_in(
        conn: &Connection,
        drive_id: &str,
        mount: &Path,
        src: &Path,
        dst: &Path,
        parents: Option<(PathBuf, Vec<std::ffi::OsString>)>,
    ) -> Result<()> {
        let (Some((old_parent, parent_names)), Some(old_name), Some(new_name)) =
            (parents, src.file_name(), dst.file_name())
        else {
            return Ok(());
        };
        let Ok(metadata) = std::fs::symlink_metadata(dst) else {
            return Ok(());
        };
        let new_name = stored_name(dst).unwrap_or_else(|| new_name.to_owned());
        let Some(old_parent) = folder_id_at(conn, drive_id, mount, &old_parent)? else {
            return Ok(());
        };
        let Some((id, old_raw)) = conn
            .query_row(
                "SELECT id, name FROM entry
             WHERE parent_id = ?1 AND (name = ?2 OR name_nfc = ?3) AND file_id IS ?4
               AND (kind != 'file' OR (size = ?5 AND mtime IS ?6))
             ORDER BY name = ?2 DESC LIMIT 1",
                params![
                    old_parent,
                    crate::name::raw_bytes(old_name),
                    crate::name::nfc(old_name),
                    crate::listing::file_id(&metadata).map(|id| id as i64),
                    metadata.len() as i64,
                    crate::listing::mtime_ms(&metadata)
                ],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .optional()?
        else {
            return Ok(());
        };
        let Some(parent) = ensure_move_parent(conn, drive_id, mount, &parent_names)? else {
            return Ok(());
        };
        let raw = crate::name::raw_bytes(&new_name);
        if old_parent == parent && raw == old_raw {
            return Ok(());
        }
        conn.execute(
            "DELETE FROM entry WHERE parent_id = ?1 AND name = ?2 AND id != ?3",
            params![parent, raw, id],
        )?;
        conn.execute(
            "UPDATE entry SET parent_id = ?1, name = ?2, name_nfc = ?3,
                 pos = COALESCE((SELECT MAX(pos) + 1 FROM entry WHERE parent_id = ?1), 0)
             WHERE id = ?4",
            params![parent, raw, crate::name::nfc(&new_name), id],
        )?;
        clear_hash_up_for_move(conn, old_parent)?;
        clear_hash_up_for_move(conn, parent)?;
        bump_entry_move_version(conn)
    }

    /// `id` とその祖先のハッシュを消す（中が変わったので、作り直しが要る）。
    pub fn clear_hash_up(&mut self, id: i64) -> Result<()> {
        clear_hash_up(&self.conn, id)
    }

    /// フォルダの `entry.id`。覚えていなければ、ドライブの一番上から名前でたどる。
    fn folder_id(&mut self, drive_id: &str, mount: &Path, path: &Path) -> Result<Option<i64>> {
        self.refresh_folder_cache()?;
        if let Some(&id) = self.folders.get(path) {
            return Ok(Some(id));
        }
        self.find_folder_ref(drive_id, mount, path)
    }

    fn refresh_folder_cache(&mut self) -> Result<()> {
        let version = self.listing_version()?;
        if self.folders_version != Some(version) {
            self.folders.clear();
            self.folders_version = Some(version);
        }
        Ok(())
    }
}

fn move_parents(
    mount: &Path,
    src: &Path,
    dst: &Path,
) -> Option<(PathBuf, Vec<std::ffi::OsString>)> {
    let old = src.parent()?;
    let new = dst.parent()?;
    let mut resolved = HashMap::new();
    let old_names = stored_components_cached(mount, old, &mut resolved)?;
    let new_names = stored_components_cached(mount, new, &mut resolved)?;
    let old_path = old_names
        .iter()
        .fold(mount.to_owned(), |path, name| path.join(name));
    Some((old_path, new_names))
}

pub(crate) fn stored_components(mount: &Path, path: &Path) -> Option<Vec<std::ffi::OsString>> {
    stored_components_cached(mount, path, &mut HashMap::new())
}

fn stored_components_cached(
    mount: &Path,
    path: &Path,
    resolved: &mut HashMap<PathBuf, std::ffi::OsString>,
) -> Option<Vec<std::ffi::OsString>> {
    let mut current = mount.to_path_buf();
    Some(
        path.strip_prefix(mount)
            .ok()?
            .components()
            .map(|component| {
                current.push(component);
                let name = resolved
                    .entry(current.clone())
                    .or_insert_with(|| {
                        stored_name(&current).unwrap_or_else(|| component.as_os_str().to_owned())
                    })
                    .clone();
                current.set_file_name(&name);
                name
            })
            .collect(),
    )
}

/// 一覧に保存される名前を取得する。別名のハードリンクは除外する。
pub(crate) fn stored_name(path: &Path) -> Option<std::ffi::OsString> {
    let name = path.file_name()?;
    if std::fs::symlink_metadata(path).ok()?.is_symlink() {
        return None;
    }
    let identity = crate::organize::file_identity(path)?;
    let folded = crate::name::nfc(name).to_lowercase();
    let mut alias = None;
    for item in std::fs::read_dir(path.parent()?).ok()?.flatten() {
        let candidate = item.file_name();
        if candidate == name {
            return Some(candidate);
        }
        if crate::name::nfc(&candidate).to_lowercase() == folded
            && crate::organize::file_identity(&item.path()) == Some(identity)
        {
            if alias.is_some() {
                return None;
            }
            alias = Some(candidate);
        }
    }
    alias
}

fn folder_id_at(
    conn: &Connection,
    drive_id: &str,
    mount: &Path,
    path: &Path,
) -> Result<Option<i64>> {
    let Ok(relative) = path.strip_prefix(mount) else {
        return Ok(None);
    };
    let mut id: Option<i64> = conn
        .query_row(
            "SELECT id FROM entry WHERE drive_id = ?1 AND parent_id IS NULL",
            [drive_id],
            |r| r.get(0),
        )
        .optional()?;
    for part in relative.components() {
        let Some(parent) = id else {
            return Ok(None);
        };
        let name = crate::name::raw_bytes(part.as_os_str());
        id = conn
            .prepare_cached("SELECT id FROM entry WHERE parent_id = ?1 AND name = ?2")?
            .query_row(params![parent, name], |r| r.get(0))
            .optional()?;
    }
    Ok(id)
}

fn ensure_move_parent(
    conn: &Connection,
    drive_id: &str,
    mount: &Path,
    parent_names: &[std::ffi::OsString],
) -> Result<Option<i64>> {
    let Some(mut parent) = folder_id_at(conn, drive_id, mount, mount)? else {
        return Ok(None);
    };
    for name in parent_names {
        let raw = crate::name::raw_bytes(name);
        let child = conn
            .query_row(
                "SELECT id FROM entry WHERE parent_id = ?1 AND name = ?2 AND kind = 'dir'",
                params![parent, raw],
                |row| row.get(0),
            )
            .optional()?;
        parent = if let Some(child) = child {
            child
        } else {
            clear_hash_up(conn, parent)?;
            conn.query_row(
                    "INSERT INTO entry (drive_id, parent_id, pos, name, name_nfc, kind, size,
                                        cloud_only, other_volume, status, listed)
                     VALUES (?1, ?2, COALESCE((SELECT MAX(pos) + 1 FROM entry WHERE parent_id = ?2), 0),
                             ?3, ?4, 'dir', 0, 0, 0, 'unloaded', 0) RETURNING id",
                    params![drive_id, parent, raw, crate::name::nfc(name)],
                    |row| row.get(0),
                )?
        };
    }
    Ok(Some(parent))
}

pub(crate) fn save_moved_marker(
    conn: &Connection,
    path: &Path,
    marker: &crate::merged::Marker,
    metadata: &std::fs::Metadata,
) -> Result<()> {
    let drive_id = &marker.source.volume_id;
    let mount: Option<String> = conn
        .query_row(
            "SELECT last_mount_path FROM drive WHERE id=?1",
            [drive_id],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    let (Some(mount), Some(parent), Some(name)) = (mount, path.parent(), path.file_name()) else {
        return Ok(());
    };
    let mount = Path::new(&mount);
    let parent = if let Some(id) = folder_id_at(conn, drive_id, mount, parent)? {
        id
    } else {
        let Some(names) = stored_components(mount, parent) else {
            return Ok(());
        };
        let Some(id) = ensure_move_parent(conn, drive_id, mount, &names)? else {
            return Ok(());
        };
        id
    };
    let raw = crate::name::raw_bytes(name);
    conn.execute(
        "DELETE FROM entry WHERE parent_id=?1 AND name=?2 AND kind<>'file'",
        params![parent, raw],
    )?;
    let json = serde_json::to_string(marker)
        .map_err(|error| Error::ToSqlConversionFailure(Box::new(error)))?;
    let size = metadata.len() as i64;
    let mtime = crate::listing::mtime_ms(metadata);
    let file_id = crate::listing::file_id(metadata).map(|id| id as i64);
    // UPSERT の UPDATE はカタログのトリガー内の OR IGNORE を上書きするため、文を分ける。
    let updated = conn.query_row(
        "UPDATE entry SET size=?3, mtime=?4, file_id=?5, error=NULL, merged=?6, merged_verified=1
         WHERE parent_id=?1 AND name=?2 RETURNING id",
        params![parent, raw, size, mtime, file_id, json],
        |row| row.get(0),
    ).optional()?;
    let id = if let Some(id) = updated {
        id
    } else {
        conn.query_row(
            "INSERT INTO entry (drive_id, parent_id, pos, name, name_nfc, kind, size, mtime,
                                file_id, cloud_only, other_volume, status, listed, merged, merged_verified)
             VALUES (?1, ?2, COALESCE((SELECT MAX(pos) + 1 FROM entry WHERE parent_id = ?2), 0),
                     ?3, ?4, 'file', ?5, ?6, ?7, 0, 0, 'unloaded', 0, ?8, 1) RETURNING id",
            params![drive_id, parent, raw, crate::name::nfc(name), size, mtime, file_id, json],
            |row| row.get(0),
        )?
    };
    clear_hash_up_for_move(conn, id)?;
    bump_entry_move_version(conn)
}

fn bump_entry_move_version(conn: &Connection) -> Result<()> {
    conn.execute(
        "INSERT INTO meta VALUES ('entry_move_version', '1')
         ON CONFLICT(key) DO UPDATE SET value = CAST(value AS INTEGER) + 1",
        [],
    )?;
    Ok(())
}

fn clear_hash_up(conn: &Connection, id: i64) -> Result<()> {
    clear_hash_ancestors(conn, id, None)
}

fn clear_hash_up_for_move(conn: &Connection, id: i64) -> Result<()> {
    clear_hash_ancestors(conn, id, Some(now_ms()))
}

fn clear_hash_ancestors(conn: &Connection, id: i64, enqueue_at: Option<i64>) -> Result<()> {
    let mut cur = Some(id);
    while let Some(i) = cur {
        cur = conn
            .prepare_cached("UPDATE entry SET hash = NULL WHERE id = ?1 RETURNING parent_id")?
            .query_row([i], |r| r.get(0))
            .optional()?
            .flatten();
        if let Some(now) = enqueue_at {
            hash_queue::enqueue(conn, i, HASH_CHANGED, now)?;
        }
    }
    Ok(())
}

fn listing_version(conn: &Connection) -> Result<i64> {
    conn.query_row("PRAGMA main.data_version", [], |r| r.get(0))
}

fn apply_listing<'a>(
    conn: &Connection,
    drive_id: &str,
    folder: i64,
    result: &'a std::result::Result<Listing, String>,
    version: Option<i64>,
    folders: &mut Vec<(&'a OsStr, i64)>,
) -> Result<ListingSave> {
    let (was_listed, old_error): (bool, Option<String>) = conn.query_row(
        "SELECT listed, error FROM entry WHERE id = ?1",
        [folder],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let can_write = || -> Result<bool> {
        Ok(match version {
            Some(version) => listing_version(conn)? == version,
            None => true,
        })
    };
    let listing = match result {
        Err(error) => {
            if was_listed && old_error.as_deref() == Some(error.as_str()) {
                return Ok(ListingSave::Unchanged);
            }
            if !can_write()? {
                return Ok(ListingSave::Retry);
            }
            conn.execute(
                "UPDATE entry SET listed = 1, status = 'failed', error = ?2 WHERE id = ?1",
                params![folder, error],
            )?;
            let changed = was_listed;
            if changed {
                clear_hash_up(conn, folder)?;
            }
            conn.prepare_cached("DELETE FROM hash_queue WHERE entry_id = ?1")?
                .execute([folder])?;
            let parent: Option<i64> = conn
                .prepare_cached("SELECT parent_id FROM entry WHERE id = ?1")?
                .query_row([folder], |row| row.get(0))?;
            if let Some(parent) = parent {
                hash_queue::enqueue(conn, parent, HASH_CHANGED, now_ms())?;
            }
            return Ok(if !was_listed {
                ListingSave::First
            } else {
                ListingSave::Changed
            });
        }
        Ok(listing) => listing,
    };

    let diff = ListingDiff::read(conn, folder, listing, folders)?;
    if was_listed && old_error.is_none() && diff.is_empty() {
        if hash_queue::has_missing_child(conn, folder)? {
            if !can_write()? {
                return Ok(ListingSave::Retry);
            }
            hash_queue::reconcile_missing_children(conn, folder, now_ms())?;
        }
        return Ok(ListingSave::Unchanged);
    }
    if !can_write()? {
        return Ok(ListingSave::Retry);
    }

    let invalidate_hash = old_error.is_some() || diff.hash_changed;
    let now = now_ms();
    let priority = if was_listed {
        HASH_CHANGED
    } else {
        HASH_SCHEDULED
    };
    diff.write(conn, drive_id, folder, folders, priority, now)?;
    hash_queue::reconcile_missing_children(conn, folder, now)?;
    conn.execute(
        "UPDATE entry SET listed = 1, status = 'unloaded', error = NULL WHERE id = ?1",
        [folder],
    )?;
    if was_listed && invalidate_hash {
        clear_hash_up(conn, folder)?;
    }
    hash_queue::enqueue(conn, folder, priority, now)?;
    Ok(if was_listed {
        ListingSave::Changed
    } else {
        ListingSave::First
    })
}

struct OldEntry {
    id: i64,
    pos: i64,
    name_nfc: String,
    kind: String,
    size: i64,
    mtime: Option<i64>,
    file_id: Option<i64>,
    nlink: Option<i64>,
    cloud_only: bool,
    other_volume: bool,
    merged: Option<String>,
    merged_verified: bool,
}

struct ListingRow<'a> {
    pos: i64,
    entry: &'a Entry,
    kind: &'static str,
    size: i64,
    file_id: Option<i64>,
    nlink: Option<i64>,
    merged: Option<String>,
    merged_verified: bool,
}

struct ListingDiff<'a> {
    inserts: Vec<ListingRow<'a>>,
    updates: Vec<(i64, ListingRow<'a>, bool)>,
    deletes: Vec<i64>,
    hash_changed: bool,
}

impl<'a> ListingDiff<'a> {
    fn read(
        conn: &Connection,
        folder: i64,
        listing: &'a Listing,
        folders: &mut Vec<(&'a OsStr, i64)>,
    ) -> Result<Self> {
        let mut old = HashMap::with_capacity(listing.entries.len());
        {
            let mut select = conn.prepare_cached(
                "SELECT name, id, pos, name_nfc, kind, size, mtime, file_id, nlink, cloud_only, other_volume, merged, merged_verified
                 FROM entry WHERE parent_id = ?1",
            )?;
            let mut rows = select.query([folder])?;
            while let Some(r) = rows.next()? {
                old.insert(
                    r.get::<_, Vec<u8>>(0)?,
                    OldEntry {
                        id: r.get(1)?,
                        pos: r.get(2)?,
                        name_nfc: r.get(3)?,
                        kind: r.get(4)?,
                        size: r.get(5)?,
                        mtime: r.get(6)?,
                        file_id: r.get(7)?,
                        nlink: r.get(8)?,
                        cloud_only: r.get(9)?,
                        other_volume: r.get(10)?,
                        merged: r.get(11)?,
                        merged_verified: r.get(12)?,
                    },
                );
            }
        }
        let mut inserts = Vec::new();
        let mut updates = Vec::new();
        let mut hash_changed = false;
        for (pos, entry) in listing.entries.iter().enumerate() {
            let pos = pos as i64;
            let kind = kind_name(entry.kind);
            let size = if entry.kind == Kind::Dir {
                0
            } else {
                entry.size as i64
            };
            let file_id = entry.file_id.map(|i| i as i64);
            let nlink = entry.nlink.map(|n| n as i64);
            let merged_verified = crate::organize::markers::verified(conn, entry)?;
            let merged = entry
                .merged
                .as_ref()
                .and_then(|marker| serde_json::to_string(marker).ok());
            let row = ListingRow {
                merged,
                merged_verified,
                pos,
                entry,
                kind,
                size,
                file_id,
                nlink,
            };
            let Some(previous) = old.remove(&entry.name_raw) else {
                inserts.push(row);
                continue;
            };
            let content_changed = previous.kind != kind
                || previous.size != size
                || previous.mtime != entry.mtime_ms
                || previous.file_id != file_id
                || previous.cloud_only != entry.cloud_only
                || previous.other_volume != entry.other_volume
                || previous.merged != row.merged
                || previous.merged_verified != merged_verified;
            hash_changed |= content_changed;
            if content_changed
                || previous.pos != pos
                || previous.name_nfc != entry.name_nfc
                || previous.nlink != nlink
                || previous.cloud_only != entry.cloud_only
                || previous.other_volume != entry.other_volume
            {
                updates.push((previous.id, row, content_changed));
            }
            if entry.kind == Kind::Dir {
                folders.push((&entry.name, previous.id));
            }
        }
        hash_changed |= !inserts.is_empty() || !old.is_empty();
        Ok(Self {
            inserts,
            updates,
            deletes: old.into_values().map(|entry| entry.id).collect(),
            hash_changed,
        })
    }

    fn is_empty(&self) -> bool {
        self.inserts.is_empty() && self.updates.is_empty() && self.deletes.is_empty()
    }

    fn write(
        self,
        conn: &Connection,
        drive_id: &str,
        folder: i64,
        folders: &mut Vec<(&'a OsStr, i64)>,
        priority: i64,
        now: i64,
    ) -> Result<()> {
        if !self.inserts.is_empty() {
            let mut insert = conn.prepare_cached(
                "INSERT INTO entry (drive_id, parent_id, pos, name, name_nfc, kind, size, mtime,
                                    file_id, nlink, cloud_only, other_volume, status, listed, merged, merged_verified)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 'unloaded', 0, ?13, ?14)
                 RETURNING id",
            )?;
            for ListingRow {
                pos,
                entry,
                kind,
                size,
                file_id,
                nlink,
                merged,
                merged_verified,
            } in self.inserts
            {
                let id = insert.query_row(
                    params![
                        drive_id,
                        folder,
                        pos,
                        entry.name_raw,
                        entry.name_nfc,
                        kind,
                        size,
                        entry.mtime_ms,
                        file_id,
                        nlink,
                        entry.cloud_only,
                        entry.other_volume,
                        merged,
                        merged_verified
                    ],
                    |r| r.get(0),
                )?;
                if entry.kind == Kind::Dir {
                    folders.push((&entry.name, id));
                } else if entry.kind == Kind::File && !entry.cloud_only && !merged_verified {
                    hash_queue::enqueue(conn, id, priority, now)?;
                }
            }
        }
        if !self.updates.is_empty() {
            let mut update = conn.prepare_cached(
                "UPDATE entry SET pos = ?2, name_nfc = ?3, kind = ?4, size = ?5, mtime = ?6,
                 file_id = ?7, nlink = ?8, cloud_only = ?9, other_volume = ?10,
                 hash = CASE WHEN ?11 THEN NULL ELSE hash END,
                 error = CASE WHEN ?11 THEN NULL ELSE error END,
                 listed = CASE WHEN kind = ?4 THEN listed ELSE 0 END,
                 merged = ?12, merged_verified = ?13 WHERE id = ?1",
            )?;
            for (
                id,
                ListingRow {
                    pos,
                    entry,
                    kind,
                    size,
                    file_id,
                    nlink,
                    merged,
                    merged_verified,
                },
                content_changed,
            ) in self.updates
            {
                update.execute(params![
                    id,
                    pos,
                    entry.name_nfc,
                    kind,
                    size,
                    entry.mtime_ms,
                    file_id,
                    nlink,
                    entry.cloud_only,
                    entry.other_volume,
                    content_changed,
                    merged,
                    merged_verified
                ])?;
                if content_changed {
                    if entry.kind == Kind::File && !entry.cloud_only && !merged_verified {
                        hash_queue::enqueue(conn, id, HASH_CHANGED, now)?;
                    } else {
                        conn.prepare_cached("DELETE FROM hash_queue WHERE entry_id = ?1")?
                            .execute([id])?;
                        if entry.kind == Kind::Dir {
                            hash_queue::enqueue(conn, id, HASH_CHANGED, now)?;
                        }
                    }
                }
            }
        }
        if !self.deletes.is_empty() {
            let mut delete = conn.prepare_cached("DELETE FROM entry WHERE id = ?1")?;
            for id in self.deletes {
                delete.execute([id])?;
            }
        }
        Ok(())
    }
}

/// 前の版の DB に、あとから足した列を足す（中身は空のまま）。
fn migrate(conn: &Connection) -> Result<()> {
    let mut statement = conn.prepare("SELECT name FROM pragma_table_info('entry')")?;
    let columns: Vec<String> = statement
        .query_map([], |r| r.get(0))?
        .collect::<Result<_>>()?;
    for (name, kind) in [
        ("hash", "BLOB"),
        ("algo", "TEXT"),
        ("hashed_size", "INTEGER"),
        ("hashed_mtime", "INTEGER"),
        ("merged", "TEXT"),
        ("merged_verified", "INTEGER NOT NULL DEFAULT 0"),
    ] {
        if !columns.iter().any(|c| c == name) {
            conn.execute_batch(&format!("ALTER TABLE entry ADD COLUMN {name} {kind}"))?;
        }
    }
    Ok(())
}

fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::File => "file",
        Kind::Dir => "dir",
        Kind::Symlink => "symlink",
        Kind::Other => "other",
    }
}

fn kind_from(name: &str) -> Kind {
    match name {
        "dir" => Kind::Dir,
        "symlink" => Kind::Symlink,
        "other" => Kind::Other,
        _ => Kind::File,
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::listing::read_dir;
    use std::fs;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("lisa-db-{name}-{}", std::process::id()));
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

    /// ドライブの木を全部組む（テスト用）。
    fn load_tree(db: &Db, drive_id: &str) -> Stored {
        fn fill(db: &Db, node: &mut Stored) {
            node.children = db.children(node.id).unwrap();
            for child in &mut node.children {
                fill(db, child);
            }
        }
        let mut root = db.drive_root(drive_id).unwrap().unwrap();
        fill(db, &mut root);
        root
    }

    fn volume(mount: &Path) -> Volume {
        Volume {
            id: Some("test:1".into()),
            label: "テスト".into(),
            fs: "apfs".into(),
            mount_path: mount.to_path_buf(),
            capacity: Some(100),
            available: Some(50),
            removable: false,
            network: false,
            read_only: false,
            case_sensitive: Some(false),
            hidden: false,
        }
    }

    #[test]
    fn device_id_is_kept() {
        let mut db = Db::open_in_memory().unwrap();
        let a = db.this_device("mac", "mac.local", "macos").unwrap();
        let b = db.this_device("mac", "mac.local", "macos").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn saved_listings_come_back_as_a_tree() {
        let tmp = TempDir::new("tree");
        fs::create_dir(tmp.0.join("sub")).unwrap();
        fs::create_dir(tmp.0.join("unread")).unwrap();
        fs::write(tmp.0.join("a.txt"), b"12345").unwrap();
        fs::write(tmp.0.join("sub/b.txt"), b"xy").unwrap();

        let mut db = Db::open_in_memory().unwrap();
        let device = db.this_device("mac", "mac.local", "macos").unwrap();
        db.upsert_drive(&device, &volume(&tmp.0)).unwrap();
        db.begin().unwrap();
        db.save_listing(
            "test:1",
            &tmp.0,
            &tmp.0,
            &read_dir(&tmp.0).map_err(|e| e.to_string()),
        )
        .unwrap();
        let sub = tmp.0.join("sub");
        db.save_listing(
            "test:1",
            &tmp.0,
            &sub,
            &read_dir(&sub).map_err(|e| e.to_string()),
        )
        .unwrap();
        db.commit().unwrap();

        let root = load_tree(&db, "test:1");
        assert!(root.listed);
        let names: Vec<&str> = root.children.iter().map(|c| c.name_nfc.as_str()).collect();
        assert_eq!(names, ["a.txt", "sub", "unread"]);
        assert_eq!(root.children[0].size, 5);
        let sub = &root.children[1];
        assert!(sub.listed);
        assert_eq!(sub.children.len(), 1);
        assert_eq!(sub.children[0].size, 2);
        assert!(!root.children[2].listed);
    }

    #[test]
    fn children_and_unlisted_folders_one_level_at_a_time() {
        let tmp = TempDir::new("lazy");
        for dir in ["b", "a", "a/x", "a/y"] {
            fs::create_dir(tmp.0.join(dir)).unwrap();
        }
        let mut db = Db::open_in_memory().unwrap();
        let device = db.this_device("mac", "mac.local", "macos").unwrap();
        db.upsert_drive(&device, &volume(&tmp.0)).unwrap();
        let read = |p: &Path| read_dir(p).map_err(|e| e.to_string());
        db.save_listing("test:1", &tmp.0, &tmp.0, &read(&tmp.0))
            .unwrap();
        let a = tmp.0.join("a");
        db.save_listing("test:1", &tmp.0, &a, &read(&a)).unwrap();

        let root = db.drive_root("test:1").unwrap().unwrap();
        assert!(root.listed && root.children.is_empty());
        let top = db.children(root.id).unwrap();
        let names: Vec<&str> = top.iter().map(|c| c.name_nfc.as_str()).collect();
        assert_eq!(names, ["a", "b"]);
        assert_eq!(db.children(top[0].id).unwrap().len(), 2);
        assert_eq!(
            db.find_folder("test:1", &tmp.0, &a).unwrap(),
            Some(top[0].id)
        );
        // 深さ優先の順: a/x、a/y、b。
        assert_eq!(
            db.unlisted_folders("test:1", &tmp.0).unwrap(),
            [a.join("x"), a.join("y"), tmp.0.join("b")]
        );
    }

    #[test]
    fn saving_again_replaces_children() {
        let tmp = TempDir::new("again");
        fs::write(tmp.0.join("old.txt"), b"1").unwrap();
        let mut db = Db::open_in_memory().unwrap();
        let device = db.this_device("mac", "mac.local", "macos").unwrap();
        db.upsert_drive(&device, &volume(&tmp.0)).unwrap();
        let save = |db: &mut Db| {
            db.save_listing(
                "test:1",
                &tmp.0,
                &tmp.0,
                &read_dir(&tmp.0).map_err(|e| e.to_string()),
            )
            .unwrap()
        };
        assert!(!save(&mut db));
        crate::hash::hash_drive(
            &mut db,
            "test:1",
            &tmp.0,
            1,
            &mut |_| {},
            &crate::hash::Pace::FREE,
        )
        .unwrap();
        assert!(db.drive_root("test:1").unwrap().unwrap().hash.is_some());
        fs::remove_file(tmp.0.join("old.txt")).unwrap();
        fs::write(tmp.0.join("new.txt"), b"22").unwrap();
        assert!(save(&mut db));
        let root = load_tree(&db, "test:1");
        let names: Vec<&str> = root.children.iter().map(|c| c.name_nfc.as_str()).collect();
        assert_eq!(names, ["new.txt"]);
        assert!(root.listed);
        assert!(root.hash.is_none());
        assert_eq!(root.children[0].size, 2);
    }

    #[test]
    fn reading_again_marks_changed_files_and_ancestors() {
        let tmp = TempDir::new("reread");
        fs::create_dir(tmp.0.join("sub")).unwrap();
        fs::write(tmp.0.join("sub/a.txt"), b"one").unwrap();
        fs::write(tmp.0.join("sub/b.txt"), b"two").unwrap();
        let mut db = Db::open_in_memory().unwrap();
        let device = db.this_device("mac", "mac.local", "macos").unwrap();
        db.upsert_drive(&device, &volume(&tmp.0)).unwrap();
        let read = |p: &Path| read_dir(p).map_err(|e| e.to_string());
        let sub = tmp.0.join("sub");
        assert!(
            !db.save_listing("test:1", &tmp.0, &tmp.0, &read(&tmp.0))
                .unwrap()
        );
        assert!(
            !db.save_listing("test:1", &tmp.0, &sub, &read(&sub))
                .unwrap()
        );
        crate::hash::hash_drive(
            &mut db,
            "test:1",
            &tmp.0,
            1,
            &mut |_| {},
            &crate::hash::Pace::FREE,
        )
        .unwrap();
        // 変わっていなければ、何も消さない。
        assert!(
            !db.save_listing("test:1", &tmp.0, &sub, &read(&sub))
                .unwrap()
        );
        assert!(db.drive_root("test:1").unwrap().unwrap().hash.is_some());
        // a.txt の大きさを変えると、a.txt と sub と一番上のハッシュが消え、b.txt は残る。
        fs::write(tmp.0.join("sub/a.txt"), b"longer").unwrap();
        let changes_before = db.conn.total_changes();
        assert!(
            db.save_listing("test:1", &tmp.0, &sub, &read(&sub))
                .unwrap()
        );
        // エントリ 4 行と、変わったファイル・親のキュー 2 行だけを書く。
        assert_eq!(db.conn.total_changes() - changes_before, 6);
        let root = db.drive_root("test:1").unwrap().unwrap();
        assert!(root.hash.is_none());
        let sub_row = &db.children(root.id).unwrap()[0];
        assert!(sub_row.hash.is_none());
        let files = db.children(sub_row.id).unwrap();
        assert!(files[0].hash.is_none());
        assert!(files[1].hash.is_some());
    }

    #[test]
    fn initial_batch_rechecks_version_and_rolls_back_before_publishing() {
        let tmp = TempDir::new("initial-batch");
        let mount = tmp.0.join("mount");
        let child = mount.join("child");
        fs::create_dir_all(&child).unwrap();
        fs::write(child.join("first"), b"data").unwrap();
        fs::write(child.join("second"), b"data").unwrap();
        let db_path = tmp.0.join("test.db");
        let mut scan = Db::open(&db_path).unwrap();
        let device = scan.this_device("test", "test.local", "test").unwrap();
        scan.upsert_drive(&device, &volume(&mount)).unwrap();
        let root = Ok(read_dir(&mount).unwrap());
        let nested = Ok(read_dir(&child).unwrap());
        let batch = [(mount.as_path(), &root), (child.as_path(), &nested)];
        let mut writer = Db::open(&db_path).unwrap();
        let version = scan.listing_version().unwrap();
        writer.set_meta("other", "changed").unwrap();
        assert!(
            scan.save_initial_listings("test:1", &mount, version, batch, Duration::MAX)
                .unwrap()
                .is_empty()
        );

        scan.conn
            .execute_batch(
                "CREATE TEMP TRIGGER interrupt_listing BEFORE INSERT ON entry
             WHEN NEW.name_nfc = 'second' BEGIN SELECT RAISE(ABORT, 'interrupt'); END;",
            )
            .unwrap();
        let version = scan.listing_version().unwrap();
        assert!(
            scan.save_initial_listings("test:1", &mount, version, batch, Duration::MAX)
                .is_err()
        );
        let drive = writer.drive_root("test:1").unwrap().unwrap();
        assert!(!drive.listed);
        assert!(writer.children(drive.id).unwrap().is_empty());

        scan.conn
            .execute_batch("DROP TRIGGER interrupt_listing")
            .unwrap();
        let version = scan.listing_version().unwrap();
        assert_eq!(
            scan.save_initial_listings("test:1", &mount, version, batch, Duration::ZERO)
                .unwrap()
                .len(),
            1
        );
        let folders = writer.children(drive.id).unwrap();
        assert!(!folders[0].listed);
        assert!(writer.children(folders[0].id).unwrap().is_empty());
        assert_eq!(
            scan.save_initial_listings("test:1", &mount, version, [batch[1]], Duration::MAX)
                .unwrap()
                .len(),
            1
        );
        assert!(writer.children(drive.id).unwrap()[0].listed);
        assert_eq!(writer.children(folders[0].id).unwrap().len(), 2);
    }

    #[test]
    fn guarded_save_propagates_plain_busy() {
        let tmp = TempDir::new("plain-busy");
        let mount = tmp.0.join("mount");
        fs::create_dir(&mount).unwrap();
        fs::write(mount.join("new"), b"data").unwrap();
        let path = tmp.0.join("test.db");
        let mut scan = Db::open(&path).unwrap();
        let device = scan.this_device("test", "test.local", "test").unwrap();
        scan.upsert_drive(&device, &volume(&mount)).unwrap();
        let writer = Db::open(&path).unwrap();
        writer.conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        scan.conn.execute_batch("PRAGMA busy_timeout = 0").unwrap();
        let version = scan.listing_version().unwrap();
        let saved = scan.save_listing_at(
            "test:1",
            &mount,
            &mount,
            &Ok(read_dir(&mount).unwrap()),
            Some(version),
        );
        assert!(matches!(saved, Err(Error::SqliteFailure(error, _))
            if error.extended_code == rusqlite::ffi::SQLITE_BUSY));
        writer.conn.execute_batch("ROLLBACK").unwrap();
    }

    #[test]
    fn repeated_read_error_does_not_write() {
        let tmp = TempDir::new("same-error");
        let mut scan = Db::open_in_memory().unwrap();
        let device = scan.this_device("test", "test.local", "test").unwrap();
        scan.upsert_drive(&device, &volume(&tmp.0)).unwrap();
        let fail = Err("permission denied".to_string());
        let version = scan.listing_version().unwrap();
        assert!(matches!(
            scan.save_listing_at("test:1", &tmp.0, &tmp.0, &fail, Some(version))
                .unwrap(),
            ListingSave::First
        ));
        let before = scan.conn.total_changes();
        assert!(matches!(
            scan.save_listing_at("test:1", &tmp.0, &tmp.0, &fail, Some(version))
                .unwrap(),
            ListingSave::Unchanged
        ));
        assert_eq!(scan.conn.total_changes(), before);
    }

    #[test]
    fn missing_folder_is_distinct_from_a_saved_listing() {
        let tmp = TempDir::new("missing-folder");
        let mut scan = Db::open_in_memory().unwrap();
        let device = scan.this_device("test", "test.local", "test").unwrap();
        scan.upsert_drive(&device, &volume(&tmp.0)).unwrap();
        let version = scan.listing_version().unwrap();
        let missing = tmp.0.join("missing");
        let listing = Ok(Listing::default());
        let saved = scan
            .save_initial_listings(
                "test:1",
                &tmp.0,
                version,
                [(missing.as_path(), &listing)],
                Duration::MAX,
            )
            .unwrap();
        assert!(matches!(saved.as_slice(), [ListingSave::Missing]));
        assert!(matches!(
            scan.save_listing_at("test:1", &tmp.0, &missing, &listing, Some(version))
                .unwrap(),
            ListingSave::Missing
        ));
    }

    #[test]
    fn unchanged_listing_does_not_take_the_write_lock() {
        let tmp = TempDir::new("no-write");
        let mount = tmp.0.join("mount");
        fs::create_dir(&mount).unwrap();
        fs::write(mount.join("file"), b"data").unwrap();
        let path = tmp.0.join("test.db");
        let mut scan = Db::open(&path).unwrap();
        let device = scan.this_device("test", "test.local", "test").unwrap();
        scan.upsert_drive(&device, &volume(&mount)).unwrap();
        let listing = Ok(read_dir(&mount).unwrap());
        scan.save_listing("test:1", &mount, &mount, &listing)
            .unwrap();
        let writer = Db::open(&path).unwrap();
        writer.conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        scan.conn.execute_batch("PRAGMA busy_timeout = 0").unwrap();

        let version = scan.listing_version().unwrap();
        let before = scan.conn.total_changes();
        assert!(matches!(
            scan.save_listing_at("test:1", &mount, &mount, &listing, Some(version))
                .unwrap(),
            ListingSave::Unchanged
        ));
        assert_eq!(scan.conn.total_changes(), before);
        writer.conn.execute_batch("ROLLBACK").unwrap();
    }

    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    #[test]
    fn an_entry_created_while_waiting_for_the_db_lock_is_not_replaced() {
        for reverse in [false, true] {
            let tmp = TempDir::new("rename-collision");
            let a = tmp.0.join("a");
            let b = tmp.0.join("b");
            fs::create_dir(&a).unwrap();
            fs::create_dir(&b).unwrap();
            let (from, to) = if reverse { (b, a) } else { (a, b) };
            let src = from.join("source");
            let dst = to.join("destination");
            fs::write(&src, b"source").unwrap();
            let db_path = tmp.0.join("test.db");
            let locked = Db::open(&db_path).unwrap();
            let mut mover = Db::open(&db_path).unwrap();
            locked.conn.execute_batch("BEGIN IMMEDIATE").unwrap();
            let (ready, wait) = std::sync::mpsc::channel();
            std::thread::scope(|scope| {
                let src = &src;
                let dst = &dst;
                let mount = &tmp.0;
                let task = scope.spawn(move || {
                    assert!(!dst.exists());
                    ready.send(()).unwrap();
                    mover.rename_preserving_entry(
                        "test:1",
                        mount,
                        src,
                        dst,
                        crate::organize::rename_without_replace,
                    )
                });
                wait.recv().unwrap();
                assert!(!task.is_finished());
                fs::write(dst, b"external").unwrap();
                locked.conn.execute_batch("COMMIT").unwrap();
                assert_eq!(
                    task.join().unwrap().unwrap_err().kind(),
                    std::io::ErrorKind::AlreadyExists
                );
                assert_eq!(fs::read(src).unwrap(), b"source");
                assert_eq!(fs::read(dst).unwrap(), b"external");
            });
        }
    }

    #[test]
    fn scan_cannot_delete_the_row_between_rename_and_hash_preservation() {
        let tmp = TempDir::new("rename-scan-lock");
        let mount = tmp.0.join("mount");
        fs::create_dir(&mount).unwrap();
        let (src, dst) = (mount.join("old"), mount.join("new"));
        fs::write(&src, b"data").unwrap();
        let path = tmp.0.join("test.db");
        let mut writer = Db::open(&path).unwrap();
        let device = writer.this_device("test", "test", "test").unwrap();
        writer.upsert_drive(&device, &volume(&mount)).unwrap();
        writer
            .save_listing("test:1", &mount, &mount, &Ok(read_dir(&mount).unwrap()))
            .unwrap();
        let root = writer.drive_root("test:1").unwrap().unwrap().id;
        let old = writer.children(root).unwrap().remove(0);
        writer
            .set_hash(old.id, &[3; 32], old.size, old.mtime)
            .unwrap();
        let mut scan = Db::open(&path).unwrap();
        scan.conn.busy_timeout(Duration::ZERO).unwrap();

        let warning = writer
            .rename_preserving_entry("test:1", &mount, &src, &dst, |src, dst| {
                fs::rename(src, dst)?;
                let listing = Ok(read_dir(&mount).unwrap());
                let queued = scan.save_listing_at(
                    "test:1",
                    &mount,
                    &mount,
                    &listing,
                    Some(scan.listing_version().unwrap()),
                );
                assert!(
                    matches!(queued, Ok(ListingSave::Retry))
                        || matches!(queued, Err(Error::SqliteFailure(err, _))
                    if err.code == ErrorCode::DatabaseBusy)
                );
                Ok(())
            })
            .unwrap();
        assert!(warning.is_none());
        let renamed = scan.children(root).unwrap().remove(0);
        assert_eq!(renamed.name_nfc, "new");
        assert_eq!(renamed.id, old.id);
        assert_eq!(renamed.hash, Some([3; 32]));
    }

    #[test]
    fn a_committed_rename_cannot_be_reverted_by_an_older_listing() {
        let tmp = TempDir::new("concurrent-rename");
        let mount = tmp.0.join("mount");
        fs::create_dir(&mount).unwrap();
        fs::write(mount.join("old"), b"data").unwrap();
        let path = tmp.0.join("test.db");
        let mut scan = Db::open(&path).unwrap();
        let device = scan.this_device("test", "test.local", "test").unwrap();
        scan.upsert_drive(&device, &volume(&mount)).unwrap();
        let mut writer = Db::open(&path).unwrap();
        let read = || Ok(read_dir(&mount).unwrap());
        scan.save_listing("test:1", &mount, &mount, &read())
            .unwrap();

        let version = scan.listing_version().unwrap();
        fs::write(mount.join("added"), b"also present").unwrap();
        let older_listing = read();
        // 差分ができた時点で、別接続からディスクと一覧の改名を確定する。
        let tx = scan.conn.savepoint().unwrap();
        let root = folder_id_at(&tx, "test:1", &mount, &mount)
            .unwrap()
            .unwrap();
        let mut folders = Vec::new();
        let diff =
            ListingDiff::read(&tx, root, older_listing.as_ref().unwrap(), &mut folders).unwrap();
        assert_eq!(diff.inserts.len(), 1);
        fs::rename(mount.join("old"), mount.join("new")).unwrap();
        writer
            .move_entry("test:1", &mount, &mount.join("old"), &mount.join("new"))
            .unwrap();
        let error = diff
            .write(&tx, "test:1", root, &mut folders, HASH_CHANGED, now_ms())
            .unwrap_err();
        assert!(matches!(
            error,
            Error::SqliteFailure(error, _) if error.extended_code == rusqlite::ffi::SQLITE_BUSY_SNAPSHOT
        ));
        tx.finish().unwrap();

        // 新しいスナップショットを開き直しても、古いディスク一覧は適用できない。
        assert!(matches!(
            scan.save_listing_at("test:1", &mount, &mount, &older_listing, Some(version))
                .unwrap(),
            ListingSave::Retry
        ));
        writer
            .conn
            .execute_batch("PRAGMA busy_timeout = 0")
            .unwrap();
        let (saved, current_listing) = scan.save_fresh_listing("test:1", &mount, &mount, || {
            // 再試行では、ディスクを読む前に別接続の確定を止める。
            let error = writer.set_meta("writer", "blocked").unwrap_err();
            assert!(matches!(
                error,
                Error::SqliteFailure(error, _) if error.code == rusqlite::ErrorCode::DatabaseBusy
            ));
            read()
        }).unwrap();
        assert!(matches!(saved, ListingSave::Changed));
        writer.set_meta("writer", "released").unwrap();
        let rows = scan.children(root).unwrap();
        let entries = &current_listing.unwrap().entries;
        assert_eq!(rows.len(), entries.len());
        for (row, entry) in rows.iter().zip(entries) {
            assert_eq!(row.name_raw, entry.name_raw);
            assert_eq!(row.size, entry.size);
            assert_eq!(row.mtime, entry.mtime_ms);
            assert!(!row.listed);
        }
        assert!(scan.drive_root("test:1").unwrap().unwrap().listed);
    }

    #[test]
    fn snapshots_chain_and_trees_are_kept() {
        let tmp = TempDir::new("snap");
        fs::write(tmp.0.join("a.txt"), b"one").unwrap();
        let mut db = Db::open_in_memory().unwrap();
        let device = db.this_device("mac", "mac.local", "macos").unwrap();
        db.upsert_drive(&device, &volume(&tmp.0)).unwrap();
        let read = |p: &Path| read_dir(p).map_err(|e| e.to_string());
        let hash = |db: &mut Db| {
            crate::hash::hash_drive(
                db,
                "test:1",
                &tmp.0,
                1,
                &mut |_| {},
                &crate::hash::Pace::FREE,
            )
            .unwrap()
        };
        // ハッシュの前は撮れない。
        db.save_listing("test:1", &tmp.0, &tmp.0, &read(&tmp.0))
            .unwrap();
        assert_eq!(db.take_snapshot("test:1").unwrap(), None);
        hash(&mut db);
        let first = db.take_snapshot("test:1").unwrap().unwrap();
        // 変わっていなければ撮らない。
        assert_eq!(db.take_snapshot("test:1").unwrap(), None);
        let root = db.drive_root("test:1").unwrap().unwrap().hash.unwrap();
        let entries = db.tree(&root).unwrap().unwrap();
        assert_eq!(*blake3::hash(&entries).as_bytes(), root);
        // 変えてハッシュし直すと、前につないで撮る。
        fs::write(tmp.0.join("b.txt"), b"two").unwrap();
        assert!(
            db.save_listing("test:1", &tmp.0, &tmp.0, &read(&tmp.0))
                .unwrap()
        );
        hash(&mut db);
        let second = db.take_snapshot("test:1").unwrap().unwrap();
        let list = db.snapshots("test:1").unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, second);
        assert_eq!(list[0].prev_id, Some(first));
        assert!(db.tree(&list[1].root_hash).unwrap().is_some());
        // tree の表が空でも、子の行から作り直せる。
        db.conn.execute_batch("DELETE FROM tree").unwrap();
        assert!(db.fill_trees(&|| true).unwrap() >= 1);
        assert!(db.tree(&list[0].root_hash).unwrap().is_some());
    }

    #[test]
    fn failures_are_kept() {
        let tmp = TempDir::new("fail");
        let mut db = Db::open_in_memory().unwrap();
        let device = db.this_device("mac", "mac.local", "macos").unwrap();
        db.upsert_drive(&device, &volume(&tmp.0)).unwrap();
        db.save_listing("test:1", &tmp.0, &tmp.0, &Err("Permission".into()))
            .unwrap();
        let root = load_tree(&db, "test:1");
        assert!(root.listed);
        assert_eq!(root.error.as_deref(), Some("Permission"));
    }
}
