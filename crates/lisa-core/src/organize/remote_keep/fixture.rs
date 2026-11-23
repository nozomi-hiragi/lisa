//! 明示的に作った合成領域だけを使う検証用アダプター。

use std::{os::unix::fs::MetadataExt, path::Path};

use crate::{
    hash::{self, Hash},
    listing::Kind,
    name,
};

use super::{KeepReference, MAX_PROTECTED_ENTRIES, display};

pub(super) fn identify(path: &Path) -> Result<[u64; 2], String> {
    fixture(path)?;
    let metadata = std::fs::symlink_metadata(path).map_err(display)?;
    Ok([metadata.dev(), metadata.ino()])
}

fn fixture(path: &Path) -> Result<(), String> {
    let root = std::env::var_os("LISA_VERIFY_KEEP_ROOT")
        .ok_or("残す側が macOS のため、整理は依頼できません。比較表示のみ")?;
    let root = std::fs::canonicalize(root).map_err(display)?;
    let temporary = std::fs::canonicalize(std::env::temp_dir()).map_err(display)?;
    if root.parent() != Some(temporary.as_path())
        || !root
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("lisa-keep-fixture-"))
        || !path.starts_with(&root)
        || std::fs::canonicalize(path).map_err(display)? != path
    {
        return Err("検証用の合成ディレクトリの外です".into());
    }
    Ok(())
}

pub(super) fn verify(item: &KeepReference, path: &Path) -> Result<(), String> {
    let identity = identify(path)?;
    let kind = if item.key.directory {
        Kind::Dir
    } else {
        Kind::File
    };
    let mut count = 0;
    let (size, digest) = snapshot(path, kind, &mut count)?;
    if item.identity != identity || item.key.size != size || item.key.hash != digest {
        return Err(
            "合成ファイルの実体または中身が変わりました。重複を再取得して確認してください".into(),
        );
    }
    Ok(())
}

fn snapshot(path: &Path, kind: Kind, count: &mut usize) -> Result<(u64, Hash), String> {
    *count += 1;
    if *count > MAX_PROTECTED_ENTRIES {
        return Err("検証する子孫の上限を超えました".into());
    }
    let meta = std::fs::symlink_metadata(path).map_err(display)?;
    if kind == Kind::File && meta.is_file() {
        return Ok((meta.len(), hash::hash_file(path).map_err(display)?));
    }
    if kind != Kind::Dir || !meta.is_dir() {
        return Err("合成項目の種別が変わりました".into());
    }
    let mut entries = Vec::new();
    for child in std::fs::read_dir(path).map_err(display)? {
        let child = child.map_err(display)?;
        let raw = child.file_name();
        if name::is_os_junk(&raw) || name::is_quarantine_dir(&raw) {
            continue;
        }
        let kind = child.file_type().map_err(display)?;
        let kind = if kind.is_dir() {
            Kind::Dir
        } else if kind.is_file() {
            Kind::File
        } else {
            return Err("保護できない合成項目です".into());
        };
        let (size, digest) = snapshot(&child.path(), kind, count)?;
        entries.push((name::nfc(&raw), name::raw_bytes(&raw), kind, size, digest));
    }
    entries.sort_by(|a, b| name::hash_order(&a.0, &a.1, &b.0, &b.1));
    let parts: Vec<_> = entries
        .iter()
        .map(|(name, _, kind, size, digest)| hash::Child {
            kind: *kind,
            name_nfc: name,
            hash: Some(digest),
            size: *size,
        })
        .collect();
    Ok((
        parts.iter().map(|part| part.size).sum(),
        hash::folder_hash(&parts),
    ))
}
