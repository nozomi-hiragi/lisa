use std::fs::{self, File};
use std::io::{Read, Seek};
use std::os::windows::{
    ffi::OsStrExt,
    fs::{MetadataExt, OpenOptionsExt},
    io::AsRawHandle,
};
use std::path::{Path, PathBuf};

use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_ID_INFO, FILE_RENAME_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FileIdInfo, FileRenameInfo, GetFileInformationByHandleEx, SetFileInformationByHandle,
};

use super::{InspectRole, KeepReference, MAX_PROTECTED_ENTRIES, display};
use crate::{
    db::Db,
    hash::{self, Hash},
    listing::{self, Kind},
    name, query, volumes,
};

/// 開く対象の役割。残す側は書込・削除・改名を拒否する。隔離元は確認のあいだ書込を拒否する。
/// NTFS は配下に開いたハンドルがあるフォルダを共有モードに関係なく改名させない（実機で
/// ERROR_ACCESS_DENIED）ので、隔離元の子孫は改名の直前に閉じ、隔離元そのものだけを持って改名する。
#[derive(Clone, Copy, PartialEq)]
enum Role {
    Keep,
    Source,
    SourceDescendant,
}

impl Role {
    fn share(self) -> u32 {
        match self {
            Self::Keep => FILE_SHARE_READ,
            Self::Source | Self::SourceDescendant => FILE_SHARE_READ | FILE_SHARE_DELETE,
        }
    }
}

type Listing = (PathBuf, Vec<Entry>);

pub(super) struct Protected {
    mount: PathBuf,
    opened: Vec<File>,
    root: usize,
    listings: Vec<Listing>,
}

impl Protected {
    pub(super) fn open(
        db: &Db,
        keep: &KeepReference,
        path: &Path,
        inspect: InspectRole,
    ) -> Result<Self, String> {
        let mount = local_mount(db, keep, inspect)?;
        let (opened, root, listings) = protect(&mount, keep, path, Role::Keep, inspect)?;
        Ok(Self {
            mount,
            opened,
            root,
            listings,
        })
    }

    pub(super) fn source(db: &Db, item: &KeepReference, path: &Path) -> Result<Self, String> {
        let mount = local_mount(db, item, InspectRole::Source)?;
        let (opened, root, listings) =
            protect(&mount, item, path, Role::Source, InspectRole::Source)?;
        Ok(Self {
            mount,
            opened,
            root,
            listings,
        })
    }

    pub(super) fn isolate(mut self, destination: &Path) -> std::io::Result<()> {
        // 子孫のハンドルを閉じる（開いたままだとフォルダの改名が拒否される）。
        // 閉じてから改名までのあいだに子が書き換わっても、隔離物として残るだけで、残す側は守られたまま。
        self.opened.truncate(self.root + 1);
        let parent = destination
            .parent()
            .ok_or(std::io::ErrorKind::InvalidInput)?;
        let file = &self.opened[self.root];
        let id = identity(file).map_err(std::io::Error::other)?;
        let relative = parent
            .strip_prefix(&self.mount)
            .map_err(std::io::Error::other)?;
        let mut handles = Vec::new();
        let mut part = self.mount.clone();
        open_directory_for_destination(&part, id[0], &mut handles)
            .map_err(std::io::Error::other)?;
        for component in relative.components() {
            let std::path::Component::Normal(component) = component else {
                return Err(std::io::ErrorKind::InvalidInput.into());
            };
            part.push(component);
            open_directory_for_destination(&part, id[0], &mut handles)
                .map_err(std::io::Error::other)?;
        }
        // Win32 の SetFileInformationByHandle はディレクトリハンドル相対の改名を受け付けない
        // （ERROR_INVALID_PARAMETER）。絶対パスで渡し、途中のフォルダは上で削除共有なしに
        // 開いたまま持つことで、差し替えられないようにする。
        if destination.file_name().is_none() {
            return Err(std::io::ErrorKind::InvalidInput.into());
        }
        let filename: Vec<_> = destination.as_os_str().encode_wide().chain([0]).collect();
        let bytes = std::mem::offset_of!(FILE_RENAME_INFO, FileName) + filename.len() * 2;
        let bytes = bytes.max(std::mem::size_of::<FILE_RENAME_INFO>());
        let mut storage = vec![0usize; bytes.div_ceil(std::mem::size_of::<usize>())];
        let info = storage.as_mut_ptr().cast::<FILE_RENAME_INFO>();
        // SAFETY: 領域は構造体に整列し、可変長の名前と終端まで確保済み。ハンドルは開いている。
        let result = unsafe {
            (*info).Anonymous.ReplaceIfExists = false;
            (*info).RootDirectory = std::ptr::null_mut();
            (*info).FileNameLength = ((filename.len() - 1) * 2) as u32;
            std::ptr::copy_nonoverlapping(
                filename.as_ptr(),
                (*info).FileName.as_mut_ptr(),
                filename.len(),
            );
            SetFileInformationByHandle(
                file.as_raw_handle().cast(),
                FileRenameInfo,
                info.cast(),
                bytes as u32,
            )
        };
        let error = (result == 0).then(std::io::Error::last_os_error);
        // 改名が済むまで途中のフォルダを開いたままにしておく。
        drop(handles);
        error.map_or(Ok(()), Err)
    }

    pub(super) fn verify(&mut self, keep: &KeepReference, path: &Path) -> Result<(), String> {
        let (opened, _, listings) =
            protect(&self.mount, keep, path, Role::Keep, InspectRole::Keep)?;
        self.opened = opened;
        self.listings = listings;
        Ok(())
    }

    /// 予約中は共有拒否で開いたままのハンドルが書込・削除・改名を止めているので、
    /// 再ハッシュはせず、ハンドルが生きていることと物理識別子・子の一覧だけを確認する。
    pub(super) fn verify_held(&mut self, keep: &KeepReference, path: &Path) -> Result<(), String> {
        let changed = || "残す項目が変更されました。重複を再取得して確認してください".to_string();
        let root = self.opened.get(self.root).ok_or_else(changed)?;
        if identity(root)? != keep.identity
            || identify(&self.mount, path, InspectRole::Keep)? != keep.identity
        {
            return Err(changed());
        }
        for file in &self.opened {
            identity(file)?;
        }
        for (directory, listed) in &self.listings {
            if *listed != entries_of(directory, 0, InspectRole::Keep)? {
                return Err(
                    "残すフォルダの子孫が変わりました。重複を再取得して確認してください".into(),
                );
            }
        }
        Ok(())
    }
}

pub(super) fn local_mount(
    db: &Db,
    keep: &KeepReference,
    inspect: InspectRole,
) -> Result<PathBuf, String> {
    let mount = query::drives(db)
        .map_err(display)?
        .into_iter()
        .find(|drive| drive.id == keep.drive_id)
        .and_then(|drive| drive.mount)
        .ok_or_else(|| format!("{}のボリュームが見つかりません", inspect.side()))?;
    let local_ntfs = volumes::list().map_err(display)?.iter().any(|volume| {
        !volume.network
            && !volume.removable
            && volume.fs.eq_ignore_ascii_case("NTFS")
            && volume.id.as_deref() == Some(&keep.drive_id)
            && volume.mount_path == mount
    });
    if !local_ntfs {
        return Err(format!(
            "{}がローカル NTFS ではないため、整理は依頼できません。比較表示のみ",
            inspect.side()
        ));
    }
    Ok(mount)
}

fn protect(
    mount: &Path,
    keep: &KeepReference,
    path: &Path,
    role: Role,
    inspect: InspectRole,
) -> Result<(Vec<File>, usize, Vec<Listing>), String> {
    let relative = path.strip_prefix(mount).map_err(display)?;
    let mut handles = Vec::new();
    let mut current = mount.to_path_buf();
    for part in relative.components() {
        let std::path::Component::Normal(part) = part else {
            return Err(format!("{}のパスを確認できません", inspect.item()));
        };
        open(
            &current,
            Kind::Dir,
            keep.identity[0],
            &mut handles,
            Role::Keep,
            inspect,
        )?;
        current.push(part);
    }
    let kind = if keep.key.directory {
        Kind::Dir
    } else {
        Kind::File
    };
    let root = handles.len();
    let mut listings = Vec::new();
    let (identity, size, hash) = snapshot(
        &current,
        kind,
        keep.identity[0],
        &mut handles,
        role,
        inspect,
        &mut listings,
    )?;
    if identity != keep.identity || size != keep.key.size || hash != keep.key.hash {
        return Err(format!(
            "{}が変更されました。重複を再取得して確認してください",
            inspect.item()
        ));
    }
    Ok((handles, root, listings))
}

fn open(
    path: &Path,
    kind: Kind,
    volume: u64,
    handles: &mut Vec<File>,
    role: Role,
    inspect: InspectRole,
) -> Result<[u64; 2], String> {
    if handles.len() >= MAX_PROTECTED_ENTRIES {
        return Err(format!(
            "{}の保護上限（{MAX_PROTECTED_ENTRIES} 件）を超えました",
            inspect.item()
        ));
    }
    let mut options = File::options();
    options.read(true);
    if role == Role::Source {
        options.access_mode(windows_sys::Win32::Foundation::GENERIC_READ | DELETE);
    }
    let file = options
        .share_mode(role.share())
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|e| format!("{}を安定して開けません: {e}", inspect.item()))?;
    let metadata = file.metadata().map_err(display)?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || listing::cloud_only(&metadata)
        || (kind == Kind::Dir && !metadata.is_dir())
        || (kind == Kind::File && !metadata.is_file())
    {
        return Err(format!(
            "{}の種別が変わったか、リンク・クラウド項目を含みます",
            inspect.item()
        ));
    }
    let id = identity(&file)?;
    if id[0] != volume {
        return Err(format!(
            "{}とボリュームの同一性を確認できません",
            inspect.item()
        ));
    }
    handles.push(file);
    Ok(id)
}

pub(super) fn identify(
    mount: &Path,
    path: &Path,
    inspect: InspectRole,
) -> Result<[u64; 2], String> {
    let relative = path.strip_prefix(mount).map_err(display)?;
    let mut current = mount.to_path_buf();
    let mut ancestors = Vec::new();
    for component in relative.components() {
        let std::path::Component::Normal(component) = component else {
            return Err(format!("{}のパスを確認できません", inspect.item()));
        };
        let dir = File::options()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&current)
            .map_err(display)?;
        let info = dir.metadata().map_err(display)?;
        if !info.is_dir()
            || info.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || listing::cloud_only(&info)
        {
            return Err(format!("{}の親フォルダを保護できません", inspect.item()));
        }
        ancestors.push(dir);
        current.push(component);
    }
    let file = File::options()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&current)
        .map_err(display)?;
    let info = file.metadata().map_err(display)?;
    if info.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 || listing::cloud_only(&info) {
        return Err(format!("{}がリンクまたはクラウド項目です", inspect.item()));
    }
    identity(&file)
}

fn open_directory_for_destination(
    path: &Path,
    volume: u64,
    handles: &mut Vec<File>,
) -> Result<(), String> {
    if handles.len() >= MAX_PROTECTED_ENTRIES {
        return Err("隔離先の親フォルダの確認上限を超えました".into());
    }
    let file = File::options()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(display)?;
    let metadata = file.metadata().map_err(display)?;
    if !metadata.is_dir()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || listing::cloud_only(&metadata)
        || identity(&file)?[0] != volume
    {
        return Err("隔離先が元のボリュームの通常フォルダではありません".into());
    }
    handles.push(file);
    Ok(())
}

fn identity(file: &File) -> Result<[u64; 2], String> {
    let mut info = FILE_ID_INFO::default();
    // SAFETY: ハンドルは借用中開いており、構造体の全領域を書き込み用に渡す。
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle().cast(),
            FileIdInfo,
            (&mut info as *mut FILE_ID_INFO).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    } == 0
    {
        return Err(display(std::io::Error::last_os_error()));
    }
    let id = info.FileId.Identifier;
    if id[8..] != [0; 8] {
        return Err("NTFS のファイル ID として確認できません".into());
    }
    Ok([
        info.VolumeSerialNumber,
        u64::from_le_bytes(id[..8].try_into().expect("8 bytes")),
    ])
}

fn snapshot(
    path: &Path,
    kind: Kind,
    volume: u64,
    handles: &mut Vec<File>,
    role: Role,
    inspect: InspectRole,
    listings: &mut Vec<Listing>,
) -> Result<([u64; 2], u64, Hash), String> {
    let id = open(path, kind, volume, handles, role, inspect)?;
    if kind == Kind::File {
        let file = handles.last_mut().expect("open 直後");
        let size = file.metadata().map_err(display)?.len();
        file.rewind().map_err(display)?;
        let mut hasher = hash::Hasher::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let n = file.read(&mut buffer).map_err(display)?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
        }
        return Ok((id, size, *hasher.finalize().as_bytes()));
    }
    let entries = entries_of(path, handles.len(), inspect)?;
    let mut observed = Vec::new();
    let mut total = 0u64;
    let descendant = match role {
        Role::Keep => Role::Keep,
        Role::Source | Role::SourceDescendant => Role::SourceDescendant,
    };
    for (name, _, child, kind) in &entries {
        let (_, size, hash) =
            snapshot(child, *kind, volume, handles, descendant, inspect, listings)?;
        total = total
            .checked_add(size)
            .ok_or_else(|| format!("{}のサイズが大きすぎます", inspect.folder()))?;
        observed.push((name, *kind, size, hash));
    }
    // 配下の既存ハンドルを保持していても、新規項目の作成は別の操作なので一覧を再確認する。
    if entries != entries_of(path, 0, inspect)? {
        return Err(format!(
            "{}の子孫が変わりました。重複を再取得して確認してください",
            inspect.folder()
        ));
    }
    let children: Vec<_> = observed
        .iter()
        .map(|(name, kind, size, hash)| hash::Child {
            name_nfc: name,
            kind: *kind,
            size: *size,
            hash: Some(hash),
        })
        .collect();
    let folder = hash::folder_hash(&children);
    drop(children);
    drop(observed);
    listings.push((path.to_path_buf(), entries));
    Ok((id, total, folder))
}

type Entry = (String, Vec<u8>, PathBuf, Kind);

fn entries_of(path: &Path, held: usize, inspect: InspectRole) -> Result<Vec<Entry>, String> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(path).map_err(display)? {
        let entry = entry.map_err(display)?;
        let raw_name = entry.file_name();
        if name::is_os_junk(&raw_name) || name::is_quarantine_dir(&raw_name) {
            continue;
        }
        if entries.len() + held >= MAX_PROTECTED_ENTRIES {
            return Err(format!(
                "{}の保護上限（{MAX_PROTECTED_ENTRIES} 件）を超えました",
                inspect.item()
            ));
        }
        let typ = entry.file_type().map_err(display)?;
        let kind = if typ.is_dir() {
            Kind::Dir
        } else if typ.is_file() {
            Kind::File
        } else {
            return Err(format!(
                "{}に保護できない項目が含まれています",
                inspect.folder()
            ));
        };
        entries.push((
            name::nfc(&raw_name),
            name::raw_bytes(&raw_name),
            entry.path(),
            kind,
        ));
    }
    entries.sort_by(|a, b| name::hash_order(&a.0, &a.1, &b.0, &b.1));
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_isolated_side_allows_delete_sharing_and_nobody_allows_write_sharing() {
        assert_eq!(Role::Keep.share() & FILE_SHARE_DELETE, 0);
        assert_ne!(Role::Source.share() & FILE_SHARE_DELETE, 0);
        assert_ne!(Role::SourceDescendant.share() & FILE_SHARE_DELETE, 0);
        for role in [Role::Keep, Role::Source, Role::SourceDescendant] {
            assert_eq!(role.share() & FILE_SHARE_WRITE, 0);
        }
    }
}
