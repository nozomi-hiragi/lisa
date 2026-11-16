//! フォルダの中身を画像にキャッシュする。
//!
//! 画面に映らないカメラで、フォルダの場面を1枚の画像に描く（render to texture）。
//! 深い階層のカメラから先に描くので、親の画像には子の焼き上がった画像が入る。
//! 焼くのは、並べてあるもの（基準のフォルダの周り、`layout.rs`）だけ。基準が変わったら、
//! 新しく並んだものの入れ物を足し、並ばなくなったものは捨てて、まとめて焼き直す。
//! 焼き直している間は、前に焼いた画像をそのまま見せる（同じ画像に上書きする）。
//!
//! 焼き直すのは、中身が変わったもの（`stale`）だけ。フォルダの画像は、そのフォルダより下の木
//! だけで決まる（並べ方は子の数と名前順だけで決まり、画像は本体に対する相対の位置で描く）ので、
//! フォルダが読めたら、そのフォルダと祖先だけを焼き直せばよい。基準を付け替えても中身は同じ。
//! 耳の高さの設定が変わったときだけ、全部を焼き直す。

use std::collections::{HashMap, HashSet};

use bevy::camera::RenderTarget;
use bevy::camera::visibility::RenderLayers;
use bevy::prelude::*;
use bevy::render::render_resource::TextureFormat;
use bevy::sprite_render::ColorMaterial;
use bevy::window::RequestRedraw;

use crate::layout::{BODY_ASPECT, Layouts};
use crate::model::tree::{FileTree, NodeId};
use crate::render::ear::EarSizes;
use crate::render::scene::{self, LabelFont, SceneSource};

/// 画像の幅（ピクセル）。高さは本体の縦横比（√2:1）に合わせる。
const IMAGE_WIDTH: u32 = 1024;
/// 一度に持つ画像の数の上限。
const MAX_IMAGES: usize = 300;
/// 焼くカメラを何フレーム残すか（描き終わるまでの余裕）。
const BAKE_FRAMES: u32 = 3;

/// フォルダごとの画像。キーは今はノードの番号だが、いずれフォルダのハッシュにする。
#[derive(Resource, Default)]
pub struct FolderImages {
    images: HashMap<NodeId, Handle<Image>>,
    /// 一度でも焼き上がったもの。
    baked: HashSet<NodeId>,
    /// 次に焼くときに焼き直すもの（入れ物を作ったばかりのものと、中身が変わったもの）。
    stale: HashSet<NodeId>,
}

impl FolderImages {
    /// 焼き上がっていれば画像を返す。
    pub fn ready(&self, id: NodeId) -> Option<Handle<Image>> {
        if self.baked.contains(&id) {
            self.images.get(&id).cloned()
        } else {
            None
        }
    }

    /// フォルダの中身が変わった（読めた、状態が変わった）。そのフォルダと祖先を焼き直す。
    pub fn mark_changed(&mut self, tree: &FileTree, id: NodeId) {
        let mut cur = Some(id);
        while let Some(i) = cur {
            self.stale.insert(i);
            cur = tree.nodes[i].parent;
        }
    }

    /// 全部を焼き直す（見た目の設定が変わったとき）。
    pub fn mark_all(&mut self) {
        self.stale.extend(self.images.keys().copied());
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
        self.images.retain(|id, _| wanted.contains(id));
        self.baked.retain(|id| wanted.contains(id));
        let height = (IMAGE_WIDTH as f32 / BODY_ASPECT).round() as u32;
        for id in wanted {
            let stale = &mut self.stale;
            self.images.entry(id).or_insert_with(|| {
                stale.insert(id);
                assets.add(Image::new_target_texture(
                    IMAGE_WIDTH,
                    height,
                    TextureFormat::Rgba8Unorm,
                    Some(TextureFormat::Rgba8UnormSrgb),
                ))
            });
        }
    }
}

/// 焼く作業の進み具合。
#[derive(Resource)]
pub enum Bake {
    /// フォントの読み込み待ち。
    Waiting,
    /// 焼いている最中。
    Rendering {
        frames: u32,
        entities: Vec<Entity>,
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
    asset_server: Res<AssetServer>,
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
            if !asset_server.is_loaded_with_dependencies(&font.0) {
                return;
            }
            // 焼き直すのは、中身が変わったものだけ。
            let stale = std::mem::take(&mut images.stale);
            let targets: Vec<(NodeId, Handle<Image>)> = images
                .images
                .iter()
                .filter(|(id, _)| stale.contains(id))
                .map(|(&id, image)| (id, image.clone()))
                .collect();
            if targets.is_empty() {
                *bake = Bake::Done;
                return;
            }
            // 親の場面には子の画像を貼る。子は同じフレームの先に焼かれる。
            images.baked.extend(targets.iter().map(|(id, _)| *id));
            let source = SceneSource {
                tree: &tree,
                layouts: &layouts,
                images: &images,
                font: &font.0,
                ears: &ears,
            };
            let mut entities = Vec::new();
            for (i, (id, image)) in targets.into_iter().enumerate() {
                // `layer` は const で 64 個までしか作れないので、`from_layers` を使う。
                let layer = RenderLayers::from_layers(&[i + 1]);
                let frame = layouts[id].frame;
                let scale = frame.width() / IMAGE_WIDTH as f32;
                entities.push(scene::spawn(
                    &mut commands,
                    &mut meshes,
                    &mut materials,
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
                            RenderTarget::Image(image.clone().into()),
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
            *bake = Bake::Rendering {
                frames: 0,
                entities,
            };
        }
        Bake::Rendering { frames, entities } => {
            redraw.write(RequestRedraw);
            *frames += 1;
            if *frames < BAKE_FRAMES {
                return;
            }
            for entity in entities.drain(..) {
                commands.entity(entity).despawn();
            }
            *bake = Bake::Done;
            commands.insert_resource(BakeFinished);
        }
        Bake::Done => {}
    }
}
