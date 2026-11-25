//! ファイルのプレビューを「カートリッジ」として差し替え・追加できる仕組み。

pub mod cartridges;

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
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
        let mut registry = PreviewRegistry::default();
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
}

/// フォルダの直接の子のファイル（ノード・名前・大きさ）をまとめた値。読みを頼み直すかを決める。
fn files_signature(tree: &crate::model::tree::FileTree, folder: usize) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for &c in &tree.nodes[folder].children {
        let node = &tree.nodes[c];
        if !node.is_dir {
            (c, &node.name, node.size).hash(&mut hasher);
        }
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
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        // クラウドにだけあるファイルは読まない（読むとダウンロードが始まる）。
        if !meta.is_file() || lisa_core::listing::cloud_only(&meta) {
            continue;
        }
        let key = PreviewKey {
            path,
            len: meta.len(),
            mtime: meta.modified().ok(),
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
    key: PreviewKey,
    cartridge: Arc<dyn Cartridge>,
}

type Done = (u64, PreviewKey, Option<LoadedPreview>);

/// カートリッジが裏で読んだ絵の置き場（メモリだけ）。開いたフォルダの子だけ `request` で読みを頼む。
pub struct PreviewImages {
    /// パス → 今のファイルの鍵（フォルダを開き直すたびに作り直す）。
    current: HashMap<PathBuf, PreviewKey>,
    capture: HashMap<PathBuf, PreviewKey>,
    images: HashMap<PreviewKey, Preview>,
    /// 入れた順（上限を超えたら先頭から捨てる）。
    order: VecDeque<PreviewKey>,
    /// 読みを頼んだ鍵（読み中・読めなかったものも含む。捨てるまで頼み直さない）。
    requested: HashMap<PreviewKey, u64>,
    ticket: u64,
    revision: u64,
    generations: Arc<[AtomicU64; 2]>,
    metadata: Sender<MetadataJob>,
    resolved: Mutex<Receiver<Resolved>>,
    capture_pending: bool,
    capture_invalidations: Vec<crate::scan::RefreshTarget>,
    skipped: Mutex<Receiver<Job>>,
    jobs: Sender<Job>,
    done: Mutex<Receiver<Done>>,
}

impl Default for PreviewImages {
    fn default() -> Self {
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
            std::thread::spawn(move || {
                loop {
                    let job = match job_rx.lock() {
                        Ok(rx) => rx.recv(),
                        Err(_) => return,
                    };
                    let Ok(job) = job else { return };
                    if active[job.scope as usize].load(Ordering::Relaxed) != job.generation {
                        if skipped_tx.send(job).is_err() {
                            return;
                        }
                        continue;
                    }
                    // macOS の読み込み（ImageIO・AVFoundation）が返す autorelease のものを、
                    // 1 枚ごとに解放する（裏のスレッドには自前の autorelease pool が無い）。
                    #[cfg(target_os = "macos")]
                    let loaded = objc2::rc::autoreleasepool(|_| job.cartridge.load(&job.key.path));
                    #[cfg(not(target_os = "macos"))]
                    let loaded = job.cartridge.load(&job.key.path);
                    if loaded.is_none() {
                        warn!("プレビューを読めませんでした: {}", job.key.path.display());
                    }
                    if done_tx.send((job.ticket, job.key, loaded)).is_err() {
                        return;
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
            capture_pending: false,
            capture_invalidations: Vec::new(),
            skipped: Mutex::new(skipped),
            jobs,
            done: Mutex::new(done),
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

    /// `from` が `to` へ動いた（名前の変更・移動・取り消し）。読めた絵の鍵を付け替えて、読み直さずに
    /// 使う（中身は同じなので、大きさと更新日時は変わらない）。フォルダなら下のファイルも付け替える。
    /// 読み直し前の古い名前でも、読み直し後の新しい名前でも、同じ絵を引けるようにする。
    pub fn moved(&mut self, from: &Path, to: &Path) {
        let rekey = |key: &PreviewKey| {
            let suffix = key.path.strip_prefix(from).ok()?;
            let path = if suffix.as_os_str().is_empty() {
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
        for (old, new) in moved {
            if let Some(preview) = self.images.remove(&old) {
                self.images.insert(new.clone(), preview);
            }
            if let Some(ticket) = self.requested.remove(&old) {
                self.requested.insert(new.clone(), ticket);
            }
            if let Some(slot) = self.order.iter_mut().find(|k| **k == old) {
                *slot = new.clone();
            }
            for key in self.current.values_mut().filter(|k| **k == old) {
                *key = new.clone();
            }
            self.current.insert(new.path.clone(), new);
        }
    }

    pub(crate) fn invalidate(&mut self, target: &crate::scan::RefreshTarget) {
        self.revision = self.revision.wrapping_add(1);
        self.generations[Scope::Current as usize].fetch_add(1, Ordering::Relaxed);
        // 撮影用には、更新範囲外の解決・デコードを続ける。
        if self.capture_pending {
            self.capture_invalidations.push(target.clone());
        }
        self.current.retain(|path, _| !target.contains(path));
        self.capture.retain(|path, _| !target.contains(path));
        self.images.retain(|key, _| !target.contains(&key.path));
        self.order.retain(|key| !target.contains(&key.path));
        self.requested.retain(|key, _| !target.contains(&key.path));
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
            Scope::Current => self.current.clear(),
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
            && matches!(scope, Scope::Capture)
        {
            self.capture_pending = false;
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
                    if matches!(scope, Scope::Capture) {
                        self.capture_pending = false;
                        self.capture_invalidations.clear();
                    }
                    continue;
                };
                let keys = match scope {
                    Scope::Current => &mut self.current,
                    Scope::Capture => {
                        if self
                            .capture_invalidations
                            .iter()
                            .any(|target| target.contains(&key.path))
                        {
                            continue;
                        }
                        &mut self.capture
                    }
                };
                keys.insert(key.path.clone(), key.clone());
                if self.images.contains_key(&key) {
                    changed = true;
                } else if !self.requested.contains_key(&key) {
                    self.ticket = self.ticket.wrapping_add(1);
                    self.requested.insert(key.clone(), self.ticket);
                    let _ = self.jobs.send(Job {
                        scope,
                        generation,
                        ticket: self.ticket,
                        key,
                        cartridge,
                    });
                }
            }
        }
        // 同じ鍵を別の依頼でも待っていれば、今の世代の仕事として戻す。
        if let Ok(rx) = self.skipped.get_mut() {
            for mut job in rx.try_iter() {
                if self.requested.get(&job.key) != Some(&job.ticket) {
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
            if self.requested.get(&key) != Some(&ticket) {
                continue;
            }
            self.order.push_back(key.clone());
            let preview = match loaded {
                None => continue,
                Some(LoadedPreview::Text { lines }) => Preview::Text(Arc::new(lines)),
                Some(LoadedPreview::Image {
                    width,
                    height,
                    rgba,
                }) => {
                    if rgba.len() != (width * height * 4) as usize || width == 0 || height == 0 {
                        continue;
                    }
                    let mut image = Image::new(
                        Extent3d {
                            width,
                            height,
                            depth_or_array_layers: 1,
                        },
                        TextureDimension::D2,
                        rgba,
                        TextureFormat::Rgba8UnormSrgb,
                        RenderAssetUsages::RENDER_WORLD,
                    );
                    image.texture_view_descriptor = None;
                    Preview::Image(assets.add(image), UVec2::new(width, height))
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
mod tests {
    use super::*;

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
            requested: HashMap::from([(key.clone(), 1), (sibling_key, 2)]),
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
        images.requested.insert(key.clone(), 3);
        sender
            .send((
                1,
                key.clone(),
                Some(LoadedPreview::Text {
                    lines: vec!["old".into()],
                }),
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
                Some(LoadedPreview::Text {
                    lines: vec!["new".into()],
                }),
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
