//! ファイルのプレビューを「カートリッジ」として差し替え・追加できる仕組み。

pub mod cartridges;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use bevy::asset::RenderAssetUsages;
use bevy::math::Rect;
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};

use crate::render::shapes::{Painter, over};

/// カートリッジが紙の内側を描くための材料。
pub struct DrawContext<'a, 'b, 'w, 's> {
    pub painter: &'a mut Painter<'b, 'w, 's>,
    pub name: &'a str,
    /// 紙の内側の四角。
    pub inner: Rect,
    /// 紙の色（背景に重ねた結果）。
    pub page: Color,
    /// 紙の元の色。
    pub paper: Color,
    pub z: f32,
    /// 裏で読めた画像と、その縦横（まだ・読めないときは None）。
    pub loaded: Option<(Handle<Image>, UVec2)>,
}

/// カートリッジが裏で読んだ小さな RGBA の絵。
pub struct LoadedPreview {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
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

/// 紙の形の背景（右上が折れている）と、選んだカートリッジの絵を描く。`background` は下の色。
pub fn draw(
    registry: &PreviewRegistry,
    painter: &mut Painter,
    name: &str,
    frame: Rect,
    background: Color,
    z: f32,
    loaded: Option<(Handle<Image>, UVec2)>,
) {
    let paper = Color::srgb(0.85, 0.87, 0.95);
    let page = over(background, paper, 0.10);
    let fold = over(background, paper, 0.22);
    let c = frame.height() * 0.15;
    painter.rect(
        Rect::new(frame.min.x, frame.min.y, frame.max.x - c, frame.max.y),
        page,
        z,
    );
    painter.rect(
        Rect::new(frame.max.x - c, frame.min.y, frame.max.x, frame.max.y - c),
        page,
        z,
    );
    painter.triangle(
        [
            Vec2::new(frame.max.x - c, frame.max.y),
            Vec2::new(frame.max.x - c, frame.max.y - c),
            Vec2::new(frame.max.x, frame.max.y - c),
        ],
        fold,
        z,
    );

    let pad = frame.height() * 0.14;
    let inner = Rect {
        min: frame.min + pad,
        max: frame.max - pad,
    };
    let mut ctx = DrawContext {
        painter,
        name,
        inner,
        page,
        paper,
        z: z + 0.05,
        loaded,
    };
    registry.pick(name).draw(&mut ctx);
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

/// 開いているフォルダが変わった・子の数が変わったときだけ、直接の子のファイルの読みを頼む。
/// 読めた絵が届いたら、場面の描き直しを 1 回頼む。
pub fn update_previews(
    mut registry: ResMut<PreviewRegistry>,
    mut assets: ResMut<Assets<Image>>,
    mut sync: ResMut<crate::view::SyncRequest>,
    tree: Res<crate::model::tree::FileTree>,
    nav: Res<crate::view::Nav>,
    mut last: Local<Option<(usize, usize)>>,
) {
    if registry.images.receive(&mut assets) {
        sync.request_redraw();
    }
    let folder = crate::view::scene_path(&nav, &tree)
        .last()
        .map(|l| l.node)
        .unwrap_or(crate::model::tree::FileTree::ROOT);
    let sig = (folder, tree.nodes[folder].children.len());
    if *last == Some(sig) {
        return;
    }
    *last = Some(sig);
    let files: Vec<(PathBuf, String)> = tree.nodes[folder]
        .children
        .iter()
        .filter(|&&c| !tree.nodes[c].is_dir)
        .filter_map(|&c| {
            let path = crate::ui::tooltip::local_path(&tree, c)?;
            Some((path, tree.nodes[c].name.clone()))
        })
        .collect();
    let picked: Vec<(PathBuf, Arc<dyn Cartridge>)> = files
        .into_iter()
        .filter_map(|(path, name)| {
            let c = registry.pick_arc(&name).filter(|c| c.can_load(&name))?;
            Some((path, c.clone()))
        })
        .collect();
    registry.images.request(picked);
}

/// 中身が手元に無い（iCloud などのクラウドにだけある）ファイルか。
#[cfg(target_os = "macos")]
fn cloud_only(meta: &std::fs::Metadata) -> bool {
    use std::os::macos::fs::MetadataExt;
    /// `SF_DATALESS`（sys/stat.h）。
    const SF_DATALESS: u32 = 0x4000_0000;
    meta.st_flags() & SF_DATALESS != 0
}

#[cfg(not(target_os = "macos"))]
fn cloud_only(_meta: &std::fs::Metadata) -> bool {
    false
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

struct Job {
    key: PreviewKey,
    cartridge: Arc<dyn Cartridge>,
}

type Done = (PreviewKey, Option<LoadedPreview>);

/// カートリッジが裏で読んだ絵の置き場（メモリだけ）。開いたフォルダの子だけ `request` で読みを頼む。
pub struct PreviewImages {
    /// パス → 今のファイルの鍵（フォルダを開き直すたびに作り直す）。
    current: HashMap<PathBuf, PreviewKey>,
    images: HashMap<PreviewKey, (Handle<Image>, UVec2)>,
    /// 入れた順（上限を超えたら先頭から捨てる）。
    order: VecDeque<PreviewKey>,
    /// 読みを頼んだ鍵（読み中・読めなかったものも含む。捨てるまで頼み直さない）。
    requested: HashSet<PreviewKey>,
    jobs: Sender<Job>,
    done: Mutex<Receiver<Done>>,
}

impl Default for PreviewImages {
    fn default() -> Self {
        let (jobs, job_rx) = channel::<Job>();
        let (done_tx, done) = channel::<Done>();
        let job_rx = Arc::new(Mutex::new(job_rx));
        for _ in 0..WORKERS {
            let (job_rx, done_tx) = (job_rx.clone(), done_tx.clone());
            std::thread::spawn(move || {
                loop {
                    let job = match job_rx.lock() {
                        Ok(rx) => rx.recv(),
                        Err(_) => return,
                    };
                    let Ok(job) = job else { return };
                    let loaded = job.cartridge.load(&job.key.path);
                    if loaded.is_none() {
                        warn!("プレビューを読めませんでした: {}", job.key.path.display());
                    }
                    if done_tx.send((job.key, loaded)).is_err() {
                        return;
                    }
                }
            });
        }
        Self {
            current: HashMap::new(),
            images: HashMap::new(),
            order: VecDeque::new(),
            requested: HashSet::new(),
            jobs,
            done: Mutex::new(done),
        }
    }
}

impl PreviewImages {
    /// 描くときに引く。読めていれば、絵と縦横。
    pub fn get(&self, path: &Path) -> Option<(Handle<Image>, UVec2)> {
        self.images.get(self.current.get(path)?).cloned()
    }

    /// 開いたフォルダの直接の子を渡す。鍵を作り直し、まだ無いものは読みを頼む。
    fn request(&mut self, files: Vec<(PathBuf, Arc<dyn Cartridge>)>) {
        self.current.clear();
        for (path, cartridge) in files {
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            // クラウドにだけあるファイルは読まない（読むとダウンロードが始まる）。
            if cloud_only(&meta) {
                continue;
            }
            let key = PreviewKey {
                path: path.clone(),
                len: meta.len(),
                mtime: meta.modified().ok(),
            };
            self.current.insert(path.clone(), key.clone());
            if self.requested.insert(key.clone()) {
                let _ = self.jobs.send(Job { key, cartridge });
            }
        }
    }

    /// 読めた絵を受け取って `Image` にする。1 枚でも入れたら true（再描画を 1 回頼む）。
    pub fn receive(&mut self, assets: &mut Assets<Image>) -> bool {
        let mut done = Vec::new();
        if let Ok(rx) = self.done.lock() {
            done.extend(rx.try_iter());
        }
        let mut changed = false;
        for (key, loaded) in done {
            let Some(l) = loaded else { continue };
            if l.rgba.len() != (l.width * l.height * 4) as usize || l.width == 0 || l.height == 0 {
                continue;
            }
            let mut image = Image::new(
                Extent3d {
                    width: l.width,
                    height: l.height,
                    depth_or_array_layers: 1,
                },
                TextureDimension::D2,
                l.rgba,
                TextureFormat::Rgba8UnormSrgb,
                RenderAssetUsages::RENDER_WORLD,
            );
            image.texture_view_descriptor = None;
            let handle = assets.add(image);
            self.images
                .insert(key.clone(), (handle, UVec2::new(l.width, l.height)));
            self.order.push_back(key);
            changed = true;
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
