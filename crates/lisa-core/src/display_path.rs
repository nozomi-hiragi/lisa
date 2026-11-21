//! 端末を起点とした画面表示用の場所。

use std::path::{Path, PathBuf};

use rusqlite::OptionalExtension;

use crate::db::{Db, Result};

pub const DEVICES: &str = "端末一覧";
pub const UNKNOWN_DEVICE: &str = "端末不明";

pub fn text(parts: &[String]) -> String {
    if parts.is_empty() {
        DEVICES.into()
    } else {
        parts.join(" › ")
    }
}

/// 長い場所を 1 行に収める。先頭の端末・ドライブと末尾の段を残し、中央を「…」で省く。
/// 末尾の 1 段だけで入らないときは、その段の前を切る。
pub fn text_short(parts: &[String], max_chars: usize) -> String {
    let full = text(parts);
    if full.chars().count() <= max_chars || parts.len() < 2 {
        return full;
    }
    let head_len = parts.len().min(2);
    let head = parts[..head_len].join(" › ");
    let used = |tail: &[String]| {
        head.chars().count() + " › … › ".chars().count() + tail.join(" › ").chars().count()
    };
    let mut start = parts.len();
    while start > head_len && used(&parts[start - 1..]) <= max_chars {
        start -= 1;
    }
    if start == parts.len() {
        let last = &parts[parts.len() - 1];
        let room = max_chars
            .saturating_sub(head.chars().count() + " › … › ".chars().count())
            .max(2);
        let count = last.chars().count();
        let cut: String = last.chars().skip(count.saturating_sub(room - 1)).collect();
        return format!("{head} › … › …{cut}");
    }
    if start == head_len {
        return full;
    }
    format!("{head} › … › {}", parts[start..].join(" › "))
}

/// 折り返しを「 › 」の区切りだけで行うため、段の中の空白を改行できない空白に替える。
pub fn wrap_at_separators(text: &str) -> String {
    text.replace(" › ", "\u{1}")
        .replace(' ', "\u{a0}")
        .replace('\u{1}', " › ")
}

pub fn device_label<'a>(
    name: &str,
    id: Option<&str>,
    collisions: usize,
    remote_ids: impl Iterator<Item = &'a str>,
) -> String {
    if collisions < 2 {
        return name.into();
    }
    let Some(id) = id else {
        return format!("{name} (このPC)");
    };
    let conflicting = remote_ids
        .filter(|other| *other != id)
        .map(|other| {
            id.chars()
                .zip(other.chars())
                .take_while(|(a, b)| a == b)
                .count()
        })
        .max();
    match conflicting {
        Some(shared) => {
            let suffix: String = id.chars().take(8.max(shared + 1)).collect();
            format!("{name} (LAN {suffix})")
        }
        None => format!("{name} (LAN)"),
    }
}

pub fn drive_label(label: &str, mount: Option<&Path>, id: &str) -> String {
    if !label.is_empty() {
        label.into()
    } else if let Some(mount) = mount {
        mount.display().to_string()
    } else {
        id.into()
    }
}

pub fn distinguish_drive(label: &str, mount: Option<&Path>, id: &str, same_mount: bool) -> String {
    match mount {
        Some(mount) if !same_mount => format!("{label} ({})", mount.display()),
        _ => format!("{label} ({id})"),
    }
}

/// 現在木に出ている端末とドライブ。過去の DB 行は衝突判定を変えない。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VisibleNames(pub Vec<VisibleDevice>);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisibleDevice {
    pub remote_id: Option<String>,
    pub name: String,
    pub drives: Vec<VisibleDrive>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisibleDrive {
    pub id: String,
    pub label: String,
    pub mount: Option<PathBuf>,
}

impl VisibleNames {
    pub fn device_label(&self, device: &VisibleDevice) -> String {
        let peers = || {
            self.0
                .iter()
                .filter(|candidate| candidate.name == device.name)
        };
        device_label(
            &device.name,
            device.remote_id.as_deref(),
            peers().count(),
            peers().filter_map(|peer| peer.remote_id.as_deref()),
        )
    }
}

impl VisibleDevice {
    pub fn drive_label(&self, drive: &VisibleDrive) -> String {
        let siblings = || {
            self.drives
                .iter()
                .filter(|candidate| candidate.label == drive.label)
        };
        if siblings().count() > 1 {
            distinguish_drive(
                &drive.label,
                drive.mount.as_deref(),
                &drive.id,
                siblings()
                    .filter(|candidate| candidate.mount == drive.mount)
                    .count()
                    > 1,
            )
        } else {
            drive.label.clone()
        }
    }
}

struct DriveName {
    id: String,
    owner: Option<String>,
    label: String,
    mount: Option<PathBuf>,
}

pub struct DisplayPaths {
    drives: Vec<DriveName>,
}

impl DisplayPaths {
    pub fn read(db: &Db, visible: &VisibleNames) -> Result<Self> {
        let conn = db.conn();
        let devices: Vec<(String, String, bool)> = conn.prepare(
            "SELECT d.id, d.name, EXISTS(SELECT 1 FROM meta WHERE key = 'this_device' AND value = d.id)
             FROM device d",
        )?.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<_>>()?;
        let mut drives: Vec<DriveName> = conn
            .prepare("SELECT id, last_device_id, label, last_mount_path FROM drive")?
            .query_map([], |r| {
                let id: String = r.get(0)?;
                let mount = r.get::<_, Option<String>>(3)?.map(PathBuf::from);
                let label = drive_label(&r.get::<_, String>(2)?, mount.as_deref(), &id);
                Ok(DriveName {
                    id,
                    owner: r.get(1)?,
                    label,
                    mount,
                })
            })?
            .collect::<Result<_>>()?;
        for drive in &mut drives {
            let Some(owner_id) = drive.owner.take() else {
                continue;
            };
            let stored = devices.iter().find(|(id, ..)| *id == owner_id);
            let attached = visible.0.iter().find(|device| {
                device.remote_id.as_deref() == Some(owner_id.as_str())
                    || (device.remote_id.is_none() && stored.is_some_and(|(_, _, local)| *local))
            });
            if let Some(device) = attached {
                drive.owner = Some(visible.device_label(device));
                if let Some(live_drive) = device
                    .drives
                    .iter()
                    .find(|candidate| candidate.id == drive.id)
                {
                    drive.label = device.drive_label(live_drive);
                } else if device
                    .drives
                    .iter()
                    .any(|candidate| candidate.label == drive.label)
                {
                    drive.label = distinguish_drive(
                        &drive.label,
                        drive.mount.as_deref(),
                        &drive.id,
                        device.drives.iter().any(|candidate| {
                            candidate.label == drive.label && candidate.mount == drive.mount
                        }),
                    );
                }
            } else if let Some((_, name, _)) = stored {
                // 接続中の端末と名前がぶつかるときだけ、木と同じ作り方の接尾辞で区別する。
                let collisions = visible
                    .0
                    .iter()
                    .filter(|device| device.name == *name)
                    .count();
                drive.owner = Some(device_label(
                    name,
                    Some(&owner_id),
                    if collisions > 0 { 2 } else { 1 },
                    visible
                        .0
                        .iter()
                        .filter(|device| device.name == *name)
                        .filter_map(|device| device.remote_id.as_deref()),
                ));
            }
        }
        Ok(Self { drives })
    }

    fn prefix(drive: &DriveName) -> Vec<String> {
        vec![
            drive.owner.as_deref().unwrap_or(UNKNOWN_DEVICE).into(),
            drive.label.clone(),
        ]
    }

    pub fn entry(&self, db: &Db, entry: i64) -> Result<Option<Vec<String>>> {
        let mut names = Vec::new();
        let mut next = entry;
        let mut statement = db
            .conn()
            .prepare_cached("SELECT parent_id, name_nfc, drive_id FROM entry WHERE id = ?1")?;
        loop {
            let row: Option<(Option<i64>, String, String)> = statement
                .query_row([next], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .optional()?;
            let Some((parent, name, drive_id)) = row else {
                return Ok(None);
            };
            if let Some(parent) = parent {
                names.push(name);
                next = parent;
            } else {
                names.reverse();
                return Ok(self
                    .drives
                    .iter()
                    .find(|drive| drive.id == drive_id)
                    .map(|drive| {
                        let mut parts = Self::prefix(drive);
                        parts.extend(names);
                        parts
                    }));
            }
        }
    }

    pub fn relative(&self, drive_id: &str, relative: &Path) -> Option<Vec<String>> {
        let drive = self.drives.iter().find(|drive| drive.id == drive_id)?;
        let mut parts = Self::prefix(drive);
        parts.extend(path_names(relative));
        Some(parts)
    }

    pub fn path_on_drive(&self, drive_id: &str, path: &Path) -> Option<Vec<String>> {
        let drive = self.drives.iter().find(|drive| drive.id == drive_id)?;
        self.relative(drive_id, path.strip_prefix(drive.mount.as_deref()?).ok()?)
    }

    pub fn mounted_path(&self, path: &Path) -> Option<Vec<String>> {
        let mut candidates: Vec<_> = self
            .drives
            .iter()
            .filter_map(|drive| {
                let mount = drive.mount.as_deref()?;
                let relative = path.strip_prefix(mount).ok()?;
                Some((mount.components().count(), drive, relative))
            })
            .collect();
        candidates.sort_unstable_by_key(|(depth, _, _)| std::cmp::Reverse(*depth));
        let &(depth, drive, relative) = candidates.first()?;
        if candidates
            .get(1)
            .is_some_and(|(other, _, _)| *other == depth)
        {
            return None;
        }
        self.relative(&drive.id, relative)
    }
}

pub fn path_names(path: &Path) -> impl Iterator<Item = String> + '_ {
    path.components().filter_map(|part| match part {
        std::path::Component::Normal(name) => Some(crate::name::nfc(name)),
        _ => None,
    })
}

pub fn unknown(path: &Path) -> String {
    let mut parts = vec![UNKNOWN_DEVICE.into()];
    parts.extend(path_names(path));
    text(&parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Db {
        let db = Db::open_in_memory().unwrap();
        db.conn()
            .execute_batch(
                "INSERT INTO meta VALUES ('this_device', 'local');
             INSERT INTO device VALUES
                ('local', 'PC', 'local', 'test', 0),
                ('remote-a', 'PC', 'a', 'test', 0),
                ('remote-b', 'PC', 'b', 'test', 0),
                ('ghost-device-id', 'PC', 'g', 'test', 0),
                ('peer', 'Other', 'peer', 'test', 0),
                ('retired', 'Retired', 'r', 'test', 0);
             INSERT INTO drive (id, label, fs, last_device_id, last_mount_path, last_seen_at) VALUES
                ('one', 'SSD', '', 'local', '/one', 0),
                ('two', 'SSD', '', 'local', '/two', 0),
                ('three', '', '', 'local', '/three', 0),
                ('a', 'SSD', '', 'remote-a', '/one', 0),
                ('b', 'SSD', '', 'remote-b', '/one', 0),
                ('peer', 'SSD', '', 'peer', '/peer', 0),
                ('ghost', 'SSD', '', 'ghost-device-id', '/g', 0),
                ('retired', 'SSD', '', 'retired', '/r', 0),
                ('old', 'SSD', '', 'gone', '/old', 0);
             INSERT INTO entry (id, drive_id, parent_id, pos, name, name_nfc, kind, size,
                cloud_only, other_volume, status, listed) VALUES
                (1, 'one', NULL, 0, x'', 'SSD', 'dir', 0, 0, 0, '', 1),
                (2, 'one', 1, 0, x'526F6F74', 'Root', 'dir', 0, 0, 0, '', 1),
                (3, 'one', 2, 0, x'78', 'x', 'file', 1, 0, 0, '', 0);",
            )
            .unwrap();
        db
    }

    fn drive(id: &str, label: &str, mount: &str) -> VisibleDrive {
        VisibleDrive {
            id: id.into(),
            label: label.into(),
            mount: Some(mount.into()),
        }
    }

    fn visible() -> VisibleNames {
        VisibleNames(vec![
            VisibleDevice {
                remote_id: None,
                name: "PC".into(),
                drives: vec![
                    drive("one", "SSD", "/one"),
                    drive("two", "SSD", "/two"),
                    drive("three", "/three", "/three"),
                ],
            },
            VisibleDevice {
                remote_id: Some("remote-a".into()),
                name: "PC".into(),
                drives: vec![drive("a", "SSD", "/one")],
            },
            VisibleDevice {
                remote_id: Some("remote-b".into()),
                name: "PC".into(),
                drives: vec![drive("b", "SSD", "/one")],
            },
            VisibleDevice {
                remote_id: Some("peer".into()),
                name: "Other".into(),
                drives: vec![drive("peer", "SSD", "/peer")],
            },
        ])
    }

    #[test]
    fn names_follow_device_drive_and_entry_ancestry() {
        let db = fixture();
        let paths = DisplayPaths::read(&db, &visible()).unwrap();
        assert_eq!(
            text(&paths.entry(&db, 1).unwrap().unwrap()),
            "PC (このPC) › SSD (/one)"
        );
        assert_eq!(
            text(&paths.entry(&db, 3).unwrap().unwrap()),
            "PC (このPC) › SSD (/one) › Root › x"
        );
        let show = |drive, path| text(&paths.relative(drive, Path::new(path)).unwrap());
        assert_eq!(show("two", "new/x"), "PC (このPC) › SSD (/two) › new › x");
        assert_eq!(show("three", ""), "PC (このPC) › /three");
        assert_eq!(show("a", ""), "PC (LAN remote-a) › SSD");
        assert_eq!(show("b", ""), "PC (LAN remote-b) › SSD");
        assert_eq!(show("peer", ""), "Other › SSD");
        assert_eq!(show("old", ""), "端末不明 › SSD");
        // 接続中でない古い記録は、接続中の名前とぶつかるときだけ区別する。
        assert_eq!(show("ghost", ""), "PC (LAN ghost-de) › SSD");
        assert_eq!(show("retired", ""), "Retired › SSD");
    }

    #[test]
    fn db_names_match_tree_names_for_every_visible_drive() {
        let db = fixture();
        let names = visible();
        let paths = DisplayPaths::read(&db, &names).unwrap();
        for device in &names.0 {
            for drive in &device.drives {
                let tree_side = vec![names.device_label(device), device.drive_label(drive)];
                let db_side = paths.relative(&drive.id, Path::new("")).unwrap();
                assert_eq!(tree_side, db_side, "{}", drive.id);
            }
        }
    }

    #[test]
    fn long_paths_keep_device_drive_and_tail() {
        let parts: Vec<String> = [
            "PC (このPC)",
            "Macintosh HD",
            "Library",
            "Developer",
            "Headers",
            "net",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(text_short(&parts, 100), text(&parts));
        assert_eq!(
            text_short(&parts, 50),
            "PC (このPC) › Macintosh HD › … › Headers › net"
        );
        let long = vec!["PC".to_string(), "SSD".to_string(), "x".repeat(40)];
        let cut = text_short(&long, 24);
        assert!(
            cut.starts_with("PC › SSD › … › …") && cut.ends_with('x'),
            "{cut}"
        );
    }

    #[test]
    fn wrapping_only_breaks_between_parts() {
        assert_eq!(wrap_at_separators("A B › c d"), "A\u{a0}B › c\u{a0}d");
    }

    #[test]
    fn device_suffix_appears_only_on_collision_and_extends_shared_prefixes() {
        let single = ["remote-1"];
        assert_eq!(
            device_label("PC", Some(single[0]), 1, single.into_iter()),
            "PC"
        );
        assert_eq!(
            device_label("PC", Some(single[0]), 2, single.into_iter()),
            "PC (LAN)"
        );
        let same_prefix = ["remote-id-a", "remote-id-b"];
        assert_eq!(
            device_label("PC", Some(same_prefix[0]), 2, same_prefix.into_iter()),
            "PC (LAN remote-id-a)",
        );
    }

    #[test]
    fn cached_copy_names_are_resolved_after_deserializing() {
        use crate::dups::Cached;
        let db = fixture();
        let cache: Cached = serde_json::from_value(serde_json::json!({
            "signature": [],
            "groups": [{
                "kind": "File", "name": "x", "size": 1, "hash": vec![0u8; 32],
                "copies": [{"drive_id": "one", "path": "/former/path/x",
                            "chain": [1, 2, 3], "hard_links": []}]
            }]
        }))
        .unwrap();
        let copy = &cache.groups[0].copies[0];
        let paths = DisplayPaths::read(&db, &visible()).unwrap();
        assert_eq!(
            text(
                &paths
                    .entry(&db, *copy.chain.last().unwrap())
                    .unwrap()
                    .unwrap()
            ),
            "PC (このPC) › SSD (/one) › Root › x"
        );
        assert_eq!(copy.path, Path::new("/former/path/x"));
    }
}
