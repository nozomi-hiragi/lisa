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

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bevy::prelude::*;
use bevy::winit::EventLoopProxyWrapper;
use lisa_core::db::Db;
use lisa_core::listing::{self, Kind, Listing};

use crate::layout::Layouts;
use crate::model::tree::{FileTree, NodeId};

/// 1 フレームで木に足す結果の数の上限（一度にたくさん届いても固まらないように）。
const APPLY_PER_FRAME: usize = 256;
/// 木を並べ直す間隔の下限。読めるたびに並べ直して焼き直すと重いので、まとめる。
const RELAYOUT_EVERY: Duration = Duration::from_millis(300);

/// 1 フレームで DB から子を出すフォルダの数の上限。
const LOAD_PER_FRAME: usize = 256;

mod worker;

pub use worker::Rest;

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
                (
                    apply_settings,
                    request_visible,
                    receive,
                    load_visible,
                    resolve_jump,
                )
                    .chain()
                    .before(crate::view::ViewSystems),
            );
    }
}

/// DB の場所（設定と同じ、アプリのデータのフォルダ）。
pub fn db_path() -> Option<PathBuf> {
    Some(
        directories::ProjectDirs::from("", "", "Lisa")?
            .data_dir()
            .join("lisa.db"),
    )
}

/// DB を開く。開けなければ残さずに動く。
fn open_db() -> Option<Db> {
    let path = db_path()?;
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
    /// ハッシュの進み具合（読み終えたあとに始める）。
    pub hash: HashProgress,
}

#[derive(Default)]
pub struct HashProgress {
    /// 今ファイルをハッシュしているフォルダ。
    pub folder: Option<PathBuf>,
    /// ハッシュしたファイルの数と大きさ。
    pub files: u64,
    pub bytes: u64,
    pub started: Option<Instant>,
    pub finished: Option<Instant>,
}

/// 裏で読むときの決まり（一時停止、速さの上限など）と、読む係への頼み。
#[derive(Resource)]
pub struct ScanControl {
    pub control: Arc<worker::Control>,
    requests: Mutex<std::sync::mpsc::Sender<worker::Request>>,
    /// 監視（持っているあいだだけ知らせが来る）。
    _watcher: Option<Mutex<notify::RecommendedWatcher>>,
}

/// ドライブ `roots` を監視する（Mac は FSEvents、Windows は ReadDirectoryChangesW、`notify`）。変わった
/// ものの親のフォルダを、読み直しの知らせ（`Request::Hint`）として送る。知らせは取りこぼすことが
/// あるので、ヒントとして扱う（開いたときの読み直しもある）。アプリ自身の DB と画像の置き場は見ない。
fn watch(
    roots: &[PathBuf],
    requests: std::sync::mpsc::Sender<worker::Request>,
) -> Option<notify::RecommendedWatcher> {
    use notify::Watcher;
    if roots.is_empty() {
        return None;
    }
    let own: Vec<PathBuf> = directories::ProjectDirs::from("", "", "Lisa")
        .map(|d| vec![d.data_dir().to_path_buf(), d.cache_dir().to_path_buf()])
        .unwrap_or_default();
    let handler = move |event: notify::Result<notify::Event>| {
        let Ok(event) = event else {
            return;
        };
        if event.kind.is_access() {
            return;
        }
        for path in event.paths {
            let Some(folder) = path.parent() else {
                continue;
            };
            if own.iter().any(|o| path.starts_with(o)) {
                continue;
            }
            let _ = requests.send(worker::Request::Hint(folder.to_path_buf()));
        }
    };
    let mut watcher = match notify::recommended_watcher(handler) {
        Ok(watcher) => watcher,
        Err(error) => {
            warn!("監視を始められませんでした: {error}");
            return None;
        }
    };
    for root in roots {
        if let Err(error) = watcher.watch(root, notify::RecursiveMode::Recursive) {
            warn!("{} を監視できませんでした: {error}", root.display());
        }
    }
    Some(watcher)
}

impl ScanControl {
    /// 見ているフォルダを、裏の分より先に読んで（読み終えていなければ）ハッシュしてもらう。
    pub(crate) fn visible(&self, path: PathBuf) {
        let sent = self
            .requests
            .lock()
            .is_ok_and(|s| s.send(worker::Request::Visible(path)).is_ok());
        if sent {
            self.control
                .urgent
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// 設定の値を、読む係との決まりに写す。
fn apply_settings(settings: Res<crate::settings::Settings>, control: Option<Res<ScanControl>>) {
    use std::sync::atomic::Ordering;
    let Some(control) = control else {
        return;
    };
    if !settings.is_changed() && !control.is_added() {
        return;
    }
    let c = &control.control;
    c.limit.store(
        (settings.background_mb_per_sec * 1_000_000.0) as u64,
        Ordering::Relaxed,
    );
    c.rest_on_battery
        .store(settings.rest_on_battery >= 0.5, Ordering::Relaxed);
    c.cpu_limit
        .store(settings.rest_cpu_percent as u32, Ordering::Relaxed);
    c.disk_limit.store(
        (settings.rest_disk_mb_per_sec * 1_000_000.0) as u64,
        Ordering::Relaxed,
    );
}

/// 開いている場面のフォルダが変わったら、そこを先に読んで（ハッシュして）もらう。
fn request_visible(
    nav: Res<crate::view::Nav>,
    tree: Res<FileTree>,
    control: Option<Res<ScanControl>>,
    mut last: Local<Option<NodeId>>,
) {
    let Some(control) = control else {
        return;
    };
    if !nav.is_changed() {
        return;
    }
    let path = crate::view::scene_path(&nav, &tree);
    let folder = path[path.len() - 1].node;
    if *last == Some(folder) {
        return;
    }
    *last = Some(folder);
    // 開いたら、読み直してもらう（変わっていなければ何もしない）。
    if let Some(path) = &tree.nodes[folder].path {
        control.visible(path.clone());
    }
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
    /// 木にあるフォルダ（パスからノードを引く。ハッシュができたら状態を直すため）。
    folders: HashMap<PathBuf, NodeId>,
    /// ハッシュができて DB に書いたが、まだ確定していないフォルダ。
    hashed: HashSet<PathBuf>,
    /// 読み直したら変わっていて、まだハッシュし直していないフォルダ（子を出し直しても古いにする）。
    stale: HashSet<PathBuf>,
}

enum Message {
    /// フォルダを読んだ。`stored` なら DB にも書いた（見えるところに来たら出す）。
    Listed {
        path: PathBuf,
        result: Result<Listing, String>,
        stored: bool,
    },
    /// フォルダのハッシュができた（中のファイルもハッシュした）。
    Hashed { path: PathBuf },
    /// 読み直したら DB と違っていた（このあと `Listed` が来る）。
    Changed { path: PathBuf },
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
    let Some((db, roots)) = setup.0.lock().ok().and_then(|mut s| s.take()) else {
        return;
    };
    commands.remove_resource::<Setup>();
    let (sender, receiver) = channel();
    let (requests, request_receiver) = channel();
    let progress = ScanProgress(Arc::new(Mutex::new(Progress {
        started: Some(Instant::now()),
        ..default()
    })));
    let control = Arc::new(worker::Control::default());
    let worker = worker::Worker {
        db,
        roots,
        sender,
        requests: request_receiver,
        proxy: (**proxy).clone(),
        progress: progress.0.clone(),
        control: control.clone(),
        dirty: false,
        hints: Default::default(),
    };
    let watched: Vec<PathBuf> = worker
        .roots
        .iter()
        .filter(|r| r.drive_id.is_some() && worker.db.is_some())
        .map(|r| r.mount.clone())
        .collect();
    std::thread::Builder::new()
        .name("lisa-scan".into())
        .spawn(move || worker.run())
        .expect("読む係を作る");
    // 監視: 変わったところを、読み直しの知らせとして読む係に送る。
    let watcher = watch(&watched, requests.clone());
    commands.insert_resource(ScanControl {
        control,
        requests: Mutex::new(requests),
        _watcher: watcher.map(Mutex::new),
    });
    commands.insert_resource(progress);
    let mut scanner = Scanner {
        results: Mutex::new(receiver),
        pending: HashMap::new(),
        recent: HashMap::new(),
        folders: HashMap::new(),
        hashed: HashSet::new(),
        stale: HashSet::new(),
    };
    for id in 0..tree.nodes.len() {
        scanner.track(&tree, id);
    }
    commands.insert_resource(scanner);
}

fn report(shared: &Mutex<Progress>, update: impl FnOnce(&mut Progress)) {
    if let Ok(mut progress) = shared.lock() {
        update(&mut progress);
    }
}

impl Scanner {
    /// 木にあるフォルダ `id` をパスで引けるようにする（まだ読み終えていなければ `pending` にも）。
    fn track(&mut self, tree: &FileTree, id: NodeId) {
        let node = &tree.nodes[id];
        if let (true, Some(path)) = (node.is_dir, &node.path) {
            self.folders.insert(path.clone(), id);
            if !node.listed {
                self.pending.insert(path.clone(), id);
            }
        }
    }

    /// 木に出したフォルダ `id` の子を覚えておく。
    fn track_children(&mut self, tree: &FileTree, id: NodeId) {
        for &child in &tree.nodes[id].children {
            self.track(tree, child);
        }
    }
}

/// 届いた結果を受け取る。DB に書いたものは、木にあれば「読み終えた」にするだけ（子は見えるところに
/// 来たら出す）。DB に書いていないものは、そのまま木に足す。
fn receive(
    scanner: Option<ResMut<Scanner>>,
    mut tree: ResMut<FileTree>,
    mut changed: ResMut<TreeChanged>,
    mut nav: ResMut<crate::view::Nav>,
) {
    let Some(mut scanner) = scanner else {
        return;
    };
    let scanner = &mut *scanner;
    let messages: Vec<Message> = scanner
        .results
        .get_mut()
        .expect("結果")
        .try_iter()
        .take(APPLY_PER_FRAME)
        .collect();
    let mut any = false;
    for message in messages {
        match message {
            Message::Committed => {
                scanner.recent.clear();
                scanner.hashed.clear();
            }
            Message::Hashed { path } => {
                scanner.stale.remove(&path);
                if let Some(&id) = scanner.folders.get(&path) {
                    tree.mark_hashed(id);
                    any = true;
                }
                scanner.hashed.insert(path);
            }
            Message::Changed { path } => {
                // 木にあれば、子を出し直す（このあと届く `Listed` から）。ハッシュができるまで古い。
                if let Some(&id) = scanner.folders.get(&path) {
                    tree.reset_children(id);
                    tree.mark_stale(id);
                    // 外した子を開いていたら、そのフォルダまで戻る（子は出し直すと別のノードになる）。
                    if let Some(at) = nav.stack.iter().position(|l| l.node == id)
                        && at + 1 < nav.stack.len()
                    {
                        nav.stack.truncate(at + 1);
                    }
                    any = true;
                }
                scanner.hashed.remove(&path);
                scanner.stale.insert(path);
            }
            Message::Listed {
                path,
                result,
                stored: false,
            } => {
                if let Some(id) = scanner.pending.remove(&path) {
                    tree.add_listing(id, result);
                    scanner.track_children(&tree, id);
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
        load_children(&mut tree, &store, reader.as_deref_mut(), scanner, id);
    }
    changed.pending = true;
}

/// フォルダ `id` の子を、まだ確定していない結果か DB から 1 段出す。出せたら true。
fn load_children(
    tree: &mut FileTree,
    store: &Store,
    db: Option<&mut Db>,
    scanner: &mut Scanner,
    id: NodeId,
) -> bool {
    let Some(path) = tree.nodes[id].path.clone() else {
        return false;
    };
    if let Some(result) = scanner.recent.remove(&path) {
        tree.add_listing(id, result);
    } else {
        let Some(db) = db else {
            return false;
        };
        let Some((drive_id, mount)) = drive_of(tree, store, id) else {
            return false;
        };
        let entry = match tree.nodes[id].entry {
            Some(entry) => Some(entry),
            None => db.find_folder(&drive_id, &mount, &path).unwrap_or(None),
        };
        let Some(entry) = entry else {
            return false;
        };
        match db.children(entry) {
            Ok(rows) => {
                let (recent, hashed) = (&scanner.recent, &scanner.hashed);
                tree.add_stored_children(
                    id,
                    rows,
                    |p| recent.contains_key(p),
                    |p| hashed.contains(p),
                );
            }
            Err(error) => {
                warn!("DB から読めませんでした（{}）: {error}", path.display());
                return false;
            }
        }
    }
    scanner.track_children(tree, id);
    if scanner.stale.contains(&path) {
        tree.mark_stale(id);
    }
    true
}

/// 行きたい場所（DB の `entry.id` の列。ドライブの一番上から）。`resolve_jump` が木の上のノードの
/// 道のりにする（足りない子は DB から出す）。
#[derive(Resource)]
pub struct JumpTo(pub Vec<i64>);

/// 行きたい場所の、Root からの道のり（`view.rs` の `jump` が開く）。
#[derive(Resource)]
pub struct JumpPath(pub Vec<NodeId>);

/// `JumpTo` を木の上の道のりにする。
fn resolve_jump(
    mut commands: Commands,
    jump: Option<Res<JumpTo>>,
    scanner: Option<ResMut<Scanner>>,
    store: Res<Store>,
    mut tree: ResMut<FileTree>,
    mut changed: ResMut<TreeChanged>,
) {
    let (Some(jump), Some(mut scanner)) = (jump, scanner) else {
        return;
    };
    commands.remove_resource::<JumpTo>();
    let chain = &jump.0;
    // 一番上は、ドライブのノード（`entry` が chain[0]）。
    let Some(mut node) =
        (0..tree.nodes.len()).find(|&i| tree.nodes[i].entry == chain.first().copied())
    else {
        warn!("行き先のドライブが木にありません");
        return;
    };
    let mut reader = store.reader.as_ref().and_then(|r| r.lock().ok());
    for &entry in &chain[1..] {
        if !tree.nodes[node].loaded
            && !load_children(&mut tree, &store, reader.as_deref_mut(), &mut scanner, node)
        {
            warn!("行き先の途中を DB から出せませんでした");
            return;
        }
        let Some(child) = tree.nodes[node]
            .children
            .iter()
            .copied()
            .find(|&c| tree.nodes[c].entry == Some(entry))
        else {
            warn!("行き先が木にありません（読み直しが要るかもしれません）");
            return;
        };
        node = child;
    }
    let mut path = vec![node];
    while let Some(parent) = tree.nodes[path[path.len() - 1]].parent {
        path.push(parent);
    }
    path.reverse();
    changed.pending = true;
    commands.insert_resource(JumpPath(path));
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
