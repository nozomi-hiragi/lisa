//! 重複の一覧。同じ中身（ハッシュ）のファイルとフォルダをまとめる。表示のためだけで、何も動かさない。
//!
//! - 中身がまるごと同じフォルダ（フォルダのハッシュが同じ）は 1 つの組にする。その中のファイルや
//!   フォルダは、個別の組には出さない（親がもう重複しているので）。
//! - ハードリンク（同じドライブで同じファイル ID）は同じ実体なので、重複に数えない。
//! - 大きさ 0 のもの、クラウドにだけあるもの、ハッシュの無いものは除く。
//! - 組は、空く大きさ（大きさ ×（数 − 1））の大きい順。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use rusqlite::Connection;

use crate::db::{Db, Result};
use crate::hash::Hash;
use crate::listing::Kind;

mod comparison;
pub use comparison::*;

/// 組の中の 1 か所。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Copy {
    pub drive_id: String,
    /// 場所（ドライブを最後につないでいた場所と、その下のパス）。
    pub path: PathBuf,
    /// ドライブの一番上からこれまでの `entry.id`（木の上でたどるため）。
    pub chain: Vec<i64>,
    /// 同じ実体を指すハードリンク（数えないが、場所は見せる）。
    pub hard_links: Vec<PathBuf>,
}

/// 同じ中身のファイルかフォルダの組。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Group {
    pub kind: Kind,
    /// 最初のものの名前（表示用）。
    pub name: String,
    /// 1 つあたりの大きさ。フォルダは中のファイルの合計。
    pub size: u64,
    pub hash: Hash,
    pub copies: Vec<Copy>,
}

impl Group {
    /// 1 つだけ残せば空く大きさ。
    pub fn wasted(&self) -> u64 {
        self.size * (self.copies.len() as u64).saturating_sub(1)
    }

    /// 全部の大きさ。
    pub fn total(&self) -> u64 {
        self.size * self.copies.len() as u64
    }
}

/// 一覧を作ったときの DB の状態（ドライブごとの一番上のハッシュ）。これが同じなら、一覧も同じ。
pub type Signature = Vec<(String, Option<Hash>)>;

/// 今の DB の状態。どれかのドライブの一番上のハッシュが無ければ（ハッシュの途中なら）、一覧が同じとは
/// 言えないので None。
pub fn signature(db: &Db) -> Result<Option<Signature>> {
    let ids: Vec<String> = db
        .conn()
        .prepare("SELECT id FROM drive ORDER BY id")?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_>>()?;
    let mut signature = Vec::new();
    for id in ids {
        let hash = db.drive_root(&id)?.and_then(|root| root.hash);
        if hash.is_none() {
            return Ok(None);
        }
        signature.push((id, hash));
    }
    // 目印への置き換えでは親のハッシュを維持するが、重複候補は変わる。
    let mut statement = db
        .conn()
        .prepare("SELECT id, parent_id, merged FROM entry WHERE merged IS NOT NULL ORDER BY id")?;
    let mut rows = statement.query([])?;
    let mut markers = blake3::Hasher::new();
    while let Some(row) = rows.next()? {
        markers.update(&row.get::<_, i64>(0)?.to_le_bytes());
        markers.update(&row.get::<_, i64>(1)?.to_le_bytes());
        let json: String = row.get(2)?;
        markers.update(&(json.len() as u64).to_le_bytes());
        markers.update(json.as_bytes());
    }
    signature.push((
        "lisa-merged".to_string(),
        Some(*markers.finalize().as_bytes()),
    ));
    Ok(Some(signature))
}

/// 除外規則が変わったら以前の一覧を読み込まない。
pub const CACHE_FILE: &str = "duplicates-v3.json";

/// キャッシュに残す一覧と、そのときの DB の状態。
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Cached {
    pub signature: Signature,
    pub groups: Vec<Group>,
}

/// 木の形（全部の行の親と、フォルダの中の合計）。`entry.id` で引く。
struct Shape {
    parent: Vec<i64>,
    total: Vec<u64>,
    excluded: Vec<bool>,
    contains_marker: Vec<bool>,
}

/// ボリューム直下で OS が管理するフォルダ。
fn is_volume_metadata(name: &str) -> bool {
    [
        ".fseventsd",
        ".Spotlight-V100",
        ".DocumentRevisions-V100",
        ".Trashes",
        "System Volume Information",
        "$RECYCLE.BIN",
    ]
    .iter()
    .any(|reserved| name.eq_ignore_ascii_case(reserved))
}

impl Shape {
    fn load(conn: &Connection) -> Result<Self> {
        let max: i64 =
            conn.query_row("SELECT coalesce(max(id), 0) FROM entry", [], |r| r.get(0))?;
        let n = max as usize + 1;
        let mut parent = vec![0i64; n];
        let mut total = vec![0u64; n];
        let mut excluded = vec![false; n];
        let mut contains_marker = vec![false; n];
        let mut statement =
            conn.prepare("SELECT id, parent_id, kind, size, name_nfc, cloud_only, other_volume, merged IS NOT NULL FROM entry ORDER BY id")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            parent[id as usize] = row.get::<_, Option<i64>>(1)?.unwrap_or(0);
            let index = id as usize;
            let p = parent[index] as usize;
            let name: String = row.get(4)?;
            let kind: String = row.get(2)?;
            excluded[index] = excluded[p]
                || row.get::<_, bool>(5)?
                || row.get::<_, bool>(6)?
                || crate::name::is_os_junk(name.as_ref())
                || crate::name::is_quarantine_dir(name.as_ref())
                || (p != 0 && parent[p] == 0 && kind == "dir" && is_volume_metadata(&name));
            contains_marker[index] = row.get(7)?;
            if !excluded[index] && !contains_marker[index] && kind == "file" {
                total[index] = row.get::<_, i64>(3)? as u64;
            }
        }
        // 子は親より後に書くので、番号の大きいほうから親へ足し上げればよい。
        for id in (1..n).rev() {
            let p = parent[id] as usize;
            if p != 0 {
                total[p] += total[id];
                contains_marker[p] |= contains_marker[id];
            }
        }
        Ok(Self {
            parent,
            total,
            excluded,
            contains_marker,
        })
    }

    /// `id` の祖先（自分は入れない）に `marked` のものがあるか。
    fn has_marked_ancestor(&self, id: i64, marked: &[bool]) -> bool {
        let mut cur = self.parent[id as usize];
        while cur != 0 {
            if marked[cur as usize] {
                return true;
            }
            cur = self.parent[cur as usize];
        }
        false
    }

    /// ドライブの一番上から `id` までの番号。
    fn chain(&self, id: i64) -> Vec<i64> {
        let mut chain = vec![id];
        let mut cur = self.parent[id as usize];
        while cur != 0 {
            chain.push(cur);
            cur = self.parent[cur as usize];
        }
        chain.reverse();
        chain
    }
}

/// 見つけた組: (種類, ハッシュ, 大きさ, 実体ごとの番号の列（先頭が数える 1 つ、残りはハードリンク）)。
type Found = (Kind, Vec<u8>, u64, Vec<Vec<i64>>);
/// 実体の見分け（ドライブ、ファイル ID）。
type Physical = (String, Option<i64>);

const FILE_HASH_INDEX: &str = "CREATE INDEX IF NOT EXISTS entry_hash ON entry(hash)
    WHERE hash IS NOT NULL AND kind = 'file'";

/// 重複の組を、空く大きさの順に `limit` 個まで。全部の行を一度読むので、大きな DB では数十秒かかる。
pub fn groups(db: &Db, limit: usize) -> Result<Vec<Group>> {
    let conn = db.conn();
    conn.execute_batch(FILE_HASH_INDEX)?;
    let shape = Shape::load(conn)?;

    // まず、中身が同じフォルダ。
    let mut folders: HashMap<Vec<u8>, Vec<i64>> = HashMap::new();
    {
        let mut statement =
            conn.prepare("SELECT hash, id FROM entry WHERE kind = 'dir' AND hash IS NOT NULL")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(1)?;
            if !shape.excluded[id as usize]
                && !shape.contains_marker[id as usize]
                && shape.total[id as usize] > 0
            {
                folders.entry(row.get(0)?).or_default().push(id);
            }
        }
    }
    folders.retain(|_, ids| ids.len() > 1 && shape.total[ids[0] as usize] > 0);
    let mut in_dup_folder = vec![false; shape.parent.len()];
    for ids in folders.values() {
        for &id in ids {
            in_dup_folder[id as usize] = true;
        }
    }
    let mut found: Vec<Found> = Vec::new();
    for (hash, ids) in folders {
        let kept: Vec<Vec<i64>> = ids
            .into_iter()
            .filter(|&id| !shape.has_marked_ancestor(id, &in_dup_folder))
            .map(|id| vec![id])
            .collect();
        if kept.len() > 1 {
            let size = shape.total[kept[0][0] as usize];
            found.push((Kind::Dir, hash, size, kept));
        }
    }

    // 次に、同じ中身のファイル（重複しているフォルダの中のものは除く）。
    {
        let mut statement = conn.prepare(
            "SELECT e.hash, e.id, e.drive_id, e.file_id, e.size FROM entry e
             JOIN (SELECT hash FROM entry WHERE kind = 'file' AND hash IS NOT NULL
                   GROUP BY hash HAVING count(*) > 1) d ON e.hash = d.hash
             WHERE e.kind = 'file' AND e.size > 0 AND e.cloud_only = 0 AND e.merged IS NULL
             ORDER BY e.hash, e.id",
        )?;
        let mut rows = statement.query([])?;
        let mut current: Option<(Vec<u8>, u64)> = None;
        // 実体（ドライブ、ファイル ID）ごとの番号。
        let mut physical: Vec<(Physical, Vec<i64>)> = Vec::new();
        let mut flush = |current: &Option<(Vec<u8>, u64)>,
                         physical: &mut Vec<(Physical, Vec<i64>)>| {
            if let Some((hash, size)) = current
                && physical.len() > 1
            {
                let copies = physical.drain(..).map(|(_, ids)| ids).collect();
                found.push((Kind::File, hash.clone(), *size, copies));
            }
            physical.clear();
        };
        while let Some(row) = rows.next()? {
            let hash: Vec<u8> = row.get(0)?;
            let id: i64 = row.get(1)?;
            if current.as_ref().is_none_or(|(h, _)| *h != hash) {
                flush(&current, &mut physical);
                current = Some((hash, row.get::<_, i64>(4)? as u64));
            }
            if shape.excluded[id as usize] || shape.has_marked_ancestor(id, &in_dup_folder) {
                continue;
            }
            let key = (row.get::<_, String>(2)?, row.get::<_, Option<i64>>(3)?);
            match physical
                .iter_mut()
                .find(|(k, _)| k.1.is_some() && *k == key)
            {
                Some((_, ids)) => ids.push(id),
                None => physical.push((key, vec![id])),
            }
        }
        flush(&current, &mut physical);
    }

    found.sort_by_key(|(_, _, size, copies)| std::cmp::Reverse(size * (copies.len() as u64 - 1)));
    found.truncate(limit);

    // 見せる分だけ、場所と名前を作る。
    let mounts: HashMap<String, PathBuf> = conn
        .prepare("SELECT id, last_mount_path FROM drive")?
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                PathBuf::from(r.get::<_, Option<String>>(1)?.unwrap_or_default()),
            ))
        })?
        .collect::<Result<_>>()?;
    let mut row_of =
        conn.prepare_cached("SELECT name, name_nfc, drive_id FROM entry WHERE id = ?1")?;
    let mut names: HashMap<i64, (Vec<u8>, String, String)> = HashMap::new();
    let mut lookup = |id: i64| -> Result<(Vec<u8>, String, String)> {
        if let Some(row) = names.get(&id) {
            return Ok(row.clone());
        }
        let row: (Vec<u8>, String, String) =
            row_of.query_row([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        names.insert(id, row.clone());
        Ok(row)
    };
    // （ドライブ、場所、名前）。
    let mut path_of = |id: i64| -> Result<(String, PathBuf, String)> {
        let chain = shape.chain(id);
        let (_, _, drive_id) = lookup(chain[0])?;
        let mut path = mounts.get(&drive_id).cloned().unwrap_or_default();
        let mut name = String::new();
        for &part in &chain[1..] {
            let (raw, nfc, _) = lookup(part)?;
            path.push(crate::name::from_raw_bytes(&raw));
            name = nfc;
        }
        Ok((drive_id, path, name))
    };
    let mut groups = Vec::new();
    for (kind, hash, size, copies) in found {
        let Ok(hash) = hash.try_into() else { continue };
        let mut out = Vec::new();
        let mut name = String::new();
        for ids in &copies {
            let (drive_id, path, first_name) = path_of(ids[0])?;
            if name.is_empty() {
                name = first_name;
            }
            let mut hard_links = Vec::new();
            for &link in &ids[1..] {
                hard_links.push(path_of(link)?.1);
            }
            out.push(Copy {
                drive_id,
                path,
                chain: shape.chain(ids[0]),
                hard_links,
            });
        }
        groups.push(Group {
            kind,
            name,
            size,
            hash,
            copies: out,
        });
    }
    Ok(groups)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::listing::read_dir;
    use crate::volumes::Volume;
    use std::fs;
    use std::path::Path;

    #[test]
    fn same_folders_become_one_group_and_hard_links_count_once() {
        let tmp = std::env::temp_dir().join(format!("lisa-dups-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        for dir in ["a", "b", "c"] {
            fs::create_dir_all(tmp.join(dir)).unwrap();
        }
        // a と b は中身がまるごと同じ（フォルダの組になり、中の x.bin は個別に出ない）。
        for dir in ["a", "b"] {
            fs::write(tmp.join(dir).join("x.bin"), vec![7u8; 3000]).unwrap();
        }
        // c/y.bin は a/x.bin と同じ中身だが、a と b のフォルダの組があるので… c は別のフォルダ。
        fs::write(tmp.join("c/y.bin"), vec![1u8; 1000]).unwrap();
        fs::write(tmp.join("z.bin"), vec![1u8; 1000]).unwrap();
        // z.bin のハードリンク（数えない）。
        fs::hard_link(tmp.join("z.bin"), tmp.join("link.bin")).unwrap();
        fs::write(tmp.join("empty1"), b"").unwrap();
        fs::write(tmp.join("empty2"), b"").unwrap();

        let volume = Volume {
            id: Some("test:d".into()),
            label: "t".into(),
            fs: String::new(),
            mount_path: tmp.clone(),
            capacity: None,
            available: None,
            removable: false,
            network: false,
            read_only: false,
            case_sensitive: None,
            hidden: false,
        };
        let mut db = Db::open_in_memory().unwrap();
        let device = db.this_device("d", "d", "macos").unwrap();
        db.upsert_drive(&device, &volume).unwrap();
        let read = |p: &Path| read_dir(p).map_err(|e| e.to_string());
        for dir in [tmp.clone(), tmp.join("a"), tmp.join("b"), tmp.join("c")] {
            db.save_listing("test:d", &tmp, &dir, &read(&dir)).unwrap();
        }
        crate::hash::hash_drive(
            &mut db,
            "test:d",
            &tmp,
            2,
            &mut |_| {},
            &crate::hash::Pace::FREE,
        )
        .unwrap();

        let found = groups(&db, 10).unwrap();
        assert_eq!(found.len(), 2, "{found:#?}");
        // 空く量の順: フォルダ a と b（3000）、ファイル c/y.bin と z.bin（1000）。
        assert_eq!(found[0].kind, Kind::Dir);
        assert_eq!(found[0].size, 3000);
        let paths: Vec<&PathBuf> = found[0].copies.iter().map(|c| &c.path).collect();
        assert_eq!(paths, [&tmp.join("a"), &tmp.join("b")]);
        assert_eq!(found[1].kind, Kind::File);
        assert_eq!(found[1].wasted(), 1000);
        assert_eq!(found[1].copies.len(), 2);
        let links: Vec<&PathBuf> = found[1].copies.iter().flat_map(|c| &c.hard_links).collect();
        assert_eq!(links.len(), 1);
        // 木の上でたどる番号は、ドライブの一番上から。
        let root = db.drive_root("test:d").unwrap().unwrap();
        assert_eq!(found[0].copies[0].chain[0], root.id);
        let _ = fs::remove_dir_all(&tmp);
    }
}
