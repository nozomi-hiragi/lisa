//! 本物のフォルダを裏で読んで、木に足す。
//!
//! フォルダは 1 つずつ、名前順で上のフォルダから深さ優先で掘る（あるフォルダの下を全部読み
//! 終えてから、次のフォルダへ進む）。なので、上から順にフォルダの中身と大きさが確定していく。
//! 読むのは裏のスレッド 1 本で、読めたらイベントループを起こして（止まっている間は 1 秒に 1 回
//! しか描かないため）、木に足す。ファイルは今は一覧の中で大きさを見るだけ（ハッシュは並列にする予定）。
//! 木が変わったら、動きが止まるのを待って並べ直す（`view.rs` の `relayout`）。
//!
//! 読んだ結果は、読む係が SQLite に書く（`lisa_core::db`、0.3 秒ごとにまとめて確定）。木（メモリ）に
//! 出すのは、見えるところ（並べてある範囲）だけ。読み終えたフォルダでも、並べる範囲に入るまでは子を
//! 出さず、入ったら DB から 1 段ずつ出す（`load_visible`）。起動したら、DB に残っているところは
//! そのまま使い、まだ読み終えていないフォルダから続きを読む。読み終えたフォルダを読み直すのは、まだしない。
//! DB が開けないときと、ID の無いドライブは、読んだ結果をそのまま木に足す（全部メモリに持つ）。
//!
//! `LISA_DUMMY=1` で起動すると、今までのダミーの木を使い、何も読まない。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bevy::prelude::*;
use bevy::winit::{EventLoopProxyWrapper, WinitUserEvent};
use lisa_core::db::Db;
use lisa_core::listing::{self, Kind, Listing};

use crate::layout::Layouts;
use crate::model::tree::{FileTree, NodeId};
use crate::render::cache::FolderImages;

/// 1 フレームで木に足す結果の数の上限（一度にたくさん届いても固まらないように）。
const APPLY_PER_FRAME: usize = 256;
/// DB に書いたものを確定する間隔。
const COMMIT_EVERY: Duration = Duration::from_millis(300);
/// 木を並べ直す間隔の下限。読めるたびに並べ直して焼き直すと重いので、まとめる。
const RELAYOUT_EVERY: Duration = Duration::from_millis(300);

/// 1 フレームで DB から子を出すフォルダの数の上限。
const LOAD_PER_FRAME: usize = 256;

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
        let mut tree = FileTree::this_device(&name, &drives);
        // `this_device` は、隠すもの以外のドライブを、この順で端末の下に並べる。
        let visible: Vec<&lisa_core::volumes::Volume> =
            drives.iter().filter(|d| !d.hidden).collect();
        let drive_nodes = tree.nodes[FileTree::ROOT + 1].children.clone();
        // 書くのは読む係、木に出すために読むのは UI。別々につなぐ（WAL なので同時に使える）。
        let mut db = open_db();
        let mut reader = db.as_ref().and_then(|_| open_db());
        if let Some(store) = db.as_mut() {
            let os = std::env::consts::OS;
            match store.this_device(&name, &name, os) {
                Ok(device) => {
                    for (volume, &node) in visible.iter().zip(&drive_nodes) {
                        let Some(id) = &volume.id else { continue };
                        if let Err(error) = store.upsert_drive(&device, volume) {
                            warn!("ドライブを DB に書けませんでした: {error}");
                            continue;
                        }
                        match store.drive_root(id) {
                            Ok(Some(row)) => tree.set_drive_row(node, &row),
                            Ok(None) => {}
                            Err(error) => warn!("DB からドライブを読めませんでした: {error}"),
                        }
                    }
                }
                Err(error) => {
                    warn!("DB に端末を書けませんでした: {error}");
                    db = None;
                    reader = None;
                }
            }
        }
        let roots: Vec<Root> = visible
            .iter()
            .zip(&drive_nodes)
            .map(|(volume, &node)| Root {
                node,
                // DB が無ければ、どのドライブも残さない。
                drive_id: volume.id.clone().filter(|_| db.is_some()),
                mount: volume.mount_path.clone(),
                listed: tree.nodes[node].listed,
            })
            .collect();
        app.insert_resource(tree)
            .insert_resource(Store {
                reader: reader.map(Mutex::new),
                roots: roots
                    .iter()
                    .map(|r| (r.node, (r.drive_id.clone(), r.mount.clone())))
                    .collect(),
            })
            .insert_resource(Setup(Mutex::new(Some((db, roots)))))
            .init_resource::<TreeChanged>()
            .add_systems(Startup, start_workers)
            .add_systems(
                Update,
                (receive, load_visible)
                    .chain()
                    .before(crate::view::ViewSystems),
            );
    }
}

/// DB を開く（設定と同じ場所のデータのフォルダ）。開けなければ残さずに動く。
fn open_db() -> Option<Db> {
    let path = directories::ProjectDirs::from("", "", "Lisa")?
        .data_dir()
        .join("lisa.db");
    match Db::open(&path) {
        Ok(db) => {
            info!("DB: {}", path.display());
            Some(db)
        }
        Err(error) => {
            warn!("DB を開けませんでした（{}）: {error}", path.display());
            None
        }
    }
}

/// 読み始めるドライブ。`drive_id` の無いドライブは読むが DB には残さない。
struct Root {
    node: NodeId,
    drive_id: Option<String>,
    mount: PathBuf,
    /// DB で、一番上を読み終えている（続きから読む）。
    listed: bool,
}

/// 起動のときに開いた DB と、読むドライブ（読む係に渡すまで預かる）。
#[derive(Resource)]
struct Setup(Mutex<Option<(Option<Db>, Vec<Root>)>>);

/// 木に出すために DB を読む口と、ドライブのノードごとの（DB の ID、つないでいる場所）。
#[derive(Resource)]
struct Store {
    reader: Option<Mutex<Db>>,
    roots: HashMap<NodeId, (Option<String>, PathBuf)>,
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
    results: Mutex<Receiver<Message>>,
    /// 木にあって、まだ読み終えていないフォルダ（パスからノードを引く）。
    pending: HashMap<PathBuf, NodeId>,
    /// 読み終えて DB に書いたが、まだ確定していないもの（確定したら DB から読めるので捨てる）。
    recent: HashMap<PathBuf, Result<Listing, String>>,
}

enum Message {
    /// フォルダを読んだ。`stored` なら DB にも書いた（見えるところに来たら出す）。
    Listed {
        path: PathBuf,
        result: Result<Listing, String>,
        stored: bool,
    },
    /// ここまでに書いたものを確定した。
    Committed,
}

/// 中に入って読むもの（フォルダで、クラウドのみでも別のボリュームでもない）。
/// `FileTree::add_listing` がパスを付けるものと同じ。
fn readable(entry: &listing::Entry) -> bool {
    entry.kind == Kind::Dir && !entry.cloud_only && !entry.other_volume
}

fn start_workers(
    mut commands: Commands,
    proxy: Res<EventLoopProxyWrapper>,
    tree: Res<FileTree>,
    setup: Res<Setup>,
) {
    let Some((mut db, roots)) = setup.0.lock().ok().and_then(|mut s| s.take()) else {
        return;
    };
    commands.remove_resource::<Setup>();
    let (sender, receiver) = channel();
    let proxy = (**proxy).clone();
    let progress = ScanProgress(Arc::new(Mutex::new(Progress {
        started: Some(Instant::now()),
        ..default()
    })));
    let shared = progress.0.clone();
    std::thread::Builder::new()
        .name("lisa-scan".into())
        .spawn(move || {
            // 読むものを、深さ優先で後ろから先に積む。DB に残っているドライブは、まだ読み終えて
            // いないフォルダから（DB の木の並びの順に）。
            let mut stack: Vec<(usize, PathBuf)> = Vec::new();
            for (index, root) in roots.iter().enumerate().rev() {
                let todo = match (db.as_ref(), &root.drive_id) {
                    (Some(store), Some(drive_id)) if root.listed => store
                        .unlisted_folders(drive_id, &root.mount)
                        .unwrap_or_else(|error| {
                            warn!("DB から続きを読めませんでした: {error}");
                            vec![root.mount.clone()]
                        }),
                    _ => vec![root.mount.clone()],
                };
                stack.extend(todo.into_iter().rev().map(|path| (index, path)));
            }
            if stack.is_empty() {
                report(&shared, |p| p.finished = Some(Instant::now()));
            }
            let mut last_commit = Instant::now();
            // 深さ優先: 取り出したフォルダの子を、名前順の逆に積む（上の子から先に取り出される）。
            while let Some((index, path)) = stack.pop() {
                report(&shared, |p| {
                    p.current = Some(path.clone());
                    p.queued = stack.len();
                });
                let result = listing::read_dir(&path).map_err(|e| e.to_string());
                if let Ok(listing) = &result {
                    for entry in listing.entries.iter().rev().filter(|e| readable(e)) {
                        stack.push((index, path.join(&entry.name)));
                    }
                }
                let root = &roots[index];
                let mut stored = false;
                let mut committed = false;
                if let (Some(store), Some(drive_id)) = (db.as_mut(), &root.drive_id) {
                    let saved = store
                        .begin()
                        .and_then(|()| store.save_listing(drive_id, &root.mount, &path, &result));
                    match saved {
                        Ok(()) => stored = true,
                        Err(error) => warn!("DB に書けませんでした（{}）: {error}", path.display()),
                    }
                    if stack.is_empty() || last_commit.elapsed() >= COMMIT_EVERY {
                        match store.commit() {
                            Ok(()) => committed = true,
                            Err(error) => warn!("DB に確定できませんでした: {error}"),
                        }
                        last_commit = Instant::now();
                    }
                }
                let failed = result.is_err();
                report(&shared, |p| {
                    p.done += 1;
                    p.failed += failed as u64;
                    p.queued = stack.len();
                    p.next = stack
                        .iter()
                        .rev()
                        .take(SHOWN_NEXT)
                        .map(|(_, p)| p.clone())
                        .collect();
                    if stack.is_empty() {
                        p.current = None;
                        p.finished = Some(Instant::now());
                    }
                });
                let message = Message::Listed {
                    path,
                    result,
                    stored,
                };
                if sender.send(message).is_err()
                    || (committed && sender.send(Message::Committed).is_err())
                {
                    break;
                }
                // 止まっている間でも、すぐに木に足して描けるように起こす。
                let _ = proxy.send_event(WinitUserEvent::WakeUp);
            }
            if let Some(store) = db.as_mut() {
                let _ = store.commit();
            }
        })
        .expect("読む係を作る");
    commands.insert_resource(progress);
    let pending = (0..tree.nodes.len())
        .filter_map(|id| {
            let node = &tree.nodes[id];
            (node.is_dir && !node.listed)
                .then(|| node.path.clone().map(|p| (p, id)))
                .flatten()
        })
        .collect();
    commands.insert_resource(Scanner {
        results: Mutex::new(receiver),
        pending,
        recent: HashMap::new(),
    });
}

fn report(shared: &Mutex<Progress>, update: impl FnOnce(&mut Progress)) {
    if let Ok(mut progress) = shared.lock() {
        update(&mut progress);
    }
}

/// 木に出したフォルダ `id` の子のうち、まだ読み終えていないフォルダを覚えておく。
fn track_children(tree: &FileTree, id: NodeId, pending: &mut HashMap<PathBuf, NodeId>) {
    for &child in &tree.nodes[id].children {
        let node = &tree.nodes[child];
        if let (Some(path), false) = (&node.path, node.listed) {
            pending.insert(path.clone(), child);
        }
    }
}

/// 届いた結果を受け取る。DB に書いたものは、木にあれば「読み終えた」にするだけ（子は見えるところに
/// 来たら出す）。DB に書いていないものは、そのまま木に足す。
fn receive(
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
    for message in results.try_iter().take(APPLY_PER_FRAME) {
        match message {
            Message::Committed => scanner.recent.clear(),
            Message::Listed {
                path,
                result,
                stored: false,
            } => {
                if let Some(id) = scanner.pending.remove(&path) {
                    tree.add_listing(id, result);
                    track_children(&tree, id, &mut scanner.pending);
                    images.mark_changed(&tree, id);
                    any = true;
                }
            }
            Message::Listed {
                path,
                result,
                stored: true,
            } => {
                if let Some(id) = scanner.pending.remove(&path) {
                    tree.mark_listed(id);
                    images.mark_changed(&tree, id);
                    any = true;
                }
                scanner.recent.insert(path, result);
            }
        }
    }
    if any {
        changed.pending = true;
    }
}

/// 並べてあるフォルダのうち、読み終えているのに子をまだ出していないものを、DB（か、まだ確定して
/// いない結果）から 1 段出す。
fn load_visible(
    scanner: Option<ResMut<Scanner>>,
    store: Res<Store>,
    layouts: Res<Layouts>,
    mut tree: ResMut<FileTree>,
    mut changed: ResMut<TreeChanged>,
    mut images: ResMut<FolderImages>,
) {
    let Some(mut scanner) = scanner else {
        return;
    };
    let scanner = &mut *scanner;
    if !layouts.is_changed() && !tree.is_changed() {
        return;
    }
    let wanted: Vec<NodeId> = layouts
        .ids()
        .filter(|&id| {
            let node = &tree.nodes[id];
            node.is_dir && node.listed && !node.loaded && node.path.is_some()
        })
        .take(LOAD_PER_FRAME)
        .collect();
    if wanted.is_empty() {
        return;
    }
    let mut reader = store.reader.as_ref().and_then(|r| r.lock().ok());
    for id in wanted {
        let path = tree.nodes[id].path.clone().expect("パスのあるフォルダ");
        if let Some(result) = scanner.recent.remove(&path) {
            tree.add_listing(id, result);
        } else {
            let Some(db) = reader.as_deref_mut() else {
                continue;
            };
            let Some((drive_id, mount)) = drive_of(&tree, &store, id) else {
                continue;
            };
            let entry = match tree.nodes[id].entry {
                Some(entry) => Some(entry),
                None => db.find_folder(&drive_id, &mount, &path).unwrap_or(None),
            };
            let Some(entry) = entry else {
                continue;
            };
            match db.children(entry) {
                Ok(rows) => {
                    let recent = &scanner.recent;
                    tree.add_stored_children(id, rows, |p| recent.contains_key(p));
                }
                Err(error) => {
                    warn!("DB から読めませんでした（{}）: {error}", path.display());
                    continue;
                }
            }
        }
        track_children(&tree, id, &mut scanner.pending);
        images.mark_changed(&tree, id);
    }
    changed.pending = true;
}

/// ノード `id` があるドライブの（DB の ID、つないでいる場所）。DB に残さないドライブなら None。
fn drive_of(tree: &FileTree, store: &Store, id: NodeId) -> Option<(String, PathBuf)> {
    let mut cur = Some(id);
    while let Some(i) = cur {
        if let Some((drive_id, mount)) = store.roots.get(&i) {
            return drive_id.clone().map(|d| (d, mount.clone()));
        }
        cur = tree.nodes[i].parent;
    }
    None
}
