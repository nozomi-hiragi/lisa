use super::tree::{FileTree, RemoteRef};
use crate::model::tree::Node;
use crate::model::tree::NodeId;
use bevy::prelude::*;
use lisa_core::display_path::{self, VisibleDevice, VisibleDrive, VisibleNames, text};

pub(crate) fn tree_text(tree: &FileTree, node: NodeId) -> String {
    text(&tree_parts(tree, node))
}

pub(crate) fn tree_parts(tree: &FileTree, node: NodeId) -> Vec<String> {
    let Some(path) = tree.path_from_root(node) else {
        return Vec::new();
    };
    let mut parts: Vec<_> = path[1..]
        .iter()
        .map(|&id| tree.nodes[id].name.clone())
        .collect();
    let Some(&device_id) = path.get(1) else {
        return parts;
    };
    let names = visible_names(tree);
    let device_index = tree.nodes[FileTree::ROOT]
        .children
        .iter()
        .position(|&id| id == device_id)
        .unwrap();
    let device = &names.0[device_index];
    parts[0] = names.device_label(device);
    if let Some(&drive_id) = path.get(2) {
        let drive_index = tree.nodes[device_id]
            .children
            .iter()
            .position(|&id| id == drive_id)
            .unwrap();
        parts[1] = device.drive_label(&device.drives[drive_index]);
    }
    parts
}

/// 表示名に関わる木の入力が変わった時だけ更新する。
#[derive(Resource, Default)]
pub(crate) struct DisplayNames {
    pub(crate) visible: VisibleNames,
    pub(crate) generation: u64,
}

pub(crate) fn refresh_display_names(tree: Res<FileTree>, mut names: ResMut<DisplayNames>) {
    if tree.is_changed() {
        let visible = visible_names(&tree);
        if names.visible != visible {
            names.visible = visible;
            names.generation += 1;
        }
    }
}

fn visible_names(tree: &FileTree) -> VisibleNames {
    VisibleNames(
        tree.nodes[FileTree::ROOT]
            .children
            .iter()
            .map(|&device| {
                let device = &tree.nodes[device];
                VisibleDevice {
                    remote_id: device
                        .remote
                        .as_ref()
                        .map(|remote| remote.device_id.clone()),
                    name: device.name.clone(),
                    drives: device
                        .children
                        .iter()
                        .map(|&drive| {
                            let drive = &tree.nodes[drive];
                            VisibleDrive {
                                id: drive
                                    .drive_id
                                    .clone()
                                    .unwrap_or_else(|| drive.incarnation.to_string()),
                                label: drive.name.clone(),
                                mount: drive_mount(drive).map(std::path::Path::to_path_buf),
                            }
                        })
                        .collect(),
                }
            })
            .collect(),
    )
}

fn drive_mount(node: &Node) -> Option<&std::path::Path> {
    node.remote_mount.as_deref().or(node.path.as_deref())
}

/// 登録前の項目は、同じ場所に見える別の項目と入れ替わっていないことを確認する。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    Remote(RemoteRef),
    Entry(i64),
    Attached(u64),
}

impl Step {
    fn new(node: &Node) -> Self {
        if let Some(remote) = &node.remote {
            Self::Remote(remote.clone())
        } else if let Some(entry) = node.entry {
            Self::Entry(entry)
        } else {
            Self::Attached(node.incarnation)
        }
    }

    fn matches(&self, node: &Node) -> bool {
        match self {
            Self::Remote(remote) => node.remote.as_ref() == Some(remote),
            Self::Entry(entry) => node.remote.is_none() && node.entry == Some(*entry),
            Self::Attached(incarnation) => node.incarnation == *incarnation,
        }
    }
}

pub(crate) fn steps(tree: &FileTree, node: NodeId) -> Option<Vec<Step>> {
    Some(
        tree.path_from_root(node)?[1..]
            .iter()
            .map(|&id| Step::new(&tree.nodes[id]))
            .collect(),
    )
}

pub(crate) fn resolve_steps(tree: &FileTree, steps: &[Step]) -> Vec<NodeId> {
    let mut path = vec![FileTree::ROOT];
    let mut node = FileTree::ROOT;
    for step in steps {
        let Some(child) = tree.nodes[node]
            .children
            .iter()
            .copied()
            .find(|&child| step.matches(&tree.nodes[child]))
        else {
            break;
        };
        node = child;
        path.push(node);
    }
    path
}

pub(crate) fn resolve_breadcrumb(tree: &FileTree, target: &[Step]) -> Option<Vec<NodeId>> {
    let path = resolve_steps(tree, target);
    (path.len() == target.len() + 1).then_some(path)
}

pub(crate) fn entry_texts(
    names: &DisplayNames,
    store: Option<&crate::scan::Store>,
    rows: impl Iterator<Item = (Option<i64>, impl AsRef<std::path::Path>)>,
) -> Vec<String> {
    let mut rows = Some(rows);
    let mut collect = |resolve: &dyn Fn(i64) -> Option<String>| {
        rows.take()
            .unwrap()
            .map(|(entry, fallback)| {
                entry
                    .and_then(resolve)
                    .unwrap_or_else(|| display_path::unknown(fallback.as_ref()))
            })
            .collect()
    };
    store
        .and_then(|store| store.with_display_entries(&names.visible, &mut collect))
        .unwrap_or_else(|| collect(&|_| None))
}

pub(crate) fn mounted_tree_text(
    tree: &FileTree,
    path: &std::path::Path,
    max_chars: usize,
) -> String {
    let mut best = None;
    let mut depth = 0;
    for &device in &tree.nodes[FileTree::ROOT].children {
        for &drive in &tree.nodes[device].children {
            let Some(mount) = tree.nodes[drive].path.as_deref() else {
                continue;
            };
            let Ok(relative) = path.strip_prefix(mount) else {
                continue;
            };
            let count = mount.components().count();
            if count > depth {
                depth = count;
                best = Some((drive, relative));
            } else if count == depth {
                best = None;
            }
        }
    }
    best.map_or_else(
        || display_path::unknown(path),
        |(drive, relative)| {
            let mut parts = tree_parts(tree, drive);
            parts.extend(display_path::path_names(relative));
            display_path::text_short(&parts, max_chars)
        },
    )
}

pub(crate) fn mounted_text(
    paths: Option<&display_path::DisplayPaths>,
    drive_id: Option<&str>,
    path: &std::path::Path,
) -> String {
    paths
        .and_then(|paths| match drive_id {
            Some(id) => paths.path_on_drive(id, path),
            None => paths.mounted_path(path),
        })
        .map_or_else(|| display_path::unknown(path), |parts| text(&parts))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::tree::{LoadState, RemoteDrive};

    fn local() -> FileTree {
        use lisa_core::volumes::Volume;
        FileTree::this_device(
            "PC",
            &[
                Volume {
                    id: Some("one".into()),
                    label: "SSD".into(),
                    fs: String::new(),
                    mount_path: "/test/one".into(),
                    capacity: None,
                    available: None,
                    removable: false,
                    network: false,
                    read_only: false,
                    case_sensitive: None,
                    hidden: false,
                },
                Volume {
                    id: Some("two".into()),
                    label: "SSD".into(),
                    fs: String::new(),
                    mount_path: "/test/two".into(),
                    capacity: None,
                    available: None,
                    removable: false,
                    network: false,
                    read_only: false,
                    case_sensitive: None,
                    hidden: false,
                },
            ],
        )
    }

    fn remote_drive(mount: &str, entry_id: i64) -> RemoteDrive {
        RemoteDrive {
            id: None,
            label: "SSD".into(),
            mount: Some(mount.into()),
            entry_id,
            listed: true,
            has_hash: true,
            has_error: false,
        }
    }

    fn remote_folder(tree: &mut FileTree, drive: NodeId, id: i64) -> NodeId {
        tree.add_remote_children(
            drive,
            vec![lisa_core::query::Item {
                id,
                name: "Root".into(),
                kind: "dir".into(),
                size: 0,
                mtime: None,
                hash: None,
                cloud_only: false,
                other_volume: false,
                listed: true,
                error: None,
            }],
        );
        tree.nodes[drive].children[0]
    }

    #[test]
    fn tree_drops_only_virtual_root_and_does_not_repeat_drive() {
        let mut tree = local();
        let peer = tree.ensure_remote_device("peer", "PC", LoadState::Unloaded);
        tree.add_remote_drives(peer, vec![remote_drive("/test/remote", 1)]);
        let drive = tree.nodes[peer].children[0];
        let folder = remote_folder(&mut tree, drive, 2);
        assert_eq!(tree_text(&tree, FileTree::ROOT), "端末一覧");
        assert_eq!(tree_text(&tree, drive), "PC (LAN) › SSD");
        assert_eq!(tree_text(&tree, folder), "PC (LAN) › SSD › Root");
        assert_eq!(tree_text(&tree, 2), "PC (このPC) › SSD (/test/one)");
        assert_eq!(tree_text(&tree, 3), "PC (このPC) › SSD (/test/two)");
    }

    #[test]
    fn local_ancestors_use_entry_ids_and_reject_detached_drive() {
        let mut tree = local();
        tree.set_entry(2, 100);
        tree.set_entry(3, 200);
        let target = steps(&tree, 3).unwrap();
        assert_eq!(target[1], Step::Entry(200));
        assert_eq!(resolve_breadcrumb(&tree, &target), Some(vec![0, 1, 3]));
        assert_eq!(resolve_breadcrumb(&tree, &target[..1]), Some(vec![0, 1]));
        assert_eq!(resolve_breadcrumb(&tree, &[]), Some(vec![0]));
        tree.reset_children(1);
        assert_eq!(resolve_breadcrumb(&tree, &target), None);
    }

    #[test]
    fn lan_ancestors_use_peer_and_entry_after_reload_and_reject_wrong_drive() {
        let mut tree = local();
        tree.set_entry(2, 100);
        let first = tree.ensure_remote_device("peer-1", "PC", LoadState::Unloaded);
        let second = tree.ensure_remote_device("peer-2", "PC", LoadState::Unloaded);
        tree.add_remote_drives(
            first,
            vec![
                remote_drive("/test/first", 100),
                remote_drive("/test/next", 200),
            ],
        );
        tree.add_remote_drives(second, vec![remote_drive("/test/second", 100)]);
        let (first_drive, other_drive) =
            (tree.nodes[first].children[0], tree.nodes[first].children[1]);
        let remote = remote_folder(&mut tree, first_drive, 300);
        remote_folder(&mut tree, other_drive, 301);
        let second_drive = tree.nodes[second].children[0];
        remote_folder(&mut tree, second_drive, 300);
        let target = steps(&tree, remote).unwrap();
        assert_eq!(
            resolve_breadcrumb(&tree, &target),
            Some(vec![0, first, first_drive, remote])
        );
        assert_eq!(
            resolve_breadcrumb(&tree, &target[..2]),
            Some(vec![0, first, first_drive])
        );
        assert_eq!(
            resolve_breadcrumb(&tree, &target[..1]),
            Some(vec![0, first])
        );
        tree.reset_children(first);
        assert_eq!(resolve_breadcrumb(&tree, &target), None);
        tree.add_remote_drives(
            first,
            vec![
                remote_drive("/test/next", 200),
                remote_drive("/test/first", 100),
            ],
        );
        let reloaded = tree.nodes[first]
            .children
            .iter()
            .copied()
            .find(|&drive| tree.nodes[drive].remote.as_ref().unwrap().entry_id == Some(100))
            .unwrap();
        let new_folder = remote_folder(&mut tree, reloaded, 300);
        assert_eq!(
            resolve_breadcrumb(&tree, &target),
            Some(vec![0, first, reloaded, new_folder])
        );
    }

    #[test]
    fn unregistered_ancestor_does_not_match_a_reused_slot() {
        let tree = local();
        let target = steps(&tree, 2).unwrap();
        assert_eq!(resolve_breadcrumb(&tree, &target), Some(vec![0, 1, 2]));
        assert_eq!(resolve_breadcrumb(&local(), &target), None);
    }
}
