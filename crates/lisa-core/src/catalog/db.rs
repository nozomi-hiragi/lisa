//! カタログ（型）を DB の木に当てはめて、結果を保存・読み出しする。

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::Read;
use std::path::Path;

use regex::RegexSet;
use rusqlite::{OptionalExtension, params};

use super::content::{Captures, Check, Content, FileFacts};
use super::eval::{Failed, Node, Reader, State, Tree, evaluate};
use super::pattern::{Pattern, Segment};
use super::{Catalog, CatalogSet, Role, Rule};
use crate::db::Db;
use crate::listing::Kind;
use crate::query;

/// DB に保存されたフォルダの木（`catalog::Tree` の実装）。
pub struct DbTree<'a> {
    db: &'a Db,
    cache: RefCell<HashMap<i64, Option<Vec<Node>>>>,
    error: RefCell<Option<rusqlite::Error>>,
}

impl<'a> DbTree<'a> {
    pub fn new(db: &'a Db) -> Self {
        Self {
            db,
            cache: RefCell::new(HashMap::new()),
            error: RefCell::new(None),
        }
    }

    /// これまで調べたフォルダの数。
    pub(crate) fn checked_folders(&self) -> u64 {
        self.cache.borrow().len() as u64
    }

    /// 走査中に DB エラーが起きていれば返す。
    pub(crate) fn take_error(&self) -> rusqlite::Result<()> {
        match self.error.borrow_mut().take() {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    fn load_children(&self, dir: i64) -> rusqlite::Result<Option<Vec<Node>>> {
        let Some(folder) = self.db.entry(dir)? else {
            return Ok(None);
        };
        if !folder.listed {
            // 読まないフォルダ（別ボリューム・クラウドのみ）は空扱いにして、ずっと確かめ中にしない。
            if folder.other_volume || folder.cloud_only {
                return Ok(Some(Vec::new()));
            }
            return Ok(None);
        }
        let rows = self.db.children(dir)?;
        let nodes = rows
            .into_iter()
            .map(|s| Node {
                id: s.id,
                name_nfc: s.name_nfc,
                is_dir: s.kind == Kind::Dir,
                facts: FileFacts {
                    size: s.size,
                    hash: s.hash,
                    cloud_only: s.cloud_only,
                },
            })
            .collect();
        Ok(Some(nodes))
    }
}

impl Tree for DbTree<'_> {
    fn children(&self, dir: i64) -> Option<Vec<Node>> {
        if let Some(cached) = self.cache.borrow().get(&dir) {
            return cached.clone();
        }
        match self.load_children(dir) {
            Ok(children) => {
                self.cache.borrow_mut().insert(dir, children.clone());
                children
            }
            Err(err) => {
                let mut slot = self.error.borrow_mut();
                if slot.is_none() {
                    *slot = Some(err);
                }
                None
            }
        }
    }
}

/// 実ファイルと `content_check` 表から中身を調べる `catalog::Reader` の実装。
pub struct FsReader<'a> {
    db: &'a Db,
    cache_writes: bool,
    read_bytes: u64,
}

impl<'a> FsReader<'a> {
    pub fn new(db: &'a Db, cache_writes: bool) -> Self {
        Self {
            db,
            cache_writes,
            read_bytes: 0,
        }
    }

    /// これまでに実ファイルから読んだ合計バイト数。
    pub fn read_bytes(&self) -> u64 {
        self.read_bytes
    }
}

impl Reader for FsReader<'_> {
    fn head(&mut self, node: &Node, content: &Content) -> Option<Check> {
        if node.facts.cloud_only {
            return Some(Check::Unknown);
        }
        if !content.needs_bytes() {
            return Some(content.check(&node.facts, None));
        }

        let rule_hash = rule_blake3(content);
        if let Some(hash) = node.facts.hash
            && let Ok(Some(cached)) = lookup_content_check(self.db, &hash, &rule_hash)
        {
            return Some(cached);
        }

        let Ok(Some(located)) = query::path_of(self.db, node.id) else {
            return Some(Check::Unknown);
        };
        let Ok(bytes) = read_head(&located.path, content.head_len()) else {
            return Some(Check::Unknown);
        };
        self.read_bytes += bytes.len() as u64;

        let result = content.check(&node.facts, Some(&bytes));
        if self.cache_writes
            && let Some(hash) = node.facts.hash
        {
            let _ = save_content_check(self.db, &hash, &rule_hash, &result);
        }
        Some(result)
    }
}

fn rule_blake3(content: &Content) -> [u8; 32] {
    *blake3::hash(content.rule_key().as_bytes()).as_bytes()
}

fn read_head(path: &Path, head_len: usize) -> std::io::Result<Vec<u8>> {
    let file = File::open(path)?;
    let mut buf = Vec::with_capacity(head_len.min(64 * 1024));
    file.take(head_len as u64).read_to_end(&mut buf)?;
    Ok(buf)
}

fn lookup_content_check(
    db: &Db,
    hash: &[u8; 32],
    rule: &[u8; 32],
) -> rusqlite::Result<Option<Check>> {
    let row: Option<(i64, String)> = db
        .conn()
        .prepare_cached(
            "SELECT hit, captures FROM content_check
             WHERE hash = ?1 AND algo = ?2 AND rule = ?3",
        )?
        .query_row(params![&hash[..], crate::hash::ALGO, &rule[..]], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .optional()?;
    let Some((hit, captures_json)) = row else {
        return Ok(None);
    };
    if hit == 0 {
        return Ok(Some(Check::Miss));
    }
    match serde_json::from_str::<Captures>(&captures_json) {
        Ok(caps) => Ok(Some(Check::Hit(caps))),
        Err(_) => Ok(None),
    }
}

fn save_content_check(
    db: &Db,
    hash: &[u8; 32],
    rule: &[u8; 32],
    result: &Check,
) -> rusqlite::Result<()> {
    let (hit, captures_json) = match result {
        Check::Hit(caps) => (
            1i64,
            serde_json::to_string(caps).unwrap_or_else(|_| "{}".to_string()),
        ),
        Check::Miss => (0i64, "{}".to_string()),
        Check::Unknown => return Ok(()),
    };
    db.conn()
        .prepare_cached(
            "INSERT OR REPLACE INTO content_check (hash, algo, rule, hit, captures)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?
        .execute(params![
            &hash[..],
            crate::hash::ALGO,
            &rule[..],
            hit,
            captures_json
        ])?;
    Ok(())
}

/// 当たったフォルダの 1 件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub entry_id: i64,
    pub catalog_id: String,
    pub state: State,
    pub captures: Captures,
}

/// 探索の進み具合と読み取り量の記録。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Progress {
    pub folders_checked: u64,
    pub candidates: u64,
    pub bytes_read: u64,
}

/// 外れた候補のうち、外れた `match` が 1 つだけのもの（`--near` 用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NearMiss {
    pub entry_id: i64,
    pub catalog_id: String,
    /// 外れた `match` の番号（0 始まり）。
    pub failed_match: usize,
}

struct Anchor {
    catalog_idx: usize,
    regex_str: String,
    want_dir: bool,
}

/// 型ごとに、候補を絞るための代表 `match` を 1 つ選ぶ（`**` でなく、なるべく文字そのもののもの）。
fn pick_anchor_rule(cat: &Catalog) -> Option<&Rule> {
    cat.matches
        .iter()
        .filter_map(|rule| match rule.path.first() {
            Segment::Name(m) => {
                let is_lit = m.literal().is_some();
                let lit_len = m.literal().map_or(0, str::len);
                Some(((is_lit, lit_len), rule))
            }
            Segment::Any => None,
        })
        .max_by_key(|(score, _)| *score)
        .map(|(_, rule)| rule)
}

fn rule_anchor(catalog_idx: usize, rule: &Rule) -> Option<Anchor> {
    let regex_str = rule.path.first_segment_regex()?;
    // 2 段以上なら 1 段目は途中のフォルダ、1 段ならパターン末尾の / で種類が決まる。
    let want_dir = rule.path.segments().len() > 1 || rule.path.is_dir();
    Some(Anchor {
        catalog_idx,
        regex_str,
        want_dir,
    })
}

fn anchors(set: &CatalogSet) -> Vec<Anchor> {
    set.catalogs
        .iter()
        .enumerate()
        .filter_map(|(idx, loaded)| rule_anchor(idx, pick_anchor_rule(&loaded.catalog)?))
        .collect()
}

fn collect_candidates(
    db: &Db,
    anchors: &[Anchor],
    drive_id: Option<&str>,
    shape: Option<&mut FolderShape>,
    step: &dyn Fn() -> bool,
) -> rusqlite::Result<Option<Vec<(i64, usize)>>> {
    if !step() {
        return Ok(None);
    }
    if anchors.is_empty() && shape.is_none() {
        return Ok(Some(Vec::new()));
    }
    let regexes = anchor_regexes(anchors);
    let mut seen = HashSet::new();
    let mut shape = shape;
    let finished = scan_entries(db, drive_id, step, |id, parent, name, kind, size| {
        if let Some(shape) = shape.as_deref_mut() {
            shape.add(id, parent, kind, size)?;
        }
        collect_anchor_hits(&mut seen, anchors, &regexes, parent, name, kind == "dir");
        Ok(())
    })?;
    Ok(finished.then(|| sorted_candidates(seen)))
}

fn anchor_regexes(anchors: &[Anchor]) -> RegexSet {
    let patterns = anchors.iter().map(|anchor| &anchor.regex_str);
    RegexSet::new(patterns).expect("Pattern の正規表現は解析済み")
}

fn collect_anchor_hits(
    seen: &mut HashSet<(i64, usize)>,
    anchors: &[Anchor],
    regexes: &RegexSet,
    parent: i64,
    name: &str,
    is_dir: bool,
) {
    for index in regexes.matches(name).iter() {
        let anchor = &anchors[index];
        if anchor.want_dir == is_dir {
            seen.insert((parent, anchor.catalog_idx));
        }
    }
}

fn sorted_candidates(seen: HashSet<(i64, usize)>) -> Vec<(i64, usize)> {
    let mut candidates: Vec<_> = seen.into_iter().collect();
    candidates.sort_unstable();
    candidates
}

fn scan_entries(
    db: &Db,
    drive_id: Option<&str>,
    step: &dyn Fn() -> bool,
    mut visit: impl FnMut(i64, i64, &str, &str, i64) -> rusqlite::Result<()>,
) -> rusqlite::Result<bool> {
    let sql = match drive_id {
        Some(_) => {
            "SELECT id, parent_id, name_nfc, kind, size FROM entry
             WHERE drive_id = ?3 AND id > ?1 AND id <= ?2 AND parent_id IS NOT NULL
             ORDER BY id LIMIT 512"
        }
        None => {
            "SELECT id, parent_id, name_nfc, kind, size FROM entry
             WHERE id > ?1 AND id <= ?2 AND parent_id IS NOT NULL ORDER BY id LIMIT 512"
        }
    };
    let conn = db.conn();
    let end = conn.query_row("SELECT COALESCE(MAX(id), 0) FROM entry", [], |row| {
        row.get::<_, i64>(0)
    })?;
    let mut stmt = conn.prepare(sql)?;
    let mut after = i64::MIN;
    loop {
        // 1 ページずつ読み取りを終え、読み取りトランザクションを持ったまま待たない。
        {
            let mut rows = match drive_id {
                Some(id) => stmt.query(params![after, end, id])?,
                None => stmt.query(params![after, end])?,
            };
            let mut found = false;
            while let Some(row) = rows.next()? {
                after = row.get(0)?;
                visit(
                    after,
                    row.get(1)?,
                    row.get_ref(2)?.as_str()?,
                    row.get_ref(3)?.as_str()?,
                    row.get(4)?,
                )?;
                found = true;
            }
            if !found {
                return Ok(true);
            }
        }
        if !step() {
            return Ok(false);
        }
    }
}

/// 全部のドライブ（`drive_id` を渡せばそのドライブだけ）の当たりを探す。DB は読むだけ
/// （`content_check` への書き込みは `write_cache` が true のときだけ）。
/// `step` は数百件ごとに呼び、false が返ったらやめて None を返す。
pub fn find(
    db: &Db,
    set: &CatalogSet,
    drive_id: Option<&str>,
    write_cache: bool,
    step: &dyn Fn() -> bool,
) -> rusqlite::Result<Option<(Vec<Found>, Progress)>> {
    let anchors = anchors(set);

    let Some(candidates) = collect_candidates(db, &anchors, drive_id, None, step)? else {
        return Ok(None);
    };

    let tree = DbTree::new(db);
    let mut reader = FsReader::new(db, write_cache);
    let mut found = Vec::new();
    let total_candidates = candidates.len() as u64;

    for (i, (parent_id, cat_idx)) in candidates.into_iter().enumerate() {
        if i % 256 == 255 && !step() {
            return Ok(None);
        }
        let cat = &set.catalogs[cat_idx].catalog;
        let outcome = evaluate(cat, &tree, parent_id, &mut reader);
        tree.take_error()?;
        if outcome.state != State::Miss {
            found.push(Found {
                entry_id: parent_id,
                catalog_id: cat.id.clone(),
                state: outcome.state,
                captures: outcome.captures,
            });
        }
    }

    let found = drop_inside_cache(db, set, &tree, found)?;
    let progress = Progress {
        folders_checked: tree.checked_folders(),
        candidates: total_candidates,
        bytes_read: reader.read_bytes(),
    };
    Ok(Some((found, progress)))
}

fn read_mounts(db: &Db) -> rusqlite::Result<Vec<(String, Option<String>, bool)>> {
    db.conn()
        .prepare_cached("SELECT id, last_mount_path FROM drive ORDER BY id")?
        .query_map([], |row| {
            let path: Option<String> = row.get(1)?;
            let seen = path.as_deref().is_some_and(|p| Path::new(p).exists());
            Ok((row.get(0)?, path, seen))
        })?
        .collect()
}

/// 変わったフォルダがこれより多ければ、差分ではなく全体を当て直す。
const DIRTY_FULL_LIMIT: i64 = 2048;

/// 自動更新の前回結果。cache の中を除く前の結果も保持する。
#[derive(Default)]
pub struct IncrementalMatches {
    raw: BTreeMap<(i64, usize), Found>,
    initialized: bool,
    shape: FolderShape,
    /// 前回のドライブごとの場所と、そこが見えていたか。
    mounts: Vec<(String, Option<String>, bool)>,
}

impl IncrementalMatches {
    /// 変更が DB に書かれたとき、影響するフォルダを同じトランザクションで記録する。
    /// ハッシュは見ない。中身が変われば大きさか更新日時も変わり、ハッシュの有無で当てはめの結果は
    /// 変わらない（読めたときは同じ中身を読む）。ハッシュを付けている間に全体の当て直しにならないように。
    fn install(db: &Db) -> rusqlite::Result<()> {
        db.conn().execute_batch(
            "CREATE TABLE IF NOT EXISTS catalog_dirty (id INTEGER PRIMARY KEY);
             CREATE TRIGGER IF NOT EXISTS catalog_dirty_insert AFTER INSERT ON entry BEGIN
                 INSERT OR IGNORE INTO catalog_dirty SELECT new.parent_id WHERE new.parent_id IS NOT NULL;
             END;
             CREATE TRIGGER IF NOT EXISTS catalog_dirty_delete AFTER DELETE ON entry BEGIN
                 INSERT OR IGNORE INTO catalog_dirty SELECT old.parent_id WHERE old.parent_id IS NOT NULL;
                 INSERT OR IGNORE INTO catalog_dirty SELECT old.id WHERE old.kind = 'dir';
             END;
             CREATE TRIGGER IF NOT EXISTS catalog_dirty_update AFTER UPDATE OF parent_id, name_nfc,
                 kind, listed, size, mtime, cloud_only, other_volume ON entry
             WHEN old.parent_id IS NOT new.parent_id OR old.name_nfc IS NOT new.name_nfc
                 OR old.kind IS NOT new.kind OR old.listed IS NOT new.listed OR old.size IS NOT new.size
                 OR old.mtime IS NOT new.mtime
                 OR old.cloud_only IS NOT new.cloud_only OR old.other_volume IS NOT new.other_volume
             BEGIN
                 INSERT OR IGNORE INTO catalog_dirty SELECT old.parent_id WHERE old.parent_id IS NOT NULL;
                 INSERT OR IGNORE INTO catalog_dirty SELECT new.parent_id WHERE new.parent_id IS NOT NULL;
                 INSERT OR IGNORE INTO catalog_dirty SELECT new.id WHERE old.kind = 'dir' OR new.kind = 'dir';
             END;",
        )
    }

    /// 最初と明示的な更新では全体を見て、通常は変更があったフォルダとその親だけを当て直す。
    pub fn refresh(
        &mut self,
        db: &Db,
        set: &CatalogSet,
        full: bool,
    ) -> rusqlite::Result<Vec<Found>> {
        // ドライブを付け外しすると、entry が変わらなくても中身を読めるかが変わるので全体を当て直す。
        let mounts = read_mounts(db)?;
        // 走査の直後など変わったフォルダが多いときは、1 つずつ読むより全体を読む方が速い。
        let full = full
            || !self.initialized
            || mounts != self.mounts
            || self.dirty_count(db)? > DIRTY_FULL_LIMIT;
        // 差分を取り出した後で失敗したら、次は全体を読んで取りこぼしを埋める。
        self.initialized = false;
        let anchors = anchors(set);
        let candidates = if full {
            Self::install(db)?;
            db.conn().execute("DELETE FROM catalog_dirty", [])?;
            self.raw.clear();
            self.shape = FolderShape::default();
            collect_candidates(db, &anchors, None, Some(&mut self.shape), &|| true)?
                .unwrap_or_default()
        } else {
            self.changed_candidates(db, &anchors)?
        };
        self.shape.read_roots(db)?;
        let tree = DbTree::new(db);
        let mut reader = FsReader::new(db, true);
        for (parent, catalog) in candidates {
            let cat = &set.catalogs[catalog].catalog;
            let outcome = evaluate(cat, &tree, parent, &mut reader);
            tree.take_error()?;
            if outcome.state != State::Miss {
                self.raw.insert(
                    (parent, catalog),
                    Found {
                        entry_id: parent,
                        catalog_id: cat.id.clone(),
                        state: outcome.state,
                        captures: outcome.captures,
                    },
                );
            }
        }
        let result = drop_inside_cache(db, set, &tree, self.raw.values().cloned().collect())?;
        tree.take_error()?;
        self.mounts = mounts;
        self.initialized = true;
        Ok(result)
    }

    fn dirty_count(&self, db: &Db) -> rusqlite::Result<i64> {
        db.conn()
            .query_row("SELECT COUNT(*) FROM catalog_dirty", [], |row| row.get(0))
    }

    /// ドライブのハッシュに表れない変更（ドライブの付け外しも）も、次の自動更新で反映する。
    pub fn has_changes(&self, db: &Db) -> rusqlite::Result<bool> {
        if !self.initialized || read_mounts(db)? != self.mounts {
            return Ok(true);
        }
        db.conn()
            .query_row("SELECT EXISTS (SELECT 1 FROM catalog_dirty)", [], |row| {
                row.get(0)
            })
    }

    /// フォルダの中のファイルの大きさの合計を、取得済みの親子関係と大きさから求める
    /// （`query::folder_total` の大きさと同じ）。
    pub fn folder_size(&self, folder: i64) -> u64 {
        self.shape.total_size(folder)
    }

    /// 大きな未分類フォルダを、取得済みの親子関係と大きさから求める。
    pub fn unclassified(
        &self,
        matched: &HashSet<i64>,
        min_size: u64,
        limit: usize,
    ) -> Vec<(i64, u64)> {
        self.shape.unclassified(matched, None, min_size, limit)
    }

    fn changed_candidates(
        &mut self,
        db: &Db,
        anchors: &[Anchor],
    ) -> rusqlite::Result<Vec<(i64, usize)>> {
        let conn = db.conn();
        let count = self.dirty_count(db)?;
        let mut take = conn.prepare_cached(
            "DELETE FROM catalog_dirty
             WHERE id IN (SELECT id FROM catalog_dirty ORDER BY id LIMIT 512)
             RETURNING id",
        )?;
        let mut dirty: Vec<i64> = Vec::new();
        // 書き込みを 1 ページずつ終える。更新中に追加された残りは次回に回す。
        for _ in 0..(count as u64).div_ceil(512) {
            let mut rows = take.query([])?;
            while let Some(row) = rows.next()? {
                dirty.push(row.get(0)?);
            }
        }
        let mut affected = HashSet::new();
        let mut lookup = conn.prepare_cached("SELECT parent_id FROM entry WHERE id = ?1")?;
        for id in dirty {
            self.shape.read_folder(db, id)?;
            let mut next = Some(id);
            while let Some(folder) = next {
                if !affected.insert(folder) {
                    break;
                }
                next = lookup
                    .query_row([folder], |row| row.get::<_, Option<i64>>(0))
                    .optional()?
                    .flatten();
            }
        }
        // 確かめ中・たぶんの当たりは、ドライブの付け外しなど entry に表れない変化で結果が変わるので
        // 毎回当て直す。
        let mut candidates: HashSet<(i64, usize)> = self
            .raw
            .iter()
            .filter(|(_, found)| found.state != State::Match)
            .map(|(&key, _)| key)
            .collect();
        // 消えたフォルダは当て直さない（読めないフォルダとして確かめ中に戻ってしまう）。
        let mut exists = conn.prepare_cached("SELECT 1 FROM entry WHERE id = ?1")?;
        let mut gone = Vec::new();
        for &key @ (folder, _) in &candidates {
            if !exists.exists([folder])? {
                gone.push(key);
            }
        }
        for key in &gone {
            candidates.remove(key);
            self.raw.remove(key);
        }
        self.raw
            .retain(|key @ (folder, _), _| !affected.contains(folder) && !candidates.contains(key));
        if anchors.is_empty() {
            return Ok(sorted_candidates(candidates));
        }
        let regexes = anchor_regexes(anchors);
        let mut children =
            conn.prepare_cached("SELECT name_nfc, kind FROM entry WHERE parent_id = ?1")?;
        for folder in affected {
            let mut rows = children.query([folder])?;
            while let Some(row) = rows.next()? {
                let name = row.get_ref(0)?.as_str()?;
                let is_dir = row.get_ref(1)?.as_str()? == "dir";
                collect_anchor_hits(&mut candidates, anchors, &regexes, folder, name, is_dir);
            }
        }
        Ok(sorted_candidates(candidates))
    }
}

/// ほかの当たりの `cache` の中にある当たりを除く（`node_modules` の中の 1 つ 1 つのパッケージなど。
/// 作り直せるものの中なので、並べても役に立たない）。`install` の中（Steam のゲームなど）は残す。
fn drop_inside_cache(
    db: &Db,
    set: &CatalogSet,
    tree: &DbTree,
    found: Vec<Found>,
) -> rusqlite::Result<Vec<Found>> {
    let mut caches = HashSet::new();
    for f in &found {
        let Some(loaded) = set.get(&f.catalog_id) else {
            continue;
        };
        if !loaded.catalog.parts.iter().any(|p| p.role == Role::Cache) {
            continue;
        }
        for (entry, role) in super::parts(&loaded.catalog, tree, f.entry_id) {
            if role == Role::Cache {
                caches.insert(entry);
            }
        }
    }
    if caches.is_empty() {
        return Ok(found);
    }
    let mut parent = db
        .conn()
        .prepare_cached("SELECT parent_id FROM entry WHERE id = ?1")?;
    let mut kept = Vec::with_capacity(found.len());
    for f in found {
        let mut cur: Option<i64> = Some(f.entry_id);
        let mut inside = false;
        while let Some(id) = cur {
            if caches.contains(&id) {
                inside = true;
                break;
            }
            cur = parent
                .query_row([id], |r| r.get::<_, Option<i64>>(0))
                .optional()?
                .flatten();
        }
        if !inside {
            kept.push(f);
        }
    }
    Ok(kept)
}

/// どの当たり（`matched` の `entry.id`）にも入らない、大きなフォルダ（`docs/catalog.md` の
/// `lisa_unclassified`）。大きさは当たったフォルダの分を除いたもの。大きなものを含む一番深い
/// フォルダ（その中に `min_size` 以上の子フォルダが無いもの）を、大きい順に `limit` 個返す。
/// `under` があればその下だけ。DB は読むだけ。
pub fn unclassified(
    db: &Db,
    matched: &HashSet<i64>,
    under: Option<i64>,
    min_size: u64,
    limit: usize,
) -> rusqlite::Result<Vec<(i64, u64)>> {
    Ok(FolderShape::load(db)?.unclassified(matched, under, min_size, limit))
}

/// フォルダの直下のサイズと子フォルダ。自動更新では変わった親の分だけ読み直す。
#[derive(Default)]
struct FolderShape {
    /// 直下のフォルダ以外（リンクなども含む）の大きさの合計。未分類の計算に使う。
    files: HashMap<i64, i64>,
    /// 直下のファイルだけの大きさの合計（`query::folder_total` と同じ数え方）。
    file_bytes: HashMap<i64, i64>,
    children: HashMap<i64, Vec<i64>>,
    roots: Vec<i64>,
}

impl FolderShape {
    fn load(db: &Db) -> rusqlite::Result<Self> {
        let mut shape = Self::default();
        scan_entries(db, None, &|| true, |id, parent, _, kind, size| {
            shape.add(id, parent, kind, size)
        })?;
        shape.read_roots(db)?;
        Ok(shape)
    }

    fn add(&mut self, id: i64, parent: i64, kind: &str, size: i64) -> rusqlite::Result<()> {
        if kind == "dir" {
            self.children.entry(parent).or_default().push(id);
            return Ok(());
        }
        let overflow = || rusqlite::Error::IntegralValueOutOfRange(3, size);
        let total = self.files.entry(parent).or_default();
        *total = total.checked_add(size).ok_or_else(overflow)?;
        if kind == "file" {
            let total = self.file_bytes.entry(parent).or_default();
            *total = total.checked_add(size).ok_or_else(overflow)?;
        }
        Ok(())
    }

    /// フォルダの中のファイルの大きさの合計（子孫も含む）。
    fn total_size(&self, folder: i64) -> u64 {
        let mut total = 0u64;
        let mut stack = vec![folder];
        while let Some(id) = stack.pop() {
            if let Some(kids) = self.children.get(&id) {
                stack.extend(kids);
            }
            total += self.file_bytes.get(&id).copied().unwrap_or(0).max(0) as u64;
        }
        total
    }

    fn read_roots(&mut self, db: &Db) -> rusqlite::Result<()> {
        self.roots = db
            .conn()
            .prepare_cached("SELECT id FROM entry WHERE parent_id IS NULL AND kind = 'dir'")?
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(())
    }

    fn read_folder(&mut self, db: &Db, folder: i64) -> rusqlite::Result<()> {
        self.files.remove(&folder);
        self.file_bytes.remove(&folder);
        self.children.remove(&folder);
        let mut stmt = db
            .conn()
            .prepare_cached("SELECT id, kind, size FROM entry WHERE parent_id = ?1")?;
        let mut rows = stmt.query([folder])?;
        while let Some(row) = rows.next()? {
            self.add(row.get(0)?, folder, row.get_ref(1)?.as_str()?, row.get(2)?)?;
        }
        Ok(())
    }

    fn unclassified(
        &self,
        matched: &HashSet<i64>,
        under: Option<i64>,
        min_size: u64,
        limit: usize,
    ) -> Vec<(i64, u64)> {
        let starts = under.map_or_else(|| self.roots.clone(), |id| vec![id]);
        // 当たりを除いた大きさを、下から足す（深い木でもあふれないよう、手で積む）。
        let mut rest: HashMap<i64, u64> = HashMap::new();
        let mut stack: Vec<(i64, bool)> = starts.iter().map(|&id| (id, false)).collect();
        while let Some((id, done)) = stack.pop() {
            if matched.contains(&id) {
                continue;
            }
            let kids = self.children.get(&id).map(Vec::as_slice).unwrap_or(&[]);
            if done {
                let sum = self.files.get(&id).copied().unwrap_or(0).max(0) as u64
                    + kids.iter().filter_map(|k| rest.get(k)).sum::<u64>();
                rest.insert(id, sum);
            } else {
                stack.push((id, true));
                stack.extend(kids.iter().map(|&k| (k, false)));
            }
        }
        let mut out = Vec::new();
        let mut stack = starts;
        while let Some(id) = stack.pop() {
            let Some(&size) = rest.get(&id) else {
                continue;
            };
            if size < min_size {
                continue;
            }
            let big: Vec<i64> = self
                .children
                .get(&id)
                .map(|kids| {
                    kids.iter()
                        .copied()
                        .filter(|k| rest.get(k).is_some_and(|&s| s >= min_size))
                        .collect()
                })
                .unwrap_or_default();
            if big.is_empty() {
                out.push((id, size));
            } else {
                stack.extend(big);
            }
        }
        out.sort_by_key(|&(id, size)| (std::cmp::Reverse(size), id));
        out.truncate(limit);
        out
    }
}

/// 1 か所の仕分け（`assignment` の 1 行）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    pub id: i64,
    pub drive_id: String,
    /// ドライブの一番上から見たパス（NFC、区切りは `/`。一番上は空）。
    pub path: String,
    pub file_id: Option<i64>,
    pub app: String,
    pub role: Option<Role>,
    pub note: Option<String>,
    /// 付けたのは `user` か `mcp` か。
    pub by: String,
    pub created_at: i64,
    pub lost_at: Option<i64>,
}

fn row_to_assignment(r: &rusqlite::Row) -> rusqlite::Result<Assignment> {
    let role: Option<String> = r.get(6)?;
    Ok(Assignment {
        id: r.get(0)?,
        drive_id: r.get(1)?,
        path: r.get(2)?,
        file_id: r.get(3)?,
        app: r.get(4)?,
        role: role.as_deref().and_then(Role::from_name),
        note: r.get(5)?,
        by: r.get(7)?,
        created_at: r.get(8)?,
        lost_at: r.get(9)?,
    })
}

const ASSIGNMENT_COLUMNS: &str =
    "id, drive_id, path, file_id, app, note, role, by, created_at, lost_at";

/// `entry_id` の、ドライブの一番上から見たパス（NFC、`/` 区切り）とドライブ。
fn relative_path(
    db: &Db,
    entry_id: i64,
) -> rusqlite::Result<Option<(String, String, Option<i64>)>> {
    let conn = db.conn();
    let mut names = Vec::new();
    let mut cur = Some(entry_id);
    let mut drive = String::new();
    let mut file_id = None;
    while let Some(id) = cur {
        let row: Option<(Option<i64>, String, String, Option<i64>)> = conn
            .prepare_cached(
                "SELECT parent_id, name_nfc, drive_id, file_id FROM entry WHERE id = ?1",
            )?
            .query_row([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .optional()?;
        let Some((parent, name, drive_id, fid)) = row else {
            return Ok(None);
        };
        if id == entry_id {
            file_id = fid;
        }
        if parent.is_some() {
            names.push(name);
        }
        drive = drive_id;
        cur = parent;
    }
    names.reverse();
    Ok(Some((drive, names.join("/"), file_id)))
}

/// フォルダを `app` 用と仕分ける（同じドライブ・パスがあれば上書き）。書くのは `assignment` だけ。
pub fn assign(
    db: &mut Db,
    entry_id: i64,
    app: &str,
    role: Option<Role>,
    note: Option<&str>,
    by: &str,
) -> rusqlite::Result<Assignment> {
    let Some((drive_id, path, file_id)) = relative_path(db, entry_id)? else {
        return Err(rusqlite::Error::QueryReturnedNoRows);
    };
    let conn = db.conn();
    conn.execute(
        "INSERT INTO assignment (drive_id, path, file_id, app, role, note, by, created_at, lost_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL)
         ON CONFLICT(drive_id, path) DO UPDATE SET
           file_id = excluded.file_id, app = excluded.app, role = excluded.role,
           note = excluded.note, by = excluded.by, created_at = excluded.created_at, lost_at = NULL",
        params![
            drive_id,
            path,
            file_id,
            app,
            role.map(Role::as_str),
            note,
            by,
            crate::db::now_ms()
        ],
    )?;
    conn.query_row(
        &format!("SELECT {ASSIGNMENT_COLUMNS} FROM assignment WHERE drive_id = ?1 AND path = ?2"),
        params![drive_id, path],
        row_to_assignment,
    )
}

/// 仕分けの行を消す（ユーザーのファイルは触らない）。消えたら true。
pub fn unassign(db: &mut Db, id: i64) -> rusqlite::Result<bool> {
    Ok(db
        .conn()
        .execute("DELETE FROM assignment WHERE id = ?1", [id])?
        > 0)
}

/// 仕分けの一覧（付けた順）。
pub fn assignments(db: &Db) -> rusqlite::Result<Vec<Assignment>> {
    db.conn()
        .prepare(&format!(
            "SELECT {ASSIGNMENT_COLUMNS} FROM assignment ORDER BY id"
        ))?
        .query_map([], row_to_assignment)?
        .collect()
}

/// ドライブの一番上から、NFC の名前で下りて entry を探す。
fn entry_by_path(db: &Db, drive_id: &str, path: &str) -> rusqlite::Result<Option<i64>> {
    let Some(root) = db.drive_root(drive_id)? else {
        return Ok(None);
    };
    let mut id = root.id;
    for name in path.split('/').filter(|s| !s.is_empty()) {
        let next: Option<i64> = db
            .conn()
            .prepare_cached("SELECT id FROM entry WHERE parent_id = ?1 AND name_nfc = ?2")?
            .query_row(params![id, name], |r| r.get(0))
            .optional()?;
        match next {
            Some(n) => id = n,
            None => return Ok(None),
        }
    }
    Ok(Some(id))
}

/// 仕分けの行が今どの entry を指すか。パスで引き、無ければ同じドライブで `file_id` が同じフォルダを
/// 探して（動いた）パスを直す。どちらでも無ければ `lost_at` を付ける（行は消さない）。見つかれば
/// `lost_at` を消す。
pub fn resolve_assignments(db: &mut Db) -> rusqlite::Result<Vec<(Assignment, Option<i64>)>> {
    let mut out = Vec::new();
    for mut a in assignments(db)? {
        let mut entry = entry_by_path(db, &a.drive_id, &a.path)?;
        if entry.is_none()
            && let Some(file_id) = a.file_id
        {
            let moved: Option<i64> = db
                .conn()
                .query_row(
                    "SELECT id FROM entry WHERE drive_id = ?1 AND file_id = ?2 AND kind = 'dir' LIMIT 1",
                    params![a.drive_id, file_id],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(moved) = moved
                && let Some((_, path, _)) = relative_path(db, moved)?
            {
                let taken: bool = db.conn().query_row(
                    "SELECT count(*) > 0 FROM assignment WHERE drive_id = ?1 AND path = ?2",
                    params![a.drive_id, path],
                    |r| r.get(0),
                )?;
                if !taken {
                    db.conn().execute(
                        "UPDATE assignment SET path = ?1 WHERE id = ?2",
                        params![path, a.id],
                    )?;
                    a.path = path;
                    entry = Some(moved);
                }
            }
        }
        match (entry, a.lost_at) {
            (Some(_), Some(_)) => {
                db.conn()
                    .execute("UPDATE assignment SET lost_at = NULL WHERE id = ?1", [a.id])?;
                a.lost_at = None;
            }
            (None, None) => {
                let now = crate::db::now_ms();
                db.conn().execute(
                    "UPDATE assignment SET lost_at = ?1 WHERE id = ?2",
                    params![now, a.id],
                )?;
                a.lost_at = Some(now);
            }
            _ => {}
        }
        out.push((a, entry));
    }
    Ok(out)
}

/// 外れた候補のうち、外れた `match` が 1 つだけのものを探す（`--near` 用）。
/// 当たりを探すときと違い、どの `match` が 1 つだけ欠けているかも拾えるよう、すべての非 `**` な `match` で候補を集める。
pub fn find_near_misses(
    db: &Db,
    set: &CatalogSet,
    drive_id: Option<&str>,
    write_cache: bool,
    step: &dyn Fn() -> bool,
) -> rusqlite::Result<Option<Vec<NearMiss>>> {
    let mut anchors = Vec::new();
    for (idx, loaded) in set.catalogs.iter().enumerate() {
        for rule in &loaded.catalog.matches {
            if let Some(anchor) = rule_anchor(idx, rule) {
                anchors.push(anchor);
            }
        }
    }

    let Some(candidates) = collect_candidates(db, &anchors, drive_id, None, step)? else {
        return Ok(None);
    };

    let tree = DbTree::new(db);
    let mut reader = FsReader::new(db, write_cache);
    let mut nears = Vec::new();

    for (i, (parent_id, cat_idx)) in candidates.into_iter().enumerate() {
        if i % 256 == 255 && !step() {
            return Ok(None);
        }
        let cat = &set.catalogs[cat_idx].catalog;
        let outcome = evaluate(cat, &tree, parent_id, &mut reader);
        tree.take_error()?;
        if outcome.state != State::Miss {
            continue;
        }
        let Some(Failed::Match(failed_idx)) = outcome.failed else {
            continue;
        };
        if is_single_match_failure(cat, failed_idx, &tree, parent_id, &mut reader)? {
            nears.push(NearMiss {
                entry_id: parent_id,
                catalog_id: cat.id.clone(),
                failed_match: failed_idx,
            });
        }
    }

    Ok(Some(nears))
}

/// `failed_idx` 番目以外の `match` がすべて揃い、`exclude` にも当たらないかを確かめる。
fn is_single_match_failure(
    cat: &Catalog,
    failed_idx: usize,
    tree: &DbTree<'_>,
    parent_id: i64,
    reader: &mut FsReader<'_>,
) -> rusqlite::Result<bool> {
    if cat.matches.len() <= 1 {
        return Ok(true);
    }
    let mut without = cat.clone();
    without.matches.remove(failed_idx);
    let rest = evaluate(&without, tree, parent_id, reader);
    tree.take_error()?;
    Ok(rest.state != State::Miss)
}

/// パターンを人間向けの文字列に整形する（`--near` の表示用）。
pub fn pattern_text(pattern: &Pattern) -> String {
    let mut out = pattern
        .segments()
        .iter()
        .map(|seg| match seg {
            Segment::Any => "**".to_string(),
            Segment::Name(m) => m
                .literal()
                .map(str::to_string)
                .unwrap_or_else(|| m.regex_str().to_string()),
        })
        .collect::<Vec<_>>()
        .join("/");
    if pattern.is_dir() {
        out.push('/');
    }
    out
}

/// `find` の結果で `catalog_match` を置き換える（1 つのトランザクション。前の行は全部消して入れ直す。
/// ただし `drive_id` を渡していたら、そのドライブの行だけ置き換える）。
pub fn save(
    db: &mut Db,
    set: &CatalogSet,
    found: &[Found],
    drive_id: Option<&str>,
) -> rusqlite::Result<()> {
    let matched_at = crate::db::now_ms();
    let tx = db.conn_mut().transaction()?;
    match drive_id {
        Some(id) => {
            tx.execute(
                "DELETE FROM catalog_match
                 WHERE entry_id IN (SELECT id FROM entry WHERE drive_id = ?1)",
                [id],
            )?;
        }
        None => {
            tx.execute("DELETE FROM catalog_match", [])?;
        }
    }
    {
        let mut stmt = tx.prepare(
            "INSERT OR REPLACE INTO catalog_match
             (entry_id, catalog_id, catalog_rev, state, captures, matched_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for item in found {
            let Some(state) = state_to_str(item.state) else {
                continue;
            };
            let rev = set
                .get(&item.catalog_id)
                .map(|l| l.catalog.rev.as_str())
                .unwrap_or("");
            let captures_json =
                serde_json::to_string(&item.captures).unwrap_or_else(|_| "{}".to_string());
            stmt.execute(params![
                item.entry_id,
                item.catalog_id,
                rev,
                state,
                captures_json,
                matched_at
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// `catalog_match` 表の 1 行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchRow {
    pub entry_id: i64,
    pub catalog_id: String,
    pub catalog_rev: String,
    pub state: State,
    pub captures: Captures,
    pub matched_at: i64,
}

/// `catalog_match` の全行を読み出す。
pub fn matches(db: &Db) -> rusqlite::Result<Vec<MatchRow>> {
    let mut stmt = db.conn().prepare(
        "SELECT entry_id, catalog_id, catalog_rev, state, captures, matched_at
         FROM catalog_match ORDER BY catalog_id, entry_id",
    )?;
    let rows = stmt.query_map([], row_to_match_row)?;
    rows.collect()
}

/// 指定した型 ID の当たりを読み出す。
pub fn matches_of(db: &Db, catalog_id: &str) -> rusqlite::Result<Vec<MatchRow>> {
    let mut stmt = db.conn().prepare(
        "SELECT entry_id, catalog_id, catalog_rev, state, captures, matched_at
         FROM catalog_match WHERE catalog_id = ?1 ORDER BY entry_id",
    )?;
    let rows = stmt.query_map([catalog_id], row_to_match_row)?;
    rows.collect()
}

/// 型ごとの当たった数（`lisa_catalogs` 用）。
pub fn counts(db: &Db) -> rusqlite::Result<HashMap<String, usize>> {
    let mut stmt = db
        .conn()
        .prepare("SELECT catalog_id, COUNT(*) FROM catalog_match GROUP BY catalog_id")?;
    let mut out = HashMap::new();
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as usize))
    })?;
    for row in rows {
        let (id, count) = row?;
        out.insert(id, count);
    }
    Ok(out)
}

fn state_to_str(state: State) -> Option<&'static str> {
    match state {
        State::Match => Some("match"),
        State::Maybe => Some("maybe"),
        State::Pending => Some("pending"),
        State::Miss => None,
    }
}

fn str_to_state(s: &str) -> rusqlite::Result<State> {
    match s {
        "match" => Ok(State::Match),
        "maybe" => Ok(State::Maybe),
        "pending" => Ok(State::Pending),
        _ => Err(rusqlite::Error::FromSqlConversionFailure(
            3,
            rusqlite::types::Type::Text,
            format!("不正な catalog_match.state: {s}").into(),
        )),
    }
}

fn row_to_match_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<MatchRow> {
    let state_str = r.get_ref(3)?.as_str()?;
    let state = str_to_state(state_str)?;
    let cap_str = r.get_ref(4)?.as_str()?;
    let captures = serde_json::from_str::<Captures>(cap_str).unwrap_or_default();
    Ok(MatchRow {
        entry_id: r.get(0)?,
        catalog_id: r.get(1)?,
        catalog_rev: r.get(2)?,
        state,
        captures,
        matched_at: r.get(5)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::catalog::{Loaded, parse};
    use crate::listing::read_dir;
    use crate::volumes::Volume;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "lisa-catalog-db-{name}-{}-{}",
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

    fn volume(id: &str, mount: &Path) -> Volume {
        Volume {
            id: Some(id.into()),
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

    fn save_tree(db: &mut Db, drive_id: &str, mount: &Path, dir: &Path) {
        let res = read_dir(dir).map_err(|e| e.to_string());
        db.save_listing(drive_id, mount, dir, &res).unwrap();
        if let Ok(listing) = res {
            for e in listing.entries {
                if e.kind == Kind::Dir && !e.other_volume && !e.cloud_only {
                    save_tree(db, drive_id, mount, &dir.join(&e.name));
                }
            }
        }
    }

    fn unity_set() -> CatalogSet {
        let cat = parse(
            r#"
id = "unity-project"
name = "Unity プロジェクト"
app = "Unity"
label = "Unity {ver}"

[[match]]
path = "Assets/"

[[match]]
path = "ProjectSettings/ProjectVersion.txt"
content = 're:m_EditorVersion: (?<ver>\S+)'

[[exclude]]
path = "package.json"
"#,
        )
        .unwrap();
        CatalogSet {
            catalogs: vec![Loaded {
                catalog: cat,
                source: PathBuf::from("test:unity"),
            }],
            errors: Vec::new(),
        }
    }

    #[test]
    fn schema_is_created_even_on_older_db() {
        let tmp = TempDir::new("old-db");
        let db_path = tmp.0.join("lisa.db");
        {
            // カタログ関連の 3 表が無い古い DB を先に作っておく。
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 CREATE TABLE device (id TEXT PRIMARY KEY, name TEXT NOT NULL, hostname TEXT NOT NULL,
                     os TEXT NOT NULL, last_seen_at INTEGER NOT NULL);
                 CREATE TABLE drive (id TEXT PRIMARY KEY, label TEXT NOT NULL, fs TEXT NOT NULL,
                     capacity INTEGER, case_sensitive INTEGER, last_device_id TEXT,
                     last_mount_path TEXT, last_seen_at INTEGER NOT NULL);
                 CREATE TABLE entry (id INTEGER PRIMARY KEY, drive_id TEXT NOT NULL,
                     parent_id INTEGER, pos INTEGER NOT NULL, name BLOB NOT NULL,
                     name_nfc TEXT NOT NULL, kind TEXT NOT NULL, size INTEGER NOT NULL,
                     mtime INTEGER, file_id INTEGER, nlink INTEGER, cloud_only INTEGER NOT NULL,
                     other_volume INTEGER NOT NULL, status TEXT NOT NULL, error TEXT,
                     listed INTEGER NOT NULL);",
            )
            .unwrap();
        }
        let db = Db::open(&db_path).unwrap();
        for table in ["catalog_match", "content_check", "assignment"] {
            let exists: bool = db
                .conn()
                .query_row(
                    "SELECT count(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(exists, "table {table} should exist");
        }
    }

    #[test]
    fn find_unity_project_captures_version_and_saves() {
        let tmp = TempDir::new("unity");
        let game = tmp.0.join("MyGame");
        fs::create_dir_all(game.join("Assets")).unwrap();
        fs::create_dir_all(game.join("ProjectSettings")).unwrap();
        fs::write(
            game.join("ProjectSettings/ProjectVersion.txt"),
            b"m_EditorVersion: 6000.0.23f1\nm_EditorVersionWithRevision: 6000.0.23f1 (abc)\n",
        )
        .unwrap();

        // exclude（package.json）がある方は外れる。
        let node_like = tmp.0.join("NotUnity");
        fs::create_dir_all(node_like.join("Assets")).unwrap();
        fs::create_dir_all(node_like.join("ProjectSettings")).unwrap();
        fs::write(
            node_like.join("ProjectSettings/ProjectVersion.txt"),
            b"m_EditorVersion: 2022.3.1f1\n",
        )
        .unwrap();
        fs::write(node_like.join("package.json"), b"{}").unwrap();

        let mut db = Db::open_in_memory().unwrap();
        let dev = db.this_device("mac", "mac.local", "macos").unwrap();
        db.upsert_drive(&dev, &volume("test:1", &tmp.0)).unwrap();
        save_tree(&mut db, "test:1", &tmp.0, &tmp.0);

        let set = unity_set();
        let (found, progress) = find(&db, &set, None, false, &|| true).unwrap().unwrap();
        assert_eq!(found.len(), 1);
        let game_id = db.find_folder("test:1", &tmp.0, &game).unwrap().unwrap();
        assert_eq!(found[0].entry_id, game_id);
        assert_eq!(found[0].catalog_id, "unity-project");
        assert_eq!(found[0].state, State::Match);
        assert_eq!(
            found[0].captures.get("ver").map(String::as_str),
            Some("6000.0.23f1")
        );
        assert!(progress.candidates >= 2);
        assert!(progress.folders_checked > 0);
        assert!(progress.bytes_read > 0);

        // save / matches / matches_of / counts
        save(&mut db, &set, &found, None).unwrap();
        assert_eq!(matches(&db).unwrap().len(), 1);
        assert_eq!(matches_of(&db, "unity-project").unwrap().len(), 1);
        assert_eq!(counts(&db).unwrap().get("unity-project"), Some(&1));

        // フォルダが無くなって当て直して save すると古い行が消える。
        fs::remove_dir_all(&game).unwrap();
        save_tree(&mut db, "test:1", &tmp.0, &tmp.0);
        let (found2, _) = find(&db, &set, None, false, &|| true).unwrap().unwrap();
        assert!(found2.is_empty());
        save(&mut db, &set, &found2, None).unwrap();
        assert!(matches(&db).unwrap().is_empty());
        assert!(counts(&db).unwrap().is_empty());
    }

    #[test]
    fn incremental_matches_follow_nested_changes_and_deletions() {
        let tmp = TempDir::new("catalog-incremental");
        let game = tmp.0.join("Game");
        let settings = game.join("ProjectSettings");
        let version = settings.join("ProjectVersion.txt");
        fs::create_dir_all(game.join("Assets")).unwrap();
        fs::create_dir_all(&settings).unwrap();
        fs::write(&version, b"m_EditorVersion: 6000.0.23f1\n").unwrap();

        let mut db = Db::open_in_memory().unwrap();
        let dev = db.this_device("mac", "mac.local", "macos").unwrap();
        db.upsert_drive(&dev, &volume("test:1", &tmp.0)).unwrap();
        save_tree(&mut db, "test:1", &tmp.0, &tmp.0);
        let set = unity_set();
        let mut incremental = IncrementalMatches::default();
        let full = |db: &Db| find(db, &set, None, false, &|| true).unwrap().unwrap().0;
        let initial = incremental.refresh(&db, &set, false).unwrap();
        assert_eq!(initial, full(&db));
        assert_eq!(initial.len(), 1);

        fs::remove_file(&version).unwrap();
        save_tree(&mut db, "test:1", &tmp.0, &tmp.0);
        let changed = incremental.refresh(&db, &set, false).unwrap();
        assert_eq!(changed, full(&db));
        assert!(changed.is_empty());
        assert_eq!(
            incremental.unclassified(&HashSet::new(), 1, 20),
            unclassified(&db, &HashSet::new(), None, 1, 20).unwrap(),
        );

        fs::write(&version, b"m_EditorVersion: 6000.0.40f1\n").unwrap();
        save_tree(&mut db, "test:1", &tmp.0, &tmp.0);
        let restored = incremental.refresh(&db, &set, false).unwrap();
        assert_eq!(restored, full(&db));
        assert_eq!(
            incremental.unclassified(&HashSet::new(), 1, 20),
            unclassified(&db, &HashSet::new(), None, 1, 20).unwrap(),
        );
        assert_eq!(
            restored[0].captures.get("ver").map(String::as_str),
            Some("6000.0.40f1")
        );
        for folder in [&tmp.0, &game, &settings] {
            let id = db.find_folder("test:1", &tmp.0, folder).unwrap().unwrap();
            assert_eq!(
                incremental.folder_size(id),
                query::folder_total(&db, id).unwrap().2
            );
        }

        fs::remove_dir_all(&game).unwrap();
        save_tree(&mut db, "test:1", &tmp.0, &tmp.0);
        assert_eq!(incremental.refresh(&db, &set, false).unwrap(), full(&db));
    }

    #[test]
    fn incremental_matches_follow_moves_renames_cascades_and_drives() {
        let make = |root: &Path, name: &str| {
            let game = root.join(name);
            fs::create_dir_all(game.join("Assets")).unwrap();
            fs::create_dir_all(game.join("ProjectSettings")).unwrap();
            fs::write(
                game.join("ProjectSettings/ProjectVersion.txt"),
                b"m_EditorVersion: 6000.0.23f1\n",
            )
            .unwrap();
            game
        };
        let one = TempDir::new("catalog-incremental-moves-1");
        let two = TempDir::new("catalog-incremental-moves-2");
        let game = make(&one.0, "Game");
        let inner = make(&one.0.join("Archive"), "Inner");
        let other = make(&two.0, "Other");

        let mut db = Db::open_in_memory().unwrap();
        let dev = db.this_device("mac", "mac.local", "macos").unwrap();
        db.upsert_drive(&dev, &volume("test:1", &one.0)).unwrap();
        db.upsert_drive(&dev, &volume("test:2", &two.0)).unwrap();
        save_tree(&mut db, "test:1", &one.0, &one.0);
        save_tree(&mut db, "test:2", &two.0, &two.0);
        let set = unity_set();
        let full = |db: &Db| find(db, &set, None, false, &|| true).unwrap().unwrap().0;
        let mut incremental = IncrementalMatches::default();
        let mut check = |db: &Db, hits: usize| {
            let got = incremental.refresh(db, &set, false).unwrap();
            assert_eq!(got, full(db));
            assert_eq!(got.len(), hits);
            got
        };
        check(&db, 3);

        let id = |db: &mut Db, drive: &str, mount: &Path, path: &Path| {
            db.find_folder(drive, mount, path).unwrap().unwrap()
        };
        let game_id = id(&mut db, "test:1", &one.0, &game);
        let archive_id = id(&mut db, "test:1", &one.0, &one.0.join("Archive"));
        let settings_id = id(&mut db, "test:1", &one.0, &game.join("ProjectSettings"));
        let exec = |db: &Db, sql: &str, args: &[&dyn rusqlite::ToSql]| {
            db.conn().execute(sql, args).unwrap();
        };

        // 整理がフォルダを移動すると旧親、新親、フォルダ自身が再評価に入る。
        let moved = one.0.join("Archive").join(game.file_name().unwrap());
        fs::rename(&game, &moved).unwrap();
        db.move_entry("test:1", &one.0, &game, &moved).unwrap();
        let dirty: BTreeSet<i64> = db
            .conn()
            .prepare("SELECT id FROM catalog_dirty")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            dirty,
            BTreeSet::from([
                game_id,
                archive_id,
                db.drive_root("test:1").unwrap().unwrap().id
            ])
        );
        check(&db, 3);
        // 当たりの目印のファイルの名前を変える。
        exec(
            &db,
            "UPDATE entry SET name_nfc = 'Other.txt' WHERE parent_id = ?1",
            &[&settings_id],
        );
        check(&db, 2);
        exec(
            &db,
            "UPDATE entry SET name_nfc = 'ProjectVersion.txt' WHERE parent_id = ?1",
            &[&settings_id],
        );
        check(&db, 3);
        // まだ読んでいない扱いに戻す（後から読むときの逆）。
        exec(
            &db,
            "UPDATE entry SET listed = 0 WHERE id = ?1",
            &[&settings_id],
        );
        check(&db, 3);
        exec(
            &db,
            "UPDATE entry SET listed = 1 WHERE id = ?1",
            &[&settings_id],
        );
        let before = check(&db, 3);
        let inner_id = id(&mut db, "test:1", &one.0, &inner);
        assert!(
            before
                .iter()
                .any(|f| f.entry_id == inner_id && f.state == State::Match)
        );
        // 親を消すと子もまとめて消える（ON DELETE CASCADE）。当たっていた Inner も消える。
        exec(&db, "DELETE FROM entry WHERE id = ?1", &[&archive_id]);
        check(&db, 1);
        // もう 1 つのドライブの当たりは残り、そのドライブを消すと無くなる。
        let other_id = id(&mut db, "test:2", &two.0, &other);
        exec(&db, "DELETE FROM entry WHERE id = ?1", &[&other_id]);
        check(&db, 0);

        // カタログの定義が変わったときは呼ぶ側が全体を求める。
        let empty = CatalogSet::default();
        assert!(incremental.refresh(&db, &empty, true).unwrap().is_empty());
    }

    #[test]
    fn incremental_matches_recheck_pending_without_entry_changes() {
        let tmp = TempDir::new("catalog-incremental-pending");
        let game = tmp.0.join("Game");
        fs::create_dir_all(game.join("Assets")).unwrap();
        fs::create_dir_all(game.join("ProjectSettings")).unwrap();
        fs::write(
            game.join("ProjectSettings/ProjectVersion.txt"),
            b"m_EditorVersion: 6000.0.23f1\n",
        )
        .unwrap();

        let mut db = Db::open_in_memory().unwrap();
        let dev = db.this_device("mac", "mac.local", "macos").unwrap();
        db.upsert_drive(&dev, &volume("test:1", &tmp.0)).unwrap();
        save_tree(&mut db, "test:1", &tmp.0, &tmp.0);
        let set = unity_set();
        let full = |db: &Db| find(db, &set, None, false, &|| true).unwrap().unwrap().0;
        let set_mount = |db: &Db, path: &Path| {
            db.conn()
                .execute(
                    "UPDATE drive SET last_mount_path = ?1 WHERE id = 'test:1'",
                    [path.to_str().unwrap()],
                )
                .unwrap();
        };

        // ドライブが外れていて中身を読めない間は、確かめきれない。
        set_mount(&db, &tmp.0.join("unmounted"));
        let mut incremental = IncrementalMatches::default();
        let away = incremental.refresh(&db, &set, false).unwrap();
        assert_eq!(away, full(&db));
        assert_ne!(away[0].state, State::Match);

        // つなぎ直しても entry は変わらないが、当て直す。
        set_mount(&db, &tmp.0);
        assert!(incremental.has_changes(&db).unwrap());
        let back = incremental.refresh(&db, &set, false).unwrap();
        assert_eq!(back, full(&db));
        assert_eq!(back[0].state, State::Match);
        assert!(!incremental.has_changes(&db).unwrap());

        // 中身を読んで当たったあとでドライブが外れたら、全件と同じく確かめきれない側に戻る。
        set_mount(&db, &tmp.0.join("unmounted"));
        assert!(incremental.has_changes(&db).unwrap());
        let gone = incremental.refresh(&db, &set, false).unwrap();
        assert_eq!(gone, full(&db));
        assert_ne!(gone[0].state, State::Match);
        set_mount(&db, &tmp.0);
        assert_eq!(incremental.refresh(&db, &set, false).unwrap(), full(&db));

        // ハッシュを付けるだけでは当て直さない。
        db.conn()
            .execute("UPDATE entry SET hash = zeroblob(32)", [])
            .unwrap();
        assert!(!incremental.has_changes(&db).unwrap());

        // 変わったフォルダが多いときは全体を当て直し、記録も空にする。
        db.conn()
            .execute(
                "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < ?1)
                 INSERT OR IGNORE INTO catalog_dirty SELECT i FROM n",
                [DIRTY_FULL_LIMIT + 1],
            )
            .unwrap();
        assert_eq!(incremental.refresh(&db, &set, false).unwrap(), full(&db));
        assert!(!incremental.has_changes(&db).unwrap());
    }

    #[test]
    fn pending_and_cloud_only_behave_as_designed() {
        let tmp = TempDir::new("states");
        let pending_game = tmp.0.join("PendingGame");
        fs::create_dir_all(pending_game.join("Assets")).unwrap();
        fs::create_dir_all(pending_game.join("ProjectSettings")).unwrap();

        let cloud_game = tmp.0.join("CloudGame");
        fs::create_dir_all(cloud_game.join("Assets")).unwrap();
        fs::create_dir_all(cloud_game.join("ProjectSettings")).unwrap();
        fs::write(
            cloud_game.join("ProjectSettings/ProjectVersion.txt"),
            b"m_EditorVersion: 2023.2.0f1\n",
        )
        .unwrap();

        let mut db = Db::open_in_memory().unwrap();
        let dev = db.this_device("mac", "mac.local", "macos").unwrap();
        db.upsert_drive(&dev, &volume("test:1", &tmp.0)).unwrap();

        // PendingGame は ProjectSettings の中身を DB に読まない（listed = 0 のまま）。
        let read = |p: &Path| read_dir(p).map_err(|e| e.to_string());
        db.save_listing("test:1", &tmp.0, &tmp.0, &read(&tmp.0))
            .unwrap();
        db.save_listing("test:1", &tmp.0, &pending_game, &read(&pending_game))
            .unwrap();
        db.save_listing(
            "test:1",
            &tmp.0,
            &pending_game.join("Assets"),
            &read(&pending_game.join("Assets")),
        )
        .unwrap();

        // CloudGame は全部読むが、ProjectVersion.txt に cloud_only = 1 を立てる。
        save_tree(&mut db, "test:1", &tmp.0, &cloud_game);
        let pv_id = query::locate(&db, &cloud_game.join("ProjectSettings/ProjectVersion.txt"))
            .unwrap()
            .unwrap()
            .chain
            .pop()
            .unwrap();
        db.conn()
            .execute("UPDATE entry SET cloud_only = 1 WHERE id = ?1", [pv_id])
            .unwrap();

        let set = unity_set();
        let (found, progress) = find(&db, &set, None, false, &|| true).unwrap().unwrap();
        assert_eq!(progress.bytes_read, 0);

        let pending_id = db
            .find_folder("test:1", &tmp.0, &pending_game)
            .unwrap()
            .unwrap();
        let cloud_id = db
            .find_folder("test:1", &tmp.0, &cloud_game)
            .unwrap()
            .unwrap();

        let pending_res = found.iter().find(|f| f.entry_id == pending_id).unwrap();
        assert_eq!(pending_res.state, State::Pending);

        let cloud_res = found.iter().find(|f| f.entry_id == cloud_id).unwrap();
        assert_eq!(cloud_res.state, State::Maybe);

        // other_volume / cloud_only のフォルダは listed = 0 でも Pending にならず空扱い（Miss）。
        let ps_id = db
            .find_folder("test:1", &tmp.0, &pending_game.join("ProjectSettings"))
            .unwrap()
            .unwrap();
        db.conn()
            .execute("UPDATE entry SET other_volume = 1 WHERE id = ?1", [ps_id])
            .unwrap();
        let (found_after, _) = find(&db, &set, None, false, &|| true).unwrap().unwrap();
        assert!(!found_after.iter().any(|f| f.entry_id == pending_id));
    }

    #[test]
    fn content_check_cache_avoids_reading_file_second_time() {
        let tmp = TempDir::new("cache");
        let game = tmp.0.join("CachedGame");
        fs::create_dir_all(game.join("Assets")).unwrap();
        fs::create_dir_all(game.join("ProjectSettings")).unwrap();
        let pv = game.join("ProjectSettings/ProjectVersion.txt");
        let pv_bytes = b"m_EditorVersion: 6000.1.0f1\n";
        fs::write(&pv, pv_bytes).unwrap();

        let mut db = Db::open_in_memory().unwrap();
        let dev = db.this_device("mac", "mac.local", "macos").unwrap();
        db.upsert_drive(&dev, &volume("test:1", &tmp.0)).unwrap();
        save_tree(&mut db, "test:1", &tmp.0, &tmp.0);

        // ファイルのハッシュを DB に書いておく。
        let pv_id = query::locate(&db, &pv)
            .unwrap()
            .unwrap()
            .chain
            .pop()
            .unwrap();
        let hash = *blake3::hash(pv_bytes).as_bytes();
        db.set_hash(pv_id, &hash, pv_bytes.len() as u64, None)
            .unwrap();

        let set = unity_set();
        // 1 回目: write_cache = true でファイルを読む。
        let (found1, p1) = find(&db, &set, None, true, &|| true).unwrap().unwrap();
        assert_eq!(found1.len(), 1);
        assert_eq!(p1.bytes_read, pv_bytes.len() as u64);

        // 実ファイルが無くても（読めなくても）、2 回目は content_check を使って read_bytes == 0 で同じ結果になる。
        fs::remove_file(&pv).unwrap();
        let (found2, p2) = find(&db, &set, None, true, &|| true).unwrap().unwrap();
        assert_eq!(p2.bytes_read, 0);
        assert_eq!(found2, found1);
    }

    #[test]
    fn save_with_drive_id_only_replaces_that_drive() {
        let tmp1 = TempDir::new("d1");
        let tmp2 = TempDir::new("d2");
        for root in [&tmp1.0, &tmp2.0] {
            let g = root.join("Game");
            fs::create_dir_all(g.join("Assets")).unwrap();
            fs::create_dir_all(g.join("ProjectSettings")).unwrap();
            fs::write(
                g.join("ProjectSettings/ProjectVersion.txt"),
                b"m_EditorVersion: 2022.3.0f1\n",
            )
            .unwrap();
        }

        let mut db = Db::open_in_memory().unwrap();
        let dev = db.this_device("mac", "mac.local", "macos").unwrap();
        db.upsert_drive(&dev, &volume("drive:1", &tmp1.0)).unwrap();
        db.upsert_drive(&dev, &volume("drive:2", &tmp2.0)).unwrap();
        save_tree(&mut db, "drive:1", &tmp1.0, &tmp1.0);
        save_tree(&mut db, "drive:2", &tmp2.0, &tmp2.0);

        let set = unity_set();
        let (all, _) = find(&db, &set, None, false, &|| true).unwrap().unwrap();
        assert_eq!(all.len(), 2);
        save(&mut db, &set, &all, None).unwrap();
        assert_eq!(matches(&db).unwrap().len(), 2);

        // drive:1 だけを空の結果で上書きしても drive:2 は残る。
        save(&mut db, &set, &[], Some("drive:1")).unwrap();
        let left = matches(&db).unwrap();
        assert_eq!(left.len(), 1);
        let d2_game = db
            .find_folder("drive:2", &tmp2.0, &tmp2.0.join("Game"))
            .unwrap()
            .unwrap();
        assert_eq!(left[0].entry_id, d2_game);
    }

    #[test]
    fn near_misses_report_single_missing_match() {
        let tmp = TempDir::new("near");
        let almost = tmp.0.join("AlmostUnity");
        // Assets/ はあるが ProjectSettings/ が無い（match 1 つ外れ）
        fs::create_dir_all(almost.join("Assets")).unwrap();

        let mut db = Db::open_in_memory().unwrap();
        let dev = db.this_device("mac", "mac.local", "macos").unwrap();
        db.upsert_drive(&dev, &volume("test:1", &tmp.0)).unwrap();
        save_tree(&mut db, "test:1", &tmp.0, &tmp.0);

        let set = unity_set();
        let nears = find_near_misses(&db, &set, None, false, &|| true)
            .unwrap()
            .unwrap();
        assert_eq!(nears.len(), 1);
        let almost_id = db.find_folder("test:1", &tmp.0, &almost).unwrap().unwrap();
        assert_eq!(nears[0].entry_id, almost_id);
        assert_eq!(nears[0].catalog_id, "unity-project");
        assert_eq!(nears[0].failed_match, 1);
    }

    #[test]
    fn builtin_catalogs_load_without_errors() {
        let set = CatalogSet::load_with_builtin(&[]);
        assert!(
            set.errors.is_empty(),
            "builtin catalog errors: {:?}",
            set.errors
        );
        assert!(!set.catalogs.is_empty());
        assert!(
            set.catalogs
                .iter()
                .all(|l| l.source.to_string_lossy().starts_with("builtin:"))
        );

        // 同じ id を別フォルダに置くと上書きされる。
        let tmp = TempDir::new("override");
        fs::write(
            tmp.0.join("unity.toml"),
            "id = \"unity-project\"\nname = \"Custom Unity\"\napp = \"Unity\"\n[[match]]\npath = \"Assets/\"\n",
        )
        .unwrap();
        let overridden = CatalogSet::load_with_builtin(std::slice::from_ref(&tmp.0));
        assert!(overridden.errors.is_empty());
        let unity = overridden.get("unity-project").unwrap();
        assert_eq!(unity.catalog.name, "Custom Unity");
        assert_eq!(unity.source, tmp.0.join("unity.toml"));
    }

    #[test]
    fn find_stops_when_step_returns_false() {
        let db = Db::open_in_memory().unwrap();
        let set = unity_set();
        assert_eq!(find(&db, &set, None, false, &|| false).unwrap(), None);
    }

    fn node_set() -> CatalogSet {
        let cat = parse(
            r#"
id = "node-package"
name = "Node.js パッケージ"
app = "Node.js"

[[match]]
path = "package.json"

[[match]]
path = "node_modules/"

[[part]]
path = "node_modules/"
role = "cache"
"#,
        )
        .unwrap();
        CatalogSet {
            catalogs: vec![Loaded {
                catalog: cat,
                source: PathBuf::from("test:node"),
            }],
            errors: Vec::new(),
        }
    }

    #[test]
    fn matches_inside_cache_are_dropped() {
        let tmp = TempDir::new("nested");
        let app = tmp.0.join("app");
        // node_modules の中のパッケージも同じ形をしているが、外側の cache の中なので出さない。
        let inner = app.join("node_modules/left-pad");
        fs::create_dir_all(inner.join("node_modules/x")).unwrap();
        fs::write(inner.join("package.json"), b"{}").unwrap();
        fs::write(app.join("package.json"), b"{}").unwrap();

        let mut db = Db::open_in_memory().unwrap();
        let dev = db.this_device("mac", "mac.local", "macos").unwrap();
        db.upsert_drive(&dev, &volume("test:1", &tmp.0)).unwrap();
        save_tree(&mut db, "test:1", &tmp.0, &tmp.0);

        let (found, _) = find(&db, &node_set(), None, false, &|| true)
            .unwrap()
            .unwrap();
        let app_id = db.find_folder("test:1", &tmp.0, &app).unwrap().unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].entry_id, app_id);
    }

    #[test]
    fn unclassified_returns_deepest_big_folders_without_matches() {
        let tmp = TempDir::new("unclassified");
        let big = tmp.0.join("a/big");
        fs::create_dir_all(&big).unwrap();
        fs::write(big.join("one.bin"), vec![0u8; 3000]).unwrap();
        let small = tmp.0.join("a/small");
        fs::create_dir_all(&small).unwrap();
        fs::write(small.join("two.bin"), vec![0u8; 100]).unwrap();
        let matched_dir = tmp.0.join("m");
        fs::create_dir_all(&matched_dir).unwrap();
        fs::write(matched_dir.join("three.bin"), vec![0u8; 5000]).unwrap();

        let mut db = Db::open_in_memory().unwrap();
        let dev = db.this_device("mac", "mac.local", "macos").unwrap();
        db.upsert_drive(&dev, &volume("test:1", &tmp.0)).unwrap();
        save_tree(&mut db, "test:1", &tmp.0, &tmp.0);

        let big_id = db.find_folder("test:1", &tmp.0, &big).unwrap().unwrap();
        let m_id = db
            .find_folder("test:1", &tmp.0, &matched_dir)
            .unwrap()
            .unwrap();
        // 当たったフォルダは除くので、大きな m は出ず、a の中の big が一番深い大きなフォルダ。
        let matched: HashSet<i64> = [m_id].into_iter().collect();
        let out = unclassified(&db, &matched, None, 1000, 10).unwrap();
        assert_eq!(out, vec![(big_id, 3000)]);
        // 当たりが無ければ m も出る（大きい順）。
        let out = unclassified(&db, &HashSet::new(), None, 1000, 10).unwrap();
        assert_eq!(out, vec![(m_id, 5000), (big_id, 3000)]);
    }

    #[test]
    fn assignments_follow_moves_and_mark_lost() {
        let tmp = TempDir::new("assign");
        let work = tmp.0.join("work");
        fs::create_dir_all(work.join("game")).unwrap();

        let mut db = Db::open_in_memory().unwrap();
        let dev = db.this_device("mac", "mac.local", "macos").unwrap();
        db.upsert_drive(&dev, &volume("test:1", &tmp.0)).unwrap();
        save_tree(&mut db, "test:1", &tmp.0, &tmp.0);
        let game = db
            .find_folder("test:1", &tmp.0, &work.join("game"))
            .unwrap()
            .unwrap();

        let a = assign(
            &mut db,
            game,
            "My Game",
            Some(Role::Data),
            Some("メモ"),
            "mcp",
        )
        .unwrap();
        assert_eq!(a.path, "work/game");
        assert_eq!(a.role, Some(Role::Data));
        // 同じ場所に付け直すと上書き（行は増えない）。
        assign(&mut db, game, "My Game 2", None, None, "user").unwrap();
        assert_eq!(assignments(&db).unwrap().len(), 1);
        let resolved = resolve_assignments(&mut db).unwrap();
        assert_eq!(resolved[0].1, Some(game));

        // 動かしたら file_id で追いかけ、パスを直す。
        fs::rename(work.join("game"), work.join("moved")).unwrap();
        save_tree(&mut db, "test:1", &tmp.0, &tmp.0);
        let resolved = resolve_assignments(&mut db).unwrap();
        assert!(resolved[0].1.is_some());
        assert_eq!(resolved[0].0.path, "work/moved");
        assert_eq!(resolved[0].0.lost_at, None);

        // 消えたら見失った印を付け、行は残す。
        fs::remove_dir_all(work.join("moved")).unwrap();
        save_tree(&mut db, "test:1", &tmp.0, &tmp.0);
        let resolved = resolve_assignments(&mut db).unwrap();
        assert_eq!(resolved[0].1, None);
        assert!(resolved[0].0.lost_at.is_some());

        assert!(unassign(&mut db, resolved[0].0.id).unwrap());
        assert!(assignments(&db).unwrap().is_empty());
    }
}
