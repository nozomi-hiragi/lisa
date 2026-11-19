//! LAN の相手のカタログと当たりを見る（`docs/catalog.md` の「複数の PC」）。
//!
//! - 相手の木への当てはめは、相手の Lisa がやる（中身の条件はファイルのある PC でしか読めないため）。
//!   こちらは、ペアリングした相手の `lisa_catalog_matches`（中の役割つき）と `lisa_catalogs`（本文つき）を
//!   1 分ごとに読み、相手の木のノードに札と暗い膜を描く。
//! - 相手にしか無い型は、パネルに「取り込む」を出す。押したときだけ、ユーザーのカタログのフォルダに
//!   `<id>.toml` として書く（同じ名前があれば書かない）。ユーザーのファイルには触らない。

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use bevy::prelude::*;
use lisa_core::catalog::{self, CatalogSet, Role};
use serde_json::{Value, json};

use crate::model::tree::FileTree;

/// 相手を読み直す間隔。
const FETCH_EVERY: Duration = Duration::from_secs(60);
/// 1 回に読む当たりの上限（`lisa_catalog_matches` の上限）。
const MATCH_LIMIT: usize = 1000;

/// 相手の当たり 1 か所。
#[derive(Clone, Debug)]
pub struct RemoteHit {
    /// 相手の DB の `entry.id`。
    pub entry_id: i64,
    pub app: String,
    pub label: String,
    pub color: Color,
}

/// 相手にしか無い型。
#[derive(Clone, Debug)]
pub struct RemoteCatalog {
    pub name: String,
    pub app: String,
    /// TOML の本文（取り込むときにそのまま書く）。
    pub text: String,
}

/// 相手 1 台分。
#[derive(Clone, Debug, Default)]
pub struct RemoteSnapshot {
    pub name: String,
    pub hits: Vec<RemoteHit>,
    pub roles: HashMap<i64, Role>,
    pub only_there: Vec<RemoteCatalog>,
    pub error: Option<String>,
}

/// 相手ごとの当たり（端末の ID で引く）。
#[derive(Resource, Default)]
pub struct RemoteCatalogs {
    pub by_device: HashMap<String, RemoteSnapshot>,
    receiver: Option<Mutex<Receiver<(String, RemoteSnapshot)>>>,
    sender: Option<Mutex<Sender<(String, RemoteSnapshot)>>>,
    last: HashMap<String, Instant>,
}

/// 木に出ている相手の端末のうち、ペアリング済みのものを 1 分ごとに読み直す。
pub(super) fn fetch_remote(tree: Res<FileTree>, mut remote: ResMut<RemoteCatalogs>) {
    if remote.sender.is_none() {
        let (tx, rx) = channel();
        remote.sender = Some(Mutex::new(tx));
        remote.receiver = Some(Mutex::new(rx));
    }
    // 届いた結果を入れる。
    let arrived: Vec<(String, RemoteSnapshot)> = remote
        .receiver
        .as_ref()
        .and_then(|r| r.lock().ok().map(|r| r.try_iter().collect()))
        .unwrap_or_default();
    for (device, snapshot) in arrived {
        remote.by_device.insert(device, snapshot);
    }

    let devices: HashSet<&str> = tree
        .nodes
        .iter()
        .filter_map(|n| n.remote.as_ref())
        .filter(|r| r.entry_id.is_none())
        .map(|r| r.device_id.as_str())
        .collect();
    if devices.is_empty() {
        return;
    }
    let Some(data_dir) = crate::scan::data_dir() else {
        return;
    };
    let now = Instant::now();
    for peer in crate::lan::load_saved_peers(&data_dir) {
        if !devices.contains(peer.device_id.as_str())
            || remote
                .last
                .get(&peer.device_id)
                .is_some_and(|t| now.duration_since(*t) < FETCH_EVERY)
        {
            continue;
        }
        remote.last.insert(peer.device_id.clone(), now);
        let Some(sender) = remote
            .sender
            .as_ref()
            .and_then(|s| s.lock().ok().map(|s| s.clone()))
        else {
            return;
        };
        std::thread::spawn(move || {
            let snapshot = fetch(&peer).unwrap_or_else(|error| RemoteSnapshot {
                name: peer.name.clone(),
                error: Some(error),
                ..default()
            });
            let _ = sender.send((peer.device_id.clone(), snapshot));
        });
    }
}

fn color_of(hex: Option<&str>, app: &str) -> Color {
    let rgb = hex
        .filter(|h| h.len() == 7 && h.starts_with('#'))
        .and_then(|h| {
            Some([
                u8::from_str_radix(&h[1..3], 16).ok()?,
                u8::from_str_radix(&h[3..5], 16).ok()?,
                u8::from_str_radix(&h[5..7], 16).ok()?,
            ])
        })
        .unwrap_or_else(|| catalog::app_color(app));
    Color::srgb_u8(rgb[0], rgb[1], rgb[2])
}

/// 相手の当たりと型を読む。
fn fetch(peer: &crate::lan::SavedPeer) -> Result<RemoteSnapshot, String> {
    let call = |tool: &str, args: Value| {
        crate::lan::call_mcp_tool(&peer.addr, peer.port, &peer.key, tool, args)
            .map_err(|(_, e)| format!("{tool}: {e}"))
    };
    let catalogs = call("lisa_catalogs", json!({"include_text": true}))?;
    let matches = call(
        "lisa_catalog_matches",
        json!({"limit": MATCH_LIMIT, "parts": true, "totals": false}),
    )?;

    let list = catalogs
        .get("catalogs")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let str_of = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let colors: HashMap<String, Option<String>> = list
        .iter()
        .map(|c| {
            (
                str_of(c, "id"),
                c.get("color").and_then(Value::as_str).map(str::to_string),
            )
        })
        .collect();

    // こちらに無い型（組み込みと、こちらのカタログのフォルダにある id 以外）。
    let local = CatalogSet::load_with_builtin(&super::catalog_dirs());
    let only_there = list
        .iter()
        .filter(|c| local.get(&str_of(c, "id")).is_none())
        .filter(|c| c.get("text").and_then(Value::as_str).is_some())
        .map(|c| RemoteCatalog {
            name: str_of(c, "name"),
            app: str_of(c, "app"),
            text: str_of(c, "text"),
        })
        .collect();

    let mut snapshot = RemoteSnapshot {
        name: peer.name.clone(),
        only_there,
        ..default()
    };
    for row in matches
        .get("matches")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(entry_id) = row.get("entry_id").and_then(Value::as_i64) else {
            continue;
        };
        let app = str_of(row, "app");
        let catalog_id = row.get("catalog").and_then(Value::as_str);
        let color = color_of(
            catalog_id
                .and_then(|id| colors.get(id))
                .and_then(|c| c.as_deref()),
            &app,
        );
        for part in row
            .get("parts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let (Some(entry), Some(role)) = (
                part.get("entry_id").and_then(Value::as_i64),
                part.get("role")
                    .and_then(Value::as_str)
                    .and_then(Role::from_name),
            ) {
                snapshot.roles.entry(entry).or_insert(role);
            }
        }
        snapshot.hits.push(RemoteHit {
            entry_id,
            label: str_of(row, "label"),
            app,
            color,
        });
    }
    Ok(snapshot)
}

/// 相手にしか無い型を、こちらのユーザーのカタログのフォルダに書く（同じ名前があれば書かない）。
pub fn import(text: &str) -> Result<std::path::PathBuf, String> {
    let cat = catalog::parse(text)?;
    if cat.id.contains("..") || cat.id.contains('/') || cat.id.contains('\\') {
        return Err(format!("id がファイル名として使えません: {}", cat.id));
    }
    let data_dir = crate::scan::data_dir().ok_or("データのフォルダが分かりません")?;
    let dir = catalog::user_dir(&data_dir);
    write_new(&dir, &cat.id, text)
}

fn write_new(dir: &Path, id: &str, text: &str) -> Result<std::path::PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join(format!("{id}.toml"));
    if std::fs::symlink_metadata(&path).is_ok() {
        return Err(format!("すでにあります: {}", path.display()));
    }
    let tmp = dir.join(format!(".{id}.toml.tmp"));
    std::fs::write(&tmp, text).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_new_does_not_overwrite() {
        let dir = std::env::temp_dir().join(format!("lisa-import-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = write_new(&dir, "x", "a").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a");
        assert!(write_new(&dir, "x", "b").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn colors_fall_back_to_app() {
        assert_eq!(color_of(Some("#ff0000"), "x"), Color::srgb_u8(255, 0, 0));
        let rgb = catalog::app_color("Unity");
        assert_eq!(
            color_of(None, "Unity"),
            Color::srgb_u8(rgb[0], rgb[1], rgb[2])
        );
    }
}
