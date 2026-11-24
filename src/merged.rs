//! マージ済みリンクを明示的に開いたときの照合と、フォルダのその場展開。

use std::path::PathBuf;
use std::sync::mpsc;

use bevy::prelude::*;
use bevy::window::RequestRedraw;
use bevy::winit::{EventLoopProxyWrapper, WinitUserEvent};
use lisa_core::{db::Db, merged::Marker, query::Item};

use crate::lan::{LanState, SavedPeer};
use crate::model::tree::{FileTree, LoadState, NodeId};
use crate::scan::TreeChanged;
use crate::view::{Nav, ViewSystems};

pub(crate) struct MarkerViewPlugin;

impl Plugin for MarkerViewPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, folder_links.before(ViewSystems));
    }
}

pub(crate) enum Target {
    Local { db: Db, chain: Vec<i64> },
    Remote { peer: SavedPeer, chain: Vec<i64> },
}

enum ResolveError {
    Offline(String),
    Unpaired(String),
    Invalid(String),
}

impl ResolveError {
    fn message(self) -> String {
        match self {
            Self::Offline(message) | Self::Unpaired(message) | Self::Invalid(message) => message,
        }
    }
}

pub(crate) fn verified_target(marker: &Marker, peer: Option<SavedPeer>) -> Result<Target, String> {
    resolve(marker, peer).map_err(ResolveError::message)
}

fn resolve(marker: &Marker, peer: Option<SavedPeer>) -> Result<Target, ResolveError> {
    let database_error = |error: lisa_core::db::Error| ResolveError::Invalid(error.to_string());
    if let Some(path) = crate::scan::db_path().filter(|path| path.exists()) {
        let db = Db::open(&path).map_err(database_error)?;
        if db.meta("this_device").map_err(database_error)?.as_deref()
            == Some(&marker.target.device_id)
        {
            let chain =
                lisa_core::merged::resolve_target(&db, marker).map_err(ResolveError::Invalid)?;
            return Ok(Target::Local { db, chain });
        }
    }
    let name = &marker.target.device_name;
    let peer =
        peer.ok_or_else(|| ResolveError::Unpaired(format!("{name} とのペアリングがありません")))?;
    let result = crate::lan::call_mcp_tool(
        &peer.addr,
        peer.port,
        &peer.key,
        "lisa_merged_resolve",
        serde_json::json!({ "marker": marker }),
    )
    .map_err(|(unauthorized, error)| {
        if unauthorized {
            ResolveError::Unpaired(format!("{name} とのペアリングを確認できません"))
        } else if error.starts_with("知らない道具") {
            ResolveError::Invalid(format!("{name} はマージ済みリンクの確認に未対応です"))
        } else {
            ResolveError::Offline(format!("{name} はオフライン"))
        }
    })?;
    if let Some(reason) = result["error"].as_str() {
        return Err(ResolveError::Invalid(reason.to_owned()));
    }
    let chain = serde_json::from_value(result["chain"].clone())
        .map_err(|_| ResolveError::Invalid("マージ先の所在を確認できません".into()))?;
    Ok(Target::Remote { peer, chain })
}

#[derive(Clone)]
enum Children {
    Local {
        path: PathBuf,
        rows: Vec<lisa_core::db::Stored>,
    },
    Remote {
        device_id: String,
        rows: Vec<Item>,
    },
}

impl Children {
    fn len(&self) -> usize {
        match self {
            Self::Local { rows, .. } => rows.len(),
            Self::Remote { rows, .. } => rows.len(),
        }
    }

    fn attach(self, tree: &mut FileTree, root: NodeId) {
        clear_view(tree, root);
        tree.nodes[root].loaded = false;
        match self {
            Self::Local { path, rows } => {
                tree.nodes[root].path = Some(path);
                tree.add_stored_children(root, rows, |_| false, |_| false);
            }
            Self::Remote { device_id, rows } => {
                tree.add_remote_children_from(root, &device_id, rows)
            }
        }
    }
}

struct CachedChildren {
    checked_at: i64,
    children: Children,
}

struct Response {
    root: NodeId,
    incarnation: u64,
    result: Result<Children, ResolveError>,
}

#[derive(Default)]
struct MarkerView {
    open: Option<u64>,
    receive: Option<std::sync::Mutex<mpsc::Receiver<Response>>>,
    observed: std::collections::HashMap<u64, i64>,
    cache: std::collections::HashMap<String, CachedChildren>,
}

fn folder_links(
    mut state: Local<MarkerView>,
    nav: Res<Nav>,
    mut tree: ResMut<FileTree>,
    lan: Option<Res<LanState>>,
    proxy: Option<Res<EventLoopProxyWrapper>>,
    mut changed: ResMut<TreeChanged>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    let response = state
        .receive
        .as_mut()
        .and_then(|rx| match rx.get_mut().ok()?.try_recv() {
            Ok(response) => Some(Some(response)),
            Err(mpsc::TryRecvError::Disconnected) => Some(None),
            Err(mpsc::TryRecvError::Empty) => None,
        });
    let response = response.and_then(|response| {
        state.receive = None;
        response
    });
    if lan.as_ref().is_some_and(|lan| lan.is_changed()) {
        state.cache.retain(|_, cached| match &cached.children {
            Children::Local { .. } => true,
            Children::Remote { device_id, .. } => lan
                .as_deref()
                .and_then(|lan| lan.saved_peer(device_id))
                .is_some(),
        });
    }
    if let Some(lan) = lan.as_deref()
        && let Some((root, name)) = nav.stack.iter().find_map(|level| {
            let node = tree.nodes.get(level.node)?;
            let target = &node.merged.as_ref()?.target;
            (node.is_dir
                && !lan.is_this_device(&target.device_id)
                && lan.saved_peer(&target.device_id).is_none())
            .then(|| (level.node, target.device_name.clone()))
        })
    {
        let message = format!("{name} とのペアリングがありません");
        if tree.nodes[root].marker_status.as_deref() != Some(&message) {
            clear_view(&mut tree, root);
            tree.nodes[root].marker_status = Some(message);
            changed.pending = true;
            redraw.write(RequestRedraw);
        }
        state.open = None;
        let node = &tree.nodes[root];
        state.observed.remove(&node.incarnation);
        if let Some(marker) = &node.merged {
            state.cache.remove(&marker.link_id);
        }
        return;
    }
    let root = nav.stack.iter().rev().map(|level| level.node).find(|&id| {
        tree.nodes
            .get(id)
            .is_some_and(|node| node.is_dir && node.merged.is_some())
    });
    let Some(root) = root else {
        state.open = None;
        return;
    };
    if tree.marker_view_read_only(root) {
        state.open = None;
        return;
    }
    let node = &tree.nodes[root];
    let incarnation = node.incarnation;
    let marker = node.merged.as_ref().expect("リンクのフォルダ").clone();
    let peer = lan
        .as_deref()
        .and_then(|lan| lan.saved_peer(&marker.target.device_id));
    if state.open != Some(incarnation) {
        if state.receive.is_some() {
            return;
        }
        state.open = Some(incarnation);
        let cyclic = nav
            .stack
            .iter()
            .map(|level| level.node)
            .take_while(|&id| id != root)
            .filter_map(|id| tree.nodes[id].merged.as_ref())
            .any(|ancestor| {
                let (a, b) = (&ancestor.target, &marker.target);
                a.device_id == b.device_id
                    && a.volume_id == b.volume_id
                    && a.relative_path == b.relative_path
            });
        if cyclic {
            clear_view(&mut tree, root);
            tree.nodes[root].marker_status = Some("循環リンク".into());
        } else {
            tree.nodes[root].marker_status =
                Some(format!("{} を照合中…", marker.target.device_name));
            tree.nodes[root].marker_read_only = true;
            let (send, receive) = mpsc::channel();
            let proxy = proxy.as_deref().map(|proxy| (**proxy).clone());
            let request = std::thread::Builder::new()
                .name("lisa-marker-folder".into())
                .spawn(move || {
                    let result = load_children(&marker, peer);
                    let _ = send.send(Response {
                        result,
                        root,
                        incarnation,
                    });
                    if let Some(proxy) = proxy {
                        let _ = proxy.send_event(WinitUserEvent::WakeUp);
                    }
                });
            match request {
                Ok(_) => state.receive = Some(std::sync::Mutex::new(receive)),
                Err(error) => {
                    tree.nodes[root].marker_status = Some(format!("中身を取得できません: {error}"))
                }
            }
        }
        changed.pending = true;
        redraw.write(RequestRedraw);
    }
    if let Some(response) = response {
        if response.root != root || response.incarnation != incarnation {
            return;
        }
        let metadata = tree.nodes[root].merged.as_ref().expect("リンクのフォルダ");
        let link_id = metadata.link_id.clone();
        let target_name = metadata.target.device_name.clone();
        let message = match response.result {
            Ok(children) => {
                let message = format!("{target_name} を参照中");
                let now = lisa_core::db::now_ms();
                if children.len() <= 2048 {
                    if state.cache.len() >= 24
                        && !state.cache.contains_key(&link_id)
                        && let Some(oldest) = state
                            .cache
                            .iter()
                            .min_by_key(|(_, cached)| cached.checked_at)
                            .map(|(id, _)| id.clone())
                    {
                        state.cache.remove(&oldest);
                    }
                    state.cache.insert(
                        link_id.clone(),
                        CachedChildren {
                            checked_at: now,
                            children: children.clone(),
                        },
                    );
                } else {
                    state.cache.remove(&link_id);
                }
                children.attach(&mut tree, root);
                tree.nodes[root].marker_read_only = false;
                if state.observed.len() >= 128
                    && !state.observed.contains_key(&incarnation)
                    && let Some(oldest) = state
                        .observed
                        .iter()
                        .min_by_key(|(_, at)| *at)
                        .map(|(id, _)| *id)
                {
                    state.observed.remove(&oldest);
                }
                state.observed.insert(incarnation, now);
                message
            }
            Err(ResolveError::Offline(message)) => {
                if let Some(at) = state.observed.get(&incarnation) {
                    format!(
                        "{message}／前回確認 {}（閲覧のみ）",
                        crate::ui::tooltip::format_local(*at)
                    )
                } else if let Some(cached) = state.cache.get(&link_id) {
                    cached.children.clone().attach(&mut tree, root);
                    format!(
                        "{message}／前回確認 {}（閲覧のみ）",
                        crate::ui::tooltip::format_local(cached.checked_at)
                    )
                } else {
                    clear_view(&mut tree, root);
                    format!("{message}／中身は未取得")
                }
            }
            Err(error) => {
                clear_view(&mut tree, root);
                state.observed.remove(&incarnation);
                state.cache.remove(&link_id);
                error.message()
            }
        };
        tree.nodes[root].marker_status = Some(message);
        changed.pending = true;
        redraw.write(RequestRedraw);
    }
}

fn clear_view(tree: &mut FileTree, root: NodeId) {
    tree.reset_children(root);
    let node = &mut tree.nodes[root];
    node.path = None;
    node.loaded = true;
    node.state = LoadState::Loaded;
    node.marker_read_only = true;
}

fn load_children(marker: &Marker, peer: Option<SavedPeer>) -> Result<Children, ResolveError> {
    match resolve(marker, peer)? {
        Target::Local { db, chain } => {
            let entry = *chain
                .last()
                .ok_or_else(|| ResolveError::Invalid("マージ先が見つかりません".into()))?;
            let folder = lisa_core::query::path_of(&db, entry)
                .map_err(|error| ResolveError::Invalid(error.to_string()))?
                .ok_or_else(|| ResolveError::Invalid("マージ先が見つかりません".into()))?;
            let rows = db
                .children(entry)
                .map_err(|error| ResolveError::Invalid(error.to_string()))?;
            Ok(Children::Local {
                path: folder.path,
                rows,
            })
        }
        Target::Remote { peer, chain } => {
            let entry = *chain
                .last()
                .ok_or_else(|| ResolveError::Invalid("マージ先が見つかりません".into()))?;
            let rows = crate::lan::fetch_children(&peer, entry).map_err(|(unauthorized, _)| {
                if unauthorized {
                    ResolveError::Unpaired(format!("{} とのペアリングを確認できません", peer.name))
                } else {
                    ResolveError::Offline(format!("{} はオフライン", peer.name))
                }
            })?;
            Ok(Children::Remote {
                device_id: peer.device_id,
                rows,
            })
        }
    }
}
