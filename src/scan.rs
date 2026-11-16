//! 本物のフォルダを裏で読んで、木に足す。
//!
//! フォルダは 1 つずつ、名前順で上のフォルダから深さ優先で掘る（あるフォルダの下を全部読み
//! 終えてから、次のフォルダへ進む）。なので、上から順にフォルダの中身と大きさが確定していく。
//! 読むのは裏のスレッド 1 本で、読めたらイベントループを起こして（止まっている間は 1 秒に 1 回
//! しか描かないため）、木に足す。ファイルは今は一覧の中で大きさを見るだけ（ハッシュは並列にする予定）。
//! 木が変わったら、動きが止まるのを待って並べ直す（`view.rs` の `relayout`）。
//!
//! `LISA_DUMMY=1` で起動すると、今までのダミーの木を使い、何も読まない。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bevy::prelude::*;
use bevy::winit::{EventLoopProxyWrapper, WinitUserEvent};
use lisa_core::listing::{self, Kind, Listing};

use crate::model::tree::{FileTree, NodeId};
use crate::render::cache::FolderImages;

/// 1 フレームで木に足す結果の数の上限（一度にたくさん届いても固まらないように）。
const APPLY_PER_FRAME: usize = 256;
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
            .add_systems(Update, apply_listings.before(crate::view::ViewSystems));
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

/// このあと読むフォルダを、読み込み中のリストにいくつ出すか。
const SHOWN_NEXT: usize = 5;

/// 読む係の進み具合（読み込み中のリストに出す）。読む係が書き、UI が読む。
#[derive(Default)]
pub struct Progress {
    /// 今読んでいるフォルダ。
    pub current: Option<PathBuf>,
    /// このあと読むフォルダ（先に読むものから、`SHOWN_NEXT` 個まで）。
    pub next: Vec<PathBuf>,
    /// まだ読んでいないフォルダの数（今わかっている分）。
    pub queued: usize,
    /// 読んだフォルダの数。
    pub done: u64,
    /// そのうち読めなかったものの数。
    pub failed: u64,
    /// 読み始めた時刻と、全部読み終えた時刻。
    pub started: Option<Instant>,
    pub finished: Option<Instant>,
}

/// 読む係の進み具合を UI から覗く。
#[derive(Resource, Clone)]
pub struct ScanProgress(pub Arc<Mutex<Progress>>);

/// 読む係から届く結果を受け取る。
#[derive(Resource)]
struct Scanner {
    results: Mutex<Receiver<Listed>>,
    /// 読むと決まっていて、まだ結果が届いていないフォルダ（パスからノードを引く）。
    pending: HashMap<PathBuf, NodeId>,
}

struct Listed {
    path: PathBuf,
    result: Result<Listing, String>,
}

/// 中に入って読むもの（フォルダで、クラウドのみでも別のボリュームでもない）。
/// `FileTree::add_listing` がパスを付けるものと同じ。
fn readable(entry: &listing::Entry) -> bool {
    entry.kind == Kind::Dir && !entry.cloud_only && !entry.other_volume
}

fn start_workers(mut commands: Commands, proxy: Res<EventLoopProxyWrapper>, tree: Res<FileTree>) {
    // 読み始めるのは、まだ読んでいないドライブ（木の並びの順）。
    let roots: Vec<(NodeId, PathBuf)> = (0..tree.nodes.len())
        .filter_map(|id| {
            let node = &tree.nodes[id];
            let path = node.path.as_ref()?;
            (node.is_dir && !node.listed).then(|| (id, path.clone()))
        })
        .collect();
    let (sender, receiver) = channel();
    let proxy = (**proxy).clone();
    let mut stack: Vec<PathBuf> = roots.iter().rev().map(|(_, p)| p.clone()).collect();
    let progress = ScanProgress(Arc::new(Mutex::new(Progress {
        started: Some(Instant::now()),
        ..default()
    })));
    let shared = progress.0.clone();
    std::thread::Builder::new()
        .name("lisa-scan".into())
        .spawn(move || {
            // 深さ優先: 取り出したフォルダの子を、名前順の逆に積む（上の子から先に取り出される）。
            while let Some(path) = stack.pop() {
                report(&shared, |p| {
                    p.current = Some(path.clone());
                    p.queued = stack.len();
                });
                let result = listing::read_dir(&path).map_err(|e| e.to_string());
                if let Ok(listing) = &result {
                    for entry in listing.entries.iter().rev().filter(|e| readable(e)) {
                        stack.push(path.join(&entry.name));
                    }
                }
                let failed = result.is_err();
                report(&shared, |p| {
                    p.done += 1;
                    p.failed += failed as u64;
                    p.queued = stack.len();
                    p.next = stack.iter().rev().take(SHOWN_NEXT).cloned().collect();
                    if stack.is_empty() {
                        p.current = None;
                        p.finished = Some(Instant::now());
                    }
                });
                if sender.send(Listed { path, result }).is_err() {
                    return;
                }
                // 止まっている間でも、すぐに木に足して描けるように起こす。
                let _ = proxy.send_event(WinitUserEvent::WakeUp);
            }
        })
        .expect("読む係を作る");
    commands.insert_resource(progress);
    commands.insert_resource(Scanner {
        results: Mutex::new(receiver),
        pending: roots.into_iter().map(|(id, p)| (p, id)).collect(),
    });
}

fn report(shared: &Mutex<Progress>, update: impl FnOnce(&mut Progress)) {
    if let Ok(mut progress) = shared.lock() {
        update(&mut progress);
    }
}

/// 届いた結果を木に足す。読めたフォルダの子のフォルダは、このあと届くので覚えておく。
fn apply_listings(
    scanner: Option<ResMut<Scanner>>,
    mut tree: ResMut<FileTree>,
    mut changed: ResMut<TreeChanged>,
    mut images: ResMut<FolderImages>,
) {
    let Some(mut scanner) = scanner else {
        return;
    };
    let scanner = &mut *scanner;
    let results = scanner.results.get_mut().expect("結果");
    let mut any = false;
    for listed in results.try_iter().take(APPLY_PER_FRAME) {
        let Some(id) = scanner.pending.remove(&listed.path) else {
            continue;
        };
        tree.add_listing(id, listed.result);
        images.mark_changed(&tree, id);
        for &child in &tree.nodes[id].children {
            let node = &tree.nodes[child];
            if let (Some(path), false) = (&node.path, node.listed) {
                scanner.pending.insert(path.clone(), child);
            }
        }
        any = true;
    }
    if any {
        changed.pending = true;
    }
}
