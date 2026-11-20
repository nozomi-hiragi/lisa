//! フォルダの中身を画像（サムネ）にキャッシュする。
//!
//! フォルダを開いたとき、または中心のフォルダへ寄り始めたときに、
//! そのフォルダの本体の範囲（耳に譲った後の矩形）を裏撮影専用レイヤーだけ
//! 映す一時カメラ 1 台で 1024×724 の `Rgba8UnormSrgb` 画像へ撮り、`ReadbackOnce` で読み戻して
//! WebP に保存し、そのままメモリにも載せる。重複の印・カタログの札や暗い膜・読み込み中のスピナーは
//! `OVERLAY_LAYER` に分けてあるため、サムネには入らない。開いたフォルダ自身の外枠と耳も別レイヤーに置く。
//!
//! もう一度そのフォルダを開き直したら撮り直して上書きする。まだ一度も開いていないフォルダは
//! ディスクに前回の画像があれば裏スレッドで読み込み、無ければ画像なし（空の枠）で描く。
//!
//! 撮った直後の画像も、ディスクから読み込んだ画像も、同じ `loaded_image`（`TextureFormat::Rgba8UnormSrgb`、
//! `view_formats` 不使用で OpenGL でも安全）を通すため、色が食い違わない。

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, SystemTime};

use bevy::asset::RenderAssetUsages;
use bevy::camera::visibility::RenderLayers;
use bevy::camera::{RenderTarget, ScalingMode};
use bevy::ecs::system::SystemParam;
use bevy::prelude::*;
use bevy::render::gpu_readback::{ReadbackComplete, ReadbackOnce};
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat, TextureUsages};
use bevy::window::{PrimaryWindow, RequestRedraw};

use crate::layout::{BODY_ASPECT, Layouts, anchor_body, layout};
use crate::model::tree::{FileTree, NodeId};
use crate::preview::PreviewRegistry;
use crate::render::scene::{self, CAPTURE_LAYER, LabelFont, SceneSource, size_of, slot_of};
use crate::render::shapes::UiThemeAssets;
use crate::settings::Settings;
use crate::view::{
    Animating, CenterApproach, Fade, Nav, SceneVisit, SyncRequest, WindowOccludedState, ear_spec,
    fit, scene_path,
};

/// 画像の幅（ピクセル）。高さは本体の縦横比（√2:1）に合わせる。
const IMAGE_WIDTH: u32 = 1024;
/// メモリに保持するサムネの上限枚数。
const MAX_CACHED_IMAGES: usize = 300;
/// 撮影カメラを何フレーム動かしてから読み戻すか（描き終わるまでの余裕）。
const CAPTURE_FRAMES: u32 = 2;
/// 描き方・色空間の版。本体の中身を透明背景で撮影する。
const DRAW_VERSION: u32 = 8;
/// 使わないまま、これより経った画像は消す。
const KEEP_UNUSED: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// 画像のキャッシュの大きさの上限（超えた分は、使っていない古いものから消す）。
const KEEP_BYTES: u64 = 1 << 30;

/// フォルダごとのサムネの鍵（パスまたはリモート識別子の blake3）。
pub type Key = [u8; 32];

fn image_height() -> u32 {
    (IMAGE_WIDTH as f32 / BODY_ASPECT).round() as u32
}

/// フォルダ `id` のサムネの鍵。Root やファイルは `None`。
///
/// - 手元のフォルダ: `DRAW_VERSION` + ディスク上のパス
/// - LAN の相手のフォルダ: `DRAW_VERSION` + 端末 ID + 端末ノード以下の名前の道のり
/// - ダミーの木や端末ノードなど: `DRAW_VERSION` + Root からの名前の道のり
pub fn folder_key(tree: &FileTree, id: NodeId) -> Option<Key> {
    let node = tree.nodes.get(id)?;
    if id == FileTree::ROOT || !node.is_dir {
        return None;
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(&DRAW_VERSION.to_le_bytes());
    hasher.update(&IMAGE_WIDTH.to_le_bytes());
    if let Some(remote) = &node.remote {
        hasher.update(b"remote\0");
        hasher.update(remote.device_id.as_bytes());
        hasher.update(b"\0");
        let mut names = Vec::new();
        let mut cur = Some(id);
        while let Some(i) = cur {
            let n = &tree.nodes[i];
            let same_device = n
                .remote
                .as_ref()
                .is_some_and(|r| r.device_id == remote.device_id);
            if !same_device {
                break;
            }
            // 端末ノード自身（entry_id == None）は表示名が変わっても鍵がぶれないよう省く。
            if n.remote.as_ref().and_then(|r| r.entry_id).is_some() {
                names.push(n.name.as_str());
            }
            cur = n.parent;
        }
        names.reverse();
        for name in names {
            hasher.update(&(name.len() as u64).to_le_bytes());
            hasher.update(name.as_bytes());
        }
    } else if let Some(path) = &node.path {
        hasher.update(b"local\0");
        hasher.update(path.as_os_str().as_encoded_bytes());
    } else {
        hasher.update(b"tree\0");
        hasher.update(tree.path(id).as_bytes());
    }
    Some(*hasher.finalize().as_bytes())
}

/// フォルダごとのサムネ（鍵 → 画像ハンドル）。`NodeId` ではなく `Key` で持つため、
/// 読み直し（`reset_children`）でノード番号が変わっても画像が消えない。
#[derive(Resource, Default)]
pub struct FolderImages {
    images: HashMap<Key, Handle<Image>>,
    order: VecDeque<Key>,
    /// 起動中に一度ディスクを確認した鍵（存在しないファイルへの再 stat を防ぐ）。
    disk_checked: HashSet<Key>,
}

impl FolderImages {
    pub(crate) fn counts(&self) -> (usize, usize) {
        (self.images.len(), self.images.len())
    }

    /// フォルダ `id` のサムネがメモリにあれば返す。
    pub fn ready(&self, tree: &FileTree, id: NodeId) -> Option<Handle<Image>> {
        let key = folder_key(tree, id)?;
        self.images.get(&key).cloned()
    }

    /// サムネをメモリへ登録（既にあれば上書き）し、上限を超えたら古いものから外す。
    fn insert(&mut self, key: Key, handle: Handle<Image>) {
        let existed = self.images.insert(key, handle).is_some();
        self.disk_checked.insert(key);
        if !existed {
            self.order.push_back(key);
            while self.order.len() > MAX_CACHED_IMAGES {
                if let Some(old) = self.order.pop_front() {
                    self.images.remove(&old);
                    self.disk_checked.remove(&old);
                }
            }
        }
    }
}

/// 撮影用の描画先テクスチャ（読み戻せるように `COPY_SRC` を付ける）。
fn target_image() -> Image {
    // 中身は sRGB のまま持つ（描くのも貼るのも sRGB として扱う）。Unorm の画像に sRGB の見方を
    // 足す形（view_formats）は、OpenGL（WGPU_BACKEND=gl）では作れずに落ちるので使わない。
    let mut image = Image::new_target_texture(
        IMAGE_WIDTH,
        image_height(),
        TextureFormat::Rgba8UnormSrgb,
        None,
    );
    image.texture_descriptor.usage |= TextureUsages::COPY_SRC;
    image
}

/// 読み戻したバイト列・ディスクから読んだ WebP を貼るための画像。
/// 描画先と同じ `TextureFormat::Rgba8UnormSrgb`（`view_formats` 不使用）にそろえ、
/// 撮影直後と再読込後で色が完全に一致するようにする。
fn loaded_image(rgba: Vec<u8>) -> Image {
    let mut image = Image::new(
        Extent3d {
            width: IMAGE_WIDTH,
            height: image_height(),
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        rgba,
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::RENDER_WORLD,
    );
    image.texture_view_descriptor = None;
    image
}

/// ディスクの画像の置き場と、書く係・読む係・読み戻し結果のやりとり。
#[derive(Resource)]
pub struct ImageCache {
    dir: Option<PathBuf>,
    save: Mutex<Sender<(Key, Vec<u8>)>>,
    read_req: Mutex<Sender<(Key, PathBuf)>>,
    read_res: Mutex<Receiver<(Key, Option<Vec<u8>>)>>,
    capture_tx: Mutex<Sender<(Key, Vec<u8>)>>,
    capture_rx: Mutex<Receiver<(Key, Vec<u8>)>>,
}

impl ImageCache {
    pub fn new() -> Self {
        let dir = crate::scan::cache_dir().map(|dir| dir.join("folder-images"));
        if let Some(dir) = &dir {
            let _ = std::fs::create_dir_all(dir);
            // 使われなくなった画像を、裏で消す。
            let sweep_dir = dir.clone();
            std::thread::Builder::new()
                .name("lisa-image-sweeper".into())
                .spawn(move || {
                    lisa_core::pace::set_thread_background(true);
                    let (files, bytes) =
                        sweep(&sweep_dir, SystemTime::now(), KEEP_UNUSED, KEEP_BYTES);
                    if files > 0 {
                        info!("使われなくなった画像を {files} 枚（{bytes} バイト）消しました");
                    }
                })
                .expect("画像を掃除する係を作る");
        }
        let (save, save_rx) = channel::<(Key, Vec<u8>)>();
        let writer_dir = dir.clone();
        std::thread::Builder::new()
            .name("lisa-image-writer".into())
            .spawn(move || {
                for (key, rgba) in save_rx {
                    if let Some(dir) = &writer_dir {
                        write_webp(dir, &key, &rgba);
                    }
                }
            })
            .expect("画像を書く係を作る");

        let (read_req, read_req_rx) = channel::<(Key, PathBuf)>();
        let (read_res_tx, read_res) = channel::<(Key, Option<Vec<u8>>)>();
        std::thread::Builder::new()
            .name("lisa-image-reader".into())
            .spawn(move || {
                for (key, path) in read_req_rx {
                    if read_res_tx.send((key, read_webp(&path))).is_err() {
                        return;
                    }
                }
            })
            .expect("画像を読む係を作る");

        let (capture_tx, capture_rx) = channel::<(Key, Vec<u8>)>();
        Self {
            dir,
            save: Mutex::new(save),
            read_req: Mutex::new(read_req),
            read_res: Mutex::new(read_res),
            capture_tx: Mutex::new(capture_tx),
            capture_rx: Mutex::new(capture_rx),
        }
    }

    fn path(&self, key: &Key) -> Option<PathBuf> {
        self.dir.as_ref().map(|dir| dir.join(file_name(key)))
    }
}

fn file_name(key: &Key) -> String {
    let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
    format!("{hex}.webp")
}

/// 透明な描画先では RGB にアルファが乗っている。貼り直しでは通常のアルファ合成を使う
/// ので、sRGB を線形値に戻してから除算し、二重に暗くなるのを防ぐ。
fn straighten_alpha(rgba: &mut [u8]) {
    let linear: [f32; 256] =
        std::array::from_fn(|i| LinearRgba::from(Srgba::new(i as f32 / 255.0, 0.0, 0.0, 1.0)).red);
    for pixel in rgba.chunks_exact_mut(4) {
        let alpha = pixel[3];
        if alpha == 0 {
            pixel[..3].fill(0);
        } else if alpha < 255 {
            for channel in &mut pixel[..3] {
                let value = linear[*channel as usize] * (255.0 / f32::from(alpha));
                let nearest = linear.partition_point(|&x| x < value).clamp(1, 255);
                let prev = nearest - 1;
                *channel = if value - linear[prev] < linear[nearest] - value {
                    prev as u8
                } else {
                    nearest as u8
                };
            }
        }
    }
}

fn write_webp(dir: &std::path::Path, key: &Key, rgba: &[u8]) {
    let expected = (IMAGE_WIDTH * image_height() * 4) as usize;
    if rgba.len() != expected {
        warn!(
            "読み戻した画像の大きさが違います: {} / {expected}",
            rgba.len()
        );
        return;
    }
    let encoded = webp::Encoder::from_rgba(rgba, IMAGE_WIDTH, image_height()).encode_lossless();
    let path = dir.join(file_name(key));
    let tmp = path.with_extension("webp.tmp");
    if std::fs::write(&tmp, &*encoded).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

fn read_webp(path: &std::path::Path) -> Option<Vec<u8>> {
    let bytes = std::fs::read(path).ok()?;
    // 使った印（掃除のときの、最後に使った日時）。
    let _ = std::fs::File::options()
        .write(true)
        .open(path)
        .and_then(|f| f.set_modified(SystemTime::now()));
    let image = webp::Decoder::new(&bytes).decode()?;
    if image.width() != IMAGE_WIDTH || image.height() != image_height() {
        return None;
    }
    if image.is_alpha() {
        Some(image.to_vec())
    } else {
        // RGB しか無ければ、不透明の RGBA にする。
        Some(
            image
                .as_chunks::<3>()
                .0
                .iter()
                .flat_map(|p| [p[0], p[1], p[2], 255])
                .collect(),
        )
    }
}

/// 使われなくなった画像を消す。最後に使ってから（ファイルの更新日時。読むたびに付け直す）`keep` より
/// 経ったものと、全部で `max_bytes` を超える分の古いものから。途中で残った書きかけ（`.webp.tmp`）も
/// 消す。消すのはキャッシュのフォルダの中の画像だけ。（消した数、消した大きさ）を返す。
fn sweep(dir: &std::path::Path, now: SystemTime, keep: Duration, max_bytes: u64) -> (u64, u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (0, 0);
    };
    let mut files: Vec<(SystemTime, u64, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?;
            if !(name.ends_with(".webp") || name.ends_with(".webp.tmp")) {
                return None;
            }
            let meta = entry.metadata().ok().filter(|m| m.is_file())?;
            Some((meta.modified().ok()?, meta.len(), path))
        })
        .collect();
    // 新しいものから並べて、残す分を決める。
    files.sort_by_key(|(time, ..)| std::cmp::Reverse(*time));
    let mut kept = 0u64;
    let (mut removed, mut removed_bytes) = (0, 0);
    for (time, size, path) in files {
        let old = now.duration_since(time).unwrap_or_default() > keep;
        let tmp = path.extension().is_some_and(|e| e == "tmp");
        if old || tmp || kept + size > max_bytes {
            if std::fs::remove_file(&path).is_ok() {
                removed += 1;
                removed_bytes += size;
            }
        } else {
            kept += size;
        }
    }
    (removed, removed_bytes)
}

/// 開いているフォルダのサムネ撮影と、見える子フォルダのディスク読込の状態。
#[derive(Resource, Default)]
pub struct ThumbnailCapture {
    /// 最後に撮影し終えた `SceneVisit` 番号。
    last_visit: u64,
    /// 最後にディスク確認した `(scene_folder, children.len(), parent_children_len)`。
    last_disk_scan: Option<(NodeId, usize, usize)>,
    /// 撮影中のカメラ。
    active: Option<ActiveCapture>,
    /// ディスクから読み込み中の枚数。
    in_flight_reads: HashSet<Key>,
    in_flight_captures: HashSet<Key>,
}

struct ActiveCapture {
    visit: Option<u64>,
    folder: NodeId,
    key: Key,
    camera: Entity,
    scene: Entity,
    target: Handle<Image>,
    frames: u32,
    readback_sent: bool,
}

impl ThumbnailCapture {
    pub(crate) fn is_pending(&self, tree: &FileTree, id: NodeId) -> bool {
        folder_key(tree, id).is_some_and(|key| {
            self.in_flight_reads.contains(&key)
                || self.in_flight_captures.contains(&key)
                || self.active.as_ref().is_some_and(|active| active.key == key)
        })
    }

    pub fn status_label(&self) -> String {
        if let Some(a) = &self.active {
            format!("Capturing(f={},rb={})", a.frames, a.readback_sent)
        } else if !self.in_flight_reads.is_empty() || !self.in_flight_captures.is_empty() {
            format!(
                "Loading({})",
                self.in_flight_reads.len() + self.in_flight_captures.len()
            )
        } else {
            "Idle".to_string()
        }
    }
}

/// `update_thumbnails` が参照する描画・操作状態の束。
#[derive(SystemParam)]
pub(crate) struct ThumbContext<'w, 's> {
    fonts: Res<'w, Assets<Font>>,
    font: Res<'w, LabelFont>,
    tree: Res<'w, FileTree>,
    layouts: Res<'w, Layouts>,
    settings: Res<'w, Settings>,
    theme: Res<'w, UiThemeAssets>,
    previews: Res<'w, PreviewRegistry>,
    meshes: ResMut<'w, Assets<Mesh>>,
    materials: ResMut<'w, Assets<bevy::sprite_render::ColorMaterial>>,
    nav: Res<'w, Nav>,
    visit: Res<'w, SceneVisit>,
    approach: Res<'w, CenterApproach>,
    window: Single<'w, 's, &'static Window, With<PrimaryWindow>>,
    animating: Option<Res<'w, Animating>>,
    fades: Query<'w, 's, (), With<Fade>>,
    gesture: Res<'w, crate::input::gesture::Gesture>,
    readbacks: Query<'w, 's, (), With<ReadbackOnce>>,
    occluded: Res<'w, WindowOccludedState>,
}

/// 毎フレーム:
/// 1. 読み戻し完了したサムネ・ディスクから読めたサムネを `FolderImages` へ反映し、見えるタイルに必要なら再描画を頼む。
/// 2. 今開いている場面の直下の子（と隣）のうち、まだメモリに無くディスクにある WebP を裏で読む。
/// 3. 今開いたフォルダの中身を裏撮影レイヤーに作り、1 枚撮って保存する。
#[allow(clippy::too_many_arguments)]
pub(crate) fn update_thumbnails(
    mut commands: Commands,
    mut capture: ResMut<ThumbnailCapture>,
    mut images: ResMut<FolderImages>,
    cache: Res<ImageCache>,
    mut assets: ResMut<Assets<Image>>,
    mut sync: ResMut<SyncRequest>,
    mut redraw: MessageWriter<RequestRedraw>,
    mut ctx: ThumbContext,
) {
    let path = scene_path(&ctx.nav, &ctx.tree);
    let scene_folder = path[path.len() - 1].node;

    // 1a. 撮影の読み戻し結果を反映する。
    let mut updated_keys: Vec<Key> = Vec::new();
    if let Ok(rx) = cache.capture_rx.lock() {
        for (key, rgba) in rx.try_iter() {
            capture.in_flight_captures.remove(&key);
            let expected = (IMAGE_WIDTH * image_height() * 4) as usize;
            if rgba.len() == expected {
                let handle = assets.add(loaded_image(rgba));
                images.insert(key, handle);
                updated_keys.push(key);
            }
        }
    }

    // 1b. ディスクから読めた WebP を反映する。
    if let Ok(rx) = cache.read_res.lock() {
        for (key, opt_rgba) in rx.try_iter() {
            capture.in_flight_reads.remove(&key);
            if let Some(rgba) = opt_rgba
                && !images.images.contains_key(&key)
            {
                let handle = assets.add(loaded_image(rgba));
                images.insert(key, handle);
                updated_keys.push(key);
            }
        }
    }

    // いま画面に並んでいる子や隣に更新した鍵が含まれていれば、場面を描き直す（操作中は sync_view が待つ）。
    if !updated_keys.is_empty() && visible_contains_any(&ctx.tree, scene_folder, &updated_keys) {
        sync.request_redraw();
    }

    let busy = ctx.animating.is_some()
        || !ctx.fades.is_empty()
        || ctx.gesture.active()
        || ctx.gesture.has_parent_scene();

    // 2. 操作が止まっていて、場面のフォルダや子の数が変わったときだけディスク上の WebP を確認する。
    let parent_children_len = ctx.tree.nodes[scene_folder]
        .parent
        .map_or(0, |p| ctx.tree.nodes[p].children.len());
    let scan_sig = (
        scene_folder,
        ctx.tree.nodes[scene_folder].children.len(),
        parent_children_len,
    );
    if !busy && capture.last_disk_scan != Some(scan_sig) {
        capture.last_disk_scan = Some(scan_sig);
        let mut visible_dirs = Vec::new();
        for &c in &ctx.tree.nodes[scene_folder].children {
            if ctx.tree.nodes[c].is_dir {
                visible_dirs.push(c);
            }
        }
        if let Some(parent) = ctx.tree.nodes[scene_folder].parent {
            for &s in &ctx.tree.nodes[parent].children {
                if s != scene_folder && ctx.tree.nodes[s].is_dir {
                    visible_dirs.push(s);
                }
            }
        }
        for dir_id in visible_dirs {
            let Some(key) = folder_key(&ctx.tree, dir_id) else {
                continue;
            };
            if images.images.contains_key(&key) || !images.disk_checked.insert(key) {
                continue;
            }
            if let Some(path) = cache.path(&key).filter(|p| p.exists())
                && let Ok(tx) = cache.read_req.lock()
                && tx.send((key, path)).is_ok()
            {
                capture.in_flight_reads.insert(key);
            }
        }
    }

    // 裏読込や GPU 読み戻しが進行中なら、reactive の 1 秒待ちを待たずに次のフレームを回す。
    if (!capture.in_flight_reads.is_empty() || !ctx.readbacks.is_empty()) && !ctx.occluded.0 {
        redraw.write(RequestRedraw);
    }

    // 3. 今開いているフォルダのサムネ撮影を進める・始める。

    let requested = ctx.approach.requested();
    if let Some(active) = capture.active.as_mut() {
        let still_requested = requested == Some(active.folder);
        let current_visit = active.folder == scene_folder;
        if active.visit.is_none() && current_visit {
            active.visit = Some(ctx.visit.0);
        }
        let stale = !still_requested && (!current_visit || active.visit != Some(ctx.visit.0));
        if !active.readback_sent && stale {
            commands.entity(active.camera).despawn();
            commands.entity(active.scene).despawn();
            capture.active = None;
            return;
        }
        if !ctx.occluded.0 {
            redraw.write(RequestRedraw);
        }
        active.frames += 1;
        if active.frames == CAPTURE_FRAMES && !active.readback_sent {
            let key = active.key;
            let save = cache.save.lock().expect("画像を書く係").clone();
            let mem = cache.capture_tx.lock().expect("画像を受け取る係").clone();
            commands
                .spawn(ReadbackOnce::texture(active.target.clone()))
                .observe(move |event: On<ReadbackComplete>, mut commands: Commands| {
                    let mut data = event.data.clone();
                    straighten_alpha(&mut data);
                    let _ = save.send((key, data.clone()));
                    let _ = mem.send((key, data));
                    commands.entity(event.entity).despawn();
                });
            active.readback_sent = true;
            capture.in_flight_captures.insert(key);
            return;
        }
        if active.frames > CAPTURE_FRAMES {
            let active = capture.active.take().unwrap();
            commands.entity(active.camera).despawn();
            commands.entity(active.scene).despawn();
            if let Some(visit) = active.visit {
                capture.last_visit = visit;
            }
        }
        return;
    }

    // 開いた場所を優先し、次に今まさに寄っている子だけを撮る。
    if !ctx.fonts.contains(&ctx.font.0) {
        return;
    }
    let capture_key = |folder: NodeId| {
        let node = &ctx.tree.nodes[folder];
        if folder == FileTree::ROOT || !node.is_dir || !node.loaded || !ctx.layouts.contains(folder)
        {
            return None;
        }
        let key = folder_key(&ctx.tree, folder)?;
        (!capture.in_flight_captures.contains(&key) && !capture.in_flight_reads.contains(&key))
            .then_some(key)
    };
    let current = (ctx.visit.0 != capture.last_visit)
        .then(|| capture_key(scene_folder))
        .flatten()
        .map(|key| (scene_folder, key, Some(ctx.visit.0)));
    let target_folder = current.or_else(|| {
        let folder = requested?;
        let node = &ctx.tree.nodes[folder];
        if node.children.is_empty()
            || node.state == crate::model::tree::LoadState::Failed
            || images.ready(&ctx.tree, folder).is_some()
        {
            return None;
        }
        capture_key(folder).map(|key| (folder, key, None))
    });
    let Some((folder, key, visit)) = target_folder else {
        return;
    };
    let (spec, ears) = ear_spec(&ctx.settings, &ctx.window);
    let layouts = layout(&ctx.tree, folder, anchor_body(), spec);
    let (_, fit_scale) = fit(layouts[folder].fit, &ctx.window);
    if !(fit_scale > 0.0 && fit_scale.is_finite()) {
        return;
    }
    let slot = slot_of(&ctx.tree, &layouts, folder);
    let size = size_of(&ctx.tree, &layouts, &ears, folder, false);
    let (_, shown) = slot.yielded(size.height(fit_scale));
    if !(shown.width() > 0.0 && shown.height() > 0.0) {
        return;
    }
    let layer = RenderLayers::layer(CAPTURE_LAYER);
    let scene = commands
        .spawn((Transform::default(), Visibility::default(), layer.clone()))
        .id();
    let source = SceneSource {
        tree: &ctx.tree,
        layouts: &layouts,
        images: &images,
        previews: &ctx.previews,
        font: &ctx.font.0,
        ears: &ears,
        theme: &ctx.theme,
        highlight: None,
        catalog: None,
    };
    scene::draw_contents(
        &mut commands,
        &mut ctx.meshes,
        &mut ctx.materials,
        &source,
        folder,
        scene,
        layer.clone(),
        fit_scale,
        false,
    );
    let target = assets.add(target_image());
    let scale = shown.width() / IMAGE_WIDTH as f32;
    let cam_entity = commands
        .spawn((
            Camera2d,
            Camera {
                order: -1,
                clear_color: ClearColorConfig::Custom(Color::NONE),
                ..default()
            },
            RenderTarget::Image(target.clone().into()),
            Projection::Orthographic(OrthographicProjection {
                scale,
                // ピクセル単位に丸めた撮影先の縦横比にかかわらず、本体全体を写す。
                scaling_mode: ScalingMode::Fixed {
                    width: IMAGE_WIDTH as f32,
                    height: shown.height() / scale,
                },
                ..OrthographicProjection::default_2d()
            }),
            Transform::from_translation(shown.center().extend(0.0)),
            layer,
        ))
        .id();
    capture.active = Some(ActiveCapture {
        visit,
        folder,
        key,
        camera: cam_entity,
        scene,
        target,
        frames: 0,
        readback_sent: false,
    });
    redraw.write(RequestRedraw);
}

fn visible_contains_any(tree: &FileTree, scene_folder: NodeId, keys: &[Key]) -> bool {
    let matches = |id: NodeId| folder_key(tree, id).is_some_and(|k| keys.contains(&k));
    if tree.nodes[scene_folder]
        .children
        .iter()
        .copied()
        .any(matches)
    {
        return true;
    }
    if let Some(parent) = tree.nodes[scene_folder].parent
        && tree.nodes[parent].children.iter().copied().any(matches)
    {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transparent_capture_is_converted_to_straight_alpha() {
        let mut rgba = [188, 0, 0, 128, 255, 10, 60, 0, 10, 20, 31, 255];
        straighten_alpha(&mut rgba);
        assert!(rgba[0] >= 254);
        assert_eq!(rgba[1..], [0, 0, 128, 0, 0, 0, 0, 10, 20, 31, 255]);
    }

    #[test]
    fn sweep_removes_old_and_over_budget_images_only() {
        let dir = std::env::temp_dir().join(format!("lisa-sweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let now = SystemTime::now();
        let day = Duration::from_secs(24 * 60 * 60);
        let make = |name: &str, bytes: usize, age: Duration| {
            let path = dir.join(name);
            std::fs::write(&path, vec![0u8; bytes]).unwrap();
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(now - age)
                .unwrap();
        };
        make("new.webp", 100, Duration::ZERO);
        make("mid.webp", 100, day);
        make("older.webp", 100, 2 * day);
        make("ancient.webp", 10, 60 * day);
        make("half.webp.tmp", 10, Duration::ZERO);
        make("other.txt", 10, 60 * day);
        // 30 日より古いもの、書きかけ、250 バイトを超える分（古いほう）を消す。
        let (files, bytes) = sweep(&dir, now, 30 * day, 250);
        assert_eq!((files, bytes), (3, 120));
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["mid.webp", "new.webp", "other.txt"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn webp_round_trip_keeps_the_size() {
        let dir = std::env::temp_dir().join(format!("lisa-webp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (w, h) = (IMAGE_WIDTH as usize, image_height() as usize);
        let rgba: Vec<u8> = (0..w * h)
            .flat_map(|i| {
                let (x, y) = (i % w, i / w);
                [
                    (x / 4) as u8,
                    (y / 3) as u8,
                    120,
                    if x < 10 { 0 } else { 255 },
                ]
            })
            .collect();
        let key = [3u8; 32];
        write_webp(&dir, &key, &rgba);
        let path = dir.join(file_name(&key));
        let size = std::fs::metadata(&path).unwrap().len();
        let back = read_webp(&path).unwrap();
        assert_eq!(back.len(), rgba.len());
        assert!(size < 200_000, "{size} バイト");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn target_and_loaded_images_share_srgb_format_without_view_formats() {
        let target = target_image();
        let loaded = loaded_image(vec![0u8; (IMAGE_WIDTH * image_height() * 4) as usize]);
        assert_eq!(
            target.texture_descriptor.format,
            TextureFormat::Rgba8UnormSrgb
        );
        assert_eq!(
            loaded.texture_descriptor.format,
            TextureFormat::Rgba8UnormSrgb
        );
        assert!(target.texture_descriptor.view_formats.is_empty());
        assert!(loaded.texture_descriptor.view_formats.is_empty());
    }

    #[test]
    fn folder_key_is_stable_across_local_and_remote_nodes() {
        let mut tree = FileTree::dummy();
        assert!(folder_key(&tree, FileTree::ROOT).is_none());
        let dev = tree.nodes[FileTree::ROOT].children[0];
        let k1 = folder_key(&tree, dev).unwrap();
        let k2 = folder_key(&tree, dev).unwrap();
        assert_eq!(k1, k2);

        // リモート端末は端末 ID + 名前の道のりで決まる（端末の表示名が変わっても子フォルダの鍵は変わらない）。
        let peer =
            tree.ensure_remote_device("peer-1", "Peer A", crate::model::tree::LoadState::Loaded);
        tree.add_remote_drives(
            peer,
            vec![crate::model::tree::RemoteDrive {
                label: "C:".into(),
                entry_id: 1,
                listed: true,
                has_hash: true,
                has_error: false,
            }],
        );
        let drive = tree.nodes[peer].children[0];
        let before = folder_key(&tree, drive).unwrap();
        tree.nodes[peer].name = "Peer Renamed".into();
        let after = folder_key(&tree, drive).unwrap();
        assert_eq!(before, after);
    }
}
