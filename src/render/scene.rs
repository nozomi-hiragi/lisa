//! ひとつのフォルダの「場面」を描く。場面はそのフォルダの直下のタイルだけからなり、
//! 子フォルダの枠にはキャッシュした画像を貼る（孫より下は描かない）。
//! 画面に出す本物の描画にも、キャッシュの画像を焼くときにも、同じ場面を使う。

use bevy::camera::visibility::RenderLayers;
use bevy::ecs::{lifecycle::HookContext, system::SystemParam, world::DeferredWorld};
use bevy::prelude::*;
use bevy::sprite_render::ColorMaterial;

use crate::layout::Layouts;
use crate::model::tree::{FileTree, NodeId};
use crate::preview::PreviewRegistry;
use crate::render::cache::FolderImages;
use crate::render::ear::{self, EarSizes};
use crate::render::palette;
use crate::render::shapes::{Painter, UiThemeAssets, over};
use crate::render::status::{self, FolderBodyState};
use crate::render::tag;

const DENSE_CHILDREN_THRESHOLD: usize = 256;
const SIMPLE_FILE_PX: f32 = 24.0;

pub const BACKGROUND: Color = palette::BASE_200;

/// フォルダの枠・中身・耳・プレビューを描くレイヤー（本物の描画とサムネ撮影の両方で映す）。
pub const CONTENT_LAYER: usize = 0;
/// 本物の場面にだけ重ねる印（重複の枠、カタログの札・暗い膜、読み込み中のスピナー）。サムネには映さない。
pub const OVERLAY_LAYER: usize = 1;
/// 今開いているフォルダと先祖の外装。サムネには一覧の中身だけを撮る。
pub const SHELL_LAYER: usize = 2;

/// 場面の根。これを消すと場面の中身もまとめて消える。
///
/// 注意: 重なった場面どうしは、z の大小ではなく作った順に描かれる（後に作ったほうが上）。
/// Bevy 0.20 のスプライトのまとめ描きで、z の並べ替えが効かない場合があるため。
/// 上に見せたい場面は、必ず後から作る。
#[derive(Component)]
pub struct SceneRoot {
    pub folder: NodeId,
    pub focus: Option<NodeId>,
}

/// 開いたフォルダと先祖の画像・タイトルだけをまとめる。
#[derive(Component)]
pub(crate) struct FolderShell {
    pub folder: NodeId,
    pub alpha: f32,
}

fn drawn_ancestors(ancestors: &[NodeId]) -> impl Iterator<Item = NodeId> + '_ {
    ancestors[ancestors.len().saturating_sub(2)..]
        .iter()
        .copied()
        .filter(|&folder| folder != FileTree::ROOT)
}

fn draw_shell(painter: &mut Painter, source: &SceneSource, folder: NodeId, z: f32) {
    let parent = painter.parent;
    let layer = std::mem::replace(&mut painter.layer, RenderLayers::layer(SHELL_LAYER));
    painter.parent = painter
        .commands
        .spawn((
            FolderShell { folder, alpha: 1.0 },
            ChildOf(parent),
            Transform::default(),
            Visibility::default(),
        ))
        .id();
    let slot = slot_of(source.tree, source.layouts, folder);
    let size = size_of(source.tree, source.layouts, source.ears, folder, true);
    ear::draw_folder(
        painter,
        slot,
        size,
        Some(&source.tree.nodes[folder].name),
        folder_color(source.tree, folder),
        None,
        z,
    );
    painter.parent = parent;
    painter.layer = layer;
}

/// 裏撮影だけが映すレイヤー。
pub const CAPTURE_LAYER: usize = 4;

/// 本体の状態が変わったときだけ、一覧の該当タイルを作り直す。
#[derive(Component)]
pub(crate) struct FolderDisplay {
    pub folder: NodeId,
    pub appearance: Entity,
    pub body: FolderBodyState,
    pub load_state: crate::model::tree::LoadState,
    pub no_cull: bool,
    pub scale: f32,
}

pub(crate) fn folder_body(
    tree: &FileTree,
    images: &FolderImages,
    id: NodeId,
    pending: bool,
) -> FolderBodyState {
    let node = &tree.nodes[id];
    FolderBodyState::new(
        node.state,
        images.ready(tree, id),
        node.loaded && node.children.is_empty(),
        pending,
    )
}

pub(crate) fn draw_folder(
    painter: &mut Painter,
    source: &SceneSource,
    child: NodeId,
    body: &FolderBodyState,
    live: bool,
) {
    let tree = source.tree;
    let node = &tree.nodes[child];
    let slot = slot_of(tree, source.layouts, child);
    let size = size_of(tree, source.layouts, source.ears, child, live);
    status::draw_folder(
        painter,
        node.state,
        body,
        slot,
        size,
        &node.name,
        folder_color(tree, child),
        node.depth as f32,
        live,
    );
}

#[derive(Component)]
pub(crate) struct FolderTile {
    pub scene: Entity,
    pub folder: NodeId,
}

/// サムネイル画像、または画像が来るまでの三点・スピナーをまとめた部品。
#[derive(Component)]
pub(crate) struct FolderPreview;

pub struct SceneSource<'a> {
    pub tree: &'a FileTree,
    pub layouts: &'a Layouts,
    pub images: &'a FolderImages,
    pub previews: &'a PreviewRegistry,
    pub font: &'a Handle<Font>,
    pub ears: &'a EarSizes,
    pub theme: &'a UiThemeAssets,
    /// 重複の一覧で選んだ組の印（本物の場面だけ）。
    pub highlight: Option<&'a crate::dups::DupHighlight>,
    /// カタログの札と役割の印（本物の場面だけ）。
    pub catalog: Option<&'a crate::catalog::CatalogMarks>,
}

/// フォルダ `folder` の場面を作る。`scale` はこの倍率で見たときにくっきり見えるように作る。
#[derive(Component)]
#[component(on_add = queue_dense_file)]
pub(crate) struct DenseFile {
    name: Box<str>,
    state: crate::model::tree::LoadState,
    frame: Rect,
    background: Color,
    z: f32,
    no_cull: bool,
    simple: bool,
}

#[derive(Resource, Default)]
pub(crate) struct PendingDenseFiles(Vec<Entity>);

fn queue_dense_file(mut world: DeferredWorld, context: HookContext) {
    if let Some(mut pending) = world.get_resource_mut::<PendingDenseFiles>() {
        pending.0.push(context.entity);
    }
}

#[derive(SystemParam)]
pub(crate) struct DenseFileAssets<'w> {
    meshes: ResMut<'w, Assets<Mesh>>,
    materials: ResMut<'w, Assets<ColorMaterial>>,
    previews: Res<'w, PreviewRegistry>,
    theme: Res<'w, UiThemeAssets>,
    font: Res<'w, LabelFont>,
}

/// カメラが動いたフレームに、境目を越えたファイルの紙だけを差し替える。
/// 操作中の場面、耳、選択、カメラの状態はそのまま使う。
pub(crate) fn refresh_file_details(
    mut commands: Commands,
    camera: Single<&Projection, With<crate::view::MainCamera>>,
    mut pending: ResMut<PendingDenseFiles>,
    mut last_scale: Local<Option<u32>>,
    mut files: Query<(Entity, &ChildOf, &RenderLayers, &mut DenseFile)>,
    mut assets: DenseFileAssets,
) {
    let Projection::Orthographic(ortho) = *camera else {
        *last_scale = None;
        return;
    };
    let scale = ortho.scale;
    let mut materials_by_parent = bevy::platform::collections::HashMap::new();
    let mut refresh =
        |(entity, parent, layer, mut file): (Entity, &ChildOf, &RenderLayers, Mut<DenseFile>)| {
            let simple = file.frame.size().min_element() / scale < SIMPLE_FILE_PX;
            if file.simple == simple {
                return;
            }
            file.simple = simple;
            commands.entity(entity).despawn_children();
            let cache = materials_by_parent.entry(parent.parent()).or_default();
            let mut painter = Painter {
                material_cache: std::mem::take(cache),
                commands: &mut commands,
                meshes: &mut assets.meshes,
                materials: &mut assets.materials,
                parent: entity,
                layer: layer.clone(),
                scale,
                font: assets.font.0.clone(),
                theme: &assets.theme,
                no_cull: file.no_cull,
            };
            status::draw_file(
                &mut painter,
                &assets.previews,
                file.state,
                &file.name,
                file.frame,
                file.background,
                file.z,
                true,
                None,
                simple,
            );
            *cache = painter.material_cache;
        };
    if last_scale.replace(scale.to_bits()) != Some(scale.to_bits()) {
        files.iter_mut().for_each(refresh);
        if !pending.0.is_empty() {
            pending.0.clear();
        }
    } else if !pending.0.is_empty() {
        for entity in pending.0.drain(..) {
            if let Ok(file) = files.get_mut(entity) {
                refresh(file);
            }
        }
    }
}

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
            SceneRoot { folder, focus },
            Transform::default(),
            Visibility::default(),
            layer.clone(),
        ))
        .id();
    let mut painter = Painter {
        material_cache: default(),
        commands,
        meshes,
        materials,
        parent: root,
        layer,
        scale,
        font: source.font.clone(),
        theme: source.theme,
        no_cull: true,
    };

    for ancestor in drawn_ancestors(ancestors) {
        draw_shell(
            &mut painter,
            source,
            ancestor,
            tree.nodes[ancestor].depth as f32 - 0.5,
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
            None,
        );
    }

    // フォルダ自身の枠と中身（Root には枠を描かない）。本物の場面では、耳が大きくなった分だけ
    // 縮んで譲る。画像に焼くときは耳を入れず、本体を画像いっぱいに描く。
    let node = &tree.nodes[folder];
    let z = node.depth as f32;
    let color = folder_color(tree, folder);
    if folder == FileTree::ROOT {
        draw_children(&mut painter, source, folder, live, None, focus, Some(root));
    } else {
        let (slot, size) = (
            slot_of(source.tree, source.layouts, folder),
            size_of(source.tree, source.layouts, source.ears, folder, live),
        );
        if live {
            draw_shell(&mut painter, source, folder, z);
            in_group(&mut painter, slot, size, |painter| {
                draw_children(painter, source, folder, true, None, focus, Some(root));
            });
        } else {
            ear::draw_folder(
                &mut painter,
                slot,
                ear::Size::Fixed(0.0),
                None,
                color,
                None,
                z,
            );
            draw_children(&mut painter, source, folder, false, None, focus, None);
        }
        if let Some(marks) = source.catalog {
            let prev_layer = painter.layer.clone();
            painter.layer = RenderLayers::layer(OVERLAY_LAYER);
            tag::draw_tag(&mut painter, marks, folder, slot, size, &node.name, z + 0.5);
            painter.layer = prev_layer;
        }
    }
    root
}

/// 本体の中身だけを描く。画面の本シーンとは別に管理する。
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_contents(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<ColorMaterial>,
    source: &SceneSource,
    folder: NodeId,
    root: Entity,
    layer: RenderLayers,
    scale: f32,
    live_body: bool,
) {
    let mut painter = Painter {
        material_cache: default(),
        commands,
        meshes,
        materials,
        parent: root,
        layer,
        scale,
        font: source.font.clone(),
        theme: source.theme,
        no_cull: true,
    };
    let slot = slot_of(source.tree, source.layouts, folder);
    let size = size_of(source.tree, source.layouts, source.ears, folder, live_body);
    in_group(&mut painter, slot, size, |painter| {
        draw_children(painter, source, folder, false, None, None, None);
    });
}

/// フォルダ `folder` の中身のタイルを描く。`skip` は描かない子、`focus` の周りの 8 つはカリングしない。
#[allow(clippy::too_many_arguments)]
fn draw_children(
    painter: &mut Painter,
    source: &SceneSource,
    folder: NodeId,
    live: bool,
    skip: Option<NodeId>,
    focus: Option<NodeId>,
    tile_scene: Option<Entity>,
) {
    let tree = source.tree;
    let node = &tree.nodes[folder];
    let background = over(BACKGROUND, folder_color(tree, folder), 0.08);
    let dense = node.children.len() > DENSE_CHILDREN_THRESHOLD;
    let grid = source.layouts[folder].grid;
    let columns = grid.columns.max(1) as i64;
    let at = |i: usize| ((i as i64) % columns, (i as i64) / columns);
    let focus = focus.and_then(|f| node.children.iter().position(|c| *c == f).map(at));
    for (i, &child) in node.children.iter().enumerate() {
        if Some(child) == skip || !source.layouts.contains(child) {
            continue;
        }
        let (column, row) = at(i);
        painter.no_cull =
            focus.is_some_and(|(fc, fr)| (column - fc).abs() <= 1 && (row - fr).abs() <= 1);
        let l = &source.layouts[child];
        let child_node = &tree.nodes[child];
        let z = child_node.depth as f32;
        let (slot, size) = (
            slot_of(source.tree, source.layouts, child),
            size_of(source.tree, source.layouts, source.ears, child, live),
        );
        if child_node.is_dir {
            let parent = painter.parent;
            let body = folder_body(tree, source.images, child, false);
            if live {
                let tile = painter
                    .commands
                    .spawn((ChildOf(parent), Transform::default(), Visibility::default()))
                    .id();
                let appearance = painter
                    .commands
                    .spawn((ChildOf(tile), Transform::default(), Visibility::default()))
                    .id();
                painter.commands.entity(tile).insert(FolderDisplay {
                    folder: child,
                    appearance,
                    body: body.clone(),
                    load_state: child_node.state,
                    no_cull: painter.no_cull,
                    scale: painter.scale,
                });
                if let Some(scene) = tile_scene {
                    painter.commands.entity(tile).insert(FolderTile {
                        scene,
                        folder: child,
                    });
                }
                painter.parent = appearance;
                // フォルダのプレビューは場面全体とは別にフェードする。
                let scene_materials = std::mem::take(&mut painter.material_cache);
                draw_folder(painter, source, child, &body, true);
                painter.material_cache = scene_materials;
                painter.parent = tile;
            } else {
                draw_folder(painter, source, child, &body, false);
            }
            mark_catalog(painter, source, child, slot, size, z);
            mark_duplicate(painter, source, child, slot, size, z);
            painter.parent = parent;
        } else {
            let loaded = if child_node.merged.is_none() {
                crate::ui::tooltip::local_path(tree, child)
                    .and_then(|path| source.previews.images.get(&path))
            } else {
                None
            };
            let can_simplify = dense
                && loaded.is_none()
                && matches!(
                    child_node.state,
                    crate::model::tree::LoadState::Loaded | crate::model::tree::LoadState::Stale
                );
            let edge = l.frame.size().min_element();
            let simple = can_simplify && edge / painter.scale < SIMPLE_FILE_PX;
            in_group(painter, slot, size, |painter| {
                if live && can_simplify {
                    painter.commands.entity(painter.parent).insert(DenseFile {
                        name: child_node.name.as_str().into(),
                        state: child_node.state,
                        frame: l.frame,
                        background,
                        z,
                        no_cull: painter.no_cull,
                        simple,
                    });
                }
                status::draw_file(
                    painter,
                    source.previews,
                    child_node.state,
                    &child_node.name,
                    l.frame,
                    background,
                    z,
                    live,
                    loaded,
                    simple,
                );
            });
            // 名前はプレビューの下に、同じフォルダの中のフォルダの耳の名前と同じ大きさで書く。
            let color = palette::BASE_CONTENT.with_alpha(0.92);
            ear::draw_name(painter, slot, size, &child_node.name, color, z);
            mark_catalog(painter, source, child, slot, size, z);
            mark_duplicate(painter, source, child, slot, size, z);
        }
        if child_node.merged.is_some() {
            if live {
                let mut accessible = bevy::a11y::AccessibilityNode::default();
                accessible.set_label(format!("{}、マージ済みリンク", child_node.name));
                painter
                    .commands
                    .spawn((accessible, ChildOf(painter.parent)));
            }
            in_group(painter, slot, size, |painter| {
                let edge = l.frame.size().min_element();
                let height = (edge * 0.19).max(painter.scale * 5.0);
                let at = l.frame.min + Vec2::splat(height * 0.28);
                painter.text(
                    "↗".into(),
                    at,
                    bevy::sprite::Anchor::BOTTOM_LEFT,
                    height,
                    height * 2.0,
                    palette::BASE_CONTENT,
                    z + 0.5,
                );
            });
        }
    }
    painter.no_cull = true;
}

/// カタログの印（札、役割で暗くする膜、選んだ枠）。本物の場面だけ（`OVERLAY_LAYER` に描き、サムネには入れない）。
fn mark_catalog(
    painter: &mut Painter,
    source: &SceneSource,
    id: NodeId,
    slot: ear::Slot,
    size: ear::Size,
    z: f32,
) {
    let Some(marks) = source.catalog else {
        return;
    };
    let prev_layer = painter.layer.clone();
    painter.layer = RenderLayers::layer(OVERLAY_LAYER);
    let frame = source.layouts[id].frame;
    let has_body_mark =
        marks.dim.contains(&id) || marks.selected.is_some_and(|(selected, _)| selected == id);
    if has_body_mark {
        in_group(painter, slot, size, |painter| {
            tag::draw_body_marks(painter, marks, id, frame, z);
        });
    }
    if source.tree.nodes[id].is_dir {
        tag::draw_tag(
            painter,
            marks,
            id,
            slot,
            size,
            &source.tree.nodes[id].name,
            z,
        );
    }
    painter.layer = prev_layer;
}

/// 重複の印の色（パレットの warning）。
const DUPLICATE: Color = palette::WARNING;

/// 重複の一覧で選んだ組のコピーなら太い枠と薄い色、コピーを中に含むなら細い枠を描く（`OVERLAY_LAYER`、サムネには入れない）。
fn mark_duplicate(
    painter: &mut Painter,
    source: &SceneSource,
    id: NodeId,
    slot: ear::Slot,
    size: ear::Size,
    z: f32,
) {
    let Some(highlight) = source.highlight else {
        return;
    };
    let copy = highlight.copies.contains(&id);
    if !copy && !highlight.around.contains(&id) {
        return;
    }
    let prev_layer = painter.layer.clone();
    painter.layer = RenderLayers::layer(OVERLAY_LAYER);
    let frame = source.layouts[id].frame;
    in_group(painter, slot, size, |painter| {
        if copy {
            painter.rect(frame, DUPLICATE.with_alpha(0.18), z + 0.4);
        }
        let k = if copy { 0.05 } else { 0.02 };
        let w = (frame.height() * k).max(painter.scale * if copy { 3.0 } else { 1.5 });
        let (min, max) = (frame.min, frame.max);
        for rect in [
            Rect::new(min.x, min.y, max.x, min.y + w),
            Rect::new(min.x, max.y - w, max.x, max.y),
            Rect::new(min.x, min.y + w, min.x + w, max.y - w),
            Rect::new(max.x - w, min.y + w, max.x, max.y - w),
        ] {
            painter.rect(rect, DUPLICATE, z + 0.45);
        }
    });
    painter.layer = prev_layer;
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
pub(super) fn slot_of(tree: &FileTree, layouts: &Layouts, id: NodeId) -> ear::Slot {
    let l = &layouts[id];
    ear::Slot {
        cell: l.cell,
        body: l.frame,
        name_below: !tree.nodes[id].is_dir,
    }
}

/// タイル `id` の耳（ファイルは名前）の大きさの決め方。`live` ならズームに合わせて変え、そうで
/// なければ親を開いたときの大きさに決める（画像に焼くとき）。
pub(super) fn size_of<'a>(
    tree: &FileTree,
    layouts: &Layouts,
    ears: &'a EarSizes,
    id: NodeId,
    live: bool,
) -> ear::Size<'a> {
    let node = &tree.nodes[id];
    let parent = node.parent.map(|p| &layouts[p]);
    let in_list = parent.map_or(0.0, |_| {
        layouts[id].cell.height() * crate::layout::MAX_EAR_IN_CELL
    });
    let size_width = if node.is_dir {
        layouts[id].frame.width()
    } else {
        parent.map_or(0.0, |p| p.grid.body_width)
    };
    let of = ear::EarOf {
        size_width,
        selected_fit: node.is_dir.then(|| layouts[id].fit.size()),
        selected_height: (layouts[id].cell.max.y - layouts[id].frame.max.y).max(0.0),
        parent_width: parent.map_or(size_width, |p| p.frame.width()),
        in_list,
    };
    if live {
        ear::Size::Live(ears, of)
    } else {
        ear::Size::Frozen(ears, of)
    }
}

/// 端末ごとのフォルダ基準色（ローカル端末は `PRIMARY`、LAN の相手端末はパレットの別色で識別する）。
const DEVICE_COLORS: [Color; 4] = [
    palette::PRIMARY,
    palette::ACCENT,
    palette::INFO,
    palette::SUCCESS,
];

/// フォルダの枠の色。親の枠の上に端末の色を重ね、既知の空フォルダは彩度を抑える。
pub fn folder_color(tree: &FileTree, id: NodeId) -> Color {
    let node = &tree.nodes[id];
    match node.parent {
        None => BACKGROUND,
        Some(parent) => {
            let device_color = DEVICE_COLORS[node.device % DEVICE_COLORS.len()];
            let color = over(folder_color(tree, parent), device_color, 0.85);
            if node.is_dir && node.loaded && node.children.is_empty() {
                over(palette::NEUTRAL, color, 0.40)
            } else {
                color
            }
        }
    }
}

/// 名前に使うフォント。
#[derive(Resource)]
pub struct LabelFont(pub Handle<Font>);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dense_file_crosses_detail_threshold_while_view_is_busy() {
        let mut app = App::new();
        let mut meshes = Assets::default();
        let theme = UiThemeAssets::load(&mut Assets::default(), &mut meshes);
        app.insert_resource(meshes)
            .init_resource::<Assets<ColorMaterial>>()
            .init_resource::<PreviewRegistry>()
            .insert_resource(theme)
            .insert_resource(LabelFont(Handle::default()))
            .init_resource::<PendingDenseFiles>()
            .insert_resource(crate::view::Busy(true))
            .add_systems(Update, refresh_file_details);
        let mut ortho = OrthographicProjection::default_2d();
        ortho.scale = 2.0;
        let camera = app
            .world_mut()
            .spawn((crate::view::MainCamera, Projection::Orthographic(ortho)))
            .id();
        let parent = app.world_mut().spawn_empty().id();
        let file = app
            .world_mut()
            .spawn((
                DenseFile {
                    name: "sample.txt".into(),
                    state: crate::model::tree::LoadState::Loaded,
                    frame: Rect::new(0.0, 0.0, 24.0, 24.0),
                    background: BACKGROUND,
                    z: 0.0,
                    no_cull: true,
                    simple: false,
                },
                ChildOf(parent),
                RenderLayers::layer(CONTENT_LAYER),
            ))
            .id();
        app.update();
        assert_eq!(app.world().get::<Children>(file).unwrap().len(), 1);
        for (scale, simple) in [(1.0, false), (2.0, true)] {
            let mut camera = app.world_mut().get_mut::<Projection>(camera).unwrap();
            let Projection::Orthographic(ortho) = &mut *camera else {
                unreachable!()
            };
            ortho.scale = scale;
            app.update();
            assert!(app.world().resource::<crate::view::Busy>().0);
            assert_eq!(app.world().get::<DenseFile>(file).unwrap().simple, simple);
            assert_eq!(
                app.world().get::<Children>(file).unwrap().len() == 1,
                simple
            );
        }
    }

    #[test]
    fn unmarked_catalog_node_does_not_spawn_a_body_group() {
        let tree = FileTree::deep(4);
        let (spec, ears) = crate::view::ear_spec_for_view(
            &crate::settings::Settings::default(),
            Vec2::new(800.0, 600.0),
        );
        let layouts =
            crate::layout::layout(&tree, FileTree::ROOT, crate::layout::anchor_body(), spec);
        let mut world = World::new();
        let root = world.spawn_empty().id();
        let mut meshes = Assets::default();
        let mut materials = Assets::default();
        let theme = UiThemeAssets::load(&mut Assets::default(), &mut meshes);
        let id = tree.nodes[FileTree::ROOT].children[0];
        let mut marks = crate::catalog::CatalogMarks::default();
        for marked in [false, true] {
            if marked {
                marks.dim.insert(id);
            }
            let source = SceneSource {
                tree: &tree,
                layouts: &layouts,
                images: &FolderImages::default(),
                previews: &PreviewRegistry::default(),
                font: &Handle::default(),
                ears: &ears,
                theme: &theme,
                highlight: None,
                catalog: Some(&marks),
            };
            mark_catalog(
                &mut Painter {
                    commands: &mut world.commands(),
                    meshes: &mut meshes,
                    materials: &mut materials,
                    material_cache: default(),
                    parent: root,
                    layer: RenderLayers::layer(CONTENT_LAYER),
                    scale: 1.0,
                    font: Handle::default(),
                    theme: &theme,
                    no_cull: true,
                },
                &source,
                id,
                slot_of(&tree, &layouts, id),
                size_of(&tree, &layouts, &ears, id, true),
                0.0,
            );
            world.flush();
            assert_eq!(
                world
                    .get::<Children>(root)
                    .map_or(0, |children| children.len()),
                usize::from(marked)
            );
        }
    }

    #[test]
    fn fitted_folder_uses_the_exact_same_body_at_every_window_size() {
        let tree = FileTree::deep(6);
        let settings = crate::settings::Settings::default();
        let folder = tree
            .nodes
            .iter()
            .position(|node| node.name == "底")
            .unwrap();
        for (width, height) in [
            (500.0, 400.0),
            (960.0, 600.0),
            (1280.0, 800.0),
            (1920.0, 1080.0),
        ] {
            for panel in [0.0, 240.0] {
                let viewport = Vec2::new(width - panel, height - 32.0);
                let (spec, ears) = crate::view::ear_spec_for_view(&settings, viewport);
                for anchor in [folder, tree.nodes[folder].parent.unwrap()] {
                    let layouts =
                        crate::layout::layout(&tree, anchor, crate::layout::anchor_body(), spec);
                    let slot = slot_of(&tree, &layouts, folder);
                    let scale = crate::layout::fit_scale(layouts[folder].fit.size(), viewport);
                    for scale in [scale, scale * 0.65] {
                        let live = size_of(&tree, &layouts, &ears, folder, true);
                        let capture = size_of(&tree, &layouts, &ears, folder, false);
                        assert_eq!(slot.yielded(live.height(scale)), (1.0, slot.body));
                        assert_eq!(slot.yielded(capture.height(scale)), (1.0, slot.body));
                    }
                }
            }
        }
    }

    #[test]
    fn only_parent_and_grandparent_can_spawn_shells() {
        let ancestors = [FileTree::ROOT, 10, 20, 30];
        assert_eq!(drawn_ancestors(&ancestors).collect::<Vec<_>>(), [20, 30]);
        assert_eq!(drawn_ancestors(&ancestors[..2]).collect::<Vec<_>>(), [10]);
        assert_eq!(drawn_ancestors(&ancestors[..1]).count(), 0);
    }

    #[test]
    fn local_populated_folder_converges_to_primary_and_empty_is_subdued() {
        let mut tree = FileTree::deep(4);
        assert_eq!(folder_color(&tree, FileTree::ROOT), palette::BASE_200);
        let populated = tree
            .nodes
            .iter()
            .position(|node| node.is_dir && !node.children.is_empty() && node.depth >= 3)
            .unwrap();
        let populated_rgb = folder_color(&tree, populated).to_srgba();
        let primary_rgb = palette::PRIMARY.to_srgba();
        assert!((populated_rgb.red - primary_rgb.red).abs() < 0.02);
        assert!((populated_rgb.green - primary_rgb.green).abs() < 0.02);
        assert!((populated_rgb.blue - primary_rgb.blue).abs() < 0.02);

        tree.nodes[populated].loaded = true;
        tree.nodes[populated].children.clear();
        let empty_rgb = folder_color(&tree, populated).to_srgba();
        assert!(empty_rgb.green < populated_rgb.green);
        assert!(empty_rgb.red < populated_rgb.red);
    }
}
