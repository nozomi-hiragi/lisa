//! フォルダの中身を画像にキャッシュする。
//!
//! 画面に映らないカメラで、フォルダの場面を1枚の画像に描く（render to texture）。
//! 深い階層のカメラから先に描くので、親の画像には子の焼き上がった画像が入る。
//! 焼くのは、並べてあるもの（基準のフォルダの周り、`layout.rs`）だけ。並ばなくなったものは捨てる。
//! 焼き直している間は、前の画像をそのまま見せる。
//!
//! 画像ごとに鍵（`Key`）を持つ。鍵は、その画像に描かれるものから作るハッシュ: 見た目の設定
//! （画像の大きさ、耳の大きさ、描き方の版）と、子の種類・名前・状態、画像を持つ子はその子の鍵。
//! 鍵が変わったものだけを作り直す。焼いた画像は GPU から読み戻して、WebP（非可逆）でディスクに
//! 残す（ファイル名が鍵）。同じ鍵のファイルがあれば、焼かずにそれを読む。
//! いずれ鍵はフォルダのハッシュ（`docs/schema.md` の `folder_image`）にする。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, SystemTime};

use bevy::asset::RenderAssetUsages;
use bevy::camera::RenderTarget;
use bevy::camera::visibility::RenderLayers;
use bevy::prelude::*;
use bevy::render::gpu_readback::{ReadbackComplete, ReadbackOnce};
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat, TextureUsages};
use bevy::sprite_render::ColorMaterial;
use bevy::window::RequestRedraw;

use crate::layout::{BODY_ASPECT, Layouts};
use crate::model::tree::{FileTree, LoadState, NodeId};
use crate::render::ear::EarSizes;
use crate::render::scene::{self, LabelFont, SceneSource};

/// 画像の幅（ピクセル）。高さは本体の縦横比（√2:1）に合わせる。
const IMAGE_WIDTH: u32 = 1024;
/// 一度に持つ画像の数の上限。
const MAX_IMAGES: usize = 300;
/// 一度に焼く画像の数の上限。焼く画像ごとにカメラを 1 つ立てるので、一度に何十枚も焼くと、
/// 弱い GPU（Windows の古い Intel など）ではそのフレームが何秒も止まる。深いものから順に分けて焼く。
const BAKE_BATCH: usize = 8;
/// 焼くカメラを何フレーム残すか（描き終わるまでの余裕）。
const BAKE_FRAMES: u32 = 3;
/// 描き方の版。描き方を変えたら上げる（ディスクの古い画像を使わなくなる）。
const DRAW_VERSION: u32 = 1;
/// 使わないまま、これより経った画像は消す。
const KEEP_UNUSED: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// 画像のキャッシュの大きさの上限（超えた分は、使っていない古いものから消す）。
const KEEP_BYTES: u64 = 1 << 30;
/// WebP の品質（0〜100）。
const WEBP_QUALITY: f32 = 85.0;

/// 画像の鍵（描かれるもののハッシュ）。
pub type Key = [u8; 32];

fn image_height() -> u32 {
    (IMAGE_WIDTH as f32 / BODY_ASPECT).round() as u32
}

struct Slot {
    image: Handle<Image>,
    /// 今の画像（か、作っている途中の画像）の鍵。まだ何も無ければ None。
    key: Option<Key>,
    /// `image` が焼ける入れ物（ディスクから読んだ画像は焼けないので、焼くときに作り直す）。
    target: bool,
}

/// フォルダごとの画像。
#[derive(Resource, Default)]
pub struct FolderImages {
    slots: HashMap<NodeId, Slot>,
    /// 中身ができているもの。
    baked: HashSet<NodeId>,
}

impl FolderImages {
    /// 中身ができていれば画像を返す。
    pub fn ready(&self, id: NodeId) -> Option<Handle<Image>> {
        if self.baked.contains(&id) {
            self.slots.get(&id).map(|slot| slot.image.clone())
        } else {
            None
        }
    }

    /// 並べてあるフォルダ（Root 以外）のうち、基準から 2 段下までの画像の入れ物をそろえる
    /// （それより下は、画像の中でも見分けられないほど小さい）。浅いものから `MAX_IMAGES` まで。
    /// 並ばなくなったものは捨てる。
    pub fn prepare(&mut self, tree: &FileTree, layouts: &Layouts, assets: &mut Assets<Image>) {
        let until = tree.nodes[layouts.anchor].depth + 2;
        let mut candidates: Vec<NodeId> = layouts
            .ids()
            .filter(|&id| {
                let node = &tree.nodes[id];
                id != FileTree::ROOT && node.is_dir && node.depth <= until
            })
            .collect();
        candidates.sort_by_key(|&id| (tree.nodes[id].depth, id));
        candidates.truncate(MAX_IMAGES);
        let wanted: HashSet<NodeId> = candidates.into_iter().collect();
        self.slots.retain(|id, _| wanted.contains(id));
        self.baked.retain(|id| wanted.contains(id));
        for id in wanted {
            self.slots.entry(id).or_insert_with(|| Slot {
                image: assets.add(target_image()),
                key: None,
                target: true,
            });
        }
    }
}

/// 焼ける入れ物（読み戻せるように COPY_SRC も付ける）。
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

/// ディスクから読んだ画像（焼いた画像と同じ形式、同じバイト列）。
fn loaded_image(rgba: Vec<u8>) -> Image {
    let mut image = Image::new(
        Extent3d {
            width: IMAGE_WIDTH,
            height: image_height(),
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        rgba,
        TextureFormat::Rgba8Unorm,
        RenderAssetUsages::RENDER_WORLD,
    );
    image.texture_view_descriptor = None;
    image
}

/// ディスクの画像の置き場と、書く係・読む係とのやりとり。
#[derive(Resource)]
pub struct ImageCache {
    dir: Option<PathBuf>,
    save: Mutex<Sender<(Key, Vec<u8>)>>,
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
        let (save, receiver) = channel::<(Key, Vec<u8>)>();
        let writer_dir = dir.clone();
        std::thread::Builder::new()
            .name("lisa-image-writer".into())
            .spawn(move || {
                for (key, rgba) in receiver {
                    if let Some(dir) = &writer_dir {
                        write_webp(dir, &key, &rgba);
                    }
                }
            })
            .expect("画像を書く係を作る");
        Self {
            dir,
            save: Mutex::new(save),
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

fn write_webp(dir: &std::path::Path, key: &Key, rgba: &[u8]) {
    let expected = (IMAGE_WIDTH * image_height() * 4) as usize;
    if rgba.len() != expected {
        warn!(
            "読み戻した画像の大きさが違います: {} / {expected}",
            rgba.len()
        );
        return;
    }
    let encoded = webp::Encoder::from_rgba(rgba, IMAGE_WIDTH, image_height()).encode(WEBP_QUALITY);
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

/// 見た目の設定のハッシュ（鍵の元のひとつ）。
fn look(ears: &EarSizes) -> Key {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&DRAW_VERSION.to_le_bytes());
    hasher.update(&IMAGE_WIDTH.to_le_bytes());
    for value in [ears.fitted_width, ears.selected_px, ears.preview_px] {
        hasher.update(&value.to_bits().to_le_bytes());
    }
    *hasher.finalize().as_bytes()
}

/// 画像を持つフォルダの鍵を、深いものから順に作る。
fn keys(tree: &FileTree, ids: &[NodeId], look: &Key) -> HashMap<NodeId, Key> {
    let mut order = ids.to_vec();
    order.sort_by_key(|&id| std::cmp::Reverse(tree.nodes[id].depth));
    let mut keys: HashMap<NodeId, Key> = HashMap::new();
    for id in order {
        let node = &tree.nodes[id];
        let mut hasher = blake3::Hasher::new();
        hasher.update(look);
        hasher.update(&(node.device as u64).to_le_bytes());
        hasher.update(&[node.loaded as u8]);
        for &child in &node.children {
            let c = &tree.nodes[child];
            hasher.update(&[c.is_dir as u8, state_byte(c.state)]);
            hasher.update(&(c.name.len() as u64).to_le_bytes());
            hasher.update(c.name.as_bytes());
            match keys.get(&child) {
                Some(key) => hasher.update(key),
                None => hasher.update(&[0]),
            };
        }
        keys.insert(id, *hasher.finalize().as_bytes());
    }
    keys
}

fn state_byte(state: LoadState) -> u8 {
    match state {
        LoadState::Unloaded => 1,
        LoadState::Loading => 2,
        LoadState::Loaded => 3,
        LoadState::Stale => 4,
        LoadState::Failed => 5,
    }
}

/// ディスクから読んだ画像（読めなければ None）。
type Loaded = (NodeId, Key, Option<Vec<u8>>);

/// 作る作業の進み具合。
#[derive(Resource)]
pub enum Bake {
    /// フォントの読み込み待ち（と、何を作るかを決める前）。
    Waiting,
    /// ディスクから読んでいる最中。読み終えたら、残り（`to_bake`）を焼く。
    Loading {
        results: Mutex<Receiver<Loaded>>,
        remaining: usize,
        to_bake: Vec<NodeId>,
    },
    /// 焼いている最中。
    Rendering {
        frames: u32,
        entities: Vec<Entity>,
        /// 焼き終えたら読み戻してディスクに残すもの。
        saves: Vec<(Key, Handle<Image>)>,
        /// このあと焼くもの（`BAKE_BATCH` を超えた分。浅いものほど後ろ）。
        rest: Vec<NodeId>,
    },
    Done,
}

/// 焼き上がった直後の1回だけ存在する。
#[derive(Resource)]
pub struct BakeFinished;

#[allow(clippy::too_many_arguments)]
pub fn bake(
    mut commands: Commands,
    mut bake: ResMut<Bake>,
    mut images: ResMut<FolderImages>,
    cache: Res<ImageCache>,
    mut assets: ResMut<Assets<Image>>,
    fonts: Res<Assets<Font>>,
    font: Res<LabelFont>,
    tree: Res<FileTree>,
    layouts: Res<Layouts>,
    ears: Res<EarSizes>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<ColorMaterial>>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    match bake.as_mut() {
        Bake::Waiting => {
            redraw.write(RequestRedraw);
            if !fonts.contains(&font.0) {
                return;
            }
            // 鍵が変わったものだけを作り直す。
            let ids: Vec<NodeId> = images.slots.keys().copied().collect();
            let new_keys = keys(&tree, &ids, &look(&ears));
            let mut to_load = Vec::new();
            let mut to_bake = Vec::new();
            for (id, key) in new_keys {
                let slot = images.slots.get_mut(&id).expect("入れ物");
                if slot.key == Some(key) {
                    continue;
                }
                slot.key = Some(key);
                match cache.path(&key).filter(|p| p.exists()) {
                    Some(path) => to_load.push((id, key, path)),
                    None => to_bake.push(id),
                }
            }
            if to_load.is_empty() {
                *bake = start_baking(
                    &mut commands,
                    &mut images,
                    &mut assets,
                    &mut meshes,
                    &mut materials,
                    &tree,
                    &layouts,
                    &font,
                    &ears,
                    to_bake,
                );
                return;
            }
            let (sender, receiver) = channel();
            let remaining = to_load.len();
            std::thread::Builder::new()
                .name("lisa-image-reader".into())
                .spawn(move || {
                    for (id, key, path) in to_load {
                        if sender.send((id, key, read_webp(&path))).is_err() {
                            return;
                        }
                    }
                })
                .expect("画像を読む係を作る");
            *bake = Bake::Loading {
                results: Mutex::new(receiver),
                remaining,
                to_bake,
            };
        }
        Bake::Loading {
            results,
            remaining,
            to_bake,
        } => {
            redraw.write(RequestRedraw);
            let results = results.get_mut().expect("読んだ画像");
            for (id, key, rgba) in results.try_iter() {
                *remaining -= 1;
                let Some(slot) = images.slots.get_mut(&id) else {
                    continue;
                };
                if slot.key != Some(key) {
                    continue;
                }
                match rgba {
                    Some(rgba) => {
                        slot.image = assets.add(loaded_image(rgba));
                        slot.target = false;
                        images.baked.insert(id);
                    }
                    // 壊れていたら焼き直す（書き直される）。
                    None => to_bake.push(id),
                }
            }
            if *remaining > 0 {
                return;
            }
            let to_bake = std::mem::take(to_bake);
            *bake = start_baking(
                &mut commands,
                &mut images,
                &mut assets,
                &mut meshes,
                &mut materials,
                &tree,
                &layouts,
                &font,
                &ears,
                to_bake,
            );
            if matches!(*bake, Bake::Done) {
                commands.insert_resource(BakeFinished);
            }
        }
        Bake::Rendering {
            frames,
            entities,
            saves,
            rest,
        } => {
            redraw.write(RequestRedraw);
            *frames += 1;
            if *frames == BAKE_FRAMES - 1 {
                // 描き終えた画像を読み戻して、ディスクに残す。
                for (key, image) in saves.drain(..) {
                    let save = cache.save.lock().expect("画像を書く係").clone();
                    commands.spawn(ReadbackOnce::texture(image)).observe(
                        move |event: On<ReadbackComplete>, mut commands: Commands| {
                            let _ = save.send((key, event.data.clone()));
                            commands.entity(event.entity).despawn();
                        },
                    );
                }
            }
            if *frames < BAKE_FRAMES {
                return;
            }
            for entity in entities.drain(..) {
                commands.entity(entity).despawn();
            }
            let rest = std::mem::take(rest);
            *bake = start_baking(
                &mut commands,
                &mut images,
                &mut assets,
                &mut meshes,
                &mut materials,
                &tree,
                &layouts,
                &font,
                &ears,
                rest,
            );
            if matches!(*bake, Bake::Done) {
                commands.insert_resource(BakeFinished);
            }
        }
        Bake::Done => {}
    }
}

/// `targets` を焼き始める。何も無ければ `Done`。
#[allow(clippy::too_many_arguments)]
fn start_baking(
    commands: &mut Commands,
    images: &mut FolderImages,
    assets: &mut Assets<Image>,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<ColorMaterial>,
    tree: &FileTree,
    layouts: &Layouts,
    font: &LabelFont,
    ears: &EarSizes,
    mut targets: Vec<NodeId>,
) -> Bake {
    if targets.is_empty() {
        return Bake::Done;
    }
    // 子は親より先に焼く（親の場面には子の焼き上がった画像を貼る）。深いものから `BAKE_BATCH` 枚だけ。
    targets.sort_by_key(|&id| (std::cmp::Reverse(tree.nodes[id].depth), id));
    let rest = targets.split_off(targets.len().min(BAKE_BATCH));
    // ディスクから読んだ画像には焼けないので、入れ物を作り直す。
    let mut saves = Vec::new();
    for &id in &targets {
        let slot = images.slots.get_mut(&id).expect("入れ物");
        if !slot.target {
            slot.image = assets.add(target_image());
            slot.target = true;
        }
        if let Some(key) = slot.key {
            saves.push((key, slot.image.clone()));
        }
    }
    // 親の場面には子の画像を貼る。子は同じフレームの先に焼かれる。
    images.baked.extend(targets.iter().copied());
    let handles: Vec<(NodeId, Handle<Image>)> = targets
        .iter()
        .map(|&id| (id, images.slots[&id].image.clone()))
        .collect();
    let source = SceneSource {
        tree,
        layouts,
        images,
        font: &font.0,
        ears,
        // 重複の印は画像に焼かない（選ぶたびに変わるので、本物の場面にだけ描く）。
        highlight: None,
    };
    let mut entities = Vec::new();
    for (i, (id, image)) in handles.into_iter().enumerate() {
        // `layer` は const で 64 個までしか作れないので、`from_layers` を使う。
        let layer = RenderLayers::from_layers(&[i + 1]);
        let frame = layouts[id].frame;
        let scale = frame.width() / IMAGE_WIDTH as f32;
        entities.push(scene::spawn(
            commands,
            meshes,
            materials,
            &source,
            id,
            layer.clone(),
            scale,
            false,
            &[],
            None,
        ));
        entities.push(
            commands
                .spawn((
                    Camera2d,
                    Camera {
                        // 深いフォルダほど先に描く。
                        order: -1000 - tree.nodes[id].depth as isize,
                        clear_color: ClearColorConfig::Custom(Color::NONE),
                        ..default()
                    },
                    RenderTarget::Image(image.into()),
                    Projection::Orthographic(OrthographicProjection {
                        scale,
                        ..OrthographicProjection::default_2d()
                    }),
                    Transform::from_translation(frame.center().extend(0.0)),
                    layer,
                ))
                .id(),
        );
    }
    Bake::Rendering {
        frames: 0,
        entities,
        saves,
        rest,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn keys_change_when_a_child_changes() {
        let mut tree = FileTree::deep(3);
        let dirs: Vec<NodeId> = (1..tree.nodes.len())
            .filter(|&id| tree.nodes[id].is_dir)
            .collect();
        let look = [7u8; 32];
        let before = keys(&tree, &dirs, &look);
        assert_eq!(before, keys(&tree, &dirs, &look));
        // いちばん深いフォルダの子の名前を変えると、そのフォルダと祖先の鍵が変わる。
        let deepest = *dirs.iter().max_by_key(|&&id| tree.nodes[id].depth).unwrap();
        let child = tree.nodes[deepest].children[0];
        tree.nodes[child].name.push('x');
        let after = keys(&tree, &dirs, &look);
        for &id in &dirs {
            assert_ne!(before[&id], after[&id]);
        }
        // 見た目の設定が変わっても変わる。
        let other = keys(&tree, &dirs, &[8u8; 32]);
        assert_ne!(after[&deepest], other[&deepest]);
    }
}
