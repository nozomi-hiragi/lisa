//! 端末間で照合するための索引。所在は一致したキーについてだけ返す。
//!
//! 索引は作った時点の写しに固定する。背景の書き込みで DB が進んでも、写しが残っているあいだは
//! 同じ写しからページを出す。所在は今の DB で確かめ、消えた・中身が変わった項目は外す。

use super::*;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const COMPARISON_FORMAT: u32 = 1;
pub const PAGE_SIZE: usize = 4096;
pub const DETAIL_SIZE: usize = 512;

/// 写しを同時に持つ数の上限（依頼元ごとには最新の 1 つ）。
const MAX_SNAPSHOTS: usize = 4;
/// 写しの合計の大きさ（見積もり）の上限。
const MAX_SNAPSHOT_BYTES: usize = 512 << 20;
/// 最後に使ってからこの時間が過ぎた写しは捨てる。
const SNAPSHOT_IDLE: Duration = Duration::from_secs(10 * 60);

// data_version は同じ接続で比較する。DB を書き換えずに、別プロセスの確定も検知する。
struct Probe {
    path: String,
    conn: Connection,
    epoch: uuid::Uuid,
}

static PROBES: Mutex<std::collections::VecDeque<Probe>> =
    Mutex::new(std::collections::VecDeque::new());

fn generation(db: &Db) -> std::result::Result<String, String> {
    let Some(path) = db.conn().path().filter(|path| !path.is_empty()) else {
        return Ok(format!("{}:{}", db.instance, db.conn().total_changes()));
    };
    let mut probes = PROBES.lock().map_err(|e| e.to_string())?;
    let probe = if let Some(position) = probes.iter().position(|probe| probe.path == path) {
        probes.remove(position).expect("位置を確認済み")
    } else {
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|error| error.to_string())?;
        Probe {
            path: path.into(),
            conn,
            epoch: uuid::Uuid::new_v4(),
        }
    };
    let revision = probe
        .conn
        .pragma_query_value(None, "data_version", |row| row.get::<_, i64>(0))
        .map_err(|error| error.to_string())?;
    let stamp = format!("{}:{revision}", probe.epoch);
    probes.push_front(probe);
    probes.truncate(4);
    Ok(stamp)
}

fn verify_generation(db: &Db, expected: &str) -> std::result::Result<(), String> {
    if generation(db)? != expected {
        return Err(GENERATION_CHANGED.into());
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ContentKey {
    pub directory: bool,
    pub size: u64,
    pub hash: Hash,
}

#[derive(Serialize, Deserialize)]
pub struct CatalogPage {
    pub format: u32,
    pub generation: String,
    pub complete: bool,
    pub keys: Vec<ContentKey>,
    pub next: Option<usize>,
    /// 写しのキーの総数（進み具合の表示用）。
    #[serde(default)]
    pub total: usize,
    /// `keys` と同じ並びの、写しでの所在の数。空く量の上限を見積もって、所在を取る順を決める。
    #[serde(default)]
    pub counts: Vec<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Location {
    pub drive_id: String,
    pub chain: Vec<i64>,
    pub file_id: Option<i64>,
    pub parts: Vec<String>,
}

#[derive(Serialize, Deserialize)]
pub struct Locations {
    pub key: ContentKey,
    pub entries: Vec<Location>,
}

/// 写しが失われた（上限や寿命で捨てられた）ときの誤り。
pub const GENERATION_CHANGED: &str = "重複索引の世代が変わりました";

struct Index {
    database: String,
    /// 写しを作ったときの DB の世代。同じなら作り直さずに使い回す。
    source: String,
    /// 写しの ID。ページと所在はこれで同じ写しを指す。
    generation: String,
    owner: String,
    complete: bool,
    keys: Vec<ContentKey>,
    ids: Vec<Vec<i64>>,
    bytes: usize,
}

struct Held {
    index: Arc<Index>,
    used: Instant,
}

static SNAPSHOTS: Mutex<Vec<Held>> = Mutex::new(Vec::new());

const ELIGIBLE: &str = "hash IS NOT NULL AND cloud_only = 0
    AND ((kind = 'file' AND size > 0) OR (kind = 'dir' AND parent_id IS NOT NULL))";

fn database(db: &Db) -> String {
    db.conn().path().unwrap_or_default().to_string()
}

fn snapshots() -> std::result::Result<std::sync::MutexGuard<'static, Vec<Held>>, String> {
    let mut held = SNAPSHOTS.lock().map_err(|e| e.to_string())?;
    held.retain(|held| held.used.elapsed() < SNAPSHOT_IDLE);
    Ok(held)
}

/// `owner` の写しを返す。DB の世代が変わっていなければ使い回し、変わっていれば作り直す。
fn snapshot(db: &Db, owner: &str) -> std::result::Result<Arc<Index>, String> {
    let database = database(db);
    let source = generation(db)?;
    {
        let mut held = snapshots()?;
        if let Some(found) = held
            .iter_mut()
            .find(|held| held.index.database == database && held.index.source == source)
        {
            found.used = Instant::now();
            return Ok(Arc::clone(&found.index));
        }
    }
    let fresh = Arc::new(build(db, database, source, owner)?);
    let mut held = snapshots()?;
    held.retain(|held| !(held.index.database == fresh.database && held.index.owner == owner));
    held.push(Held {
        index: Arc::clone(&fresh),
        used: Instant::now(),
    });
    // 古く使われていないものから捨てる。今作ったもの（最後）は残す。
    held.sort_by_key(|held| held.used);
    while held.len() > MAX_SNAPSHOTS
        || (held.len() > 1
            && held.iter().map(|held| held.index.bytes).sum::<usize>() > MAX_SNAPSHOT_BYTES)
    {
        held.remove(0);
    }
    Ok(fresh)
}

fn lookup(db: &Db, generation: &str) -> std::result::Result<Arc<Index>, String> {
    let database = database(db);
    let mut held = snapshots()?;
    let found = held
        .iter_mut()
        .find(|held| held.index.database == database && held.index.generation == generation)
        .ok_or(GENERATION_CHANGED)?;
    found.used = Instant::now();
    Ok(Arc::clone(&found.index))
}

/// 読み取りトランザクションの中では行をメモリへ写すだけにし、並べ替えと束ねは閉じてから行う。
fn build(
    db: &Db,
    database: String,
    source: String,
    owner: &str,
) -> std::result::Result<Index, String> {
    let read_error = |error: rusqlite::Error| error.to_string();
    let conn = db.conn();
    let read = conn.unchecked_transaction().map_err(read_error)?;
    let complete = signature(db).map_err(read_error)?.is_some();
    let mut rows = Vec::new();
    {
        let shape = Shape::load(conn).map_err(read_error)?;
        let mut statement = conn
            .prepare(&format!(
                "SELECT id, kind, size, hash FROM entry WHERE {ELIGIBLE} ORDER BY id"
            ))
            .map_err(read_error)?;
        let mut cursor = statement.query([]).map_err(read_error)?;
        while let Some(row) = cursor.next().map_err(read_error)? {
            let id: i64 = row.get(0).map_err(read_error)?;
            if shape.excluded[id as usize] {
                continue;
            }
            let directory = row.get_ref(1).map_err(read_error)?.as_str() == Ok("dir");
            let size = if directory {
                shape.total[id as usize]
            } else {
                row.get::<_, i64>(2).map_err(read_error)? as u64
            };
            let hash = row.get_ref(3).map_err(read_error)?.as_blob().ok();
            if size > 0
                && let Some(Ok(hash)) = hash.map(Hash::try_from)
            {
                rows.push((
                    ContentKey {
                        directory,
                        size,
                        hash,
                    },
                    id,
                ));
            }
        }
    }
    drop(read);
    rows.sort_unstable();
    let mut keys: Vec<ContentKey> = Vec::new();
    let mut ids: Vec<Vec<i64>> = Vec::new();
    for (key, id) in rows {
        if keys.last() == Some(&key) {
            ids.last_mut().expect("キーと対").push(id);
        } else {
            keys.push(key);
            ids.push(vec![id]);
        }
    }
    let bytes = keys.len() * size_of::<ContentKey>()
        + ids
            .iter()
            .map(|ids| size_of::<Vec<i64>>() + ids.capacity() * size_of::<i64>())
            .sum::<usize>();
    Ok(Index {
        database,
        source,
        generation: uuid::Uuid::new_v4().to_string(),
        owner: owner.into(),
        complete,
        keys,
        ids,
        bytes,
    })
}

/// 比較索引の 1 ページ。最初のページ（`expected` なし）で写しを作り、続きは同じ写しから出す。
/// `owner` は依頼元（端末 ID など）。依頼元ごとに写しは最新の 1 つだけ持つ。
pub fn catalog_page(
    db: &Db,
    expected: Option<&str>,
    offset: usize,
    owner: &str,
) -> std::result::Result<CatalogPage, String> {
    let index = match expected {
        Some(expected) => lookup(db, expected)?,
        None if offset == 0 => snapshot(db, owner)?,
        None => return Err("続きのページには索引の世代が要ります".into()),
    };
    let start = offset.min(index.keys.len());
    let end = (start + PAGE_SIZE).min(index.keys.len());
    Ok(CatalogPage {
        format: COMPARISON_FORMAT,
        generation: index.generation.clone(),
        complete: index.complete,
        keys: index.keys[start..end].to_vec(),
        next: (end < index.keys.len()).then_some(end),
        total: index.keys.len(),
        counts: index.ids[start..end]
            .iter()
            .map(|ids| ids.len() as u32)
            .collect(),
    })
}

/// 写しで一致した `keys` の所在。今の DB で、消えた・種類や中身が変わった・除外になった項目は外す。
pub fn locations(
    db: &Db,
    expected: &str,
    keys: &[ContentKey],
) -> std::result::Result<Vec<Locations>, String> {
    if keys.len() > DETAIL_SIZE {
        return Err("所在を取得するキーが多すぎます".into());
    }
    use rusqlite::OptionalExtension;
    let read_error = |error: rusqlite::Error| error.to_string();
    let index = lookup(db, expected)?;
    let conn = db.conn();
    let _read = conn.unchecked_transaction().map_err(read_error)?;
    let paths =
        crate::display_path::DisplayPaths::read(db, &Default::default()).map_err(read_error)?;
    let mut current = conn
        .prepare_cached(
            "SELECT drive_id, file_id, kind, size, hash, cloud_only, other_volume
             FROM entry WHERE id = ?1",
        )
        .map_err(read_error)?;
    let mut result = Vec::new();
    for key in keys {
        let mut entries = Vec::new();
        if let Ok(position) = index.keys.binary_search(key) {
            for &id in &index.ids[position] {
                let row = current
                    .query_row([id], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, Option<i64>>(1)?,
                            row.get::<_, String>(2)? == "dir",
                            row.get::<_, i64>(3)? as u64,
                            row.get::<_, Option<Vec<u8>>>(4)?,
                            row.get::<_, bool>(5)? || row.get::<_, bool>(6)?,
                        ))
                    })
                    .optional()
                    .map_err(read_error)?;
                let Some((drive_id, file_id, directory, size, hash, excluded)) = row else {
                    continue;
                };
                // フォルダの大きさは子の合計なので、ハッシュで中身の一致を見る。
                if excluded
                    || directory != key.directory
                    || (!directory && size != key.size)
                    || hash.as_deref() != Some(&key.hash[..])
                {
                    continue;
                }
                let Some(chain) = live_chain(conn, id).map_err(read_error)? else {
                    continue;
                };
                if let Some(parts) = paths.entry(db, id).map_err(read_error)? {
                    entries.push(Location {
                        drive_id,
                        file_id,
                        chain,
                        parts,
                    });
                }
            }
        }
        result.push(Locations {
            key: key.clone(),
            entries,
        });
    }
    Ok(result)
}

/// 今の DB でドライブの一番上から `id` までの番号。途中が消えていれば `None`。
fn live_chain(conn: &Connection, id: i64) -> Result<Option<Vec<i64>>> {
    use rusqlite::OptionalExtension;
    let mut parent = conn.prepare_cached("SELECT parent_id FROM entry WHERE id = ?1")?;
    let mut chain = vec![id];
    let mut current = id;
    while chain.len() <= 4096 {
        match parent
            .query_row([current], |row| row.get::<_, Option<i64>>(0))
            .optional()?
        {
            None => return Ok(None),
            Some(None) => {
                chain.reverse();
                return Ok(Some(chain));
            }
            Some(Some(next)) => {
                chain.push(next);
                current = next;
            }
        }
    }
    Ok(None)
}

/// `copies` か所ある組を 1 つに減らしたときに空く量。並べる順に使う。
pub fn reclaim(key: &ContentKey, copies: usize) -> u64 {
    key.size.saturating_mul(copies.saturating_sub(1) as u64)
}

/// 照合後に親フォルダ配下を外す。番号は端末ごとに独立している。
pub fn compare_locations(
    left: Vec<Locations>,
    right: Vec<Locations>,
    limit: usize,
) -> Vec<(ContentKey, Vec<Location>, Vec<Location>)> {
    let mut right: HashMap<_, _> = right
        .into_iter()
        .map(|list| (list.key, list.entries))
        .collect();
    let dedup = |entries: Vec<Location>| {
        let mut seen = HashSet::new();
        entries
            .into_iter()
            .filter(|entry| {
                entry
                    .file_id
                    .is_none_or(|id| seen.insert((entry.drive_id.clone(), id)))
            })
            .collect::<Vec<_>>()
    };
    let mut groups = Vec::new();
    for list in left {
        if let Some(other) = right.remove(&list.key)
            && !list.entries.is_empty()
            && !other.is_empty()
        {
            groups.push((list.key, list.entries, other));
        }
    }
    let mut left_parents = HashSet::new();
    let mut right_parents = HashSet::new();
    for (key, left, right) in &groups {
        if key.directory {
            left_parents.extend(left.iter().filter_map(|entry| entry.chain.last().copied()));
            right_parents.extend(right.iter().filter_map(|entry| entry.chain.last().copied()));
        }
    }
    let suppress = |entries: &mut Vec<Location>, parents: &HashSet<i64>| {
        entries.retain(|entry| {
            let mut chain = entry.chain.iter();
            chain.next_back();
            !chain.any(|id| parents.contains(id))
        });
    };
    for (_, left, right) in &mut groups {
        suppress(left, &left_parents);
        suppress(right, &right_parents);
        *left = dedup(std::mem::take(left));
        *right = dedup(std::mem::take(right));
    }
    groups.retain(|(_, left, right)| !left.is_empty() && !right.is_empty());
    groups.sort_by_key(|(key, left, right)| {
        (
            std::cmp::Reverse(reclaim(key, left.len() + right.len())),
            key.clone(),
        )
    });
    groups.truncate(limit);
    groups
}

#[derive(Serialize, Deserialize)]
pub struct DeviceGroups {
    pub generation: String,
    pub complete: bool,
    pub groups: Vec<Locations>,
}

pub fn device_groups(db: &Db, limit: usize) -> std::result::Result<DeviceGroups, String> {
    let read_error = |error: rusqlite::Error| error.to_string();
    // groups が初回に索引を作る。読み取りトランザクションに入る前に済ませる。
    db.conn()
        .execute_batch(super::FILE_HASH_INDEX)
        .map_err(read_error)?;
    let current = generation(db)?;
    let _read = db.conn().unchecked_transaction().map_err(read_error)?;
    let complete = signature(db).map_err(read_error)?.is_some();
    let paths =
        crate::display_path::DisplayPaths::read(db, &Default::default()).map_err(read_error)?;
    let mut result = Vec::new();
    for group in groups(db, limit).map_err(read_error)? {
        let mut entries = Vec::new();
        for copy in group.copies {
            if let Some(id) = copy.chain.last()
                && let Some(parts) = paths.entry(db, *id).map_err(read_error)?
            {
                entries.push(Location {
                    drive_id: copy.drive_id,
                    chain: copy.chain,
                    file_id: None,
                    parts,
                });
            }
        }
        result.push(Locations {
            key: ContentKey {
                directory: group.kind == Kind::Dir,
                size: group.size,
                hash: group.hash,
            },
            entries,
        });
    }
    verify_generation(db, &current)?;
    Ok(DeviceGroups {
        generation: current,
        complete,
        groups: result,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    // 写しの置き場は全体で 1 つなので、試験どうしで追い出し合わないように順に走らせる。
    static SERIAL: Mutex<()> = Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn page(db: &Db, generation: Option<&str>, offset: usize) -> CatalogPage {
        catalog_page(db, generation, offset, &db.instance.to_string()).unwrap()
    }

    fn database(name: &str) -> Db {
        let mut db = Db::open_in_memory().unwrap();
        db.this_device(name, name, "test").unwrap();
        db.conn()
            .execute_batch(
                "INSERT INTO drive(id, label, fs, last_seen_at) VALUES('fixture', 'D:', 'test', 0);
             INSERT INTO entry(id, drive_id, parent_id, pos, name, name_nfc, kind, size,
                               cloud_only, other_volume, status, listed, hash)
               VALUES(1, 'fixture', NULL, 0, x'', '', 'dir', 0, 0, 0, '', 1, zeroblob(32));",
            )
            .unwrap();
        db
    }

    fn entry(
        db: &Db,
        id: i64,
        parent: i64,
        kind: &str,
        size: u64,
        seed: u16,
        physical: Option<i64>,
    ) {
        let mut hash = [0; 32];
        hash[..2].copy_from_slice(&seed.to_be_bytes());
        db.conn().execute(
            "INSERT INTO entry(id, drive_id, parent_id, pos, name, name_nfc, kind, size, file_id,
                               cloud_only, other_volume, status, listed, hash)
             VALUES(?1, 'fixture', ?2, ?1, CAST(?1 AS BLOB), '同名', ?3, ?4, ?5, 0, 0, '', 1, ?6)",
            params![id, parent, kind, size as i64, physical, &hash[..]],
        ).unwrap();
    }

    #[test]
    fn excludes_volume_metadata_from_single_and_cross_device_results() {
        let _serial = serial();
        let old = database("旧PC");
        let new = database("新PC");
        for db in [&old, &new] {
            entry(db, 2, 1, "dir", 0, 1, None);
            entry(db, 3, 2, "file", 20, 2, Some(3));
            entry(db, 4, 1, "file", 20, 2, Some(4));
            entry(db, 5, 1, "file", 40, 3, Some(5));
            entry(db, 6, 1, "file", 40, 3, Some(6));
            db.conn()
                .execute("UPDATE entry SET name_nfc = '.fseventsd' WHERE id = 2", [])
                .unwrap();
            let page = page(db, None, 0);
            assert_eq!(page.keys.len(), 2);
            assert!(page.keys.iter().all(|key| !key.directory));
            let found = locations(db, &page.generation, &page.keys).unwrap();
            assert!(
                found
                    .iter()
                    .flat_map(|group| &group.entries)
                    .all(|item| !item.chain.contains(&2))
            );
            let single = device_groups(db, 300).unwrap();
            assert_eq!(single.groups.len(), 1);
            assert_eq!(single.groups[0].key.size, 40);
            assert_eq!(single.groups[0].entries.len(), 2);
        }
    }

    #[test]
    fn matches_singletons_after_paging_from_a_pinned_snapshot() {
        let _serial = serial();
        let old = database("旧PC");
        let new = database("新PC");
        for id in 2..=(PAGE_SIZE as i64 + 101) {
            entry(&old, id, 1, "file", 40, id as u16, Some(id));
        }
        let last = PAGE_SIZE as i64 + 101;
        entry(&new, 2, 1, "file", 40, last as u16, Some(2));
        entry(&new, 3, 1, "file", 40, 9999, Some(3));
        let first = page(&old, None, 0);
        assert_eq!(first.total, PAGE_SIZE + 100);
        assert_eq!(first.counts.len(), first.keys.len());
        assert!(first.counts.iter().all(|&count| count == 1));
        // 背景の書き込みで DB が進んでも、同じ写しから続きが出る。
        entry(&old, last + 1, 1, "file", 40, 9998, Some(last + 1));
        let next = page(&old, Some(&first.generation), first.next.unwrap());
        assert_eq!(next.generation, first.generation);
        assert_eq!(next.next, None);
        let other = page(&new, None, 0);
        assert!(first.keys.iter().all(|key| !other.keys.contains(key)));
        let keys: Vec<_> = next
            .keys
            .into_iter()
            .filter(|key| other.keys.contains(key))
            .collect();
        assert_eq!(keys.len(), 1);
        let left = locations(&old, &first.generation, &keys).unwrap();
        let right = locations(&new, &other.generation, &keys).unwrap();
        assert!(device_groups(&new, 300).unwrap().groups.is_empty());
        let matches = compare_locations(left, right, 300);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].1[0].chain, [1, last]);
        assert_eq!(matches[0].2[0].chain, [1, 2]);

        // 写しのあとで中身が変わった項目は所在から外れ、片側だけになった組は残らない。
        old.conn()
            .execute("UPDATE entry SET hash = zeroblob(32) WHERE id = ?1", [last])
            .unwrap();
        let left = locations(&old, &first.generation, &keys).unwrap();
        assert!(left[0].entries.is_empty());
        let right = locations(&new, &other.generation, &keys).unwrap();
        assert!(compare_locations(left, right, 300).is_empty());
        // 消えた項目も外れる。
        new.conn()
            .execute("DELETE FROM entry WHERE id = 2", [])
            .unwrap();
        assert!(
            locations(&new, &other.generation, &keys).unwrap()[0]
                .entries
                .is_empty()
        );
    }

    #[test]
    fn a_lost_snapshot_reports_a_changed_generation() {
        let _serial = serial();
        let db = database("旧PC");
        entry(&db, 2, 1, "file", 40, 2, Some(2));
        let owner = "依頼元";
        let first = catalog_page(&db, None, 0, owner).unwrap();
        // DB が変わらなければ同じ写しを使い回す。
        assert_eq!(
            catalog_page(&db, None, 0, owner).unwrap().generation,
            first.generation
        );
        // DB が進んでから同じ依頼元が作り直すと、古い写しは捨てられる。
        entry(&db, 3, 1, "file", 40, 3, Some(3));
        let second = catalog_page(&db, None, 0, owner).unwrap();
        assert_ne!(second.generation, first.generation);
        for result in [
            catalog_page(&db, Some(&first.generation), 0, owner).err(),
            locations(&db, &first.generation, &second.keys).err(),
        ] {
            assert_eq!(result.as_deref(), Some(GENERATION_CHANGED));
        }
        // 使われないまま寿命を過ぎた写しも捨てられる。
        for held in SNAPSHOTS.lock().unwrap().iter_mut() {
            held.used -= SNAPSHOT_IDLE + Duration::from_secs(1);
        }
        assert_eq!(
            catalog_page(&db, Some(&second.generation), 0, owner)
                .err()
                .as_deref(),
            Some(GENERATION_CHANGED),
        );
    }

    #[test]
    fn snapshots_stay_within_the_count_limit() {
        let _serial = serial();
        let dbs: Vec<_> = (0..MAX_SNAPSHOTS + 3)
            .map(|n| {
                let db = database("旧PC");
                entry(&db, 2, 1, "file", 40, n as u16, Some(2));
                db
            })
            .collect();
        let pages: Vec<_> = dbs
            .iter()
            .enumerate()
            .map(|(n, db)| catalog_page(db, None, 0, &format!("依頼元{n}")).unwrap())
            .collect();
        assert!(SNAPSHOTS.lock().unwrap().len() <= MAX_SNAPSHOTS);
        // 最新の写しは残り、古いものから捨てられる。
        let (db, last) = (dbs.last().unwrap(), pages.last().unwrap());
        assert!(catalog_page(db, Some(&last.generation), 0, "依頼元").is_ok());
        assert_eq!(
            catalog_page(&dbs[0], Some(&pages[0].generation), 0, "依頼元0")
                .err()
                .as_deref(),
            Some(GENERATION_CHANGED),
        );
        // 繰り返し作っても増え続けない。
        for _ in 0..20 {
            for (n, db) in dbs.iter().enumerate() {
                catalog_page(db, None, 0, &format!("依頼元{n}")).unwrap();
            }
        }
        assert!(SNAPSHOTS.lock().unwrap().len() <= MAX_SNAPSHOTS);
    }

    #[test]
    fn comparison_scopes_parent_suppression_and_physical_identity_to_each_device() {
        let _serial = serial();
        let old = database("旧PC");
        let new = database("新PC");
        for db in [&old, &new] {
            entry(db, 2, 1, "dir", 0, 1, None);
            entry(db, 3, 2, "file", 20, 2, Some(3));
            entry(db, 4, 1, "file", 40, 3, Some(4));
            entry(db, 5, 1, "file", 40, 3, Some(4));
            entry(db, 6, 1, "file", 0, 4, Some(6));
            entry(db, 7, 1, "file", 40, 5, Some(7));
            db.conn()
                .execute("UPDATE entry SET cloud_only = 1 WHERE id = 7", [])
                .unwrap();
        }
        let left = page(&old, None, 0);
        let right = page(&new, None, 0);
        assert_eq!(left.keys.len(), 3);
        let matches = compare_locations(
            locations(&old, &left.generation, &left.keys).unwrap(),
            locations(&new, &right.generation, &right.keys).unwrap(),
            300,
        );
        assert_eq!(matches.len(), 2);
        for (_, left, right) in &matches {
            assert_eq!(left.len(), 1);
            assert_eq!(right.len(), 1);
        }
        assert!(!matches[0].0.directory);
        assert!(matches[1].0.directory);
    }
}
