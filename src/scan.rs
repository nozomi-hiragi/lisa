//! 本物のフォルダを裏で読んで、木に足す。
//!
//! 読むのは、並べてある（画面とその周りにある）フォルダのうち、まだ読んでいないものだけ。
//! 今開いている場面のフォルダに近いものから先に読む。読むのは裏のスレッドで、読めたら
//! イベントループを起こして（止まっている間は 1 秒に 1 回しか描かないため）、木に足す。
//! 木が変わったら、動きが止まるのを待って並べ直す（`view.rs` の `relayout`）。
//!
//! `LISA_DUMMY=1` で起動すると、今までのダミーの木を使い、何も読まない。

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use bevy::prelude::*;
use bevy::winit::{EventLoopProxyWrapper, WinitUserEvent};
use lisa_core::listing::{self, Listing};

use crate::layout::Layouts;
use crate::model::tree::{FileTree, NodeId};
use crate::view::{Nav, scene_path};

/// 読む係の数。
const WORKERS: usize = 2;
/// 1 フレームで木に足す結果の数の上限（一度にたくさん届いても固まらないように）。
const APPLY_PER_FRAME: usize = 64;
/// 木を並べ直す間隔の下限。読めるたびに並べ直して焼き直すと重いので、まとめる。
const RELAYOUT_EVERY: Duration = Duration::from_millis(300);

pub struct ScanPlugin;

impl Plugin for ScanPlugin {
    fn build(&self, app: &mut App) {
        if std::env::var_os("LISA_DUMMY").is_some() {
            app.insert_resource(FileTree::dummy());
            return;
        }
        if let Err(error) = lisa_core::cloud::prevent_downloads() {
            warn!("クラウドのファイルをダウンロードしない設定にできませんでした: {error}");
        }
        let drives = lisa_core::volumes::list().unwrap_or_else(|error| {
            warn!("ドライブの一覧が取れませんでした: {error}");
            vec![root_volume()]
        });
        let name = lisa_core::volumes::device_name();
        app.insert_resource(FileTree::this_device(&name, &drives))
            .init_resource::<TreeChanged>()
            .add_systems(Startup, start_workers)
            .add_systems(
                Update,
                (request_listings, apply_listings)
                    .chain()
                    .before(crate::view::ViewSystems),
            );
    }
}

/// ドライブの一覧が取れない OS（開発用の Linux など）では、「/」だけを出す。
fn root_volume() -> lisa_core::volumes::Volume {
    lisa_core::volumes::Volume {
        id: None,
        label: "/".into(),
        fs: String::new(),
        mount_path: PathBuf::from("/"),
        capacity: None,
        available: None,
        removable: false,
        network: false,
        read_only: false,
        case_sensitive: None,
        hidden: false,
    }
}

/// 木が変わった（並べ直しが要る）。`last` は最後に並べ直した時刻。
#[derive(Resource, Default)]
pub struct TreeChanged {
    pub pending: bool,
    pub last: Option<Instant>,
}

impl TreeChanged {
    /// 今並べ直してよいか（前に並べ直してから少し経った）。
    pub fn due(&self) -> bool {
        self.pending && self.last.is_none_or(|t| t.elapsed() >= RELAYOUT_EVERY)
    }
}

/// 読む係とのやりとり。
#[derive(Resource)]
struct Scanner {
    shared: Arc<Shared>,
    results: Mutex<Receiver<Listed>>,
}

struct Shared {
    queue: Mutex<Queue>,
    ready: Condvar,
}

#[derive(Default)]
struct Queue {
    /// 読んでほしいもの（優先度の小さい順に並べてある）。UI が毎回まるごと入れ替える。
    wanted: Vec<(u32, NodeId, PathBuf)>,
    /// 読んでいる最中か、読み終えたもの。
    taken: HashSet<NodeId>,
}

struct Listed {
    node: NodeId,
    result: Result<Listing, String>,
}

fn start_workers(mut commands: Commands, proxy: Res<EventLoopProxyWrapper>) {
    let shared = Arc::new(Shared {
        queue: Mutex::new(Queue::default()),
        ready: Condvar::new(),
    });
    let (sender, receiver) = channel();
    for i in 0..WORKERS {
        let shared = shared.clone();
        let sender: Sender<Listed> = sender.clone();
        let proxy = (**proxy).clone();
        std::thread::Builder::new()
            .name(format!("lisa-scan-{i}"))
            .spawn(move || {
                loop {
                    let (node, path) = {
                        let mut queue = shared.queue.lock().expect("待ち行列");
                        loop {
                            if !queue.wanted.is_empty() {
                                let (_, node, path) = queue.wanted.remove(0);
                                queue.taken.insert(node);
                                break (node, path);
                            }
                            queue = shared.ready.wait(queue).expect("待ち行列");
                        }
                    };
                    let result = listing::read_dir(&path).map_err(|e| e.to_string());
                    if sender.send(Listed { node, result }).is_err() {
                        return;
                    }
                    // 止まっている間でも、すぐに木に足して描けるように起こす。
                    let _ = proxy.send_event(WinitUserEvent::WakeUp);
                }
            })
            .expect("読む係を作る");
    }
    commands.insert_resource(Scanner {
        shared,
        results: Mutex::new(receiver),
    });
}

/// 並べてあるフォルダのうち、まだ読んでいないものを、今の場面のフォルダに近い順に頼む。
fn request_listings(
    scanner: Option<Res<Scanner>>,
    tree: Res<FileTree>,
    layouts: Res<Layouts>,
    nav: Res<Nav>,
) {
    let Some(scanner) = scanner else {
        return;
    };
    if !layouts.is_changed() && !tree.is_changed() && !nav.is_changed() {
        return;
    }
    let path = scene_path(&nav, &tree);
    let focus = path[path.len() - 1].node;
    let mut wanted: Vec<(u32, NodeId, PathBuf)> = layouts
        .ids()
        .filter_map(|id| {
            let node = &tree.nodes[id];
            let path = node.path.as_ref()?;
            (node.is_dir && !node.listed).then(|| (distance(&tree, focus, id), id, path.clone()))
        })
        .collect();
    wanted.sort_by_key(|(priority, id, _)| (*priority, *id));
    let mut queue = scanner.shared.queue.lock().expect("待ち行列");
    let taken = std::mem::take(&mut queue.taken);
    wanted.retain(|(_, id, _)| !taken.contains(id));
    queue.taken = taken;
    queue.wanted = wanted;
    if !queue.wanted.is_empty() {
        scanner.shared.ready.notify_all();
    }
}

/// 木の上で `a` から `b` まで何歩か。
fn distance(tree: &FileTree, a: NodeId, b: NodeId) -> u32 {
    let ancestors = |mut id: NodeId| {
        let mut list = vec![id];
        while let Some(parent) = tree.nodes[id].parent {
            list.push(parent);
            id = parent;
        }
        list
    };
    let (up_a, up_b) = (ancestors(a), ancestors(b));
    let common = up_a
        .iter()
        .position(|x| up_b.contains(x))
        .unwrap_or(up_a.len());
    let lca = up_a.get(common).copied().unwrap_or(FileTree::ROOT);
    let to_b = up_b.iter().position(|&x| x == lca).unwrap_or(up_b.len());
    (common + to_b) as u32
}

/// 届いた結果を木に足す。
fn apply_listings(
    scanner: Option<Res<Scanner>>,
    mut tree: ResMut<FileTree>,
    mut changed: ResMut<TreeChanged>,
) {
    let Some(scanner) = scanner else {
        return;
    };
    let results = scanner.results.lock().expect("結果");
    let mut any = false;
    for listed in results.try_iter().take(APPLY_PER_FRAME) {
        tree.add_listing(listed.node, listed.result);
        any = true;
    }
    if any {
        changed.pending = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance_counts_steps_through_the_common_ancestor() {
        let tree = FileTree::dummy();
        let find = |name: &str| {
            (0..tree.nodes.len())
                .find(|&i| tree.nodes[i].name == name)
                .unwrap()
        };
        let (scan, y2024) = (find("スキャン"), find("2024"));
        assert_eq!(distance(&tree, scan, scan), 0);
        // スキャン と 2024 は同じ親の子どうし。
        assert_eq!(distance(&tree, scan, y2024), 2);
        assert_eq!(
            distance(&tree, scan, FileTree::ROOT),
            tree.nodes[scan].depth as u32
        );
    }
}
