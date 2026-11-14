//! ひとつのフォルダの「場面」を描く。場面はそのフォルダの直下の行だけからなり、
//! 子フォルダの枠にはキャッシュした画像を貼る（孫より下は描かない）。
//! 画面に出す本物の描画にも、キャッシュの画像を焼くときにも、同じ場面を使う。

use bevy::camera::visibility::RenderLayers;
use bevy::math::Rect;
use bevy::prelude::*;
use bevy::sprite::Anchor;
use bevy::sprite_render::ColorMaterial;

use crate::cache::FolderImages;
use crate::layout::{DIRS, FILES, NodeLayout, column_items, folder_body, folder_tab};
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
    /// 列ごとに、指定した行から入る分だけ並べる（スクロール中の本物の場面用）。
    Scroll([usize; 2]),
    /// Scroll と同じ位置に並べるが、`column` の列は `from..to` の行も描く
    /// （見えている範囲の外にある隣のフォルダを見せるため）。`skip` は描かない
    /// （上に本物が重なっているフォルダ。画像と本物が重なると透けて二重に見える）。
    Around {
        scroll: [usize; 2],
        column: usize,
        from: usize,
        to: usize,
        skip: Option<NodeId>,
    },
}

/// フォルダ `folder` の場面を作る。`scale` はこの倍率で見たときにくっきり見えるように作る。
/// `shift` は場面全体を上下にずらす量（親の場面がスクロールしていた分）。
/// `ancestors` は外側のフォルダと、それぞれのずらす量。枠の外に見える背景として描く。
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
    ancestors: &[(NodeId, f32)],
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

    // 外側のフォルダの枠。ズームの前後で、枠の外に見える色が変わらないようにする。
    for &(ancestor, ancestor_shift) in ancestors {
        if ancestor == FileTree::ROOT {
            continue;
        }
        let offset = Vec2::new(0.0, ancestor_shift - shift);
        let frame = source.layouts[ancestor].frame;
        let frame = Rect {
            min: frame.min + offset,
            max: frame.max + offset,
        };
        let color = folder_color(tree, ancestor);
        let z = tree.nodes[ancestor].depth as f32 - 0.5;
        painter.rect(folder_tab(frame), color, z);
        painter.rect(folder_body(frame), color, z);
    }

    // フォルダ自身の枠（Root には描かない）。
    let node = &tree.nodes[folder];
    let z = node.depth as f32;
    let background = folder_color(tree, folder);
    let frame = source.layouts[folder].frame;
    if folder != FileTree::ROOT {
        painter.rect(folder_tab(frame), background, z);
        painter.rect(folder_body(frame), background, z);
    }

    let list = source.layouts[folder].list;
    let row_height = list.row_height;
    let fit = list.fit;
    let layer = painter.layer.clone();

    for column in [FILES, DIRS] {
        let items = column_items(tree, folder, column);
        let count = items.len();
        if count == 0 {
            continue;
        }

        // どの行を描くか（lo..hi）。スクロールしているときは、その列の行を first 行ぶん上へずらす。
        let (first, lo, hi, more) = match rows {
            Rows::Summary if count > fit => (0, 0, fit - 1, Some(count - (fit - 1))),
            Rows::Summary => (0, 0, count, None),
            Rows::Scroll(scroll) => {
                let first = scroll[column].min(count.saturating_sub(fit));
                (first, first, count.min(first + fit), None)
            }
            Rows::Around {
                scroll,
                column: around,
                from,
                to,
                ..
            } => {
                let first = scroll[column].min(count.saturating_sub(fit));
                let hi = count.min(first + fit);
                if around == column {
                    (first, first.min(from), hi.max(to.min(count)), None)
                } else {
                    (first, first, hi, None)
                }
            }
        };
        painter.parent = painter
            .commands
            .spawn((
                ChildOf(root),
                Transform::from_xyz(0.0, first as f32 * row_height, 0.0),
                Visibility::default(),
                layer.clone(),
            ))
            .id();

        let skip = match rows {
            Rows::Around { skip, .. } => skip,
            _ => None,
        };
        for &child in items[lo..hi].iter().filter(|c| Some(**c) != skip) {
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
            let l = &source.layouts[items[fit - 1]];
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

        // スクロールできるときは、列の右端に今どこを見ているかの棒を出す。
        if !matches!(rows, Rows::Summary) && count > fit {
            painter.parent = root;
            let rect = list.columns[column];
            let width = row_height * 0.08;
            let x = rect.max.x + width;
            let track = Rect::new(x, rect.min.y, x + width, rect.max.y);
            let top = rect.max.y - rect.height() * first as f32 / count as f32;
            let height = rect.height() * fit as f32 / count as f32;
            painter.rect(track, over(background, Color::WHITE, 0.08), z + 0.3);
            painter.rect(
                Rect::new(track.min.x, top - height, track.max.x, top),
                over(background, Color::WHITE, 0.35),
                z + 0.4,
            );
        }
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
        rect.height() * 0.45,
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
