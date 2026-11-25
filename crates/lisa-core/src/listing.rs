//! フォルダを 1 段だけ読む。
//!
//! 中身は読まず、名前、種類、大きさ、更新日時などだけを取る。シンボリックリンク（Windows の
//! ジャンクションも）はたどらず、リンク先の文字列を記録する。クラウドにだけあるファイルは印を付ける。
//! 別のボリュームの入り口（マウントした場所）には印を付け、下りるかどうかは呼ぶ側が決める。

use std::cmp::Ordering;
use std::ffi::OsString;
use std::fs::{self, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::name;

/// 種類。
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Kind {
    File,
    Dir,
    /// シンボリックリンク。Windows のジャンクションやフォルダのマウントも含む。たどらない。
    Symlink,
    /// ソケット、FIFO、デバイスなど。記録だけして読まない。
    Other,
}

/// 読めなかった理由。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// 権限が無い（Mac のフルディスクアクセスなど）。
    Permission,
    /// 読む前に消えた。
    NotFound,
    /// それ以外の読み取りエラー。
    Io,
}

impl Failure {
    pub fn of(error: &io::Error) -> Self {
        match error.kind() {
            io::ErrorKind::PermissionDenied => Failure::Permission,
            io::ErrorKind::NotFound => Failure::NotFound,
            _ => Failure::Io,
        }
    }
}

/// フォルダの中の 1 件。
#[derive(Debug)]
pub struct Entry {
    /// ディスク上の名前そのまま。
    pub name: OsString,
    /// ディスク上の名前のバイト列（DB に残す形、`name::raw_bytes`）。
    pub name_raw: Vec<u8>,
    /// NFC に正規化した名前。
    pub name_nfc: String,
    pub kind: Kind,
    pub merged: Option<crate::merged::Marker>,
    pub marker_identity: Option<(u64, [u8; 16])>,
    pub marker_parent: Option<(u64, [u8; 16])>,
    /// バイト数。フォルダとリンクは OS が返す値そのまま（中の合計ではない）。
    pub size: u64,
    /// 更新日時（UTC の Unix 時刻、ミリ秒）。取れなければ None。
    pub mtime_ms: Option<i64>,
    /// OS のファイル ID（inode）。Windows はまだ取っていない。
    pub file_id: Option<u64>,
    /// ハードリンクの数。Windows はまだ取っていない。
    pub nlink: Option<u64>,
    /// シンボリックリンクのリンク先（たどらない）。
    pub link_target: Option<PathBuf>,
    /// クラウドにだけあって、手元に中身が無い。読むとダウンロードが始まるので、ハッシュしない。
    pub cloud_only: bool,
    /// 読んでいるフォルダと別のボリューム（マウントした場所）。ふつうは下りない。
    /// Mac の起動ディスクの一番上にある firmlink（`/Users` など、中身は Data のボリューム）もここに入る。
    /// `st_dev` では見分けられないので、`/usr/share/firmlinks` の一覧で見る。
    pub other_volume: bool,
}

/// 1 段読んだ結果。
#[derive(Debug, Default)]
pub struct Listing {
    /// フォルダのハッシュと同じ並び（`name::hash_order`）。OS のゴミファイルは入れない。
    pub entries: Vec<Entry>,
    /// 外した OS のゴミファイルの数。
    pub junk: usize,
    /// 名前は見えたが、情報が取れなかったもの。
    pub failed: Vec<(OsString, Failure, io::Error)>,
}

/// `dir` の中を 1 段読む。`dir` 自体が読めなければエラー。
pub fn read_dir(dir: &Path) -> io::Result<Listing> {
    let dir_meta = fs::metadata(dir)?;
    let firmlinks = firmlinks_in(dir);
    let mut listing = Listing::default();
    for item in fs::read_dir(dir)? {
        let item = match item {
            Ok(item) => item,
            Err(error) => {
                listing
                    .failed
                    .push((OsString::new(), Failure::of(&error), error));
                continue;
            }
        };
        let file_name = item.file_name();
        if name::is_os_junk(&file_name) {
            listing.junk += 1;
            continue;
        }
        if name::is_quarantine_dir(&file_name) {
            continue;
        }
        // シンボリックリンクをたどらない情報（Unix は lstat、Windows は一覧のときに取れた値）。
        let meta = match item.metadata() {
            Ok(meta) => meta,
            Err(error) => {
                listing.failed.push((file_name, Failure::of(&error), error));
                continue;
            }
        };
        let kind = kind_of(&meta);
        let other_volume =
            kind == Kind::Dir && (other_volume(&dir_meta, &meta) || firmlinks.contains(&file_name));
        let link_target = if kind == Kind::Symlink {
            fs::read_link(item.path()).ok()
        } else {
            None
        };
        let merged = if kind == Kind::File {
            crate::merged::read_with_metadata(&item.path(), &meta)
        } else {
            None
        };
        let marker_identity = merged
            .as_ref()
            .and_then(|_| crate::organize::file_identity(&item.path()));
        let marker_parent = merged
            .as_ref()
            .and_then(|_| crate::organize::file_identity(dir));
        listing.entries.push(Entry {
            name_raw: name::raw_bytes(&file_name),
            name_nfc: name::nfc(&file_name),
            name: file_name,
            kind,
            merged,
            marker_identity,
            marker_parent,
            size: meta.len(),
            mtime_ms: mtime_ms(&meta),
            file_id: file_id(&meta),
            nlink: nlink(&meta),
            link_target,
            cloud_only: cloud_only(&meta),
            other_volume,
        });
    }
    listing.entries.sort_by(compare);
    Ok(listing)
}

/// Mac の firmlink の一覧（`/usr/share/firmlinks`）。各行は「/Users<TAB>Users」のように、起動ディスクの
/// 一番上の名前と、Data のボリュームの中の場所。
const FIRMLINKS: &str = "/usr/share/firmlinks";

/// `dir` の中にある firmlink の名前。Mac の起動ディスクの一番上（`/`）だけにある。
fn firmlinks_in(dir: &Path) -> Vec<OsString> {
    if !cfg!(target_os = "macos") || dir != Path::new("/") {
        return Vec::new();
    }
    fs::read_to_string(FIRMLINKS)
        .map(|text| parse_firmlinks(&text))
        .unwrap_or_default()
}

fn parse_firmlinks(text: &str) -> Vec<OsString> {
    text.lines()
        .filter_map(|line| line.split('\t').next())
        .filter_map(|path| path.strip_prefix('/'))
        .filter(|name| !name.is_empty() && !name.contains('/'))
        .map(OsString::from)
        .collect()
}

fn compare(a: &Entry, b: &Entry) -> Ordering {
    name::hash_order(&a.name_nfc, &a.name_raw, &b.name_nfc, &b.name_raw)
}

fn kind_of(meta: &Metadata) -> Kind {
    let t = meta.file_type();
    if t.is_symlink() {
        Kind::Symlink
    } else if t.is_dir() {
        Kind::Dir
    } else if t.is_file() {
        Kind::File
    } else {
        Kind::Other
    }
}

pub fn mtime_ms(meta: &Metadata) -> Option<i64> {
    let time = meta.modified().ok()?;
    Some(match time.duration_since(UNIX_EPOCH) {
        Ok(after) => after.as_millis() as i64,
        Err(before) => -(before.duration().as_millis() as i64),
    })
}

#[cfg(unix)]
pub(crate) fn file_id(meta: &Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(meta.ino())
}

#[cfg(unix)]
fn nlink(meta: &Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(meta.nlink())
}

#[cfg(unix)]
fn other_volume(dir: &Metadata, child: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    dir.dev() != child.dev()
}

// TODO: Windows のファイル ID とハードリンクの数は、std ではまだ取れない（unstable）。
// `windows` crate の GetFileInformationByHandle で取る（docs/scan.md）。
#[cfg(not(unix))]
pub(crate) fn file_id(_meta: &Metadata) -> Option<u64> {
    None
}

#[cfg(not(unix))]
fn nlink(_meta: &Metadata) -> Option<u64> {
    None
}

/// Windows でフォルダの中にマウントしたボリュームは、リパースポイント（ジャンクションと同じ）なので
/// `Kind::Symlink` になり、ここには来ない。
#[cfg(not(unix))]
fn other_volume(_dir: &Metadata, _child: &Metadata) -> bool {
    false
}

/// Mac: ダウンロードしていないファイル（とフォルダ）に付く印。
#[cfg(target_os = "macos")]
pub fn cloud_only(meta: &Metadata) -> bool {
    use std::os::macos::fs::MetadataExt;
    const SF_DATALESS: u32 = 0x4000_0000;
    meta.st_flags() & SF_DATALESS != 0
}

/// Windows: クラウドのプレースホルダの属性。
#[cfg(windows)]
pub fn cloud_only(meta: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    windows_cloud_only(meta.file_attributes())
}

#[cfg(any(windows, test))]
fn windows_cloud_only(attributes: u32) -> bool {
    const FILE_ATTRIBUTE_OFFLINE: u32 = 0x0000_1000;
    const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x0004_0000;
    const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;
    attributes
        & (FILE_ATTRIBUTE_OFFLINE
            | FILE_ATTRIBUTE_RECALL_ON_OPEN
            | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS)
        != 0
}

#[cfg(not(any(target_os = "macos", windows)))]
pub fn cloud_only(_meta: &Metadata) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_recall_and_offline_attributes_skip_file_content() {
        for attributes in [0x0000_1000, 0x0004_0000, 0x0040_0000, 0x0044_1020] {
            assert!(windows_cloud_only(attributes));
        }
        for attributes in [0, 0x20, 0x0080_0000] {
            assert!(!windows_cloud_only(attributes));
        }
    }

    /// テストごとの空のフォルダ。終わったら消す。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("lisa-core-test-{label}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn lists_files_and_dirs_in_hash_order() {
        let tmp = TempDir::new("order");
        fs::write(tmp.0.join("b.txt"), b"hello").unwrap();
        fs::create_dir(tmp.0.join("a")).unwrap();
        fs::write(tmp.0.join("C.txt"), b"").unwrap();
        fs::write(tmp.0.join(".DS_Store"), b"junk").unwrap();

        let listing = read_dir(&tmp.0).unwrap();
        let names: Vec<_> = listing
            .entries
            .iter()
            .map(|e| e.name_nfc.as_str())
            .collect();
        assert_eq!(names, ["C.txt", "a", "b.txt"]);
        assert_eq!(listing.junk, 1);
        assert_eq!(listing.entries[1].kind, Kind::Dir);
        assert!(!listing.entries[1].other_volume);
        assert_eq!(listing.entries[2].kind, Kind::File);
        assert_eq!(listing.entries[2].size, 5);
        assert!(listing.entries[2].mtime_ms.is_some());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_not_followed() {
        let tmp = TempDir::new("symlink");
        fs::create_dir(tmp.0.join("real")).unwrap();
        std::os::unix::fs::symlink("real", tmp.0.join("link")).unwrap();
        std::os::unix::fs::symlink("nowhere", tmp.0.join("broken")).unwrap();

        let listing = read_dir(&tmp.0).unwrap();
        let link = listing
            .entries
            .iter()
            .find(|e| e.name_nfc == "link")
            .unwrap();
        assert_eq!(link.kind, Kind::Symlink);
        assert_eq!(link.link_target.as_deref(), Some(Path::new("real")));
        let broken = listing
            .entries
            .iter()
            .find(|e| e.name_nfc == "broken")
            .unwrap();
        assert_eq!(broken.kind, Kind::Symlink);
        assert!(listing.failed.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn hard_links_share_the_file_id() {
        let tmp = TempDir::new("hardlink");
        fs::write(tmp.0.join("one"), b"x").unwrap();
        fs::hard_link(tmp.0.join("one"), tmp.0.join("two")).unwrap();

        let listing = read_dir(&tmp.0).unwrap();
        let (one, two) = (&listing.entries[0], &listing.entries[1]);
        assert_eq!(one.file_id, two.file_id);
        assert_eq!(one.nlink, Some(2));
    }

    #[test]
    fn firmlinks_are_top_level_names() {
        let text = "/AppleInternal\tAppleInternal\n/Applications\tApplications\n\
                    /Library\tLibrary\n/System/Library/Caches\tSystem/Library/Caches\n\
                    /Users\tUsers\n";
        let names = parse_firmlinks(text);
        assert_eq!(names, ["AppleInternal", "Applications", "Library", "Users"]);
    }

    #[test]
    fn missing_dir_is_an_error() {
        let tmp = TempDir::new("missing");
        let error = read_dir(&tmp.0.join("nope")).unwrap_err();
        assert_eq!(Failure::of(&error), Failure::NotFound);
    }

    #[test]
    fn quarantine_dir_is_skipped() {
        let tmp = TempDir::new("quarantine-skip");
        fs::create_dir(tmp.0.join(name::QUARANTINE_DIR_NAME)).unwrap();
        fs::write(tmp.0.join(name::QUARANTINE_DIR_NAME).join("x.txt"), b"x").unwrap();
        fs::write(tmp.0.join("keep.txt"), b"ok").unwrap();

        let listing = read_dir(&tmp.0).unwrap();
        let names: Vec<_> = listing
            .entries
            .iter()
            .map(|e| e.name_nfc.as_str())
            .collect();
        assert_eq!(names, ["keep.txt"]);
    }
}
