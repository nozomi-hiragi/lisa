//! ファイルのプレビューを「カートリッジ」として差し替え・追加できる仕組み。

pub mod cartridges;
mod disk;
#[cfg(test)]
pub(crate) use disk::tests::Temp as TestTemp;

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use bevy::asset::RenderAssetUsages;
use bevy::math::Rect;
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};

use crate::render::palette;
use crate::render::shapes::{Painter, over};

/// カートリッジが紙の内側を描くための材料。
pub struct DrawContext<'a, 'b, 'w, 's> {
    pub painter: &'a mut Painter<'b, 'w, 's>,
    pub name: &'a str,
    /// 紙の内側の四角。
    pub inner: Rect,
    /// 紙の色（背景に重ねた結果）。
    pub page: Color,
    pub z: f32,
    /// 裏で読めたもの（まだ・読めないときは None）。
    pub loaded: Option<Preview>,
}

/// 読めて、描くときに引けるプレビュー。
#[derive(Clone)]
pub enum Preview {
    /// 絵と、その縦横。
    Image(Handle<Image>, UVec2),
    /// テキストの先頭の行。
    Text(Arc<Vec<String>>),
}

/// カートリッジが裏で読んだもの。
pub enum LoadedPreview {
    /// 小さな RGBA の絵。
    Image {
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
    /// テキストの先頭の行。
    Text { lines: Vec<String> },
}

impl LoadedPreview {
    /// RGBA の絵の縦横（テキストなら None）。
    #[cfg(test)]
    pub fn size(&self) -> Option<(u32, u32)> {
        match self {
            Self::Image { width, height, .. } => Some((*width, *height)),
            Self::Text { .. } => None,
        }
    }
}

/// ファイルのプレビューを描くカートリッジ。
pub trait Cartridge: Send + Sync + 'static {
    /// カートリッジの識別子（例: `"image"`）。
    fn id(&self) -> &'static str;
    /// 担当する拡張子（小文字）。空なら受け皿（どれにも合わないファイル）。
    fn extensions(&self) -> &'static [&'static str];
    /// 同じ拡張子に複数あるときの優先度（高い方が選ばれる）。
    fn priority(&self) -> i32 {
        0
    }
    /// この名前のファイルを裏で読むカートリッジか。
    fn can_load(&self, _name: &str) -> bool {
        false
    }
    /// ファイルの中身から絵を作る（裏のスレッドで呼ぶ）。既定は読まない。
    fn load(&self, _path: &Path) -> Option<LoadedPreview> {
        None
    }
    /// 紙の形を縦 A4 にする。
    fn portrait_a4(&self) -> bool {
        false
    }
    /// 紙の内側を描く。
    fn draw(&self, ctx: &mut DrawContext<'_, '_, '_, '_>);
}

/// 受け皿のカートリッジが 1 つも登録されていないときの組み込みの受け皿。
struct BuiltinFallback;

static BUILTIN_FALLBACK: BuiltinFallback = BuiltinFallback;

impl Cartridge for BuiltinFallback {
    fn id(&self) -> &'static str {
        "fallback"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &[]
    }

    fn portrait_a4(&self) -> bool {
        true
    }

    fn draw(&self, _ctx: &mut DrawContext<'_, '_, '_, '_>) {}
}

/// 登録されたカートリッジと、拡張子ごとに選ばれたカートリッジの表。
#[derive(Resource, Default)]
pub struct PreviewRegistry {
    cartridges: Vec<Arc<dyn Cartridge>>,
    /// 拡張子（小文字）→ 選ばれたカートリッジの番号。
    by_ext: HashMap<String, usize>,
    /// 受け皿（`extensions()` が空）のカートリッジの番号。
    fallback: Option<usize>,
    /// 拡張子（小文字）→ カートリッジ id の上書き（設定から入れる口）。
    overrides: HashMap<String, String>,
    /// 裏で読んだ絵の置き場。
    pub images: PreviewImages,
}

impl PreviewRegistry {
    pub(crate) fn moved(&mut self, from: &Path, to: &Path, is_dir: bool) {
        let name = to.file_name().map(|name| name.to_string_lossy());
        let cartridge = name.as_deref().map(|name| self.pick(name).id());
        self.images
            .moved_with_cartridge(from, to, is_dir, cartridge);
    }

    /// カートリッジを追加する。同じ拡張子なら優先度の高い方（同点なら先に追加した方）が選ばれる。
    pub fn add(&mut self, c: impl Cartridge) {
        let idx = self.cartridges.len();
        self.cartridges.push(Arc::new(c));
        let prio = self.cartridges[idx].priority();
        let exts = self.cartridges[idx].extensions();
        if exts.is_empty() {
            let replace = self
                .fallback
                .is_none_or(|prev| prio > self.cartridges[prev].priority());
            if replace {
                self.fallback = Some(idx);
            }
        } else {
            for &ext in exts {
                let key = ext.to_ascii_lowercase();
                let replace = self
                    .by_ext
                    .get(&key)
                    .is_none_or(|&prev| prio > self.cartridges[prev].priority());
                if replace {
                    self.by_ext.insert(key, idx);
                }
            }
        }
    }

    /// 特定の拡張子で使うカートリッジを id で上書きする。
    #[allow(dead_code)]
    pub fn set_override(&mut self, ext: impl Into<String>, id: impl Into<String>) {
        self.overrides
            .insert(ext.into().to_ascii_lowercase(), id.into());
    }

    /// ファイル名に合うカートリッジを選ぶ。
    /// 優先順位: 上書き指定 → 拡張子が合うもの（priority が高い方） → 受け皿 → 組み込みの受け皿。
    pub fn pick(&self, name: &str) -> &dyn Cartridge {
        self.pick_arc(name)
            .map_or(&BUILTIN_FALLBACK as &dyn Cartridge, |c| c.as_ref())
    }

    fn pick_arc(&self, name: &str) -> Option<&Arc<dyn Cartridge>> {
        let ext = extension(name).to_ascii_lowercase();
        if let Some(id) = self.overrides.get(&ext)
            && let Some(c) = self.cartridges.iter().find(|c| c.id() == id)
        {
            return Some(c);
        }
        if let Some(&idx) = self.by_ext.get(&ext) {
            return Some(&self.cartridges[idx]);
        }
        if let Some(idx) = self.fallback {
            return Some(&self.cartridges[idx]);
        }
        None
    }
}

/// 拡張子（無ければ空）。
pub fn extension(name: &str) -> &str {
    name.rsplit_once('.').map_or("", |(_, ext)| ext)
}

fn paper_frame(frame: Rect, portrait_a4: bool) -> Rect {
    if !portrait_a4 {
        return frame;
    }
    let size = Vec2::new(
        (frame.height() * std::f32::consts::FRAC_1_SQRT_2).min(frame.width()),
        frame.height(),
    );
    Rect::from_center_size(frame.center(), size)
}

fn preview_geometry(
    frame: Rect,
    portrait_a4: bool,
    loaded: Option<&Preview>,
) -> (Option<Rect>, Rect) {
    if matches!(loaded, Some(Preview::Image(_, size)) if size.x > 0 && size.y > 0) {
        return (None, frame);
    }
    let paper = paper_frame(frame, portrait_a4);
    let pad = if portrait_a4 {
        paper.size().min_element()
    } else {
        paper.height()
    } * 0.14;
    (
        Some(paper),
        Rect {
            min: paper.min + pad,
            max: paper.max - pad,
        },
    )
}

/// カートリッジの絵を描く。画像がまだ無い場合やテキストには、折れ耳の紙を敷く。
#[allow(clippy::too_many_arguments)]
pub fn draw(
    registry: &PreviewRegistry,
    painter: &mut Painter,
    name: &str,
    frame: Rect,
    background: Color,
    z: f32,
    loaded: Option<Preview>,
    simple: bool,
) {
    let cartridge = registry.pick(name);
    let portrait_a4 = cartridge.portrait_a4();
    let (paper_rect, inner) = preview_geometry(frame, portrait_a4, loaded.as_ref());
    let file_accent = palette::file_color();
    let base = over(background, palette::BASE_100, 0.80);
    let page = over(base, file_accent, 0.55);
    if let Some(paper_frame) = paper_rect {
        if simple {
            painter.rect(paper_frame, page, z);
            return;
        }
        let fold = over(base, file_accent, 0.88);
        let border = over(page, file_accent, 0.38);
        let border_w = (frame.height() * 0.015).max(painter.scale);
        let c = if portrait_a4 {
            paper_frame.size().min_element() * 0.22
        } else {
            paper_frame.height() * 0.15
        };
        painter.rect(
            Rect::new(
                paper_frame.min.x,
                paper_frame.min.y,
                paper_frame.max.x - c,
                paper_frame.max.y,
            ),
            page,
            z,
        );
        painter.rect(
            Rect::new(
                paper_frame.max.x - c,
                paper_frame.min.y,
                paper_frame.max.x,
                paper_frame.max.y - c,
            ),
            page,
            z,
        );
        painter.right_triangle(paper_frame.max - Vec2::splat(c), c, fold, z);
        let (min, max) = (paper_frame.min, paper_frame.max);
        for rect in [
            Rect::new(min.x, min.y, max.x, min.y + border_w),
            Rect::new(min.x, min.y + border_w, min.x + border_w, max.y),
            Rect::new(min.x + border_w, max.y - border_w, max.x - c, max.y),
            Rect::new(max.x - border_w, min.y + border_w, max.x, max.y - c),
        ] {
            painter.rect(rect, border, z + 0.02);
        }
    }

    let mut ctx = DrawContext {
        painter,
        name,
        inner,
        page,
        z: z + 0.05,
        loaded,
    };
    cartridge.draw(&mut ctx);
}

/// プレビューのカートリッジを登録するプラグイン。
pub struct PreviewPlugin;

impl Plugin for PreviewPlugin {
    fn build(&self, app: &mut App) {
        let dir = crate::scan::cache_dir().map(|dir| dir.join("file-previews").join("v1"));
        let disk = dir.clone().and_then(disk::Disk::open);
        let mut registry = PreviewRegistry {
            images: PreviewImages::new(disk, dir),
            ..Default::default()
        };
        cartridges::register(&mut registry);
        app.insert_resource(registry);
    }
}

/// 開いているフォルダが変わった・子のファイルが変わった（名前の変更や Undo で数が同じままのときも）
/// ときだけ、直接の子のファイルの読みを頼む。
/// 読めた絵が届いたら、場面の描き直しを 1 回頼む。
pub fn update_previews(
    mut registry: ResMut<PreviewRegistry>,
    mut assets: ResMut<Assets<Image>>,
    mut sync: ResMut<crate::view::SyncRequest>,
    tree: Res<crate::model::tree::FileTree>,
    nav: Res<crate::view::Nav>,
    mut last: Local<Option<(usize, u64, u64)>>,
) {
    if registry.images.receive(&mut assets) {
        sync.request_redraw();
    }
    let notices = registry
        .images
        .disk
        .as_ref()
        .map_or_else(Vec::new, |disk| disk.notices());
    for (path, scope) in notices {
        registry.images.clear_paths(&path, scope);
        sync.request_redraw();
    }
    let folder = crate::view::scene_path(&nav, &tree)
        .last()
        .map(|l| l.node)
        .unwrap_or(crate::model::tree::FileTree::ROOT);
    if last.is_some_and(|(f, _, revision)| f == folder && revision == registry.images.revision)
        && !tree.is_changed()
    {
        return;
    }
    let sig = (
        folder,
        files_signature(&tree, folder),
        registry.images.revision,
    );
    if *last == Some(sig) {
        return;
    }
    *last = Some(sig);
    let files = registry.folder_files(&tree, folder);
    registry.images.request(files);
}

impl PreviewRegistry {
    fn folder_files(
        &self,
        tree: &crate::model::tree::FileTree,
        folder: usize,
    ) -> Vec<(PathBuf, Arc<dyn Cartridge>)> {
        tree.nodes[folder]
            .children
            .iter()
            .filter_map(|&child| {
                let node = &tree.nodes[child];
                if node.is_dir {
                    return None;
                }
                let cartridge = self
                    .pick_arc(&node.name)
                    .filter(|c| c.can_load(&node.name))?;
                let path = crate::ui::tooltip::local_path(tree, child)?;
                Some((path, cartridge.clone()))
            })
            .collect()
    }

    /// 別のタイルから描くフォルダにも、その直下のファイル画像を用意する。
    pub(crate) fn request_capture(&mut self, tree: &crate::model::tree::FileTree, folder: usize) {
        let mut files = self.folder_files(tree, folder);
        files.truncate(MAX_IMAGES);
        self.images.request_files(files, Scope::Capture);
    }

    pub(crate) fn capture_pending(&self) -> bool {
        self.images.capture_pending
            || self.images.capture.values().any(|key| {
                self.images.requested.contains_key(key) && !self.images.order.contains(key)
            })
    }

    /// 開いたローカルフォルダの直下の指紋計算用データを集める（メインスレッドでは I/O しない）。
    pub(crate) fn folder_fingerprint_input(
        &self,
        tree: &crate::model::tree::FileTree,
        folder: usize,
    ) -> FolderFingerprintInput {
        let mut entries: Vec<_> = tree.nodes[folder]
            .children
            .iter()
            .map(|&id| {
                let node = &tree.nodes[id];
                let path = crate::ui::tooltip::local_path(tree, id);
                let raw_name = node
                    .local_name
                    .as_deref()
                    .or_else(|| path.as_deref().and_then(Path::file_name))
                    .map_or_else(
                        || node.name.as_bytes().to_vec(),
                        |name| name.as_encoded_bytes().to_vec(),
                    );
                let cartridge = if node.is_dir {
                    ""
                } else {
                    self.pick(&node.name).id()
                };
                let preview_path = if !node.is_dir && self.pick(&node.name).can_load(&node.name) {
                    path
                } else {
                    None
                };
                FingerprintEntry {
                    raw_name,
                    is_dir: node.is_dir,
                    size: node.size,
                    cartridge,
                    preview_path,
                }
            })
            .collect();
        entries.sort_unstable_by(|a, b| {
            a.raw_name
                .cmp(&b.raw_name)
                .then_with(|| a.is_dir.cmp(&b.is_dir))
        });
        FolderFingerprintInput { entries }
    }
}

pub(crate) struct FolderFingerprintInput {
    entries: Vec<FingerprintEntry>,
}

struct FingerprintEntry {
    raw_name: Vec<u8>,
    is_dir: bool,
    size: u64,
    cartridge: &'static str,
    preview_path: Option<PathBuf>,
}

impl FolderFingerprintInput {
    /// 裏スレッドで呼び、開いたローカルフォルダの直下だけの指紋を計算する。
    pub(crate) fn compute(self) -> [u8; 32] {
        let mut hash = blake3::Hasher::new();
        hash.update(b"lisa-folder-fingerprint-v1");
        for entry in self.entries {
            hash.update(&(entry.raw_name.len() as u64).to_le_bytes());
            hash.update(&entry.raw_name);
            hash.update(&[u8::from(entry.is_dir)]);
            if !entry.is_dir {
                hash.update(&entry.size.to_le_bytes());
                hash.update(&(entry.cartridge.len() as u64).to_le_bytes());
                hash.update(entry.cartridge.as_bytes());
                match entry
                    .preview_path
                    .as_deref()
                    .and_then(|path| source_key(path, entry.cartridge))
                {
                    Some(key) => {
                        hash.update(&[1]);
                        hash.update(&key.len.to_le_bytes());
                        if let Some(mtime) = key.mtime {
                            let (sign, delta) = match mtime.duration_since(SystemTime::UNIX_EPOCH) {
                                Ok(delta) => (1, delta),
                                Err(error) => (2, error.duration()),
                            };
                            hash.update(&[sign]);
                            hash.update(&delta.as_secs().to_le_bytes());
                            hash.update(&delta.subsec_nanos().to_le_bytes());
                        } else {
                            hash.update(&[0]);
                        }
                    }
                    None => {
                        hash.update(&[0]);
                    }
                }
            }
        }
        *hash.finalize().as_bytes()
    }
}

/// フォルダの直接の子のファイル（ノード・名前・大きさ）をまとめた値。読みを頼み直すかを決める。
pub(crate) fn files_signature(tree: &crate::model::tree::FileTree, folder: usize) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for &c in &tree.nodes[folder].children {
        let node = &tree.nodes[c];
        (
            c,
            &node.name,
            &node.local_name,
            node.is_dir,
            (!node.is_dir).then_some(node.size),
        )
            .hash(&mut hasher);
    }
    hasher.finish()
}

/// 読んだ絵の数の上限。超えたら古いものから捨てる。
const MAX_IMAGES: usize = 500;
/// 裏で読むスレッドの数。
const WORKERS: usize = 2;

/// 絵の鍵: ファイルのパスと大きさ、更新日時。
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct PreviewKey {
    path: PathBuf,
    len: u64,
    mtime: Option<SystemTime>,
    cartridge: &'static str,
}

fn source_key(path: &Path, cartridge: &'static str) -> Option<PreviewKey> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    // クラウドにだけあるファイルは読まない（読むとダウンロードが始まる）。
    (!lisa_core::listing::cloud_only(&meta) && meta.is_file()).then(|| PreviewKey {
        path: path.to_path_buf(),
        len: meta.len(),
        mtime: meta.modified().ok(),
        cartridge,
    })
}

type PreviewFiles = Vec<(PathBuf, Arc<dyn Cartridge>)>;

#[derive(Clone, Copy)]
enum Scope {
    Current = 0,
    Capture = 1,
}

struct MetadataJob {
    scope: Scope,
    generation: u64,
    files: PreviewFiles,
}

enum Resolved {
    File(Scope, u64, PreviewKey, Arc<dyn Cartridge>),
    End(Scope, u64),
}

fn resolve_files(job: MetadataJob, generations: &[AtomicU64; 2], done: &Sender<Resolved>) {
    for (path, cartridge) in job.files {
        if generations[job.scope as usize].load(Ordering::Relaxed) != job.generation {
            return;
        }
        let Some(key) = source_key(&path, cartridge.id()) else {
            continue;
        };
        if done
            .send(Resolved::File(job.scope, job.generation, key, cartridge))
            .is_err()
        {
            return;
        }
    }
    let _ = done.send(Resolved::End(job.scope, job.generation));
}

struct Job {
    scope: Scope,
    generation: u64,
    ticket: u64,
    valid: Arc<AtomicBool>,
    key: PreviewKey,
    cartridge: Arc<dyn Cartridge>,
}

type Done = (u64, PreviewKey, Option<Arc<LoadedPreview>>);

/// カートリッジが裏で読んだ絵の置き場（メモリだけ）。開いたフォルダの子だけ `request` で読みを頼む。
pub struct PreviewImages {
    /// パス → 今のファイルの鍵（フォルダを開き直すたびに作り直す）。
    current: HashMap<PathBuf, PreviewKey>,
    capture: HashMap<PathBuf, PreviewKey>,
    images: HashMap<PreviewKey, Preview>,
    /// 入れた順（上限を超えたら先頭から捨てる）。
    order: VecDeque<PreviewKey>,
    /// 読みを頼んだ鍵（読み中・読めなかったものも含む。捨てるまで頼み直さない）。
    requested: HashMap<PreviewKey, (u64, Arc<AtomicBool>)>,
    ticket: u64,
    revision: u64,
    generations: Arc<[AtomicU64; 2]>,
    metadata: Sender<MetadataJob>,
    resolved: Mutex<Receiver<Resolved>>,
    current_pending: bool,
    current_invalidations: Vec<(PathBuf, disk::Scope)>,
    capture_pending: bool,
    capture_invalidations: Vec<(PathBuf, disk::Scope)>,
    skipped: Mutex<Receiver<Job>>,
    jobs: Sender<Job>,
    done: Mutex<Receiver<Done>>,
    disk: Option<Arc<disk::Disk>>,
    disk_dir: Option<PathBuf>,
}

impl Default for PreviewImages {
    fn default() -> Self {
        Self::new(None, None)
    }
}

impl PreviewImages {
    fn new(disk: Option<Arc<disk::Disk>>, disk_dir: Option<PathBuf>) -> Self {
        let (metadata, metadata_rx) = channel();
        let (resolved_tx, resolved) = channel();
        let generations = Arc::new([AtomicU64::new(0), AtomicU64::new(0)]);
        let active = generations.clone();
        std::thread::spawn(move || {
            while let Ok(job) = metadata_rx.recv() {
                resolve_files(job, &active, &resolved_tx);
            }
        });
        let (jobs, job_rx) = channel::<Job>();
        let (done_tx, done) = channel::<Done>();
        let (skipped_tx, skipped) = channel();
        let job_rx = Arc::new(Mutex::new(job_rx));
        for _ in 0..WORKERS {
            let (job_rx, done_tx) = (job_rx.clone(), done_tx.clone());
            let (active, skipped_tx) = (generations.clone(), skipped_tx.clone());
            let disk = disk.clone();
            std::thread::spawn(move || {
                loop {
                    let job = match job_rx.lock() {
                        Ok(rx) => rx.recv(),
                        Err(_) => return,
                    };
                    let Ok(job) = job else { return };
                    if !job.valid.load(Ordering::Acquire) {
                        continue;
                    }
                    if active[job.scope as usize].load(Ordering::Relaxed) != job.generation {
                        if skipped_tx.send(job).is_err() {
                            return;
                        }
                        continue;
                    }
                    let disk = disk.as_ref().filter(|_| disk::supported(job.key.cartridge));
                    let cached = disk.and_then(|disk| disk.load(&job.key));
                    let generated = cached.is_none();
                    let loaded = cached
                        .or_else(|| {
                            // ImageIO・AVFoundation の autorelease を 1 枚ごとに解放する。
                            #[cfg(target_os = "macos")]
                            let loaded =
                                objc2::rc::autoreleasepool(|_| job.cartridge.load(&job.key.path));
                            #[cfg(not(target_os = "macos"))]
                            let loaded = job.cartridge.load(&job.key.path);
                            if loaded.is_none() {
                                warn!("プレビューを読めませんでした: {}", job.key.path.display());
                            }
                            loaded
                        })
                        .map(Arc::new);
                    if done_tx
                        .send((job.ticket, job.key.clone(), loaded.clone()))
                        .is_err()
                    {
                        return;
                    }
                    if generated
                        && let (Some(disk), Some(image)) = (disk, loaded)
                        && matches!(&*image, LoadedPreview::Image { .. })
                        && job.key.mtime.is_some()
                        && source_key(&job.key.path, job.key.cartridge).as_ref() == Some(&job.key)
                    {
                        disk.save(job.key, image, job.valid);
                    }
                }
            });
        }
        Self {
            current: HashMap::new(),
            capture: HashMap::new(),
            images: HashMap::new(),
            order: VecDeque::new(),
            requested: HashMap::new(),
            ticket: 0,
            revision: 0,
            generations,
            metadata,
            resolved: Mutex::new(resolved),
            current_pending: false,
            current_invalidations: Vec::new(),
            capture_pending: false,
            capture_invalidations: Vec::new(),
            skipped: Mutex::new(skipped),
            jobs,
            done: Mutex::new(done),
            disk,
            disk_dir,
        }
    }
}

impl PreviewImages {
    /// 描くときに引く。読めていれば、絵と縦横。
    pub fn get(&self, path: &Path) -> Option<Preview> {
        self.images
            .get(self.current.get(path).or_else(|| self.capture.get(path))?)
            .cloned()
    }

    #[cfg(test)]
    pub(crate) fn requests_issued(&self) -> u64 {
        self.ticket
    }

    /// `from` が `to` へ動いた（名前の変更・移動・取り消し）。読めた絵の鍵を付け替えて、読み直さずに
    /// 使う（中身は同じなので、大きさと更新日時は変わらない）。フォルダなら下のファイルも付け替える。
    /// 読み直し前の古い名前でも、読み直し後の新しい名前でも、同じ絵を引けるようにする。
    #[cfg(test)]
    fn moved(&mut self, from: &Path, to: &Path) {
        self.moved_with_cartridge(from, to, false, None);
    }

    fn moved_with_cartridge(
        &mut self,
        from: &Path,
        to: &Path,
        is_dir: bool,
        cartridge: Option<&str>,
    ) {
        let is_dir = is_dir
            || std::fs::symlink_metadata(to).is_ok_and(|meta| meta.is_dir())
            || self
                .images
                .keys()
                .any(|key| key.path.starts_with(from) && key.path != from);
        let scope = if is_dir {
            disk::Scope::Descendants
        } else {
            disk::Scope::File
        };
        let already_at_dest = self
            .current
            .keys()
            .any(|path| disk::contains(to, scope, path));
        self.remove_disk(from, scope);
        self.clear_paths_only(to, scope, false);
        self.remove_disk(to, scope);
        let rekey = |key: &PreviewKey| {
            let suffix = key.path.strip_prefix(from).ok()?;
            let path = if suffix.as_os_str().is_empty() {
                if cartridge.is_some_and(|id| key.cartridge != id) {
                    return None;
                }
                to.to_path_buf()
            } else {
                to.join(suffix)
            };
            Some(PreviewKey {
                path,
                ..key.clone()
            })
        };
        // 読み中のもの（まだ絵が無い）は付け替えない。新しい鍵で読み直す。
        let moved: Vec<(PreviewKey, PreviewKey)> = self
            .images
            .keys()
            .filter_map(|old| Some((old.clone(), rekey(old)?)))
            .collect();
        let mut aliases = Vec::new();
        for (old, new) in moved {
            if let Some(preview) = self.images.remove(&old) {
                self.images.insert(new.clone(), preview);
            }
            if let Some((ticket, valid)) = self.requested.remove(&old) {
                valid.store(false, Ordering::Release);
                if self.disk.is_none() || !disk::supported(new.cartridge) {
                    self.requested
                        .insert(new.clone(), (ticket, Arc::new(AtomicBool::new(true))));
                }
            }
            if let Some(slot) = self.order.iter_mut().find(|k| **k == old) {
                *slot = new.clone();
            }
            for keys in [&mut self.current, &mut self.capture] {
                for (path, key) in keys.iter_mut().filter(|(_, key)| **key == old) {
                    *key = new.clone();
                    aliases.push((path.clone(), new.clone()));
                }
            }
            self.current.insert(new.path.clone(), new);
        }
        self.clear_paths_only(from, scope, false);
        self.current.extend(aliases);
        if already_at_dest {
            self.revision = self.revision.wrapping_add(1);
        }
    }

    pub(crate) fn invalidate(
        &mut self,
        target: &crate::scan::RefreshTarget,
    ) -> Option<Receiver<bool>> {
        let scope = if target.subtree {
            disk::Scope::Children
        } else {
            disk::Scope::File
        };
        self.clear_paths(&target.path, scope);
        self.remove_disk(&target.path, scope)
    }

    fn remove_disk(&self, path: &Path, scope: disk::Scope) -> Option<Receiver<bool>> {
        if let Some(disk) = &self.disk {
            return Some(disk.remove(path.to_path_buf(), scope));
        }
        Some(disk::remove_from_owner(self.disk_dir.clone()?, path, scope))
    }

    fn clear_paths(&mut self, path: &Path, scope: disk::Scope) {
        self.clear_paths_only(path, scope, true);
    }

    fn clear_paths_only(&mut self, path: &Path, scope: disk::Scope, bump_revision: bool) {
        if bump_revision {
            self.revision = self.revision.wrapping_add(1);
            self.generations[Scope::Current as usize].fetch_add(1, Ordering::Relaxed);
            self.current_pending = false;
            self.current_invalidations.clear();
        } else if self.current_pending {
            self.current_invalidations.push((path.to_path_buf(), scope));
        }
        // 撮影用には、更新範囲外の解決・デコードを続ける。
        if self.capture_pending {
            self.capture_invalidations.push((path.to_path_buf(), scope));
        }
        let contains = |candidate: &Path| disk::contains(path, scope, candidate);
        self.current.retain(|path, _| !contains(path));
        self.capture.retain(|path, _| !contains(path));
        self.images.retain(|key, _| !contains(&key.path));
        self.order.retain(|key| !contains(&key.path));
        self.requested.retain(|key, (_, valid)| {
            let keep = !contains(&key.path);
            if !keep {
                valid.store(false, Ordering::Release);
            }
            keep
        });
    }

    /// 開いたフォルダの直接の子を渡す。鍵を作り直し、まだ無いものは読みを頼む。
    fn request(&mut self, files: PreviewFiles) {
        self.request_files(files, Scope::Current);
    }

    fn request_files(&mut self, files: PreviewFiles, scope: Scope) {
        let generation = self.generations[scope as usize]
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        match scope {
            Scope::Current => {
                self.current.clear();
                self.current_invalidations.clear();
                self.current_pending = !files.is_empty();
            }
            Scope::Capture => {
                self.capture.clear();
                self.capture_invalidations.clear();
                self.capture_pending = !files.is_empty();
            }
        }
        if !files.is_empty()
            && self
                .metadata
                .send(MetadataJob {
                    scope,
                    generation,
                    files,
                })
                .is_err()
        {
            match scope {
                Scope::Current => self.current_pending = false,
                Scope::Capture => self.capture_pending = false,
            }
        }
    }

    /// 読めた絵を受け取って `Image` にする。1 枚でも入れたら true（再描画を 1 回頼む）。
    pub fn receive(&mut self, assets: &mut Assets<Image>) -> bool {
        let mut changed = false;
        if let Ok(rx) = self.resolved.get_mut() {
            for result in rx.try_iter() {
                let (scope, generation) = match &result {
                    Resolved::File(scope, generation, ..) | Resolved::End(scope, generation) => {
                        (*scope, *generation)
                    }
                };
                if self.generations[scope as usize].load(Ordering::Relaxed) != generation {
                    continue;
                }
                let Resolved::File(_, _, key, cartridge) = result else {
                    match scope {
                        Scope::Current => {
                            self.current_pending = false;
                            self.current_invalidations.clear();
                        }
                        Scope::Capture => {
                            self.capture_pending = false;
                            self.capture_invalidations.clear();
                        }
                    }
                    continue;
                };
                let keys =
                    match scope {
                        Scope::Current => {
                            if self.current_invalidations.iter().any(|(path, inv_scope)| {
                                disk::contains(path, *inv_scope, &key.path)
                            }) {
                                continue;
                            }
                            &mut self.current
                        }
                        Scope::Capture => {
                            if self.capture_invalidations.iter().any(|(path, inv_scope)| {
                                disk::contains(path, *inv_scope, &key.path)
                            }) {
                                continue;
                            }
                            &mut self.capture
                        }
                    };
                keys.insert(key.path.clone(), key.clone());
                if self.images.contains_key(&key) {
                    changed = true;
                }
                if !self.requested.contains_key(&key) {
                    self.ticket = self.ticket.wrapping_add(1);
                    let valid = Arc::new(AtomicBool::new(true));
                    if let Some(disk) = &self.disk
                        && disk::supported(key.cartridge)
                    {
                        disk.track(&key, &valid);
                    }
                    self.requested
                        .insert(key.clone(), (self.ticket, valid.clone()));
                    let _ = self.jobs.send(Job {
                        scope,
                        generation,
                        ticket: self.ticket,
                        valid,
                        key,
                        cartridge,
                    });
                }
            }
        }
        // 同じ鍵を別の依頼でも待っていれば、今の世代の仕事として戻す。
        if let Ok(rx) = self.skipped.get_mut() {
            for mut job in rx.try_iter() {
                if !self.requested.get(&job.key).is_some_and(|(active, valid)| {
                    *active == job.ticket && valid.load(Ordering::Acquire)
                }) {
                    continue;
                }
                if self.current.get(&job.key.path) == Some(&job.key) {
                    job.scope = Scope::Current;
                } else if self.capture.get(&job.key.path) == Some(&job.key) {
                    job.scope = Scope::Capture;
                } else {
                    self.requested.remove(&job.key);
                    continue;
                }
                job.generation = self.generations[job.scope as usize].load(Ordering::Relaxed);
                let _ = self.jobs.send(job);
            }
        }
        let mut done = Vec::new();
        if let Ok(rx) = self.done.lock() {
            done.extend(rx.try_iter());
        }
        for (ticket, key, loaded) in done {
            if !self
                .requested
                .get(&key)
                .is_some_and(|(active, valid)| *active == ticket && valid.load(Ordering::Acquire))
            {
                continue;
            }
            self.order.retain(|old| *old != key);
            self.order.push_back(key.clone());
            let preview = match loaded.as_deref() {
                None => continue,
                Some(LoadedPreview::Text { lines }) => Preview::Text(Arc::new(lines.clone())),
                Some(LoadedPreview::Image {
                    width,
                    height,
                    rgba,
                }) => {
                    if rgba.len() != (width * height * 4) as usize || *width == 0 || *height == 0 {
                        continue;
                    }
                    let mut image = Image::new(
                        Extent3d {
                            width: *width,
                            height: *height,
                            depth_or_array_layers: 1,
                        },
                        TextureDimension::D2,
                        rgba.clone(),
                        TextureFormat::Rgba8UnormSrgb,
                        RenderAssetUsages::RENDER_WORLD,
                    );
                    image.texture_view_descriptor = None;
                    Preview::Image(assets.add(image), UVec2::new(*width, *height))
                }
            };
            changed |= self.current.get(&key.path) == Some(&key)
                || self.capture.get(&key.path) == Some(&key);
            self.images.insert(key, preview);
        }
        while self.order.len() > MAX_IMAGES {
            if let Some(old) = self.order.pop_front() {
                self.images.remove(&old);
                self.requested.remove(&old);
            }
        }
        changed
    }
}

#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
enum TestPreview {
    Image(u32, u32, Vec<u8>),
    Text(Vec<String>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::{Duration, Instant};

    struct Counted {
        inner: Arc<dyn Cartridge>,
        generated: Arc<AtomicUsize>,
    }

    impl Cartridge for Counted {
        fn id(&self) -> &'static str {
            self.inner.id()
        }

        fn extensions(&self) -> &'static [&'static str] {
            self.inner.extensions()
        }

        fn can_load(&self, name: &str) -> bool {
            self.inner.can_load(name)
        }

        fn draw(&self, ctx: &mut DrawContext<'_, '_, '_, '_>) {
            self.inner.draw(ctx);
        }

        fn load(&self, path: &Path) -> Option<LoadedPreview> {
            self.generated.fetch_add(1, Ordering::Relaxed);
            self.inner.load(path)
        }
    }

    fn wait_previews(images: &mut PreviewImages, assets: &mut Assets<Image>) {
        let start = Instant::now();
        while images.current_pending
            || images
                .current
                .values()
                .any(|key| !images.order.contains(key))
        {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "画像を待ってタイムアウト"
            );
            images.receive(assets);
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn snapshot_preview(
        images: &PreviewImages,
        assets: &Assets<Image>,
        path: &Path,
    ) -> TestPreview {
        match images
            .get(path)
            .unwrap_or_else(|| panic!("画像を読めない: {}", path.display()))
        {
            Preview::Image(handle, size) => TestPreview::Image(
                size.x,
                size.y,
                assets.get(&handle).unwrap().data.clone().unwrap(),
            ),
            Preview::Text(lines) => TestPreview::Text((*lines).clone()),
        }
    }

    fn reopen(dir: &Path) -> Arc<disk::Disk> {
        let start = Instant::now();
        loop {
            if let Some(disk) = disk::Disk::open(dir.to_path_buf()) {
                return disk;
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "前の保存係が終了しなかった"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn wait_saved(disk: &disk::Disk, path: &Path, cartridge: &'static str) {
        let key = source_key(path, cartridge).unwrap();
        let start = Instant::now();
        while disk.load(&key).is_none() {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "画像が保存されなかった: {path:?}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn 実際のカートリッジは再起動後に画像を生成し直さない() {
        let tmp = disk::tests::Temp::new();
        let cache = tmp.0.join("cache");
        let png = tmp.0.join("sample.png");
        let png_image = ::image::RgbaImage::from_fn(31, 17, |x, y| {
            ::image::Rgba([
                80,
                (x * 7) as u8,
                (y * 9) as u8,
                if x % 2 == 0 { 123 } else { 0 },
            ])
        });
        png_image.save(&png).unwrap();
        ::image::DynamicImage::ImageRgba8(png_image)
            .to_rgb8()
            .save(tmp.0.join("sample.jpg"))
            .unwrap();
        let text = tmp.0.join("sample.txt");
        std::fs::write(&text, "sample document\n").unwrap();
        #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
        let mut paths = vec![png.clone(), tmp.0.join("sample.jpg"), text.clone()];
        #[cfg(target_os = "macos")]
        {
            use std::process::Command;
            let heic = tmp.0.join("sample.heic");
            let heic_source = tmp.0.join("sample.jpg");
            let converted = Command::new("sips")
                .args(["-s", "format", "heic"])
                .arg(&heic_source)
                .arg("--out")
                .arg(&heic)
                .output();
            match converted {
                Ok(out) => {
                    assert!(
                        out.status.success(),
                        "sips: {}",
                        String::from_utf8_lossy(&out.stderr)
                    );
                    paths.push(heic);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    eprintln!("sips が見つからないので HEIC を省略");
                }
                Err(error) => panic!("sips: {error}"),
            }
            let video = tmp.0.join("sample.mp4");
            let produced = Command::new("ffmpeg")
                .args([
                    "-loglevel",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc=size=320x180:rate=5:duration=2",
                    "-pix_fmt",
                    "yuv420p",
                ])
                .arg(&video)
                .output();
            match produced {
                Ok(out) => {
                    assert!(
                        out.status.success(),
                        "ffmpeg: {}",
                        String::from_utf8_lossy(&out.stderr)
                    );
                    paths.push(video);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    eprintln!("ffmpeg が見つからないので動画を省略");
                }
                Err(error) => panic!("ffmpeg: {error}"),
            }
            let path = tmp.0.join("sample.pdf");
            std::fs::write(&path, sample_pdf()).unwrap();
            paths.push(path);
        }
        let mut registry = PreviewRegistry::default();
        cartridges::register(&mut registry);
        let image_calls = Arc::new(AtomicUsize::new(0));
        let text_calls = Arc::new(AtomicUsize::new(0));
        let requests: Vec<_> = paths
            .iter()
            .map(|path| {
                let inner = registry
                    .pick_arc(path.file_name().unwrap().to_str().unwrap())
                    .unwrap()
                    .clone();
                assert!(inner.can_load(path.file_name().unwrap().to_str().unwrap()));
                let generated = if path == &text {
                    text_calls.clone()
                } else {
                    image_calls.clone()
                };
                (
                    path.clone(),
                    Arc::new(Counted { inner, generated }) as Arc<dyn Cartridge>,
                )
            })
            .collect();
        let disk = reopen(&cache);
        let mut images = PreviewImages::new(Some(disk.clone()), Some(cache.clone()));
        let mut assets = Assets::default();
        images.request(requests.clone());
        wait_previews(&mut images, &mut assets);
        let first: Vec<_> = paths
            .iter()
            .map(|path| snapshot_preview(&images, &assets, path))
            .collect();
        for (path, cartridge) in &requests {
            if disk::supported(cartridge.id()) {
                wait_saved(&disk, path, cartridge.id());
            }
        }
        assert_eq!(image_calls.load(Ordering::Relaxed), paths.len() - 1);
        assert_eq!(text_calls.load(Ordering::Relaxed), 1);
        drop(images);
        drop(disk);
        let fresh = reopen(&cache);
        let mut images = PreviewImages::new(Some(fresh), Some(cache));
        let mut assets = Assets::default();
        images.request(requests);
        wait_previews(&mut images, &mut assets);
        let second: Vec<_> = paths
            .iter()
            .map(|path| snapshot_preview(&images, &assets, path))
            .collect();
        assert_eq!(second, first);
        assert_eq!(image_calls.load(Ordering::Relaxed), paths.len() - 1);
        assert_eq!(text_calls.load(Ordering::Relaxed), 2);
    }

    #[cfg(target_os = "macos")]
    fn sample_pdf() -> Vec<u8> {
        use std::io::Write;

        let drawing = "0.1 0.5 0.8 rg\n10 20 80 40 re f\n";
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 120 80] /Contents 4 0 R >>".to_string(),
            format!(
                "<< /Length {} >>\nstream\n{drawing}endstream",
                drawing.len()
            ),
        ];
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            writeln!(pdf, "{} 0 obj\n{object}\nendobj", index + 1).unwrap();
        }
        let xref = pdf.len();
        let size = offsets.len() + 1;
        writeln!(pdf, "xref\n0 {size}\n0000000000 65535 f ").unwrap();
        for offset in offsets {
            writeln!(pdf, "{offset:010} 00000 n ").unwrap();
        }
        writeln!(
            pdf,
            "trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF"
        )
        .unwrap();
        pdf
    }

    fn system_world(
        tree: crate::model::tree::FileTree,
        folder: usize,
        registry: PreviewRegistry,
    ) -> World {
        let mut world = World::new();
        world.insert_resource(tree);
        world.insert_resource(crate::view::Nav {
            stack: vec![
                crate::view::Level {
                    node: crate::model::tree::FileTree::ROOT,
                },
                crate::view::Level { node: folder },
            ],
        });
        world.insert_resource(registry);
        world.insert_resource(Assets::<Image>::default());
        world.insert_resource(crate::view::SyncRequest::default());
        world
    }

    fn registry_with(
        disk: Arc<disk::Disk>,
        cache: &Path,
        generated: &Arc<AtomicUsize>,
    ) -> PreviewRegistry {
        let mut real = PreviewRegistry::default();
        cartridges::register(&mut real);
        let inner = real.pick_arc("a.png").unwrap().clone();
        let mut registry = PreviewRegistry {
            images: PreviewImages::new(Some(disk), Some(cache.to_path_buf())),
            ..Default::default()
        };
        registry.add(Counted {
            inner,
            generated: generated.clone(),
        });
        registry
    }

    fn run_until_shown(world: &mut World, schedule: &mut Schedule, want: usize) {
        let start = Instant::now();
        loop {
            schedule.run(world);
            let registry = world.resource::<PreviewRegistry>();
            let images = &registry.images;
            if !images.current_pending
                && images.current.len() == want
                && images
                    .current
                    .values()
                    .all(|key| images.order.contains(key))
            {
                return;
            }
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "画像を待ってタイムアウト"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn update_previews_は直下だけを要求し再起動後に生成しない() {
        let tmp = disk::tests::Temp::new();
        let cache = tmp.0.join("cache");
        let mount = tmp.0.join("vol");
        let folder_path = mount.join("d00");
        let grand = folder_path.join("d01");
        std::fs::create_dir_all(&grand).unwrap();
        let image = ::image::RgbaImage::from_fn(20, 10, |x, y| {
            ::image::Rgba([x as u8 * 9, y as u8 * 20, 7, if x < 5 { 0 } else { 255 }])
        });
        image.save(folder_path.join("p0.png")).unwrap();
        ::image::DynamicImage::ImageRgba8(image.clone())
            .to_rgb8()
            .save(folder_path.join("p1.png"))
            .unwrap();
        image.save(grand.join("q0.png")).unwrap();
        let build = || {
            use crate::model::tree::FileTree;
            let mut volume = crate::scan::root_volume();
            volume.mount_path = mount.clone();
            let mut tree = FileTree::this_device("test", &[volume]);
            let drive = tree.nodes[FileTree::ROOT + 1].children[0];
            let read = |path: &Path| lisa_core::listing::read_dir(path).map_err(|e| e.to_string());
            tree.add_listing(drive, read(&mount));
            let folder = tree.nodes[drive].children[0];
            tree.add_listing(folder, read(&folder_path));
            let sub = tree.nodes[folder]
                .children
                .iter()
                .copied()
                .find(|&c| tree.nodes[c].is_dir)
                .unwrap();
            tree.add_listing(sub, read(&grand));
            (tree, folder)
        };
        let paths = [folder_path.join("p0.png"), folder_path.join("p1.png")];
        let generated = Arc::new(AtomicUsize::new(0));
        let mut shots = Vec::new();
        for session in 0..2 {
            let disk = reopen(&cache);
            let (tree, folder) = build();
            let mut world = system_world(
                tree,
                folder,
                registry_with(disk.clone(), &cache, &generated),
            );
            let mut schedule = Schedule::default();
            schedule.add_systems(update_previews);
            run_until_shown(&mut world, &mut schedule, 2);
            if session == 0 {
                for path in &paths {
                    wait_saved(&disk, path, "image");
                }
            }
            let registry = world.resource::<PreviewRegistry>();
            assert_eq!(registry.images.ticket, 2);
            assert!(
                registry
                    .images
                    .requested
                    .keys()
                    .all(|key| key.path.parent() == Some(folder_path.as_path())),
                "孫階層は要求しない"
            );
            let assets = world.resource::<Assets<Image>>();
            shots.push(
                paths
                    .iter()
                    .map(|p| snapshot_preview(&registry.images, assets, p))
                    .collect::<Vec<_>>(),
            );
            drop(world);
        }
        assert_eq!(generated.load(Ordering::Relaxed), 2, "再起動後は生成しない");
        assert_eq!(shots[0], shots[1]);
        assert!(matches!(&shots[0][0], TestPreview::Image(20, 10, rgba) if rgba[3] == 0));
    }

    /// 先頭バイトを色にする合成の画像カートリッジ。生成回数を数える。
    struct Gen(Arc<AtomicUsize>);

    impl Cartridge for Gen {
        fn id(&self) -> &'static str {
            "image"
        }

        fn extensions(&self) -> &'static [&'static str] {
            &["png"]
        }

        fn draw(&self, _ctx: &mut DrawContext<'_, '_, '_, '_>) {}

        fn load(&self, path: &Path) -> Option<LoadedPreview> {
            self.0.fetch_add(1, Ordering::Relaxed);
            let c = *std::fs::read(path).ok()?.first()?;
            Some(LoadedPreview::Image {
                width: 2,
                height: 1,
                rgba: vec![c, 1, 2, 255, c, 3, 4, 0],
            })
        }
    }

    struct Session {
        images: PreviewImages,
        disk: Arc<disk::Disk>,
        assets: Assets<Image>,
    }

    impl Session {
        fn open(cache: &Path) -> Self {
            let disk = reopen(cache);
            let images = PreviewImages::new(Some(disk.clone()), Some(cache.to_path_buf()));
            Self {
                images,
                disk,
                assets: Assets::default(),
            }
        }

        fn show(&mut self, calls: &Arc<AtomicUsize>, path: &Path) -> u8 {
            let cartridge: Arc<dyn Cartridge> = Arc::new(Gen(calls.clone()));
            self.images.request(vec![(path.to_path_buf(), cartridge)]);
            wait_previews(&mut self.images, &mut self.assets);
            match snapshot_preview(&self.images, &self.assets, path) {
                TestPreview::Image(2, 1, rgba) => rgba[0],
                other => panic!("想定外の画像: {other:?}"),
            }
        }

        fn saved(&self, path: &Path) {
            wait_saved(&self.disk, path, "image");
        }

        fn barrier(&self) {
            let unrelated = std::env::temp_dir().join("lisa-barrier-unrelated");
            assert_eq!(
                self.disk
                    .remove(unrelated, disk::Scope::File)
                    .recv_timeout(Duration::from_secs(5)),
                Ok(true)
            );
        }
    }

    fn calls(n: &Arc<AtomicUsize>) -> usize {
        n.load(Ordering::Relaxed)
    }

    #[test]
    fn 属性の変化と手動更新は再起動後も新しい絵になる() {
        let tmp = disk::tests::Temp::new();
        let cache = tmp.0.join("cache");
        let file = tmp.0.join("a.png");
        let n = Arc::new(AtomicUsize::new(0));
        std::fs::write(&file, [10u8]).unwrap();
        let mut s = Session::open(&cache);
        assert_eq!(s.show(&n, &file), 10);
        s.saved(&file);
        drop(s);
        // 変化がなければ生成しない。
        let mut s = Session::open(&cache);
        assert_eq!(s.show(&n, &file), 10);
        assert_eq!(calls(&n), 1);
        drop(s);
        // サイズ変更。
        std::fs::write(&file, [20u8, 0]).unwrap();
        let mut s = Session::open(&cache);
        assert_eq!(s.show(&n, &file), 20);
        s.saved(&file);
        assert_eq!(calls(&n), 2);
        drop(s);
        // 同サイズで mtime だけ変更。
        let old = std::fs::metadata(&file).unwrap().modified().unwrap();
        std::fs::write(&file, [30u8, 0]).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(old + Duration::from_secs(5))
            .unwrap();
        let mut s = Session::open(&cache);
        assert_eq!(s.show(&n, &file), 30);
        s.saved(&file);
        assert_eq!(calls(&n), 3);
        drop(s);
        // 同じ属性での上書きは通常は古い絵。手動更新で原本から作り直し、再起動後も戻らない。
        let mtime = std::fs::metadata(&file).unwrap().modified().unwrap();
        std::fs::write(&file, [40u8, 0]).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        let mut s = Session::open(&cache);
        assert_eq!(s.show(&n, &file), 30);
        let rx = s.images.invalidate(&crate::scan::RefreshTarget {
            drive_id: "test".into(),
            path: file.clone(),
            subtree: false,
        });
        assert_eq!(rx.unwrap().recv_timeout(Duration::from_secs(5)), Ok(true));
        assert_eq!(s.show(&n, &file), 40);
        s.saved(&file);
        assert_eq!(calls(&n), 4);
        drop(s);
        let mut s = Session::open(&cache);
        assert_eq!(s.show(&n, &file), 40);
        assert_eq!(calls(&n), 4);
        // 削除したファイルの絵は復元されない（鍵が作れず要求されない）。
        std::fs::remove_file(&file).unwrap();
        s.images.request(vec![(
            file.clone(),
            Arc::new(Gen(n.clone())) as Arc<dyn Cartridge>,
        )]);
        assert!(s.images.current.is_empty());
        assert_eq!(calls(&n), 4);
    }

    #[test]
    fn 改名と大小文字改名とフォルダ移動とundoは旧記録を失効して再生成する() {
        let tmp = disk::tests::Temp::new();
        let cache = tmp.0.join("cache");
        let dir = tmp.0.join("dir");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let (a, b) = (dir.join("a.png"), dir.join("sub/b.png"));
        std::fs::write(&a, [1u8]).unwrap();
        std::fs::write(&b, [2u8]).unwrap();
        let n = Arc::new(AtomicUsize::new(0));
        let mut s = Session::open(&cache);
        s.show(&n, &a);
        s.show(&n, &b);
        s.saved(&a);
        s.saved(&b);
        let (key_a, key_b) = (
            source_key(&a, "image").unwrap(),
            source_key(&b, "image").unwrap(),
        );
        // 大小文字だけの改名。旧記録は新しい名前から引けず、旧パスの記録も失効する。
        let upper = dir.join("A.png");
        std::fs::rename(&a, &upper).unwrap();
        s.images.moved(&a, &upper);
        s.barrier();
        assert!(s.disk.load(&key_a).is_none());
        // メモリの絵は付け替えで使え、ディスクは新パスで要求し直す（旧パスの記録は無い）。
        assert_eq!(s.show(&n, &upper), 1);
        s.saved(&upper);
        // Undo。
        std::fs::rename(&upper, &a).unwrap();
        s.images.moved(&upper, &a);
        s.barrier();
        assert!(s.disk.load(&source_key(&a, "image").unwrap()).is_none());
        // フォルダ移動は祖先ダイジェストで配下の旧記録を失効する。
        let moved = tmp.0.join("moved");
        std::fs::rename(&dir, &moved).unwrap();
        s.images.moved(&dir, &moved);
        s.barrier();
        assert!(s.disk.load(&key_b).is_none());
        drop(s);
        // 再起動後は、旧記録が失効した分だけ原本から作り直す。
        let mut s = Session::open(&cache);
        assert_eq!(s.show(&n, &moved.join("a.png")), 1);
        assert_eq!(s.show(&n, &moved.join("sub/b.png")), 2);
        // 初回 2 + 新パスの要求 1 + 再起動後の 2。
        assert_eq!(calls(&n), 5);
    }

    #[test]
    fn 大小文字改名直後にツリー更新前の旧パスでディスク記録が復活しない() {
        let tmp = disk::tests::Temp::new();
        let cache = tmp.0.join("cache");
        let mount = tmp.0.join("vol");
        let folder_path = mount.join("d00");
        std::fs::create_dir_all(&folder_path).unwrap();
        let old_path = folder_path.join("a.png");
        let new_path = folder_path.join("A.png");
        ::image::RgbaImage::from_fn(8, 8, |_, _| ::image::Rgba([20, 40, 200, 255]))
            .save(&old_path)
            .unwrap();

        let build = || {
            use crate::model::tree::FileTree;
            let mut volume = crate::scan::root_volume();
            volume.mount_path = mount.clone();
            let mut tree = FileTree::this_device("test", &[volume]);
            let drive = tree.nodes[FileTree::ROOT + 1].children[0];
            let read = |path: &Path| lisa_core::listing::read_dir(path).map_err(|e| e.to_string());
            tree.add_listing(drive, read(&mount));
            let folder = tree.nodes[drive].children[0];
            tree.add_listing(folder, read(&folder_path));
            (tree, folder)
        };

        let generated = Arc::new(AtomicUsize::new(0));
        let disk = reopen(&cache);
        let (tree, folder) = build();
        let mut world = system_world(
            tree,
            folder,
            registry_with(disk.clone(), &cache, &generated),
        );
        let mut schedule = Schedule::default();
        schedule.add_systems(update_previews);
        run_until_shown(&mut world, &mut schedule, 1);
        wait_saved(&disk, &old_path, "image");
        let old_key = source_key(&old_path, "image").unwrap();
        let rev_before = world.resource::<PreviewRegistry>().images.revision;

        // 改名直後（FileTree はまだ旧名 "a.png" のまま）に moved を反映する。
        std::fs::rename(&old_path, &new_path).unwrap();
        world
            .resource_mut::<PreviewRegistry>()
            .moved(&old_path, &new_path, false);
        assert_eq!(
            world.resource::<PreviewRegistry>().images.revision,
            rev_before,
            "ツリー更新前に revision を進めない"
        );
        for _ in 0..5 {
            schedule.run(&mut world);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            disk.remove(tmp.0.join("barrier"), disk::Scope::File)
                .recv_timeout(Duration::from_secs(5)),
            Ok(true)
        );
        assert!(
            disk.load(&old_key).is_none(),
            "旧パスのディスク記録が復活してはいけない"
        );
        assert!(
            world
                .resource::<PreviewRegistry>()
                .images
                .get(&old_path)
                .is_some(),
            "ツリー再一覧まで旧名のエイリアスでチラつかず表示できる"
        );

        // ツリーが新名へ更新されれば新パスで要求・保存される。
        let (fresh_tree, fresh_folder) = build();
        *world.resource_mut::<crate::model::tree::FileTree>() = fresh_tree;
        world.resource_mut::<crate::view::Nav>().stack[1].node = fresh_folder;
        run_until_shown(&mut world, &mut schedule, 1);
        wait_saved(&disk, &new_path, "image");
        assert!(disk.load(&old_key).is_none());
    }

    #[test]
    fn 移動先が不在のフォルダ移動でも配下のディスク記録を失効する() {
        let tmp = disk::tests::Temp::new();
        let cache = tmp.0.join("cache");
        let dir = tmp.0.join("dir");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let file = dir.join("sub/b.png");
        std::fs::write(&file, [7u8]).unwrap();
        let n = Arc::new(AtomicUsize::new(0));
        let mut s = Session::open(&cache);
        assert_eq!(s.show(&n, &file), 7);
        s.saved(&file);
        let key_file = source_key(&file, "image").unwrap();

        // 連鎖移動（dir -> mid -> final）で通知時に mid が既に不在の状況を再現する。
        let mid = tmp.0.join("mid");
        let final_dir = tmp.0.join("final");
        std::fs::rename(&dir, &final_dir).unwrap();
        assert!(!mid.exists());
        // メモリから先に追い出された状態でも is_dir=true で Descendants 失効する。
        s.images.images.clear();
        s.images.moved_with_cartridge(&dir, &mid, true, None);
        s.barrier();
        assert!(s.disk.load(&key_file).is_none());
    }

    #[test]
    fn 失効後に完了した古い生成は保存も表示もされない() {
        let tmp = disk::tests::Temp::new();
        let cache = tmp.0.join("cache");
        let file = tmp.0.join("a.png");
        std::fs::write(&file, [10u8]).unwrap();
        let n = Arc::new(AtomicUsize::new(0));
        let mut s = Session::open(&cache);
        let key = source_key(&file, "image").unwrap();
        s.images
            .requested
            .insert(key.clone(), (1, Arc::new(AtomicBool::new(true))));
        s.images.current.insert(file.clone(), key.clone());
        s.images.invalidate(&crate::scan::RefreshTarget {
            drive_id: "test".into(),
            path: file.clone(),
            subtree: false,
        });
        s.barrier();
        // 失効前の世代の結果が手動更新のあとに届いても採用しない。
        let stale = Arc::new(AtomicBool::new(false));
        let loaded = Arc::new(LoadedPreview::Image {
            width: 1,
            height: 1,
            rgba: vec![9, 9, 9, 9],
        });
        s.disk.save(key.clone(), loaded.clone(), stale);
        let (sender, receiver) = channel();
        let worker_done = std::mem::replace(&mut s.images.done, Mutex::new(receiver));
        sender.send((1, key.clone(), Some(loaded))).unwrap();
        assert!(!s.images.receive(&mut s.assets));
        assert!(!s.images.images.contains_key(&key));
        s.images.done = worker_done;
        s.barrier();
        assert!(s.disk.load(&key).is_none());
        assert_eq!(s.show(&n, &file), 10);
    }

    struct Dummy {
        id: &'static str,
        exts: &'static [&'static str],
        priority: i32,
    }

    impl Cartridge for Dummy {
        fn id(&self) -> &'static str {
            self.id
        }

        fn extensions(&self) -> &'static [&'static str] {
            self.exts
        }

        fn priority(&self) -> i32 {
            self.priority
        }

        fn draw(&self, _ctx: &mut DrawContext<'_, '_, '_, '_>) {}
    }

    #[test]
    fn a_loaded_image_uses_the_full_frame_without_paper() {
        let frame = Rect::new(10.0, 20.0, 170.0, 140.0);
        let image = Preview::Image(Handle::default(), UVec2::new(1024, 768));
        for portrait in [false, true] {
            assert_eq!(
                preview_geometry(frame, portrait, Some(&image)),
                (None, frame)
            );
        }
    }

    #[test]
    fn loaded_image_pdf_and_video_draw_without_paper_or_border() {
        let mut registry = PreviewRegistry::default();
        cartridges::register(&mut registry);
        let mut world = World::new();
        let mut meshes = Assets::default();
        let theme = crate::render::shapes::UiThemeAssets::load(&mut Assets::default(), &mut meshes);
        let mut materials = Assets::default();
        let mut images = Assets::default();
        let thumbnail = images.add(Image::default());
        let frame = Rect::new(10.0, 20.0, 170.0, 140.0);

        for (name, expected_parts) in [("photo.png", 1), ("document.pdf", 1), ("clip.mp4", 2)] {
            let parent = world.spawn_empty().id();
            draw(
                &registry,
                &mut Painter {
                    material_cache: default(),
                    commands: &mut world.commands(),
                    meshes: &mut meshes,
                    materials: &mut materials,
                    parent,
                    layer: default(),
                    scale: 1.0,
                    font: default(),
                    theme: &theme,
                    no_cull: false,
                },
                name,
                frame,
                palette::BASE_200,
                0.0,
                Some(Preview::Image(thumbnail.clone(), UVec2::new(160, 90))),
                false,
            );
            world.flush();

            let children = world.get::<Children>(parent).unwrap();
            assert_eq!(children.len(), expected_parts, "{name}");
            let sprite = world.get::<Sprite>(children[0]).unwrap();
            assert_eq!(sprite.image, thumbnail, "{name}");
            if expected_parts == 2 {
                assert!(world.get::<Mesh2d>(children[1]).is_some(), "{name}");
            }
        }
    }

    #[test]
    fn missing_text_and_invalid_image_previews_keep_the_padded_paper() {
        let frame = Rect::new(10.0, 20.0, 170.0, 140.0);
        let text = Preview::Text(vec!["sample".to_string()].into());
        let invalid = Preview::Image(Handle::default(), UVec2::new(100, 0));
        for loaded in [None, Some(&text), Some(&invalid)] {
            for portrait in [false, true] {
                let (paper, inner) = preview_geometry(frame, portrait, loaded);
                let paper = paper.unwrap();
                let pad = if portrait {
                    paper.width()
                } else {
                    frame.height()
                } * 0.14;
                assert_eq!(paper.center(), frame.center());
                assert_eq!(inner.min, paper.min + pad);
                assert_eq!(inner.max, paper.max - pad);
                if portrait {
                    assert!(
                        (paper.width() / paper.height() - std::f32::consts::FRAC_1_SQRT_2).abs()
                            < 1e-6
                    );
                } else {
                    assert_eq!(paper, frame);
                }
            }
        }
    }

    #[test]
    fn only_document_cartridges_use_portrait_a4_paper() {
        let mut registry = PreviewRegistry::default();
        cartridges::register(&mut registry);
        let frame = Rect::new(-30.0, 10.0, 130.0, 130.0);
        for name in [
            "unknown.custom",
            "notes.txt",
            "notes.md",
            "memo.docx",
            "data.xlsx",
            "brief.pdf",
        ] {
            let paper = paper_frame(frame, registry.pick(name).portrait_a4());
            assert!(
                (paper.width() / paper.height() - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6,
                "{name}"
            );
            assert_eq!(paper.center(), frame.center(), "{name}");
            assert_eq!(paper.height(), frame.height(), "{name}");
        }
        for name in ["photo.png", "photo.heic", "clip.mov", "clip.mp4"] {
            assert_eq!(
                paper_frame(frame, registry.pick(name).portrait_a4()),
                frame,
                "{name}"
            );
        }
    }

    #[test]
    fn 更新後に届いた同じ属性の旧プレビューを採用しない() {
        let (jobs, _) = channel();
        let (sender, done) = channel();
        let path = PathBuf::from("branch/file.txt");
        let sibling = PathBuf::from("other/file.txt");
        let key = PreviewKey {
            path: path.clone(),
            len: 3,
            mtime: None,
            cartridge: "text",
        };
        let sibling_key = PreviewKey {
            path: sibling.clone(),
            ..key.clone()
        };
        let mut images = PreviewImages {
            jobs,
            done: Mutex::new(done),
            capture: HashMap::new(),
            current: HashMap::from([
                (path.clone(), key.clone()),
                (sibling.clone(), sibling_key.clone()),
            ]),
            images: HashMap::from([(
                sibling_key.clone(),
                Preview::Text(Arc::new(vec!["same".into()])),
            )]),
            order: VecDeque::from([sibling_key.clone()]),
            requested: HashMap::from([
                (key.clone(), (1, Arc::new(AtomicBool::new(true)))),
                (sibling_key, (2, Arc::new(AtomicBool::new(true)))),
            ]),
            ticket: 2,
            revision: 0,
            ..PreviewImages::default()
        };
        images.invalidate(&crate::scan::RefreshTarget {
            drive_id: "test".into(),
            path: path.clone(),
            subtree: false,
        });
        images.current.insert(path.clone(), key.clone());
        images
            .requested
            .insert(key.clone(), (3, Arc::new(AtomicBool::new(true))));
        sender
            .send((
                1,
                key.clone(),
                Some(Arc::new(LoadedPreview::Text {
                    lines: vec!["old".into()],
                })),
            ))
            .unwrap();
        let mut assets = Assets::default();
        assert!(!images.receive(&mut assets));
        assert!(images.get(&path).is_none());
        assert!(images.get(&sibling).is_some());

        sender
            .send((
                3,
                key,
                Some(Arc::new(LoadedPreview::Text {
                    lines: vec!["new".into()],
                })),
            ))
            .unwrap();
        assert!(images.receive(&mut assets));
        assert!(
            matches!(images.get(&path), Some(Preview::Text(lines)) if lines.as_slice() == ["new"])
        );
    }

    fn receive_until(images: &mut PreviewImages, ready: impl Fn(&PreviewImages) -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut assets = Assets::default();
        while !ready(images) {
            assert!(
                std::time::Instant::now() < deadline,
                "プレビューの受信がタイムアウトしました"
            );
            images.receive(&mut assets);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    #[test]
    fn metadata_jobs_skip_old_generations_and_non_files() {
        let dir = std::env::temp_dir().join(format!("lisa-preview-meta-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("file.txt");
        std::fs::write(&file, b"test").unwrap();
        let cartridge: Arc<dyn Cartridge> = Arc::new(Dummy {
            id: "text",
            exts: &["txt"],
            priority: 0,
        });
        let files = vec![
            (dir.clone(), cartridge.clone()),
            (file.clone(), cartridge.clone()),
        ];
        let (done, rx) = channel();
        let generations = [AtomicU64::new(2), AtomicU64::new(0)];

        resolve_files(
            MetadataJob {
                scope: Scope::Current,
                generation: 1,
                files: files.clone(),
            },
            &generations,
            &done,
        );
        assert!(rx.try_recv().is_err());
        let mut images = PreviewImages::default();
        images.request(files);
        assert!(
            images.current.is_empty(),
            "メインスレッドではファイルの鍵を作らない"
        );
        receive_until(&mut images, |images| images.current.contains_key(&file));
        assert_eq!(images.current.len(), 1);
        assert_eq!(images.current[&file].len, 4);
        let sibling = dir.join("sibling.txt");
        std::fs::write(&sibling, b"test").unwrap();
        images.request_files(
            vec![
                (file.clone(), cartridge.clone()),
                (sibling.clone(), cartridge),
            ],
            Scope::Capture,
        );
        images.invalidate(&crate::scan::RefreshTarget {
            drive_id: "test".into(),
            path: file.clone(),
            subtree: false,
        });
        receive_until(&mut images, |images| !images.capture_pending);
        assert!(!images.capture.contains_key(&file));
        assert!(images.capture.contains_key(&sibling));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn moved_previews_are_reused_without_reloading() {
        let tmp = std::env::temp_dir().join(format!("lisa-preview-moved-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("dir")).unwrap();
        let a = tmp.join("a.txt");
        let inner = tmp.join("dir/inner.txt");
        std::fs::write(&a, b"hello").unwrap();
        std::fs::write(&inner, b"world").unwrap();
        let cartridge: Arc<dyn Cartridge> = Arc::new(Dummy {
            id: "text",
            exts: &["txt"],
            priority: 0,
        });
        let mut images = PreviewImages::default();
        images.request(vec![
            (a.clone(), cartridge.clone()),
            (inner.clone(), cartridge.clone()),
        ]);
        receive_until(&mut images, |images| images.current.len() == 2);
        // デコード結果の代わりに、メモリキャッシュへ絵を入れる。
        for key in images.current.values() {
            let lines = Arc::new(vec![key.path.display().to_string()]);
            images.images.insert(key.clone(), Preview::Text(lines));
        }
        let requested = images.requested.len();

        let z = tmp.join("z.txt");
        std::fs::rename(&a, &z).unwrap();
        images.moved(&a, &z);
        assert!(images.get(&a).is_some(), "読み直す前の古い名前でも引ける");
        assert!(images.get(&z).is_some());
        images.request(vec![(z.clone(), cartridge.clone())]);
        receive_until(&mut images, |images| images.get(&z).is_some());
        assert_eq!(images.requested.len(), requested, "読みを頼み直さない");

        // フォルダごと動かしたら、下のファイルも付け替わる。取り消しで戻しても同じ。
        let moved = tmp.join("moved");
        std::fs::rename(tmp.join("dir"), &moved).unwrap();
        images.moved(&tmp.join("dir"), &moved);
        images.request(vec![(moved.join("inner.txt"), cartridge.clone())]);
        receive_until(&mut images, |images| {
            images.get(&moved.join("inner.txt")).is_some()
        });
        std::fs::rename(&moved, tmp.join("dir")).unwrap();
        images.moved(&moved, &tmp.join("dir"));
        images.request(vec![(inner.clone(), cartridge)]);
        receive_until(&mut images, |images| images.get(&inner).is_some());
        assert_eq!(images.requested.len(), requested);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn renaming_a_file_changes_the_signature_with_the_same_child_count() {
        let mut tree = crate::model::tree::FileTree::wide(2, 3);
        let folder = tree.nodes[crate::model::tree::FileTree::ROOT].children[0];
        let file = *tree.nodes[folder]
            .children
            .iter()
            .find(|&&c| !tree.nodes[c].is_dir)
            .expect("ファイルがある");
        let before = files_signature(&tree, folder);
        assert_eq!(files_signature(&tree, folder), before);
        let old = std::mem::replace(&mut tree.nodes[file].name, "renamed.txt".into());
        assert_ne!(files_signature(&tree, folder), before);
        tree.nodes[file].name = old;
        assert_eq!(files_signature(&tree, folder), before, "Undo で元に戻る");
    }

    #[test]
    fn extension_matching_is_case_insensitive() {
        let mut r = PreviewRegistry::default();
        cartridges::register(&mut r);
        assert_eq!(r.pick("photo.jpg").id(), "image");
        assert_eq!(r.pick("PHOTO.JPG").id(), "image");
        assert_eq!(r.pick("Screen.PnG").id(), "image");
        assert_eq!(r.pick("thumb.webp").id(), "image");
        assert_eq!(r.pick("THUMB.WEBP").id(), "image");
        assert_eq!(r.pick("README.MD").id(), "text");
        assert_eq!(r.pick("clip.MOV").id(), "video");
    }

    #[test]
    fn unknown_extension_goes_to_fallback() {
        let mut r = PreviewRegistry::default();
        cartridges::register(&mut r);
        assert_eq!(r.pick("archive.zip").id(), "other");
        assert_eq!(r.pick("Makefile").id(), "other");

        // 受け皿が無くても panic せず、組み込みの Fallback を使う。
        let empty = PreviewRegistry::default();
        assert_eq!(empty.pick("archive.zip").id(), "fallback");
        assert_eq!(empty.pick("Makefile").id(), "fallback");
    }

    #[test]
    fn higher_priority_wins_and_tie_keeps_first() {
        let mut r = PreviewRegistry::default();
        r.add(Dummy {
            id: "low",
            exts: &["png"],
            priority: 0,
        });
        r.add(Dummy {
            id: "high",
            exts: &["png"],
            priority: 10,
        });
        r.add(Dummy {
            id: "tie",
            exts: &["png"],
            priority: 10,
        });
        assert_eq!(r.pick("a.png").id(), "high");

        // 先に高い方を足しても、同点なら先の方が勝つ。
        let mut r2 = PreviewRegistry::default();
        r2.add(Dummy {
            id: "first",
            exts: &["png"],
            priority: 5,
        });
        r2.add(Dummy {
            id: "second",
            exts: &["png"],
            priority: 5,
        });
        r2.add(Dummy {
            id: "lower",
            exts: &["png"],
            priority: 1,
        });
        assert_eq!(r2.pick("a.png").id(), "first");
    }

    #[test]
    fn override_beats_priority() {
        let mut r = PreviewRegistry::default();
        r.add(Dummy {
            id: "low",
            exts: &["png"],
            priority: 0,
        });
        r.add(Dummy {
            id: "high",
            exts: &["png"],
            priority: 100,
        });
        assert_eq!(r.pick("a.png").id(), "high");

        r.set_override("PNG", "low");
        assert_eq!(r.pick("a.png").id(), "low");
        assert_eq!(r.pick("A.PNG").id(), "low");

        // 存在しない id の上書きは無視して通常の優先順位に戻る。
        r.set_override("png", "missing");
        assert_eq!(r.pick("a.png").id(), "high");
    }
}
