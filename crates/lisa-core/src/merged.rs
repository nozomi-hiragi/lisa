//! マージ済みの目印。ディスク上の書式と列挙時の識別。

use std::fs::{self, Metadata, OpenOptions};
use std::io::Read;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::listing::Kind;

const FORMAT: &str = "lisa-merged";
const VERSION: u32 = 1;

pub mod links;
const MAX_BYTES: u64 = 64 * 1024;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct Location {
    pub device_id: String,
    pub device_name: String,
    pub volume_id: String,
    pub volume_name: String,
    /// OS に依存しない、ボリュームからのパスの各要素。
    pub relative_path: Vec<String>,
    pub display_path: String,
}

impl Location {
    pub(crate) fn local(db: &crate::db::Db, volume_id: &str, path: &Path) -> Option<Self> {
        let drive = crate::query::drives(db)
            .ok()?
            .into_iter()
            .find(|drive| drive.id == volume_id)?;
        Self::from_drive(db, drive, path)
    }

    /// DB にまだ行がない移動先も、記録済みのマウントから所在を組み立てられる。
    pub(crate) fn local_for_path(db: &crate::db::Db, path: &Path) -> Option<Self> {
        let drive = crate::query::drives(db)
            .ok()?
            .into_iter()
            .filter(|drive| {
                drive
                    .mount
                    .as_ref()
                    .is_some_and(|mount| path.starts_with(mount))
            })
            .max_by_key(|drive| drive.mount.as_ref().map(|mount| mount.components().count()))?;
        let mount = drive.mount.as_ref()?;
        let actual = crate::db::stored_components(mount, path)?
            .iter()
            .fold(mount.clone(), |path, name| path.join(name));
        Self::from_drive(db, drive, &actual)
    }

    fn from_drive(db: &crate::db::Db, drive: crate::query::Drive, path: &Path) -> Option<Self> {
        let device_id = db.meta("this_device").ok()??;
        let (device_name, _) = crate::query::this_device(db).ok()??;
        let mount = drive.mount.as_ref()?;
        // 名前の無いボリューム（Windows の C: など）は、マウント先で呼ぶ。
        let volume_name = if drive.label.trim().is_empty() {
            let text = mount.to_str()?;
            let trimmed = text.trim_end_matches(['\\', '/']);
            if trimmed.is_empty() { text } else { trimmed }.to_owned()
        } else {
            drive.label
        };
        let relative_path = path
            .strip_prefix(mount)
            .ok()?
            .components()
            .map(|component| component.as_os_str().to_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()?;
        let location = Self {
            device_id,
            device_name,
            volume_id: drive.id,
            volume_name,
            relative_path,
            display_path: path.to_str()?.to_owned(),
        };
        location.valid().then_some(location)
    }

    pub(crate) fn valid(&self) -> bool {
        [
            &self.device_id,
            &self.device_name,
            &self.volume_id,
            &self.volume_name,
            &self.display_path,
        ]
        .into_iter()
        .all(|value| !value.is_empty() && !value.contains('\0'))
            && !self.relative_path.is_empty()
            && self.relative_path.iter().all(|part| {
                !matches!(part.as_str(), "" | "." | "..") && !part.contains(['/', '\0'])
            })
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ContentHash {
    pub algorithm: String,
    pub version: u32,
    pub value: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ExecutionTime {
    pub unix_ms: i64,
    pub timezone: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct Marker {
    format: String,
    version: u32,
    pub link_id: String,
    pub source: Location,
    pub target: Location,
    pub kind: Kind,
    pub size: u64,
    pub hash: ContentHash,
    pub executed_at: ExecutionTime,
    pub plan_id: i64,
    pub item_id: i64,
}

impl Marker {
    fn valid(&self) -> bool {
        self.format == FORMAT
            && self.version == VERSION
            && uuid::Uuid::parse_str(&self.link_id)
                .is_ok_and(|id| !id.is_nil() && id.to_string() == self.link_id)
            && matches!(self.kind, Kind::File | Kind::Dir)
            && self.source.valid()
            && self.target.valid()
            && self.hash.algorithm == crate::hash::ALGO
            && self.hash.version == crate::dups::COMPARISON_FORMAT
            && blake3::Hash::from_hex(&self.hash.value).is_ok()
            && self.executed_at.timezone == "UTC"
            && self.plan_id > 0
            && self.item_id >= 0
    }

    pub(crate) fn new(
        source: Location,
        target: Location,
        key: crate::dups::ContentKey,
        plan_id: i64,
        item_id: i64,
    ) -> Self {
        Self {
            format: FORMAT.into(),
            version: VERSION,
            link_id: uuid::Uuid::new_v4().to_string(),
            source,
            target,
            kind: if key.directory { Kind::Dir } else { Kind::File },
            size: key.size,
            hash: ContentHash {
                algorithm: crate::hash::ALGO.into(),
                version: crate::dups::COMPARISON_FORMAT,
                value: blake3::Hash::from_bytes(key.hash).to_hex().to_string(),
            },
            executed_at: ExecutionTime {
                unix_ms: crate::db::now_ms(),
                timezone: "UTC".into(),
            },
            plan_id,
            item_id,
        }
    }
}

/// 普通のファイルで、拡張子と本文が一致する目印だけを読む。
pub fn read(path: &Path) -> Option<Marker> {
    let metadata = fs::symlink_metadata(path).ok()?;
    read_with_metadata(path, &metadata)
}

/// Finder・エクスプローラーから開かれた目印を読む。読めなければ利用者に見せる理由を返す。
pub fn inspect(path: &Path) -> Result<Marker, String> {
    let name = path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    let kind = match path.extension().and_then(|ext| ext.to_str()) {
        Some("lmf") => Kind::File,
        Some("lmd") => Kind::Dir,
        _ => return Err(format!("{name} は .lmf／.lmd の目印ではありません")),
    };
    let metadata =
        fs::symlink_metadata(path).map_err(|error| format!("{name} を開けません: {error}"))?;
    if !metadata.is_file() {
        return Err(format!("{name} は普通のファイルではありません"));
    }
    let bytes = read_bytes(path, &metadata)
        .ok_or_else(|| format!("{name} を読めません（大きすぎるか、手元にありません）"))?;
    let marker: Marker = serde_json::from_slice(&bytes)
        .map_err(|_| format!("{name} は Lisa のマージ済みリンクの形式ではありません"))?;
    if marker.format != FORMAT || marker.version != VERSION {
        return Err(format!("{name} は対応していない版の目印です"));
    }
    if marker.kind != kind {
        return Err(format!("{name} は拡張子と中身の種類が一致しません"));
    }
    if !marker.valid() {
        return Err(format!("{name} は必要な項目が欠けているか正しくありません"));
    }
    Ok(marker)
}

pub(crate) fn read_with_metadata(path: &Path, metadata: &Metadata) -> Option<Marker> {
    let kind = match path.extension()?.to_str()? {
        "lmf" => Kind::File,
        "lmd" => Kind::Dir,
        _ => return None,
    };
    let bytes = read_bytes(path, metadata)?;
    let marker: Marker = serde_json::from_slice(&bytes).ok()?;
    (marker.kind == kind && marker.valid()).then_some(marker)
}

pub(crate) fn read_bytes(path: &Path, metadata: &Metadata) -> Option<Vec<u8>> {
    if !metadata.is_file() || metadata.len() > MAX_BYTES || crate::listing::cloud_only(metadata) {
        return None;
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path).ok()?;
    let opened = file.metadata().ok()?;
    if !opened.is_file() || crate::listing::cloud_only(&opened) {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > MAX_BYTES {
        return None;
    }
    Some(bytes)
}

/// 目印が指定する端末・ボリューム・パスの現物を再確認して、木を開くための道のりを返す。
pub fn resolve_target(db: &crate::db::Db, marker: &Marker) -> Result<Vec<i64>, String> {
    use std::path::Component;

    if !marker.valid() {
        return Err("目印の書式が不正です".into());
    }
    if db
        .meta("this_device")
        .map_err(|error| error.to_string())?
        .as_deref()
        != Some(marker.target.device_id.as_str())
    {
        return Err("マージ先の端末が一致しません".into());
    }
    let drive = crate::query::drives(db)
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|drive| drive.id == marker.target.volume_id)
        .ok_or("マージ先のボリュームが見つかりません")?;
    let mut path = drive.mount.ok_or("マージ先のボリュームが未接続です")?;
    for (index, part) in marker.target.relative_path.iter().enumerate() {
        let mut components = Path::new(part).components();
        if !matches!(components.next(), Some(Component::Normal(name)) if name == std::ffi::OsStr::new(part))
            || components.next().is_some()
            || cfg!(windows) && part.contains(':')
        {
            return Err("マージ先のパスが不正です".into());
        }
        path.push(part);
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                "マージ先が見つかりません（移動・改名・削除）".into()
            } else {
                format!("マージ先を確かめられません: {error}")
            }
        })?;
        let last = index + 1 == marker.target.relative_path.len();
        if crate::listing::cloud_only(&metadata) {
            return Err("マージ先の中身を確認できません".into());
        }
        if (!last || marker.kind == Kind::Dir) && !metadata.is_dir()
            || last && marker.kind == Kind::File && !metadata.is_file()
        {
            return Err("マージ先の種類が変わっています".into());
        }
    }
    let located = crate::query::locate(db, &path)
        .map_err(|error| error.to_string())?
        .filter(|location| location.drive_id == drive.id && location.path == path)
        .ok_or("マージ先は未検証です")?;
    let actual = if marker.kind == Kind::Dir {
        crate::organize::markers::target_folder_hash(db, &path)?
    } else {
        crate::hash::hash_file(&path)
            .map_err(|error| format!("マージ先のハッシュを確認できません: {error}"))?
    };
    if blake3::Hash::from_hex(&marker.hash.value)
        .ok()
        .as_ref()
        .map(blake3::Hash::as_bytes)
        != Some(&actual)
    {
        return Err("マージ先の内容が変わっています".into());
    }
    Ok(located.chain)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unnamed_volume_is_called_by_its_mount() {
        let root = std::env::temp_dir().join(format!("lisa-merged-label-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("vol/写真")).unwrap();
        let mut db = crate::db::Db::open(&root.join("lisa.db")).unwrap();
        let device = db.this_device("新PC", "新PC", "windows").unwrap();
        db.upsert_drive(
            &device,
            &crate::volumes::Volume {
                id: Some("test:unnamed".into()),
                label: String::new(),
                fs: "ntfs".into(),
                mount_path: root.join("vol"),
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
        let location = Location::local(&db, "test:unnamed", &root.join("vol/写真/海.jpg")).unwrap();
        assert_eq!(location.volume_name, root.join("vol").to_str().unwrap());
        assert_eq!(location.relative_path, ["写真", "海.jpg"]);
        drop(db);
        fs::remove_dir_all(&root).unwrap();
    }
}
