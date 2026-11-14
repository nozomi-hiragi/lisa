//! ひとつのフォルダの「場面」を描く。場面はそのフォルダの直下の行だけからなり、
//! 子フォルダの枠にはキャッシュした画像を貼る（孫より下は描かない）。
//! 画面に出す本物の描画にも、キャッシュの画像を焼くときにも、同じ場面を使う。

use bevy::camera::visibility::RenderLayers;
use bevy::prelude::*;
use bevy::sprite::Anchor;
use bevy::sprite_render::ColorMaterial;

use crate::cache::FolderImages;
use crate::layout::{NodeLayout, folder_body, folder_tab};
use crate::preview;
use crate::shapes::{Painter, over};
use crate::tree::{FileTree, NodeId, format_size};

pub const BACKGROUND: Color = Color::srgb(0.08, 0.08, 0.1);

/// 場面の根。これを消すと場面の中身もまとめて消える。
#[derive(Component)]
pub struct SceneRoot;

pub struct SceneSource<'a> {
    pub tree: &'a FileTree,
    pub layouts: &'a [NodeLayout],
    pub images: &'a FolderImages,
    pub font: &'a Handle<Font>,
}

/// フォルダ `folder` の場面を作る。`scale` はこの倍率で見たときにくっきり見えるように作る。
pub fn spawn(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<ColorMaterial>,
    source: &SceneSource,
    folder: NodeId,
    layer: RenderLayers,
    scale: f32,
) -> Entity {
    let tree = source.tree;
    let parent = commands
        .spawn((
            SceneRoot,
            Transform::default(),
            Visibility::default(),
            layer.clone(),
        ))
        .id();
    let mut painter = Painter {
        commands,
        meshes,
        materials,
        parent,
        layer,
        scale,
        font: source.font.clone(),
    };

    // フォルダ自身の枠（Root には描かない）。
    let node = &tree.nodes[folder];
    let z = node.depth as f32;
    let background = folder_color(tree, folder);
    if folder != FileTree::ROOT {
        let frame = source.layouts[folder].frame;
        painter.rect(folder_tab(frame), background, z);
        painter.rect(folder_body(frame), background, z);
    }

    // 直下の行。
    for &child in &node.children {
        let l = &source.layouts[child];
        let child_node = &tree.nodes[child];
        let z = child_node.depth as f32;

        if child_node.is_dir {
            match source.images.ready(child) {
                // 画像には子の枠の形と中身が焼いてある。
                Some(image) => painter.image(l.frame, image, z),
                // まだ画像が無いときは、空の枠だけ。
                None => {
                    let color = folder_color(tree, child);
                    painter.rect(folder_tab(l.frame), color, z);
                    painter.rect(folder_body(l.frame), color, z);
                }
            }
        } else {
            preview::draw(&mut painter, &child_node.name, l.frame, background, z);
        }

        // 名前。ファイルは大きさも添える。
        let text = if child_node.is_dir {
            child_node.name.clone()
        } else {
            format!("{}  {}", child_node.name, format_size(child_node.size))
        };
        let left = l.label.min.x + l.label.height() * 0.1;
        painter.text(
            text,
            Vec2::new(left, l.label.center().y),
            Anchor::CENTER_LEFT,
            l.label.height() * 0.18,
            l.label.max.x - left,
            over(background, Color::WHITE, 0.9),
            z + 0.2,
        );
    }
    parent
}

/// フォルダの枠の色。親の枠の上に、端末の色を薄く重ねる（不透明な色として求める）。
pub fn folder_color(tree: &FileTree, id: NodeId) -> Color {
    let node = &tree.nodes[id];
    match node.parent {
        None => BACKGROUND,
        Some(parent) => {
            let hue = [210.0, 140.0, 30.0, 290.0][node.device % 4];
            over(
                folder_color(tree, parent),
                Color::hsl(hue, 0.35, 0.55),
                0.10,
            )
        }
    }
}

/// 名前に使うフォント。
#[derive(Resource)]
pub struct LabelFont(pub Handle<Font>);
