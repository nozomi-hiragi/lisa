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
use std::path::{Path, PathBuf};
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

mod refresh;
mod worker;
use refresh::RefreshRequest;
pub(crate) use refresh::RefreshTarget;

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
        info!("ドライブを調べています");
        let mut drives = scan_volumes();
        // 描画の検証で、使い捨てのボリュームだけを読む。
        if crate::remote::debug_remote_enabled()
            && let Some(mounts) = std::env::var_os("LISA_DEBUG_SCAN_MOUNTS")
        {
            let mounts: HashSet<_> = std::env::split_paths(&mounts).collect();
            drives.retain_mut(|drive| {
                let selected = mounts.contains(&drive.mount_path);
                if selected {
                    // 検証用ディスクイメージは Finder に表示されない場所にもマウントできる。
                    drive.hidden = false;
                }
                selected
            });
        }
        info!(
            "ドライブ: {}",
            drives
                .iter()
                .map(|d| d.mount_path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
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
                    if let Ok(Some((name, _))) = lisa_core::query::this_device(store)
                        && !name.trim().is_empty()
                    {
                        tree.nodes[FileTree::ROOT + 1].name = name;
                    }
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
                mount_identity: lisa_core::volumes::mount_identity(&volume.mount_path),
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
            .init_resource::<RefreshStatus>()
            .init_resource::<crate::model::display_path::DisplayNames>()
            .add_systems(Startup, start_workers)
            .add_systems(
                Update,
                (
                    crate::model::display_path::refresh_display_names,
                    apply_settings,
                    request_visible,
                    receive.after(crate::organize::receive_events),
                    load_visible,
                    resolve_jump,
                )
                    .chain()
                    .before(crate::view::ViewSystems),
            );
    }
}

/// アプリのデータのフォルダ（DB や `lan.json`、`peers.json`、`control.json` の置き場）。
/// 環境変数 `LISA_DATA_DIR` があればそちらを使う（1 台の PC で 2 つ動かして試すとき用）。
pub fn data_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("LISA_DATA_DIR").filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    Some(
        directories::ProjectDirs::from("", "", "Lisa")?
            .data_dir()
            .to_path_buf(),
    )
}

/// アプリのキャッシュのフォルダ（`duplicates.json` や `folder-images` の置き場）。
/// 環境変数 `LISA_DATA_DIR` があれば、その下の `cache` フォルダにする（2 つ動かしても混ざらないように）。
pub fn cache_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("LISA_DATA_DIR").filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(dir).join("cache"));
    }
    Some(
        directories::ProjectDirs::from("", "", "Lisa")?
            .cache_dir()
            .to_path_buf(),
    )
}

/// DB の場所（設定と同じ、アプリのデータのフォルダ）。
pub fn db_path() -> Option<PathBuf> {
    Some(data_dir()?.join("lisa.db"))
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
    mount_identity: Option<lisa_core::volumes::MountIdentity>,
    /// DB で、一番上を読み終えている（続きから読む）。
    listed: bool,
}

/// 起動のときに開いた DB と、読むドライブ（読む係に渡すまで預かる）。
#[derive(Resource)]
struct Setup(Mutex<Option<(Option<Db>, Vec<Root>)>>);

/// 木に出すために DB を読む口と、ドライブのノードごとの（DB の ID、つないでいる場所）。
#[derive(Resource)]
pub(crate) struct Store {
    reader: Option<Mutex<Db>>,
    roots: HashMap<NodeId, (Option<String>, PathBuf)>,
}

impl Store {
    /// 起動時に DB を開けたか（DB を使わずに動かしているときは false）。
    pub(crate) fn has_db(&self) -> bool {
        self.reader.is_some()
    }

    /// DB にある表示名からは戻せない実ファイル名を含むパス。
    pub(crate) fn stored_path(&self, entry: i64) -> Option<PathBuf> {
        let db = self.reader.as_ref()?.lock().ok()?;
        let located = lisa_core::query::path_of(&db, entry).ok()??;
        located.path.is_absolute().then_some(located.path)
    }

    pub(crate) fn display_paths(
        &self,
        names: &crate::model::display_path::DisplayNames,
    ) -> Option<lisa_core::display_path::DisplayPaths> {
        let db = self.reader.as_ref()?.lock().ok()?;
        lisa_core::display_path::DisplayPaths::read(&db, &names.visible).ok()
    }

    pub(crate) fn with_display_entries<R>(
        &self,
        visible: &lisa_core::display_path::VisibleNames,
        use_entries: impl FnOnce(&dyn Fn(i64) -> Option<String>) -> R,
    ) -> Option<R> {
        let db = self.reader.as_ref()?.lock().ok()?;
        let names = lisa_core::display_path::DisplayPaths::read(&db, visible).ok()?;
        Some(use_entries(&|entry| {
            let parts = names.entry(&db, entry).ok()??;
            Some(lisa_core::display_path::text(&parts))
        }))
    }

    fn entry_chain_at(&self, path: &Path) -> Option<Vec<i64>> {
        let db = self.reader.as_ref()?.lock().ok()?;
        lisa_core::query::locate(&db, path)
            .ok()?
            .map(|target| target.chain)
    }

    fn entry_chain_for(&self, entry: i64) -> Option<Vec<i64>> {
        let db = self.reader.as_ref()?.lock().ok()?;
        lisa_core::query::path_of(&db, entry)
            .ok()?
            .map(|item| item.chain)
    }

    /// ノード `id` があるドライブの DB ID。DB に残さないドライブなら None。
    pub(crate) fn drive_id_of(&self, tree: &FileTree, id: NodeId) -> Option<String> {
        drive_of(tree, self, id).map(|(drive_id, _)| drive_id)
    }

    /// 端末ノード `device` の下にある、DB に残すドライブの ID 一覧。
    pub(crate) fn device_drive_ids(&self, tree: &FileTree, device: NodeId) -> Vec<String> {
        tree.nodes[device]
            .children
            .iter()
            .filter_map(|&child| self.roots.get(&child)?.0.clone())
            .collect()
    }
}

pub(crate) fn scan_volumes() -> Vec<lisa_core::volumes::Volume> {
    #[cfg(all(windows, feature = "verify-fs"))]
    if let Some(root) = std::env::var_os("LISA_VERIFY_SCAN_ROOT") {
        let Some(path) = root
            .to_str()
            .and_then(|root| lisa_mcp::isolated_directory(root).ok())
        else {
            warn!("検証用の走査ルートを確認できませんでした");
            return Vec::new();
        };
        if let Err(error) = lisa_mcp::validate_isolated_tree(&path) {
            warn!("検証用の走査ツリーを確認できませんでした: {error}");
            return Vec::new();
        }
        return vec![lisa_core::volumes::Volume {
            id: Some("verify:synthetic".into()),
            label: "検証用".into(),
            mount_path: path,
            ..root_volume()
        }];
    }
    lisa_core::volumes::list().unwrap_or_else(|error| {
        warn!("ドライブの一覧が取れませんでした: {error}");
        vec![root_volume()]
    })
}

/// ドライブの一覧が取れない OS（開発用の Linux など）では、「/」だけを出す。
pub(crate) fn root_volume() -> lisa_core::volumes::Volume {
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
    pub hash_queue: Option<lisa_core::remote::HashQueueState>,
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
    let mut own: Vec<PathBuf> = directories::ProjectDirs::from("", "", "Lisa")
        .map(|d| vec![d.data_dir().to_path_buf(), d.cache_dir().to_path_buf()])
        .unwrap_or_default();
    if let Some(dir) = data_dir() {
        own.push(dir);
    }
    let noisy = noisy_dirs();
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
            if own.iter().any(|o| path.starts_with(o))
                || is_under_any(&path, &noisy)
                || path
                    .components()
                    .any(|c| lisa_core::name::is_quarantine_dir(c.as_os_str()))
            {
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

/// OS やアプリがいつも書き換えていて、変わるたびに読み直すと重いだけのフォルダ（Windows のみ）。
/// ここの変化は知らせにしない（開いたときの読み直しでは読む）。比べるときは大文字・小文字を区別しない。
fn noisy_dirs() -> Vec<String> {
    if !cfg!(windows) {
        return Vec::new();
    }
    let env = |key: &str| std::env::var_os(key).map(PathBuf::from);
    let mut dirs: Vec<PathBuf> = Vec::new();
    dirs.extend(env("SystemRoot"));
    dirs.extend(env("ProgramData"));
    dirs.extend(env("USERPROFILE").map(|home| home.join("AppData")));
    if let Some(drive) = env("SystemDrive") {
        let root = PathBuf::from(format!("{}\\", drive.display()));
        dirs.push(root.join("$Recycle.Bin"));
        dirs.push(root.join("System Volume Information"));
    }
    dirs.iter().map(|d| fold_path(d)).collect()
}

/// 大文字・小文字と区切りの違いをならした、比べるための文字列（末尾に区切りを付ける）。
fn fold_path(path: &std::path::Path) -> String {
    let mut s = path.to_string_lossy().replace('/', "\\").to_lowercase();
    if !s.ends_with('\\') {
        s.push('\\');
    }
    s
}

fn is_under_any(path: &std::path::Path, dirs: &[String]) -> bool {
    if dirs.is_empty() {
        return false;
    }
    let folded = fold_path(path);
    dirs.iter().any(|d| folded.starts_with(d.as_str()))
}

impl ScanControl {
    /// 見ているフォルダを、裏の分より先に読んで（読み終えていなければ）ハッシュしてもらう。
    pub(crate) fn visible(&self, path: PathBuf) {
        self.send_urgent(worker::Request::Visible(path));
    }

    /// 対象を手動更新の列に積み、ワーカーを起こす。
    pub(crate) fn refresh(&self, target: RefreshTarget) -> Option<u64> {
        let sender = self.requests.lock().ok()?;
        let generation = self.control.queue_refresh(target, None)?;
        sender.send(worker::Request::ManualWake).ok()?;
        Some(generation)
    }

    pub(crate) fn list_created(
        &self,
        parent: RefreshTarget,
        created: PathBuf,
    ) -> Option<Receiver<Option<String>>> {
        let (tx, rx) = channel();
        let sender = self.requests.lock().ok()?;
        self.control.queue_refresh(parent, Some((created, tx)))?;
        sender.send(worker::Request::ManualWake).ok()?;
        Some(rx)
    }

    /// 整理による DB の更新を、一覧が同じ場合も画面に反映する。
    pub(crate) fn refresh_after_organize(&self, path: PathBuf) {
        self.send_urgent(worker::Request::Refresh(path));
    }

    /// 検証用（`LISA_DEBUG_REMOTE=1` のときのみ）: フォルダを「変わっていた」扱いにして読み直す。
    pub(crate) fn debug_force_changed(&self, path: PathBuf) {
        self.send_urgent(worker::Request::DebugForceChanged(path));
    }

    fn send_urgent(&self, request: worker::Request) {
        let Ok(sender) = self.requests.lock() else {
            return;
        };
        let order = std::sync::atomic::Ordering::Relaxed;
        // 受信側がすぐに処理しても、減算が加算を追い越さないようにする。
        self.control.urgent.fetch_add(1, order);
        if sender.send(request).is_err() {
            self.control.urgent.fetch_sub(1, order);
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

/// 開いている場面のフォルダが変わって操作が止まったら、そこを先に読んで（ハッシュして）もらう。
/// ジェスチャーやアニメーションの途中で通り掛かるだけの親フォルダは読み直さない。
fn request_visible(
    nav: Res<crate::view::Nav>,
    tree: Res<FileTree>,
    control: Option<Res<ScanControl>>,
    animating: Option<Res<crate::view::Animating>>,
    fades: Query<(), With<crate::view::Fade>>,
    gesture: Res<crate::input::gesture::Gesture>,
    mut last: Local<Option<NodeId>>,
) {
    let Some(control) = control else {
        return;
    };
    if animating.is_some() || !fades.is_empty() || gesture.active() {
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

/// 読む係が中に入らず飛ばす隔離フォルダ（データのフォルダの `trash/`。Mac の firmlink の側も含める）。
fn quarantine_skip_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut push = |base: PathBuf| {
        let trash = base.join("trash");
        if !dirs.contains(&trash) {
            dirs.push(trash.clone());
        }
        #[cfg(target_os = "macos")]
        if let Ok(rel) = trash.strip_prefix("/") {
            let data_vol = PathBuf::from("/System/Volumes/Data").join(rel);
            if !dirs.contains(&data_vol) {
                dirs.push(data_vol);
            }
        }
    };
    if let Some(dir) = data_dir() {
        push(dir);
    }
    if let Some(proj) = directories::ProjectDirs::from("", "", "Lisa") {
        push(proj.data_dir().to_path_buf());
    }
    dirs
}

/// 直近の手動更新の結果。メニューや通知から世代で照合できる。
#[derive(Resource, Default)]
pub(crate) struct RefreshStatus {
    pub last: Option<RefreshCompletion>,
    disk: HashMap<u64, Mutex<Receiver<bool>>>,
    awaiting_disk: Vec<(RefreshCompletion, Mutex<Receiver<bool>>)>,
}

impl RefreshStatus {
    fn publish(&mut self, completion: RefreshCompletion) {
        if let Some(error) = &completion.error {
            warn!(
                "手動更新が未完了（{}、世代 {}）: {error}",
                completion.target.path.display(),
                completion.generation
            );
        }
        if self
            .last
            .as_ref()
            .is_none_or(|last| last.generation < completion.generation)
        {
            self.last = Some(completion);
        }
    }

    fn receive_disk(&mut self) {
        let mut pending = Vec::new();
        for (mut completion, mut result) in std::mem::take(&mut self.awaiting_disk) {
            match result.get_mut().expect("ファイル画像の更新結果").try_recv() {
                Ok(true) => self.publish(completion),
                Err(std::sync::mpsc::TryRecvError::Empty) => pending.push((completion, result)),
                _ => {
                    let disk_error =
                        "ファイル画像のディスク更新が未完了です。更新を再実行できます。";
                    completion.error = Some(match completion.error {
                        Some(error) => format!("{error}\n{disk_error}"),
                        None => disk_error.into(),
                    });
                    self.publish(completion);
                }
            }
        }
        self.awaiting_disk = pending;
    }
}

pub(crate) struct RefreshCompletion {
    pub generation: u64,
    pub target: RefreshTarget,
    pub error: Option<String>,
}

/// 読む係の進み具合を UI から覗く。
#[derive(Resource, Clone)]
pub struct ScanProgress(pub Arc<Mutex<Progress>>);

/// 読む係から届く結果を受け取る。
#[derive(Resource)]
pub(crate) struct Scanner {
    control: Arc<worker::Control>,
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

impl Drop for Scanner {
    fn drop(&mut self) {
        self.control
            .closed
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

enum Message {
    /// フォルダを読んだ。`stored` なら DB にも書いた（見えるところに来たら出す）。
    Listed {
        path: PathBuf,
        result: Result<Listing, String>,
        stored: bool,
    },
    /// フォルダのハッシュができた（中のファイルもハッシュした）。
    Hashed {
        path: PathBuf,
    },
    RefreshStarted(RefreshRequest),
    RefreshListed(RefreshRequest),
    RefreshFolder(PathBuf),
    RefreshFinished {
        request: RefreshRequest,
        error: Option<String>,
    },
    /// 読み直したら DB と違っていた（このあと `Listed` が来る）。
    Changed {
        path: PathBuf,
        refresh_children: Option<Vec<PathBuf>>,
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
    let skip_dirs = quarantine_skip_dirs();
    let worker = worker::Worker {
        db,
        roots,
        sender,
        requests: request_receiver,
        proxy: Some((**proxy).clone()),
        progress: progress.0.clone(),
        control: control.clone(),
        dirty: false,
        hash_retry_needed: false,
        hash_stats_at: None,
        pending_visible: None,
        hints: Default::default(),
        refresh_hints: Default::default(),
        skip_dirs,
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
        control: control.clone(),
        requests: Mutex::new(requests),
        _watcher: watcher.map(Mutex::new),
    });
    commands.insert_resource(progress);
    let mut scanner = Scanner {
        control,
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

    /// `reset_children(id)` で木から外れる子孫を `folders`・`pending` から除く。
    fn remove_descendants(&mut self, tree: &mut FileTree, id: NodeId) {
        let mut stack = tree.nodes[id].children.clone();
        while let Some(child) = stack.pop() {
            if let Some(p) = &tree.nodes[child].path {
                if self.folders.get(p) == Some(&child) {
                    self.folders.remove(p);
                    tree.preview_generations.remove(p);
                }
                if self.pending.get(p) == Some(&child) {
                    self.pending.remove(p);
                }
            }
            stack.extend(tree.nodes[child].children.iter().copied());
        }
    }
}

/// 変更したフォルダから、その内容を映す祖先をドライブまでたどる。
fn preview_ancestors<'a>(path: &'a Path, mount: &'a Path) -> impl Iterator<Item = &'a Path> {
    path.ancestors().take_while(move |p| p.starts_with(mount))
}

/// 届いた結果を受け取る。DB に書いたものは、木にあれば「読み終えた」にするだけ（子は見えるところに
/// 来たら出す）。DB に書いていないものは、そのまま木に足す。
///
/// ピンチ・パン・クリックのアニメーション・Fade の最中は受け取りを保留し、操作が止まってから
/// 木に当てて `sync_view` でそろえる（操作の途中で指の動きが切れないようにする）。
#[allow(clippy::too_many_arguments)]
fn receive(
    scanner: Option<ResMut<Scanner>>,
    mut tree: ResMut<FileTree>,
    mut changed: ResMut<TreeChanged>,
    nav: Res<crate::view::Nav>,
    (location, store, cache, occluded): (
        Res<crate::view::Location>,
        Res<Store>,
        Res<crate::render::cache::ImageCache>,
        Option<Res<crate::view::WindowOccludedState>>,
    ),
    layouts: Res<Layouts>,
    (mut sync, mut dups, mut previews, mut images, mut capture, mut refresh_status): (
        ResMut<crate::view::SyncRequest>,
        ResMut<crate::dups::Dups>,
        ResMut<crate::preview::PreviewRegistry>,
        ResMut<crate::render::cache::FolderImages>,
        ResMut<crate::render::cache::ThumbnailCapture>,
        ResMut<RefreshStatus>,
    ),
    mut redraw: MessageWriter<bevy::window::RequestRedraw>,
    animating: Option<Res<crate::view::Animating>>,
    fades: Query<(), With<crate::view::Fade>>,
    gesture: Res<crate::input::gesture::Gesture>,
) {
    let Some(mut scanner) = scanner else {
        return;
    };
    if animating.is_some() || !fades.is_empty() || gesture.active() {
        return;
    }
    let scanner = &mut *scanner;
    refresh_status.receive_disk();
    let messages: Vec<Message> = scanner
        .results
        .get_mut()
        .expect("結果")
        .try_iter()
        .take(APPLY_PER_FRAME)
        .collect();
    let mut any = false;
    let mut structure_reset = false;
    let mut invalidated = HashSet::new();
    let collect_refresh_invalidations =
        |target: &RefreshTarget,
         folders: &HashMap<PathBuf, NodeId>,
         include_children: bool,
         invalidated: &mut HashSet<PathBuf>| {
            if let Some(mount) = store
                .roots
                .values()
                .find_map(|(id, mount)| (id.as_ref() == Some(&target.drive_id)).then_some(mount))
            {
                let first = if target.subtree {
                    Some(target.path.as_path())
                } else {
                    target.path.parent()
                };
                if let Some(first) = first {
                    invalidated.extend(preview_ancestors(first, mount).map(Path::to_path_buf));
                }
                if include_children && target.subtree {
                    invalidated.extend(
                        folders
                            .keys()
                            .filter(|path| path.parent() == Some(target.path.as_path()))
                            .cloned(),
                    );
                }
            }
        };
    for message in messages {
        match message {
            Message::RefreshStarted(request) => {
                if let Some((created, _)) = &request.created {
                    invalidated.insert(created.clone());
                    collect_refresh_invalidations(
                        &request.target,
                        &scanner.folders,
                        false,
                        &mut invalidated,
                    );
                }
                sync.request_redraw();
            }
            Message::RefreshListed(request) => {
                if let Some(result) = previews.images.invalidate(&request.target) {
                    refresh_status
                        .disk
                        .insert(request.generation, Mutex::new(result));
                }
                collect_refresh_invalidations(
                    &request.target,
                    &scanner.folders,
                    true,
                    &mut invalidated,
                );
                let folder = crate::view::scene_path(&nav, &tree)
                    .last()
                    .map(|l| l.node)
                    .unwrap_or(FileTree::ROOT);
                if request.target.subtree
                    && request.target.path.parent() == tree.nodes[folder].path.as_deref()
                {
                    capture.refresh_visible_child(request.target.path);
                }
                sync.request_redraw();
            }
            Message::RefreshFolder(path) => {
                invalidated.insert(path);
            }
            Message::RefreshFinished { request, error } => {
                if request.created.is_none() {
                    let completion = RefreshCompletion {
                        generation: request.generation,
                        target: request.target,
                        error,
                    };
                    if let Some(result) = refresh_status.disk.remove(&completion.generation) {
                        refresh_status.awaiting_disk.push((completion, result));
                    } else {
                        refresh_status.publish(completion);
                    }
                }
                if dups.scope == crate::dups::remote::Scope::default() {
                    dups.invalidate();
                }
            }
            Message::Committed => {
                scanner.recent.clear();
                scanner.hashed.clear();
                if dups.refresh_after_scan {
                    dups.refresh_after_scan = false;
                    if dups.scope == crate::dups::remote::Scope::default() {
                        dups.invalidate();
                    }
                }
                if let crate::view::Location::Renamed(path) = &*location
                    && (store.entry_chain_at(path).is_some()
                        || std::fs::symlink_metadata(path).is_err())
                {
                    structure_reset = true;
                }
            }
            Message::Hashed { path } => {
                scanner.stale.remove(&path);
                if let Some(&id) = scanner.folders.get(&path)
                    && tree.is_attached(id)
                {
                    tree.mark_hashed(id);
                    any = true;
                }
                scanner.hashed.insert(path);
            }
            Message::Changed {
                path,
                refresh_children,
            } => {
                if let Some(children) = refresh_children {
                    invalidated.extend(children);
                    invalidated.extend(
                        scanner
                            .folders
                            .keys()
                            .filter(|child| child.parent() == Some(path.as_path()))
                            .cloned(),
                    );
                }
                if let Some(mount) = store
                    .roots
                    .values()
                    .map(|(_, p)| p)
                    .filter(|mount| path.starts_with(mount))
                    .max_by_key(|mount| mount.components().count())
                {
                    invalidated.extend(preview_ancestors(&path, mount).map(Path::to_path_buf));
                }
                // 木にあれば、子を外して出し直す（このあと届く `Listed` から）。ハッシュができるまで古い。
                if let Some(&id) = scanner.folders.get(&path)
                    && tree.is_attached(id)
                {
                    scanner.remove_descendants(&mut tree, id);
                    tree.reset_children(id);
                    tree.mark_stale(id);
                    any = true;
                    if !tree.is_attached(nav.top().node) || !tree.is_attached(layouts.anchor) {
                        structure_reset = true;
                    }
                }
                scanner.hashed.remove(&path);
                scanner.stale.insert(path);
            }
            Message::Listed {
                path,
                result,
                stored: false,
            } => {
                if let Some(id) = scanner.pending.remove(&path)
                    && tree.is_attached(id)
                {
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
                if let Some(id) = scanner.pending.remove(&path)
                    && tree.is_attached(id)
                {
                    tree.mark_listed(id);
                    any = true;
                }
                scanner.recent.insert(path, result);
            }
        }
    }
    if !invalidated.is_empty() {
        debug!("サムネを無効化したフォルダ: {} 個", invalidated.len());
        tree.preview_revision += 1;
        let generation = tree.preview_revision;
        for path in invalidated {
            cache.invalidate_local(&path);
            images.invalidate_local(&path);
            capture.invalidate_local(&path);
            if scanner.folders.contains_key(&path) {
                tree.preview_generations.insert(path, generation);
            }
        }
    }
    refresh_status.receive_disk();
    if !refresh_status.awaiting_disk.is_empty() && !occluded.is_some_and(|o| o.0) {
        redraw.write(bevy::window::RequestRedraw);
    }
    if structure_reset {
        sync.request_reset();
    }
    if any {
        changed.pending = true;
    }
}

/// 並べてあるフォルダのうち、読み終えているのに子をまだ出していないものを、DB（か、まだ確定して
/// いない結果）から 1 段出す。操作中は保留して、止まってから出す。
#[allow(clippy::too_many_arguments)]
fn load_visible(
    scanner: Option<ResMut<Scanner>>,
    store: Res<Store>,
    layouts: Res<Layouts>,
    mut tree: ResMut<FileTree>,
    mut changed: ResMut<TreeChanged>,
    animating: Option<Res<crate::view::Animating>>,
    fades: Query<(), With<crate::view::Fade>>,
    gesture: Res<crate::input::gesture::Gesture>,
    mut dirty: Local<bool>,
) {
    let Some(mut scanner) = scanner else {
        return;
    };
    if layouts.is_changed() || tree.is_changed() {
        *dirty = true;
    }
    if !*dirty || animating.is_some() || !fades.is_empty() || gesture.active() {
        return;
    }
    *dirty = false;
    let scanner = &mut *scanner;
    let wanted: Vec<NodeId> = layouts
        .ids()
        .filter(|&id| {
            let node = &tree.nodes[id];
            node.is_dir
                && node.listed
                && !node.loaded
                && node.path.is_some()
                && tree.is_attached(id)
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
    if tree.in_marker_view(id) {
        if tree.marker_view_read_only(id) {
            return false;
        }
        let Some(rows) = db
            .zip(tree.nodes[id].entry)
            .and_then(|(db, entry)| db.children(entry).ok())
        else {
            return false;
        };
        let (recent, hashed) = (&scanner.recent, &scanner.hashed);
        tree.add_stored_children(id, rows, |p| recent.contains_key(p), |p| hashed.contains(p));
        return true;
    }
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

/// 行きたい場所（DB の `entry.id` の列。ドライブの一番上から）。`resolve_jump` が
/// 正の場所（`Location::Local`）へ移して `sync_view` で開く。
#[derive(Resource)]
pub struct JumpTo(pub Vec<i64>);

/// `JumpTo` を正の場所 `Location::Local` へ移し、`sync_view` にリセット同期を頼む。
/// 操作中（ピンチ・アニメーション・Fade）は保留し、止まってから移る。
fn resolve_jump(
    mut commands: Commands,
    jump: Option<Res<JumpTo>>,
    mut location: ResMut<crate::view::Location>,
    mut sync: ResMut<crate::view::SyncRequest>,
    animating: Option<Res<crate::view::Animating>>,
    fades: Query<(), With<crate::view::Fade>>,
    gesture: Res<crate::input::gesture::Gesture>,
) {
    let Some(jump) = jump else {
        return;
    };
    if animating.is_some() || !fades.is_empty() || gesture.active() {
        return;
    }
    let chain = jump.0.clone();
    commands.remove_resource::<JumpTo>();
    if chain.is_empty() {
        return;
    }
    *location = crate::view::Location::Local(chain);
    sync.request_reset();
}

/// 正の場所（`Location`）を現在の木の上の道のり（Root から）へ解決する。
/// 足りない子は DB から 1 段ずつ出し、途中が無ければたどれた最深のノードまで返す。
/// 戻り値は常に `FileTree::ROOT` から始まる、木につながった道のり。
pub(crate) fn resolve_location(
    tree: &mut FileTree,
    store: Option<&Store>,
    mut scanner: Option<&mut Scanner>,
    location: &mut crate::view::Location,
) -> Vec<NodeId> {
    if let crate::view::Location::Renamed(path) = location {
        let chain = store.and_then(|store| store.entry_chain_at(path));
        if let Some(chain) = chain {
            *location = crate::view::Location::Local(chain);
        } else {
            // 親だけ先に読み終える場合がある。開ける地点まで表示し、次の確定後に再試行する。
            let fallback = store.and_then(|store| {
                path.ancestors()
                    .skip(1)
                    .find_map(|p| store.entry_chain_at(p))
            });
            if std::fs::symlink_metadata(path).is_err() {
                *location = crate::view::Location::root();
            }
            return fallback.map_or_else(
                || vec![FileTree::ROOT],
                |chain| {
                    resolve_location(
                        tree,
                        store,
                        scanner,
                        &mut crate::view::Location::Local(chain),
                    )
                },
            );
        }
    }
    match location {
        crate::view::Location::Local(chain) => {
            // CLI や MCP から動かした開いているフォルダも、維持された entry.id で追う。
            let moved = if let Some(&entry) = chain.last()
                && let Some(current) = store.and_then(|store| store.entry_chain_for(entry))
                && current != *chain
            {
                *chain = current;
                true
            } else {
                false
            };
            let Some(&first_entry) = chain.first() else {
                return vec![FileTree::ROOT];
            };
            let Some(mut node) = tree.node_by_entry(first_entry) else {
                warn!("行き先のドライブが木にありません（Root を開きます）");
                return vec![FileTree::ROOT];
            };
            let mut reader = store.and_then(|s| s.reader.as_ref()?.lock().ok());
            for &entry in &chain[1..] {
                // 移動元の通知が先に届いても、移動先の既読一覧を DB から出し直す。
                if moved
                    && tree.nodes[node].loaded
                    && !tree.nodes[node]
                        .children
                        .iter()
                        .any(|&child| tree.nodes[child].entry == Some(entry))
                    && let Some(sc) = scanner.as_deref_mut()
                {
                    sc.remove_descendants(tree, node);
                    if let Some(path) = &tree.nodes[node].path {
                        sc.recent.remove(path);
                    }
                    tree.reset_children(node);
                }
                if !tree.nodes[node].loaded
                    && let (Some(store), Some(sc)) = (store, scanner.as_deref_mut())
                    && !load_children(tree, store, reader.as_deref_mut(), sc, node)
                {
                    warn!("行き先の途中を DB から出せませんでした（たどれたところまで開きます）");
                    break;
                }
                let by_entry = tree.nodes[node]
                    .children
                    .iter()
                    .copied()
                    .find(|&c| tree.nodes[c].entry == Some(entry));
                let child = match by_entry {
                    Some(c) => Some(c),
                    None => {
                        // 未確定結果（`recent`）から出した直後は `entry` が無いので、DB の名前でも照合する。
                        let want_name = reader
                            .as_deref()
                            .and_then(|db| lisa_core::query::item(db, entry).ok().flatten())
                            .map(|item| item.name);
                        want_name.and_then(|name| {
                            tree.nodes[node]
                                .children
                                .iter()
                                .copied()
                                .find(|&c| tree.nodes[c].name == name)
                        })
                    }
                };
                let Some(child) = child else {
                    warn!("行き先が木にありません（たどれたところまで開きます）");
                    break;
                };
                node = child;
            }
            tree.path_from_root(node)
                .unwrap_or_else(|| vec![FileTree::ROOT])
        }
        crate::view::Location::Renamed(_) => unreachable!("上で実パスを解決済み"),
        crate::view::Location::Attached(steps) => {
            crate::model::display_path::resolve_steps(tree, steps)
        }
        crate::view::Location::Named(names) => {
            let mut node = FileTree::ROOT;
            let mut reader = store.and_then(|s| s.reader.as_ref()?.lock().ok());
            for name in names.iter().skip(1) {
                if !tree.nodes[node].loaded
                    && let (Some(store), Some(sc)) = (store, scanner.as_deref_mut())
                {
                    let _ = load_children(tree, store, reader.as_deref_mut(), sc, node);
                }
                let Some(child) = tree.nodes[node]
                    .children
                    .iter()
                    .copied()
                    .find(|&c| tree.nodes[c].name == *name)
                else {
                    break;
                };
                node = child;
            }
            tree.path_from_root(node)
                .unwrap_or_else(|| vec![FileTree::ROOT])
        }
    }
}

/// ノード `id` があるドライブの（DB の ID、つないでいる場所）。DB に残さないドライブなら None。
pub(crate) fn drive_of(tree: &FileTree, store: &Store, id: NodeId) -> Option<(String, PathBuf)> {
    let mut cur = Some(id);
    while let Some(i) = cur {
        if let Some((drive_id, mount)) = store.roots.get(&i) {
            return drive_id.clone().map(|d| (d, mount.clone()));
        }
        cur = tree.nodes[i].parent;
    }
    None
}

/// 手元のファイルかフォルダの、DB にある行（ツールチップのハッシュ用）。DB が無い、まだ読んでいないなら None。
pub(crate) fn stored_item(store: &Store, path: &Path) -> Option<lisa_core::query::Item> {
    let db = store.reader.as_ref()?.lock().ok()?;
    let located = lisa_core::query::locate(&db, path).ok()??;
    let id = *located.chain.last()?;
    lisa_core::query::item(&db, id).ok()?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 全走査で閉じたフォルダの画像を要求しない() {
        let temp = crate::preview::TestTemp::new();
        let mount = temp.0.join("volume");
        let folder = mount.join("closed");
        std::fs::create_dir_all(&folder).unwrap();
        image::RgbImage::new(16, 8)
            .save(folder.join("sample.png"))
            .unwrap();
        let volume = lisa_core::volumes::Volume {
            id: Some("test:full-scan".into()),
            mount_path: mount.clone(),
            ..root_volume()
        };
        let mut db = Db::open(&temp.0.join("test.db")).unwrap();
        let device = db.this_device("test", "test", "test").unwrap();
        db.upsert_drive(&device, &volume).unwrap();
        let drive_id = volume.id.clone();
        let tree = FileTree::this_device("test", &[volume]);
        let drive = tree.nodes[FileTree::ROOT + 1].children[0];
        let (requests, request_rx) = channel();
        let (sender, results) = channel();
        let control = Arc::new(worker::Control::default());
        control
            .rest_on_battery
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let progress = Arc::new(Mutex::new(Progress::default()));
        let worker = worker::Worker {
            db: Some(db),
            roots: vec![Root {
                node: drive,
                drive_id: drive_id.clone(),
                mount: mount.clone(),
                mount_identity: lisa_core::volumes::mount_identity(&mount),
                listed: false,
            }],
            sender,
            requests: request_rx,
            proxy: None,
            progress: progress.clone(),
            control: control.clone(),
            dirty: false,
            hash_retry_needed: false,
            hash_stats_at: None,
            pending_visible: None,
            hints: Default::default(),
            refresh_hints: Default::default(),
            skip_dirs: vec![],
        };
        let mut scanner = Scanner {
            control,
            results: Mutex::new(results),
            pending: HashMap::new(),
            recent: HashMap::new(),
            folders: HashMap::new(),
            hashed: HashSet::new(),
            stale: HashSet::new(),
        };
        scanner.track(&tree, drive);
        let mut world = World::new();
        let mut previews = crate::preview::PreviewRegistry::default();
        crate::preview::cartridges::register(&mut previews);
        world.insert_resource(crate::layout::layout(
            &tree,
            FileTree::ROOT,
            crate::layout::anchor_body(),
            crate::layout::EarSpec::new(&crate::settings::Settings::default()),
        ));
        world.insert_resource(tree);
        world.insert_resource(scanner);
        world.insert_resource(previews);
        world.insert_resource(crate::view::Nav {
            stack: vec![crate::view::Level {
                node: FileTree::ROOT,
            }],
        });
        world.insert_resource(crate::view::Location::root());
        world.insert_resource(Store {
            reader: None,
            roots: HashMap::from([(drive, (drive_id, mount))]),
        });
        world.insert_resource(crate::render::cache::ImageCache::with_dir(None));
        world.init_resource::<TreeChanged>();
        world.init_resource::<RefreshStatus>();
        world.init_resource::<crate::view::SyncRequest>();
        world.init_resource::<crate::dups::Dups>();
        world.init_resource::<crate::render::cache::FolderImages>();
        world.init_resource::<crate::render::cache::ThumbnailCapture>();
        world.init_resource::<crate::input::gesture::Gesture>();
        world.init_resource::<Assets<Image>>();
        world.init_resource::<bevy::ecs::message::Messages<bevy::window::RequestRedraw>>();
        let mut schedule = Schedule::default();
        schedule.add_systems((receive, crate::preview::update_previews).chain());
        schedule.run(&mut world);
        drop(requests);
        worker.run();
        schedule.run(&mut world);
        assert_eq!(progress.lock().unwrap().done, 2);
        assert_eq!(progress.lock().unwrap().hash.files, 1);
        assert!(progress.lock().unwrap().hash.finished.is_some());
        assert_eq!(
            world
                .resource::<crate::preview::PreviewRegistry>()
                .images
                .requests_issued(),
            0
        );
    }

    #[test]
    fn 手動更新は_refresh_listed_までフォルダ画像を失効させない() {
        let temp = crate::preview::TestTemp::new();
        let mount = temp.0.join("volume");
        let folder_path = mount.join("sub");
        std::fs::create_dir_all(&folder_path).unwrap();
        let file_path = folder_path.join("a.png");
        let volume = lisa_core::volumes::Volume {
            id: Some("test:order".into()),
            mount_path: mount.clone(),
            ..root_volume()
        };
        let drive_id = volume.id.clone();
        let mut tree = FileTree::this_device("test", &[volume]);
        let drive = tree.nodes[FileTree::ROOT + 1].children[0];
        tree.add_listing(
            drive,
            listing::read_dir(&mount).map_err(|error| error.to_string()),
        );
        let folder = tree.nodes[drive].children[0];
        let (sender, results) = channel();
        let mut scanner = Scanner {
            control: Arc::new(worker::Control::default()),
            results: Mutex::new(results),
            pending: HashMap::new(),
            recent: HashMap::new(),
            folders: HashMap::new(),
            hashed: HashSet::new(),
            stale: HashSet::new(),
        };
        scanner.track(&tree, drive);
        scanner.track(&tree, folder);

        let mut world = World::new();
        let mut previews = crate::preview::PreviewRegistry::default();
        crate::preview::cartridges::register(&mut previews);
        world.insert_resource(crate::layout::layout(
            &tree,
            FileTree::ROOT,
            crate::layout::anchor_body(),
            crate::layout::EarSpec::new(&crate::settings::Settings::default()),
        ));
        world.insert_resource(tree);
        world.insert_resource(scanner);
        world.insert_resource(previews);
        world.insert_resource(crate::view::Nav {
            stack: vec![
                crate::view::Level {
                    node: FileTree::ROOT,
                },
                crate::view::Level { node: folder },
            ],
        });
        world.insert_resource(crate::view::Location::root());
        world.insert_resource(Store {
            reader: None,
            roots: HashMap::from([(drive, (drive_id, mount.clone()))]),
        });
        world.insert_resource(crate::render::cache::ImageCache::with_dir(None));
        world.init_resource::<TreeChanged>();
        world.init_resource::<RefreshStatus>();
        world.init_resource::<crate::view::SyncRequest>();
        world.init_resource::<crate::dups::Dups>();
        world.init_resource::<crate::render::cache::FolderImages>();
        world.init_resource::<crate::render::cache::ThumbnailCapture>();
        world.init_resource::<crate::input::gesture::Gesture>();
        world.init_resource::<bevy::ecs::message::Messages<bevy::window::RequestRedraw>>();
        let mut schedule = Schedule::default();
        schedule.add_systems(receive);

        let request = RefreshRequest {
            generation: 1,
            target: RefreshTarget {
                drive_id: "test:order".into(),
                path: file_path,
                subtree: false,
            },
            created: None,
        };
        sender
            .send(Message::RefreshStarted(request.clone()))
            .unwrap();
        schedule.run(&mut world);
        {
            let tree = world.resource::<FileTree>();
            assert_eq!(
                tree.preview_revision, 0,
                "RefreshStarted だけではフォルダ画像を失効させない"
            );
            assert!(!tree.preview_generations.contains_key(&folder_path));
        }

        sender.send(Message::RefreshListed(request)).unwrap();
        schedule.run(&mut world);
        {
            let tree = world.resource::<FileTree>();
            assert_eq!(tree.preview_revision, 1);
            assert_eq!(tree.preview_generations.get(&folder_path), Some(&1));
            assert_eq!(tree.preview_generations.get(&mount), Some(&1));
        }
    }

    #[test]
    fn ディスク失効待ちのあいだ再描画を要求し完了後に公開する() {
        let mount = std::env::temp_dir().join(format!("lisa-await-disk-{}", std::process::id()));
        let volume = lisa_core::volumes::Volume {
            id: Some("test:await".into()),
            mount_path: mount.clone(),
            ..root_volume()
        };
        let drive_id = volume.id.clone();
        let tree = FileTree::this_device("test", &[volume]);
        let drive = tree.nodes[FileTree::ROOT + 1].children[0];
        let (sender, results) = channel();
        let scanner = Scanner {
            control: Arc::new(worker::Control::default()),
            results: Mutex::new(results),
            pending: HashMap::new(),
            recent: HashMap::new(),
            folders: HashMap::new(),
            hashed: HashSet::new(),
            stale: HashSet::new(),
        };
        let mut world = World::new();
        world.insert_resource(crate::layout::layout(
            &tree,
            FileTree::ROOT,
            crate::layout::anchor_body(),
            crate::layout::EarSpec::new(&crate::settings::Settings::default()),
        ));
        world.insert_resource(tree);
        world.insert_resource(scanner);
        world.init_resource::<crate::preview::PreviewRegistry>();
        world.insert_resource(crate::view::Nav {
            stack: vec![crate::view::Level {
                node: FileTree::ROOT,
            }],
        });
        world.insert_resource(crate::view::Location::root());
        world.insert_resource(Store {
            reader: None,
            roots: HashMap::from([(drive, (drive_id, mount.clone()))]),
        });
        world.insert_resource(crate::render::cache::ImageCache::with_dir(None));
        world.init_resource::<TreeChanged>();
        let (disk_tx, disk_rx) = channel();
        let mut status = RefreshStatus::default();
        status.disk.insert(7, Mutex::new(disk_rx));
        world.insert_resource(status);
        world.init_resource::<crate::view::SyncRequest>();
        world.init_resource::<crate::dups::Dups>();
        world.init_resource::<crate::render::cache::FolderImages>();
        world.init_resource::<crate::render::cache::ThumbnailCapture>();
        world.init_resource::<crate::input::gesture::Gesture>();
        world.init_resource::<crate::view::WindowOccludedState>();
        world.init_resource::<bevy::ecs::message::Messages<bevy::window::RequestRedraw>>();
        let mut schedule = Schedule::default();
        schedule.add_systems(receive);

        let request = RefreshRequest {
            generation: 7,
            target: RefreshTarget {
                drive_id: "test:await".into(),
                path: mount.join("a.png"),
                subtree: false,
            },
            created: None,
        };
        sender
            .send(Message::RefreshFinished {
                request,
                error: None,
            })
            .unwrap();
        schedule.run(&mut world);
        assert!(world.resource::<RefreshStatus>().last.is_none());
        assert!(!world.resource::<RefreshStatus>().awaiting_disk.is_empty());
        assert!(
            !world
                .resource::<bevy::ecs::message::Messages<bevy::window::RequestRedraw>>()
                .is_empty(),
            "awaiting_disk が残っているあいだ再描画を要求する"
        );

        world
            .resource_mut::<bevy::ecs::message::Messages<bevy::window::RequestRedraw>>()
            .clear();
        disk_tx.send(true).unwrap();
        schedule.run(&mut world);
        let status = world.resource::<RefreshStatus>();
        assert!(status.awaiting_disk.is_empty());
        let last = status.last.as_ref().expect("ディスク完了後に公開される");
        assert_eq!(last.generation, 7);
        assert!(last.error.is_none());
        assert!(
            world
                .resource::<bevy::ecs::message::Messages<bevy::window::RequestRedraw>>()
                .is_empty()
        );
    }

    #[test]
    fn preview_invalidation_stops_at_each_drive_and_excludes_siblings() {
        let base = std::env::temp_dir();
        let a = base.join("drive-a");
        let b = base.join("drive-b");
        let from = a.join("parent/from");
        let to = b.join("to");
        let affected: HashSet<_> = preview_ancestors(&from, &a)
            .chain(preview_ancestors(&to, &b))
            .map(Path::to_path_buf)
            .collect();
        assert_eq!(affected, HashSet::from([from, a.join("parent"), a, to, b]));
    }

    #[test]
    fn rename_location_waits_for_descendant_and_resolves_raw_name() {
        use crate::view::Location;

        let mount =
            std::env::temp_dir().join(format!("lisa-rename-location-{}", std::process::id()));
        let folder = mount.join("Numéro");
        let target = folder.join("child");
        let destination = mount.join("destination");
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        let volume = lisa_core::volumes::Volume {
            id: Some("test:rename".into()),
            mount_path: mount.clone(),
            ..root_volume()
        };
        let mut db = Db::open_in_memory().unwrap();
        let device = db.this_device("端末", "host", "macos").unwrap();
        db.upsert_drive(&device, &volume).unwrap();
        let mut tree = FileTree::this_device("端末", &[volume]);
        let drive = tree.nodes[tree.nodes[FileTree::ROOT].children[0]].children[0];
        let drive_id = "test:rename";
        let save = |db: &mut Db, path: &Path| {
            let result = listing::read_dir(path).map_err(|e| e.to_string());
            db.save_listing(drive_id, &mount, path, &result).unwrap();
        };
        save(&mut db, &mount);
        tree.set_drive_row(drive, &db.drive_root(drive_id).unwrap().unwrap());

        let store = Store {
            reader: Some(Mutex::new(db)),
            roots: HashMap::from([(drive, (Some(drive_id.into()), mount.clone()))]),
        };
        let (_sender, receiver) = channel();
        let mut scanner = Scanner {
            control: Arc::new(worker::Control::default()),
            results: Mutex::new(receiver),
            pending: HashMap::new(),
            recent: HashMap::new(),
            folders: HashMap::new(),
            hashed: HashSet::new(),
            stale: HashSet::new(),
        };
        let mut location = Location::Renamed(target.clone());
        let resolved = resolve_location(&mut tree, Some(&store), Some(&mut scanner), &mut location);
        assert_eq!(tree.nodes[*resolved.last().unwrap()].name, "Numéro");
        assert_eq!(location, Location::Renamed(target.clone()));

        save(&mut store.reader.as_ref().unwrap().lock().unwrap(), &folder);
        let resolved = resolve_location(&mut tree, Some(&store), Some(&mut scanner), &mut location);
        assert_eq!(
            tree.nodes[*resolved.last().unwrap()].path.as_ref(),
            Some(&target)
        );
        assert!(matches!(location, Location::Local(_)));
        // 古い移動先の一覧がまだ表示されていても、移動元からの通知だけで追従できる。
        let mut destination_location = Location::Local(store.entry_chain_at(&destination).unwrap());
        let destination_nodes = resolve_location(
            &mut tree,
            Some(&store),
            Some(&mut scanner),
            &mut destination_location,
        );
        let destination_node = *destination_nodes.last().unwrap();
        let moved = destination.join("child");
        {
            let mut db = store.reader.as_ref().unwrap().lock().unwrap();
            save(&mut db, &destination);
            let _ = load_children(
                &mut tree,
                &store,
                Some(&mut db),
                &mut scanner,
                destination_node,
            );
            assert!(tree.nodes[destination_node].loaded);
            let ctx = lisa_core::organize::Context::with_builtin(mount.join("organize-state"));
            let plan = lisa_core::organize::create_plan(
                &mut db,
                &ctx,
                "test",
                None,
                &[lisa_core::organize::OpInput::Move {
                    from: target,
                    to: moved.clone(),
                }],
                false,
            )
            .unwrap()
            .id
            .unwrap();
            lisa_core::organize::approve_plan(&mut db, plan, false).unwrap();
            assert_eq!(
                lisa_core::organize::execute_plan(&mut db, &ctx, plan)
                    .unwrap()
                    .done,
                1
            );
        }
        let resolved = resolve_location(&mut tree, Some(&store), Some(&mut scanner), &mut location);
        assert_eq!(
            tree.nodes[*resolved.last().unwrap()].path.as_ref(),
            Some(&moved)
        );

        std::fs::remove_dir_all(mount).unwrap();
    }

    #[test]
    fn noisy_dirs_match_ignoring_case_and_separators() {
        let dirs = vec![
            fold_path(std::path::Path::new("C:\\Windows")),
            fold_path(std::path::Path::new("C:/Users/nozomi/AppData")),
        ];
        assert!(is_under_any(
            std::path::Path::new("c:\\windows\\Prefetch"),
            &dirs
        ));
        assert!(is_under_any(std::path::Path::new("C:\\WINDOWS"), &dirs));
        assert!(is_under_any(
            std::path::Path::new("C:\\Users\\nozomi\\AppData\\Local\\x"),
            &dirs
        ));
        assert!(!is_under_any(
            std::path::Path::new("C:\\WindowsApps2"),
            &dirs
        ));
        assert!(!is_under_any(
            std::path::Path::new("C:\\Users\\nozomi\\Documents"),
            &dirs
        ));
        assert!(!is_under_any(std::path::Path::new("C:\\Windows"), &[]));
    }
}
