//! 付け替え先と照合するための、ファイル本体を小分けにした転送。

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::db::Db;
use crate::listing::Kind;
use crate::merged::Marker;

use super::{available_space, error, exists, verify_content};

/// 1 回に送る大きさ。base64 にしても HTTP 本文の上限（6 MiB）に収まる。
pub const CHUNK_BYTES: usize = 4 * 1024 * 1024;
/// 送る側が同時に送る本数。
pub const LANES: usize = 3;
/// 先に届いた小分けを、前が届くまでメモリに持つ上限（本数）。
const MAX_AHEAD: u64 = 8;
/// 先に届いた小分けをメモリに持つ合計の上限（すべての転送で）。
const MAX_AHEAD_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_ENTRIES: usize = 1000;
pub const MAX_IDLE_MS: i64 = 24 * 60 * 60 * 1000;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct Part {
    pub components: Vec<String>,
    pub directory: bool,
    pub len: u64,
    pub modified_ms: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct Manifest {
    pub entries: Vec<Part>,
}

impl Manifest {
    pub fn scan(source: &Path) -> Result<Self, String> {
        let mut entries = Vec::new();
        scan(source, &mut Vec::new(), &mut entries)?;
        Ok(Self { entries })
    }

    pub(crate) fn validate(&self, marker: &Marker) -> Result<(), String> {
        let Some(root) = self.entries.first() else {
            return Err("転送する項目がありません".into());
        };
        if !root.components.is_empty()
            || self.entries.len() > MAX_ENTRIES
            || root.directory != (marker.kind == Kind::Dir)
        {
            return Err("転送する項目の種類または件数が一致しません".into());
        }
        let mut seen = std::collections::HashSet::new();
        let mut dirs = std::collections::HashSet::new();
        let mut bytes = 0u64;
        for part in &self.entries {
            if part.components.iter().any(|part| !normal_name(part))
                || !seen.insert(part.components.as_slice())
                || (!part.components.is_empty()
                    && !dirs.contains(&part.components[..part.components.len() - 1]))
            {
                return Err("転送するファイルの相対パスが正しくありません".into());
            }
            if part.directory {
                dirs.insert(part.components.as_slice());
            } else {
                bytes = bytes
                    .checked_add(part.len)
                    .ok_or("転送サイズが大きすぎます")?;
            }
        }
        if bytes != marker.size {
            return Err("転送する実体の大きさが一致しません".into());
        }
        Ok(())
    }
}

fn normal_name(part: &str) -> bool {
    !part.contains('\0')
        && !(cfg!(windows) && part.contains(':'))
        && matches!(Path::new(part).components().next(), Some(Component::Normal(name)) if name == std::ffi::OsStr::new(part))
}

fn scan(path: &Path, components: &mut Vec<String>, entries: &mut Vec<Part>) -> Result<(), String> {
    if entries.len() >= MAX_ENTRIES {
        return Err("一度に転送できるファイルとフォルダの件数を超えました".into());
    }
    let meta = fs::symlink_metadata(path).map_err(error)?;
    if crate::listing::cloud_only(&meta) || (!meta.is_file() && !meta.is_dir()) {
        return Err(format!("転送できない項目があります: {}", path.display()));
    }
    let modified_ms = meta
        .modified()
        .map_err(error)?
        .duration_since(UNIX_EPOCH)
        .map_err(error)?
        .as_millis()
        .try_into()
        .map_err(|_| "更新日時が範囲外です")?;
    entries.push(Part {
        components: components.clone(),
        directory: meta.is_dir(),
        len: if meta.is_file() { meta.len() } else { 0 },
        modified_ms,
    });
    if meta.is_dir() {
        let listing = crate::listing::read_dir(path).map_err(error)?;
        if !listing.failed.is_empty() {
            return Err(format!("転送できない項目があります: {}", path.display()));
        }
        for entry in listing.entries {
            if entry.other_volume || entry.cloud_only || entry.merged.is_some() {
                return Err(format!(
                    "転送できない項目があります: {}",
                    path.join(&entry.name).display()
                ));
            }
            let name = entry
                .name
                .to_str()
                .ok_or("項目名を転送用に記録できません")?;
            components.push(name.to_owned());
            scan(&path.join(name), components, entries)?;
            components.pop();
        }
    }
    Ok(())
}

fn part_path(path: &Path, part: &Part) -> PathBuf {
    let mut path = path.to_path_buf();
    path.extend(&part.components);
    path
}

/// 既にあるバイト数までを返す。stage は所有を確かめた一時領域の中に限る。
pub(crate) fn offsets(stage: &Path, manifest: &Manifest) -> Result<Vec<u64>, String> {
    manifest
        .entries
        .iter()
        .map(|part| {
            let path = part_path(stage, part);
            match fs::symlink_metadata(&path) {
                Ok(meta) if meta.is_file() && !part.directory && meta.len() <= part.len => {
                    Ok(meta.len())
                }
                Ok(meta) if meta.is_dir() && part.directory => Ok(0),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(0),
                _ => Err(format!(
                    "一時項目の種類または大きさが違います: {}",
                    path.display()
                )),
            }
        })
        .collect()
}

pub(crate) fn begin(
    stage: &Path,
    manifest: &Manifest,
    marker: &Marker,
) -> Result<Vec<u64>, String> {
    manifest.validate(marker)?;
    let parent = stage.parent().ok_or("転送先の一時領域がありません")?;
    let before = offsets(stage, manifest)?;
    let received = before.iter().sum::<u64>();
    if available_space(parent)? < marker.size.saturating_sub(received) {
        return Err(format!(
            "戻し先の空き容量が足りません: {}",
            parent.display()
        ));
    }
    for part in &manifest.entries {
        let path = part_path(stage, part);
        if exists(&path)? {
            continue;
        }
        if part.directory {
            fs::create_dir(&path).map_err(error)?;
        } else {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .map_err(error)?;
        }
    }
    offsets(stage, manifest)
}

pub(crate) fn read_chunk(source: &Path, part: &Part, offset: u64) -> Result<Vec<u8>, String> {
    if part.directory || offset >= part.len {
        return Err("転送するファイルの位置が正しくありません".into());
    }
    let path = part_path(source, part);
    let metadata = fs::symlink_metadata(&path).map_err(error)?;
    if !metadata.is_file() || metadata.len() != part.len {
        return Err(format!("転送中に実体が変わりました: {}", path.display()));
    }
    let mut file = File::open(path).map_err(error)?;
    file.seek(SeekFrom::Start(offset)).map_err(error)?;
    let mut out = vec![0u8; (part.len - offset).min(CHUNK_BYTES as u64) as usize];
    file.read_exact(&mut out).map_err(error)?;
    Ok(out)
}

pub(crate) fn append_chunk(
    stage: &Path,
    part: &Part,
    offset: u64,
    bytes: &[u8],
) -> Result<u64, String> {
    let end = offset
        .checked_add(bytes.len() as u64)
        .ok_or("転送位置が正しくありません")?;
    if part.directory || bytes.is_empty() || bytes.len() > CHUNK_BYTES || end > part.len {
        return Err("転送するバイト数が正しくありません".into());
    }
    let path = part_path(stage, part);
    let metadata = fs::symlink_metadata(&path).map_err(error)?;
    if !metadata.is_file() || metadata.len() < offset || metadata.len() > part.len {
        return Err(format!(
            "一時ファイルの転送位置が一致しません: {}",
            path.display()
        ));
    }
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .map_err(error)?;
    file.seek(SeekFrom::Start(offset)).map_err(error)?;
    let written = (metadata.len() - offset).min(bytes.len() as u64) as usize;
    if written > 0 {
        let mut previous = vec![0; written];
        file.read_exact(&mut previous).map_err(error)?;
        if previous != bytes[..written] {
            return Err(format!(
                "再送したファイルの内容が変わっています: {}",
                path.display()
            ));
        }
    }
    if written < bytes.len() {
        file.write_all(&bytes[written..]).map_err(error)?;
        file.sync_data().map_err(error)?;
    }
    Ok(metadata.len().max(end))
}

/// 先に届いた小分け（転送ごと・ファイルごと）。受けた側のプロセスが落ちれば消え、送る側が送り直す。
type Ahead = std::collections::HashMap<String, std::collections::BTreeMap<u64, Vec<u8>>>;
static AHEAD: std::sync::Mutex<Option<Ahead>> = std::sync::Mutex::new(None);

/// 小分けを受ける。前が揃っていれば書き足し、先に届いたものは少しだけメモリに持つ。
/// 返すのは先頭から連続して書けたバイト数（中断後はここから再開する）。
pub(crate) fn accept_chunk(
    key: &str,
    stage: &Path,
    part: &Part,
    offset: u64,
    bytes: Vec<u8>,
) -> Result<u64, String> {
    let path = part_path(stage, part);
    let written = fs::symlink_metadata(&path).map_err(error)?.len();
    let end = offset
        .checked_add(bytes.len() as u64)
        .ok_or("転送位置が正しくありません")?;
    if part.directory || bytes.is_empty() || bytes.len() > CHUNK_BYTES || end > part.len {
        return Err("転送するバイト数が正しくありません".into());
    }
    let mut ahead = AHEAD.lock().unwrap_or_else(|error| error.into_inner());
    let ahead = ahead.get_or_insert_with(Default::default);
    if offset > written {
        let held: usize = ahead
            .values()
            .flat_map(|chunks| chunks.values())
            .map(Vec::len)
            .sum();
        let waiting = ahead.get(key).map_or(0, |chunks| chunks.len() as u64);
        if offset - written > MAX_AHEAD * CHUNK_BYTES as u64
            || waiting >= MAX_AHEAD
            || held + bytes.len() > MAX_AHEAD_BYTES
        {
            return Err("先に届いた小分けが多すぎます".into());
        }
        ahead
            .entry(key.to_owned())
            .or_default()
            .insert(offset, bytes);
        return Ok(written);
    }
    let mut length = append_chunk(stage, part, offset, &bytes)?;
    if let Some(waiting) = ahead.get_mut(key) {
        while let Some(next) = waiting.remove(&length) {
            length = append_chunk(stage, part, length, &next)?;
        }
        waiting.retain(|&start, _| start > length);
        if waiting.is_empty() {
            ahead.remove(key);
        }
    }
    Ok(length)
}

/// 終わった・やめた転送の、メモリに持っていた小分けを捨てる。
pub(crate) fn forget_ahead(operation: &str) {
    if let Ok(mut ahead) = AHEAD.lock()
        && let Some(ahead) = ahead.as_mut()
    {
        ahead.retain(|key, _| !key.starts_with(operation));
    }
}

pub(crate) fn verify_stage(
    db: &Db,
    stage: &Path,
    manifest: &Manifest,
    marker: &Marker,
) -> Result<(), String> {
    let offsets = offsets(stage, manifest)?;
    for (part, offset) in manifest.entries.iter().zip(offsets).rev() {
        let path = part_path(stage, part);
        if offset != part.len {
            return Err(format!("転送が完了していません: {}", path.display()));
        }
        let time = UNIX_EPOCH + Duration::from_millis(part.modified_ms);
        super::super::apply::set_mtime(&path, time).map_err(error)?;
    }
    verify_content(db, stage, marker)
}
