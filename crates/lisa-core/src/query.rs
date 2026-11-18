//! アプリの DB を外から（`lisa-mcp` など）読むための問い合わせ。何も書き換えない。
//!
//! - 場所は「ドライブ（最後につないでいた場所）＋その下のパス」で表す。DB の `entry.id` の列
//!   （ドライブの一番上から）を「道のり」と呼び、アプリの画面で飛ぶとき（`remote::Command::Show`）に使う。
//! - Mac の起動ディスクは `/System/Volumes/Data` として読んでいるが、ふだんは `/Users/…` のように
//!   firmlink の側のパスで指すので、どちらでも引けるようにする。

use std::path::{Path, PathBuf};

use rusqlite::{OptionalExtension, params};

use serde::{Deserialize, Serialize};

use crate::db::{Db, Result};
use crate::hash::Hash;
use crate::listing::Kind;

/// DB にあるドライブ。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Drive {
    pub id: String,
    pub label: String,
    pub fs: String,
    pub capacity: Option<u64>,
    /// 最後につないでいた場所。
    pub mount: Option<PathBuf>,
    /// 最後に見た日時（UTC の Unix 時刻、ミリ秒）。
    pub last_seen_at: i64,
    /// 一番上のフォルダの `entry.id`。無ければ None。
    #[serde(default)]
    pub root_id: Option<i64>,
    /// 一番上のフォルダのハッシュ（16 進）。ハッシュの途中なら None。
    pub root_hash: Option<String>,
}

/// DB にあるドライブの一覧（ID 順）。
pub fn drives(db: &Db) -> Result<Vec<Drive>> {
    db.conn()
        .prepare(
            "SELECT d.id, d.label, d.fs, d.capacity, d.last_mount_path, d.last_seen_at, e.id, e.hash
             FROM drive d LEFT JOIN entry e ON e.drive_id = d.id AND e.parent_id IS NULL
             ORDER BY d.id",
        )?
        .query_map([], |r| {
            Ok(Drive {
                id: r.get(0)?,
                label: r.get(1)?,
                fs: r.get(2)?,
                capacity: r.get::<_, Option<i64>>(3)?.map(|c| c as u64),
                mount: r.get::<_, Option<String>>(4)?.map(PathBuf::from),
                last_seen_at: r.get(5)?,
                root_id: r.get(6)?,
                root_hash: r.get::<_, Option<Vec<u8>>>(7)?.map(|h| hex(&h)),
            })
        })?
        .collect()
}

/// この DB の端末の（名前, ホスト名）。`meta.this_device` と `device` の表から引く。無ければ None。
pub fn this_device(db: &Db) -> Result<Option<(String, String)>> {
    db.conn()
        .query_row(
            "SELECT d.name, d.hostname FROM meta m
             JOIN device d ON d.id = m.value
             WHERE m.key = 'this_device'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
}

/// パスを DB の中で見つけたもの。
#[derive(Debug, Clone, PartialEq)]
pub struct Located {
    pub drive_id: String,
    /// ドライブの場所と、その下のパスをつないだもの（firmlink の側で聞かれても、DB の側に直す）。
    pub path: PathBuf,
    /// ドライブの一番上からこれまでの `entry.id`。
    pub chain: Vec<i64>,
}

/// パス（ファイルでもフォルダでも）を DB から探す。まだ読んでいない（DB に無い）なら None。
pub fn locate(db: &Db, path: &Path) -> Result<Option<Located>> {
    let drives = drives(db)?;
    let mounted: Vec<(&str, &Path)> = drives
        .iter()
        .filter_map(|d| d.mount.as_deref().map(|m| (d.id.as_str(), m)))
        .collect();
    // まず、そのままのパスで、つないでいた場所がいちばん長く合うドライブ。
    let mut candidates: Vec<(&str, &Path, PathBuf)> = Vec::new();
    if let Some(&(id, mount)) = mounted
        .iter()
        .filter(|(_, m)| path.starts_with(m))
        .max_by_key(|(_, m)| m.components().count())
    {
        candidates.push((id, mount, path.to_path_buf()));
    }
    // 次に、ドライブの場所の下に足したもの（`/Users/…` → `/System/Volumes/Data/Users/…`）。
    if let Ok(relative) = path.strip_prefix("/") {
        for &(id, mount) in &mounted {
            if mount != Path::new("/") {
                candidates.push((id, mount, mount.join(relative)));
            }
        }
    }
    for (drive_id, mount, full) in candidates {
        let Ok(relative) = full.strip_prefix(mount) else {
            continue;
        };
        if let Some(chain) = chain_under(db, drive_id, relative)? {
            return Ok(Some(Located {
                drive_id: drive_id.to_string(),
                path: full,
                chain,
            }));
        }
    }
    Ok(None)
}

/// ドライブ `drive_id` の一番上から `relative` をたどった道のり。途中で無ければ None。
fn chain_under(db: &Db, drive_id: &str, relative: &Path) -> Result<Option<Vec<i64>>> {
    let conn = db.conn();
    let Some(root): Option<i64> = conn
        .query_row(
            "SELECT id FROM entry WHERE drive_id = ?1 AND parent_id IS NULL",
            [drive_id],
            |r| r.get(0),
        )
        .optional()?
    else {
        return Ok(None);
    };
    let mut chain = vec![root];
    for part in relative.components() {
        let std::path::Component::Normal(part) = part else {
            continue;
        };
        let name = crate::name::raw_bytes(part);
        let parent = chain[chain.len() - 1];
        let child: Option<i64> = conn
            .prepare_cached("SELECT id FROM entry WHERE parent_id = ?1 AND name = ?2")?
            .query_row(params![parent, name], |r| r.get(0))
            .optional()?;
        match child {
            Some(id) => chain.push(id),
            None => return Ok(None),
        }
    }
    Ok(Some(chain))
}

/// `entry.id` の道のり（ドライブの一番上から）と、ドライブ、パス。無ければ None。
pub fn path_of(db: &Db, id: i64) -> Result<Option<Located>> {
    let conn = db.conn();
    let mut chain = Vec::new();
    let mut names: Vec<Vec<u8>> = Vec::new();
    let mut drive_id = String::new();
    let mut cur = Some(id);
    while let Some(i) = cur {
        let row: Option<(Option<i64>, Vec<u8>, String)> = conn
            .prepare_cached("SELECT parent_id, name, drive_id FROM entry WHERE id = ?1")?
            .query_row([i], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()?;
        let Some((parent, name, drive)) = row else {
            return Ok(None);
        };
        chain.push(i);
        if parent.is_some() {
            names.push(name);
        }
        drive_id = drive;
        cur = parent;
    }
    chain.reverse();
    names.reverse();
    let mount: Option<String> = conn
        .query_row(
            "SELECT last_mount_path FROM drive WHERE id = ?1",
            [&drive_id],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    let mut path = PathBuf::from(mount.unwrap_or_default());
    for name in names {
        path.push(crate::name::from_raw_bytes(&name));
    }
    Ok(Some(Located {
        drive_id,
        path,
        chain,
    }))
}

/// 1 つのファイルかフォルダ（外に見せる形）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Item {
    pub id: i64,
    pub name: String,
    pub kind: String,
    /// ファイルの大きさ。フォルダは 0（`folder_total` で足す）。
    pub size: u64,
    /// 更新日時（UTC の Unix 時刻、ミリ秒）。
    pub mtime: Option<i64>,
    /// 中身のハッシュ（16 進）。まだなら None。
    pub hash: Option<String>,
    pub cloud_only: bool,
    pub other_volume: bool,
    /// フォルダの中を読み終えた。
    pub listed: bool,
    pub error: Option<String>,
}

impl From<crate::db::Stored> for Item {
    fn from(s: crate::db::Stored) -> Self {
        Self {
            id: s.id,
            name: s.name_nfc,
            kind: kind_str(s.kind).to_string(),
            size: s.size,
            mtime: s.mtime,
            hash: s.hash.map(|h| hex(&h)),
            cloud_only: s.cloud_only,
            other_volume: s.other_volume,
            listed: s.listed,
            error: s.error,
        }
    }
}

/// 1 行。
pub fn item(db: &Db, id: i64) -> Result<Option<Item>> {
    Ok(db.entry(id)?.map(Item::from))
}

/// フォルダの子（読んだときの並び）。
pub fn children(db: &Db, id: i64) -> Result<Vec<Item>> {
    Ok(db.children(id)?.into_iter().map(Item::from).collect())
}

/// フォルダの中の合計（下まで全部）: (ファイルの数, フォルダの数, 大きさ)。大きなフォルダでは数秒かかる。
pub fn folder_total(db: &Db, id: i64) -> Result<(u64, u64, u64)> {
    db.conn().query_row(
        "WITH RECURSIVE sub(id) AS (
             SELECT id FROM entry WHERE parent_id = ?1
             UNION ALL SELECT e.id FROM entry e JOIN sub ON e.parent_id = sub.id)
         SELECT coalesce(sum(kind = 'file'), 0), coalesce(sum(kind = 'dir'), 0),
                coalesce(sum(CASE WHEN kind = 'file' THEN size ELSE 0 END), 0)
         FROM entry WHERE id IN sub",
        [id],
        |r| {
            Ok((
                r.get::<_, i64>(0)? as u64,
                r.get::<_, i64>(1)? as u64,
                r.get::<_, i64>(2)? as u64,
            ))
        },
    )
}

/// 同じハッシュのもの（ファイルもフォルダも）の `entry.id`。
pub fn with_hash(db: &Db, hash: &Hash, limit: usize) -> Result<Vec<i64>> {
    db.conn()
        .prepare("SELECT id FROM entry WHERE hash = ?1 ORDER BY id LIMIT ?2")?
        .query_map(params![&hash[..], limit as i64], |r| r.get(0))?
        .collect()
}

/// 名前に `text` を含むもの（大文字小文字を区別しない。NFC で比べる）。全部の行を見るので数秒かかる。
pub fn search_name(db: &Db, text: &str, kind: Option<Kind>, limit: usize) -> Result<Vec<i64>> {
    let text = unicode_normalization::UnicodeNormalization::nfc(text).collect::<String>();
    let escaped = text
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    let pattern = format!("%{escaped}%");
    let kind = kind.map(kind_str);
    db.conn()
        .prepare(
            "SELECT id FROM entry WHERE parent_id IS NOT NULL
               AND name_nfc LIKE ?1 ESCAPE '\\'
               AND (?2 IS NULL OR kind = ?2)
             ORDER BY id LIMIT ?3",
        )?
        .query_map(params![pattern, kind, limit as i64], |r| r.get(0))?
        .collect()
}

pub fn kind_str(kind: Kind) -> &'static str {
    match kind {
        Kind::File => "file",
        Kind::Dir => "dir",
        Kind::Symlink => "symlink",
        Kind::Other => "other",
    }
}

/// バイト列を 16 進に。
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 16 進をハッシュに。長さや文字が違えば None。
pub fn parse_hash(text: &str) -> Option<Hash> {
    let text = text.trim();
    if text.len() != 64 {
        return None;
    }
    let mut hash = [0u8; 32];
    for (i, byte) in hash.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::volumes::Volume;

    fn volume(id: &str, mount: &str) -> Volume {
        Volume {
            id: Some(id.into()),
            label: "test".into(),
            fs: "apfs".into(),
            mount_path: PathBuf::from(mount),
            capacity: None,
            available: None,
            removable: false,
            network: false,
            read_only: false,
            case_sensitive: Some(false),
            hidden: false,
        }
    }

    /// `parent` の下に名前 `name` の行を足す（テスト用に直接書く）。
    fn add(db: &Db, drive: &str, parent: i64, name: &str, kind: &str, size: i64) -> i64 {
        db.conn()
            .query_row(
                "INSERT INTO entry (drive_id, parent_id, pos, name, name_nfc, kind, size,
                                    cloud_only, other_volume, status, listed)
                 VALUES (?1, ?2, 0, ?3, ?3, ?4, ?5, 0, 0, 'loaded', 1) RETURNING id",
                params![drive, parent, name.as_bytes(), kind, size],
                |r| r.get(0),
            )
            .unwrap()
    }

    #[test]
    fn locate_and_path_of() {
        let mut db = Db::open_in_memory().unwrap();
        db.upsert_drive("dev", &volume("mac:data", "/System/Volumes/Data"))
            .unwrap();
        db.upsert_drive("dev", &volume("mac:ext", "/Volumes/Ext"))
            .unwrap();
        let root = db.drive_root("mac:data").unwrap().unwrap().id;
        let users = add(&db, "mac:data", root, "Users", "dir", 0);
        let me = add(&db, "mac:data", users, "me", "dir", 0);
        let file = add(&db, "mac:data", me, "a.txt", "file", 10);
        add(&db, "mac:data", me, "b.txt", "file", 5);

        // firmlink の側のパスでも引ける。
        let found = locate(&db, Path::new("/Users/me/a.txt")).unwrap().unwrap();
        assert_eq!(found.drive_id, "mac:data");
        assert_eq!(found.chain, vec![root, users, me, file]);
        assert_eq!(
            found.path,
            PathBuf::from("/System/Volumes/Data/Users/me/a.txt")
        );
        let same = locate(&db, Path::new("/System/Volumes/Data/Users/me"))
            .unwrap()
            .unwrap();
        assert_eq!(same.chain, vec![root, users, me]);
        assert!(locate(&db, Path::new("/Users/nobody")).unwrap().is_none());

        let back = path_of(&db, file).unwrap().unwrap();
        assert_eq!(back, found);
        assert_eq!(folder_total(&db, users).unwrap(), (2, 1, 15));
        assert_eq!(search_name(&db, "A.TX", None, 10).unwrap(), vec![file]);
        assert_eq!(
            search_name(&db, "txt", Some(Kind::Dir), 10).unwrap(),
            Vec::<i64>::new()
        );
    }

    #[test]
    fn hash_hex_round_trip() {
        let hash = [0xabu8; 32];
        assert_eq!(parse_hash(&hex(&hash)), Some(hash));
        assert_eq!(parse_hash("zz"), None);
    }
}
