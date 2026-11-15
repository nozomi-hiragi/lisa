//! ひとつのフォルダの「場面」を描く。場面はそのフォルダの直下のタイルだけからなり、
//! 子フォルダの枠にはキャッシュした画像を貼る（孫より下は描かない）。
//! 画面に出す本物の描画にも、キャッシュの画像を焼くときにも、同じ場面を使う。

use bevy::camera::visibility::RenderLayers;
use bevy::prelude::*;
use bevy::sprite_render::ColorMaterial;

use crate::layout::NodeLayout;
use crate::model::tree::{FileTree, NodeId};
use crate::render::cache::FolderImages;
use crate::render::ear::{self, EarSizes};
use crate::render::preview;
use crate::render::shapes::{Painter, over};

pub const BACKGROUND: Color = Color::srgb(0.08, 0.08, 0.1);

/// 場面の根。これを消すと場面の中身もまとめて消える。
///
/// 注意: 重なった場面どうしは、z の大小ではなく作った順に描かれる（後に作ったほうが上）。
/// Bevy 0.20 のスプライトのまとめ描きで、z の並べ替えが効かない場合があるため。
/// 上に見せたい場面は、必ず後から作る。
#[derive(Component)]
pub struct SceneRoot;

pub struct SceneSource<'a> {
    pub tree: &'a FileTree,
    pub layouts: &'a [NodeLayout],
    pub images: &'a FolderImages,
    pub font: &'a Handle<Font>,
    pub ears: &'a EarSizes,
}

/// フォルダ `folder` の場面を作る。`scale` はこの倍率で見たときにくっきり見えるように作る。
/// `live` は画面に出す本物の場面か（耳をズームに合わせて変える）、画像に焼くためのものか。
/// `ancestors` は外側のフォルダ。枠の外に見える背景として描く。
/// `focus` は開いている子（この場面を親として下に敷くとき）。その子と周りの 8 つは、動かしている
/// 間にちらつかないよう、視錐台カリングしない。本物の場面には、同じ階層の隣も描く。
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<ColorMaterial>,
    source: &SceneSource,
    folder: NodeId,
    layer: RenderLayers,
    scale: f32,
    live: bool,
    ancestors: &[NodeId],
    focus: Option<NodeId>,
) -> Entity {
    let tree = source.tree;
    let root = commands
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
        parent: root,
        layer,
        scale,
        font: source.font.clone(),
        no_cull: true,
    };

    // 外側のフォルダの枠。ズームの前後で、枠の外に見える色が変わらないようにする。
    for &ancestor in ancestors {
        if ancestor == FileTree::ROOT {
            continue;
        }
        let color = folder_color(tree, ancestor);
        let z = tree.nodes[ancestor].depth as f32 - 0.5;
        let (slot, size) = (slot_of(source, ancestor), size_of(source, ancestor, true));
        in_group(&mut painter, slot, size, |painter| {
            painter.rect(slot.body, color, z);
        });
        ear::draw_ear(
            &mut painter,
            slot,
            size,
            &tree.nodes[ancestor].name,
            color,
            z,
        );
    }

    // 同じ階層の隣（親の中のほかのタイル）。開いたフォルダの周りに見切れて見える。
    // 隣の 8 つは、動かしている間にちらつかないよう、カリングしない。
    if live && let Some(parent) = tree.nodes[folder].parent {
        draw_children(
            &mut painter,
            source,
            parent,
            true,
            Some(folder),
            Some(folder),
        );
    }

    // フォルダ自身の枠と中身（Root には枠を描かない）。本物の場面では、耳が大きくなった分だけ
    // 縮んで譲る。画像に焼くときは耳を入れず、本体を画像いっぱいに描く。
    let node = &tree.nodes[folder];
    let z = node.depth as f32;
    let background = folder_color(tree, folder);
    let frame = source.layouts[folder].frame;
    if folder == FileTree::ROOT || !live {
        if folder != FileTree::ROOT {
            painter.rect(frame, background, z);
        }
        draw_children(&mut painter, source, folder, live, None, focus);
    } else {
        let (slot, size) = (slot_of(source, folder), size_of(source, folder, true));
        in_group(&mut painter, slot, size, |painter| {
            painter.rect(frame, background, z);
            draw_children(painter, source, folder, true, None, focus);
        });
        ear::draw_ear(&mut painter, slot, size, &node.name, background, z + 0.5);
    }
    root
}

/// フォルダ `folder` の中身のタイルを描く。`skip` は描かない子、`focus` の周りの 8 つはカリングしない。
fn draw_children(
    painter: &mut Painter,
    source: &SceneSource,
    folder: NodeId,
    live: bool,
    skip: Option<NodeId>,
    focus: Option<NodeId>,
) {
    let tree = source.tree;
    let node = &tree.nodes[folder];
    let background = folder_color(tree, folder);
    let grid = source.layouts[folder].grid;
    let columns = grid.columns.max(1) as i64;
    let at = |i: usize| ((i as i64) % columns, (i as i64) / columns);
    let focus = focus.and_then(|f| node.children.iter().position(|c| *c == f).map(at));
    for (i, &child) in node.children.iter().enumerate() {
        if Some(child) == skip {
            continue;
        }
        let (column, row) = at(i);
        painter.no_cull =
            focus.is_some_and(|(fc, fr)| (column - fc).abs() <= 1 && (row - fr).abs() <= 1);
        let l = &source.layouts[child];
        let child_node = &tree.nodes[child];
        let z = child_node.depth as f32;
        let (slot, size) = (slot_of(source, child), size_of(source, child, live));
        if child_node.is_dir {
            let color = folder_color(tree, child);
            let image = source.images.ready(child);
            in_group(painter, slot, size, |painter| match image {
                // 画像には子の本体と中身が焼いてある（耳は入っていない）。
                Some(image) => painter.image(l.frame, image, z),
                // まだ画像が無いときは、空の本体だけ。
                None => painter.rect(l.frame, color, z),
            });
            ear::draw_ear(painter, slot, size, &child_node.name, color, z);
        } else {
            in_group(painter, slot, size, |painter| {
                preview::draw(painter, &child_node.name, l.frame, background, z);
            });
            // 名前はプレビューの下に、同じフォルダの中のフォルダの耳の名前と同じ大きさで書く。
            let color = over(background, Color::WHITE, 0.9);
            ear::draw_name(painter, slot, size, &child_node.name, color, z);
        }
    }
    painter.no_cull = true;
}

/// 耳に譲って縮む入れ物の中に描く。
fn in_group(
    painter: &mut Painter,
    slot: ear::Slot,
    size: ear::Size,
    draw: impl FnOnce(&mut Painter),
) {
    let parent = painter.parent;
    painter.parent = ear::body_group(painter, slot, size);
    draw(painter);
    painter.parent = parent;
}

/// タイル `id` の枠の中での置き方。
fn slot_of(source: &SceneSource, id: NodeId) -> ear::Slot {
    let l = &source.layouts[id];
    ear::Slot {
        cell: l.cell,
        body: l.frame,
        name_below: !source.tree.nodes[id].is_dir,
    }
}

/// タイル `id` の耳（ファイルは名前）の大きさの決め方。`live` ならズームに合わせて変え、そうで
/// なければ親を開いたときの大きさに決める（画像に焼くとき）。
fn size_of<'a>(source: &SceneSource<'a>, id: NodeId, live: bool) -> ear::Size<'a> {
    let node = &source.tree.nodes[id];
    let parent = node.parent.map(|p| &source.layouts[p]);
    let in_list = parent.map_or(0.0, |p| p.grid.ear);
    if !live {
        return ear::Size::Fixed(in_list);
    }
    let size_width = if node.is_dir {
        source.layouts[id].frame.width()
    } else {
        parent.map_or(0.0, |p| p.grid.body_width)
    };
    ear::Size::Live(
        source.ears,
        ear::EarOf {
            size_width,
            parent_width: parent.map_or(size_width, |p| p.frame.width()),
            in_list,
        },
    )
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
