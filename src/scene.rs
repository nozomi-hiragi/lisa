//! ひとつのフォルダの「場面」を描く。場面はそのフォルダの直下の行だけからなり、
//! 子フォルダの枠にはキャッシュした画像を貼る（孫より下は描かない）。
//! 画面に出す本物の描画にも、キャッシュの画像を焼くときにも、同じ場面を使う。

use bevy::camera::visibility::RenderLayers;
use bevy::math::Rect;
use bevy::prelude::*;
use bevy::sprite::Anchor;
use bevy::sprite_render::ColorMaterial;

use crate::cache::FolderImages;
use crate::layout::{MAX_ROWS, NodeLayout, contents_area, folder_body, folder_tab};
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

/// 行の見せ方。
#[derive(Clone, Copy)]
pub enum Rows {
    /// 入る分だけ先頭から並べ、はみ出すときは最後の行を「…ほか N 件」にする（画像用）。
    Summary,
    /// 指定した行から MAX_ROWS 行ぶんを並べる（スクロール中の本物の場面用）。
    Scroll(usize),
}

/// フォルダ `folder` の場面を作る。`scale` はこの倍率で見たときにくっきり見えるように作る。
/// `shift` は場面全体を上下にずらす量（親の場面がスクロールしていた分）。
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<ColorMaterial>,
    source: &SceneSource,
    folder: NodeId,
    layer: RenderLayers,
    scale: f32,
    shift: f32,
    rows: Rows,
) -> Entity {
    let tree = source.tree;
    let root = commands
        .spawn((
            SceneRoot,
            Transform::from_xyz(0.0, shift, 0.0),
            Visibility::default(),
            layer.clone(),
        ))
        .id();
    let mut painter = Painter {
        commands,
        meshes,
        materials,
        parent: root,
        layer: layer.clone(),
        scale,
        font: source.font.clone(),
    };

    // フォルダ自身の枠（Root には描かない）。
    let node = &tree.nodes[folder];
    let z = node.depth as f32;
    let background = folder_color(tree, folder);
    let frame = source.layouts[folder].frame;
    if folder != FileTree::ROOT {
        painter.rect(folder_tab(frame), background, z);
        painter.rect(folder_body(frame), background, z);
    }

    let children = &node.children;
    if children.is_empty() {
        return root;
    }
    let count = children.len();
    let row_height = source.layouts[children[0]].row.height();

    // どの行を描くか。スクロールしているときは、行全体を上へずらす。
    let (first, shown, more) = match rows {
        Rows::Summary if count > MAX_ROWS => (0, MAX_ROWS - 1, Some(count - (MAX_ROWS - 1))),
        Rows::Summary => (0, count, None),
        Rows::Scroll(first) => (first, count.min(first + MAX_ROWS) - first, None),
    };
    painter.parent = painter
        .commands
        .spawn((
            ChildOf(root),
            Transform::from_xyz(0.0, first as f32 * row_height, 0.0),
            Visibility::default(),
            layer,
        ))
        .id();

    // 直下の行。
    for &child in &children[first..first + shown] {
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
        label(&mut painter, &l.label, text, background, 0.9, z);
    }

    // はみ出した分の数。
    if let Some(more) = more {
        let l = &source.layouts[children[MAX_ROWS - 1]];
        let z = node.depth as f32 + 1.0;
        label(
            &mut painter,
            &l.label,
            format!("…ほか {more} 件"),
            background,
            0.5,
            z,
        );
    }

    // スクロールできるときは、右端に今どこを見ているかの棒を出す。
    if let Rows::Scroll(first) = rows
        && count > MAX_ROWS
    {
        painter.parent = root;
        let area = contents_area(frame);
        let pad = frame.height() * 0.04;
        let x = area.max.x + pad * 0.35;
        let track = Rect::new(x, area.min.y, x + pad * 0.3, area.max.y);
        let top = area.max.y - area.height() * first as f32 / count as f32;
        let height = area.height() * MAX_ROWS as f32 / count as f32;
        painter.rect(track, over(background, Color::WHITE, 0.08), z + 0.3);
        painter.rect(
            Rect::new(track.min.x, top - height, track.max.x, top),
            over(background, Color::WHITE, 0.35),
            z + 0.4,
        );
    }
    root
}

/// 行の左に名前を書く。
fn label(
    painter: &mut Painter,
    rect: &Rect,
    text: String,
    background: Color,
    strength: f32,
    z: f32,
) {
    let left = rect.min.x + rect.height() * 0.1;
    painter.text(
        text,
        Vec2::new(left, rect.center().y),
        Anchor::CENTER_LEFT,
        rect.height() * 0.18,
        rect.max.x - left,
        over(background, Color::WHITE, strength),
        z + 0.2,
    );
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
