//! カメラのズーム、場面の切り替え、描画頻度の切り替え。
//!
//! 画面に本物として描くのは、今開いているフォルダの場面ひとつだけ。その中の子フォルダは
//! キャッシュした画像で見せる。子フォルダを押すと、画像のままズームし、ズームが終わったら
//! 子フォルダの本物の場面をフェードインさせる。戻るときは、先に親の場面を本物で描いておき、
//! 今の場面をフェードアウトさせながらズームアウトする。

use std::time::{Duration, Instant};

use bevy::camera::visibility::RenderLayers;
use bevy::ecs::system::SystemParam;
use bevy::math::Rect;
use bevy::prelude::*;
use bevy::sprite_render::{ColorMaterial, MeshMaterial2d};
use bevy::window::{PrimaryWindow, RequestRedraw, WindowFocused, WindowOccluded, WindowResized};
use bevy::winit::{UpdateMode, WinitSettings};

use crate::layout::{EarSpec, Layouts, anchor_body, fitted_size, layout};
use crate::model::tree::{FileTree, NodeId};
use crate::render::cache::{self, FolderImages, ThumbnailCapture};
use crate::render::ear::{EarSizes, update_ears};
use crate::render::scene::{
    self, BACKGROUND, CONTENT_LAYER, LabelFont, OVERLAY_LAYER, SceneRoot, SceneSource,
};
use crate::render::shapes::BaseAlpha;
use crate::settings::Settings;
use crate::ui::menu::{BAR_PX, MenuBreadcrumb, MenuSystems, Popup};

mod camera;
mod fade;
mod title;

pub use camera::Busy;
pub(crate) use camera::{
    Animating, Ease, MainCamera, ZOOM_SECONDS, animate_camera, fit, keep_redrawing,
    refit_on_resize, set_catalog_side_px, set_organize_side_px, set_side_px, side_px, view_width,
};
pub(crate) use fade::{Fade, fade_scenes};
pub use title::TitleSuffix;
pub(crate) use title::{Breadcrumb, show_breadcrumb, update_title};

pub struct TreeViewPlugin;

/// 画面のウィンドウが最小化・他の窓の裏・別スペース等で完全に隠れているか。
///
/// 隠れているあいだは macOS の wgpu-hal が `SurfaceError::Occluded` を返して `present()` が
/// 飛ばされるため、`RequestRedraw` を出し続けると VSync 無しで CPU が暴走する。
/// また Bevy 0.20 の `Screenshot::primary_window()` はスワップチェーンが無いと全画素 (0,0,0) の
/// PNG を保存するため、この状態で隠れ中を検知して防ぐ。
#[derive(Resource, Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowOccludedState(pub bool);

/// 画面の処理（木を読む処理 `scan.rs` は、この前に動く）。
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct ViewSystems;

impl Plugin for TreeViewPlugin {
    fn build(&self, app: &mut App) {
        let (update_settings, _) = update_settings_from_env();
        app.insert_resource(update_settings)
            .insert_resource(Location::root())
            .insert_resource(Nav {
                stack: vec![Level {
                    node: FileTree::ROOT,
                }],
            })
            .init_resource::<SyncRequest>()
            .insert_resource(SceneVisit(1))
            .init_resource::<LastSceneKey>()
            .init_resource::<ThumbnailCapture>()
            .insert_resource(cache::ImageCache::new())
            .insert_resource(ClearColor(BACKGROUND))
            .insert_resource(Breadcrumb("Root".into()))
            .init_resource::<Busy>()
            .init_resource::<WindowOccludedState>()
            .init_resource::<crate::render::status::PulseClock>()
            .init_resource::<crate::input::gesture::Gesture>()
            .init_resource::<crate::input::gesture::TrackpadInput>()
            .add_systems(
                Startup,
                (setup, crate::input::gesture::install_scroll_monitor),
            )
            .add_systems(PreUpdate, track_window_occlusion)
            .add_systems(
                Update,
                (
                    check_sync_triggers,
                    sync_view,
                    reanchor,
                    navigate,
                    crate::input::gesture::gesture,
                    refit_on_resize,
                    follow_side_panel,
                    animate_camera,
                    update_ears,
                    fade_scenes,
                    cache::update_thumbnails,
                    crate::render::status::pulse_loading,
                    keep_redrawing,
                    update_title,
                )
                    .chain()
                    .in_set(ViewSystems)
                    .after(MenuSystems),
            );
    }
}

/// ウィンドウの隠れ・復帰を追う。再び見えるようになったとき（またはフォーカスが戻ったとき）は
/// 明示的に 1 回描き直しを頼む。
fn track_window_occlusion(
    mut occluded_events: MessageReader<WindowOccluded>,
    mut focused_events: MessageReader<WindowFocused>,
    primary: Query<Entity, With<PrimaryWindow>>,
    mut state: ResMut<WindowOccludedState>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    let primary_entity = primary.iter().next();
    let mut need_redraw = false;
    for ev in occluded_events.read() {
        if primary_entity.is_some_and(|p| p != ev.window) {
            continue;
        }
        state.0 = ev.occluded;
        if !ev.occluded {
            need_redraw = true;
        }
    }
    for ev in focused_events.read() {
        if primary_entity.is_some_and(|p| p != ev.window) {
            continue;
        }
        if ev.focused {
            need_redraw = true;
        }
    }
    if need_redraw {
        redraw.write(RequestRedraw);
    }
}

/// 開いている場所の「正」。木のノード番号（`NodeId`）は読み直し（`reset_children`）で別物に
/// なるため、読み直しをまたいで変わらない道のりで持つ。
#[derive(Resource, Clone, Debug, PartialEq, Eq)]
pub enum Location {
    /// 手元の DB にあるドライブ以下: ドライブ先頭からの `entry.id` 列。
    Local(Vec<i64>),
    /// それ以外（Root、端末、ダミーの木、DB 無しドライブ、LAN の相手）: Root からの名前列。
    Named(Vec<String>),
}

impl Location {
    pub fn root() -> Self {
        Self::Named(vec!["Root".into()])
    }

    /// 木のノード `node` から正の場所を作る。ドライブ以下がすべて DB の `entry.id` を持てば
    /// `Local`、そうでなければ Root からの名前列 `Named`。
    pub fn from_tree(tree: &FileTree, node: NodeId) -> Self {
        let Some(path) = tree.path_from_root(node) else {
            return Self::root();
        };
        if let Some(first_entry) = path.iter().position(|&id| tree.nodes[id].entry.is_some())
            && path[first_entry..]
                .iter()
                .all(|&id| tree.nodes[id].entry.is_some())
        {
            let chain: Vec<i64> = path[first_entry..]
                .iter()
                .filter_map(|&id| tree.nodes[id].entry)
                .collect();
            if !chain.is_empty() {
                return Self::Local(chain);
            }
        }
        Self::Named(
            path.into_iter()
                .map(|id| tree.nodes[id].name.clone())
                .collect(),
        )
    }
}

/// 画面（配置・場面・カメラ・パンくず）を正の場所からそろえ直す頼み。
#[derive(Resource, Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct SyncRequest {
    pub mode: Option<SyncMode>,
    /// `RelayoutKeepView` 時にカメラを今の枠へ合わせ直す（耳の高さ設定変更など）。
    pub refit: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SyncMode {
    /// 今の場面だけ描き直す（重複・カタログの印、ディスクからサムネを読めたとき）。操作中は待つ。
    RedrawScene = 1,
    /// 木に子が足された・設定が変わった: 今の基準で並べ直し、場面を作り直す。操作中は待つ。
    RelayoutKeepView = 2,
    /// 構造リセット（`reset_children`）・外部ジャンプ（`JumpTo`）・孤立復旧:
    /// `Location` から `Nav`・`Layouts`・`CurrentScene`・`MainCamera`・パンくずを一括で作り直す。
    Reset = 3,
}

impl SyncRequest {
    fn upgrade(&mut self, mode: SyncMode) {
        if self.mode.is_none_or(|cur| mode > cur) {
            self.mode = Some(mode);
        }
    }

    pub fn request_redraw(&mut self) {
        self.upgrade(SyncMode::RedrawScene);
    }

    pub fn request_relayout(&mut self, refit: bool) {
        self.upgrade(SyncMode::RelayoutKeepView);
        if refit {
            self.refit = true;
        }
    }

    pub fn request_reset(&mut self) {
        self.upgrade(SyncMode::Reset);
    }
}

/// 今開いているフォルダへの訪問番号。別のフォルダへ移る／開き直す／リセットするたびに進み、
/// `cache::update_thumbnails` が「この訪問をまだ撮っていない」と判断してサムネを撮り直す。
#[derive(Resource, Default, Clone, Copy, Debug)]
pub struct SceneVisit(pub u64);

/// 直前に見ていた場面フォルダのサムネ鍵（開き直し検知用）。
#[derive(Resource, Default)]
struct LastSceneKey(Option<cache::Key>);

/// 開いてきた道のり（`Location` を現在の木へ解決した派生ビュー）。最後が今フィーチャーしているもの。
#[derive(Resource)]
pub(crate) struct Nav {
    pub(crate) stack: Vec<Level>,
}

impl Nav {
    pub(crate) fn top(&self) -> &Level {
        self.stack.last().expect("Root は常にある")
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Level {
    pub(crate) node: NodeId,
}

impl Level {
    /// 画面に収める四角（ワールド座標）。本体の上に、開いたときの耳の分を足したもの。
    pub(crate) fn frame(&self, layouts: &Layouts) -> Rect {
        layouts[self.node].fit
    }
}

/// 今画面に本物として描いている場面。
#[derive(Resource)]
pub(crate) struct CurrentScene(pub(crate) Entity);

/// 場面を作るのに要るもの。
#[derive(SystemParam)]
pub(crate) struct SceneMaker<'w, 's> {
    pub(crate) commands: Commands<'w, 's>,
    pub(crate) meshes: ResMut<'w, Assets<Mesh>>,
    pub(crate) materials: ResMut<'w, Assets<ColorMaterial>>,
    pub(crate) tree: Res<'w, FileTree>,
    pub(crate) layouts: Res<'w, Layouts>,
    pub(crate) images: Res<'w, FolderImages>,
    pub(crate) font: Res<'w, LabelFont>,
    pub(crate) ears: Res<'w, EarSizes>,
    pub(crate) highlight: Res<'w, crate::dups::DupHighlight>,
    pub(crate) catalog: Res<'w, crate::catalog::CatalogMarks>,
}

impl SceneMaker<'_, '_> {
    /// 画面に出す本物の場面を作る。`path` の最後がその場面で、手前は外側のフォルダ。
    /// `scale` はその場面を画面いっぱいに見るときの倍率。
    pub(crate) fn spawn(&mut self, path: &[Level], scale: f32) -> Entity {
        self.spawn_around(path, scale, None)
    }

    /// `spawn` と同じだが、開いている子 `focus` とその周りをカリングしない（親の場面を下に敷くとき）。
    pub(crate) fn spawn_around(
        &mut self,
        path: &[Level],
        scale: f32,
        focus: Option<NodeId>,
    ) -> Entity {
        let (level, outer) = path.split_last().expect("場面がある");
        let ancestors: Vec<NodeId> = outer.iter().map(|l| l.node).collect();
        let source = SceneSource {
            tree: &self.tree,
            layouts: &self.layouts,
            images: &self.images,
            font: &self.font.0,
            ears: &self.ears,
            highlight: Some(&self.highlight),
            catalog: Some(&self.catalog),
        };
        scene::spawn(
            &mut self.commands,
            &mut self.meshes,
            &mut self.materials,
            &source,
            level.node,
            RenderLayers::layer(CONTENT_LAYER),
            scale,
            true,
            &ancestors,
            focus,
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn setup(
    mut commands: Commands,
    tree: Res<FileTree>,
    settings: Res<Settings>,
    mut fonts: ResMut<Assets<Font>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<ColorMaterial>>,
    window: Single<&Window, With<PrimaryWindow>>,
) {
    // フォントは実行ファイルに埋め込む（`lisa.exe` 1 つを他の PC へ渡すだけで文字が出るように）。
    let font = fonts.add(Font::from_bytes(
        include_bytes!("../assets/fonts/NotoSansJP-Light.otf").to_vec(),
    ));
    // 本体はどれも √2:1（A4 の横長）。耳の高さの設定は、本体を画面に収めたときのピクセル数で決める。
    // 最初は Root を基準にして並べる。
    let (spec, ears) = ear_spec(&settings, &window);
    let layouts = layout(&tree, FileTree::ROOT, anchor_body(), spec);
    let folder_images = FolderImages::default();

    let (pos, scale) = fit(layouts[FileTree::ROOT].frame, &window);
    // UI（メニューバーや一覧）は、木を映すカメラとは別のカメラで、ウィンドウ全体に描く
    // （木のカメラは、右の一覧が開くと映す領域を狭めるため）。
    commands.spawn((
        IsDefaultUiCamera,
        Camera2d,
        Camera {
            order: 10,
            clear_color: ClearColorConfig::None,
            ..default()
        },
        RenderLayers::none(),
    ));
    commands.spawn((
        MainCamera,
        Camera2d,
        Projection::Orthographic(OrthographicProjection {
            scale,
            ..OrthographicProjection::default_2d()
        }),
        Transform::from_translation(pos.extend(0.0)),
        // 画面にはレイヤー 0（中身）とレイヤー 1（重複・カタログ印・パルス）の両方を映す。
        // サムネ撮影カメラはレイヤー 0 のみを映す。
        RenderLayers::from_layers(&[CONTENT_LAYER, OVERLAY_LAYER]),
    ));

    let source = SceneSource {
        tree: &tree,
        layouts: &layouts,
        images: &folder_images,
        font: &font,
        ears: &ears,
        highlight: None,
        catalog: None,
    };
    let root_scene = scene::spawn(
        &mut commands,
        &mut meshes,
        &mut materials,
        &source,
        FileTree::ROOT,
        RenderLayers::layer(CONTENT_LAYER),
        scale,
        true,
        &[],
        None,
    );

    commands.insert_resource(CurrentScene(root_scene));
    commands.insert_resource(layouts);
    commands.insert_resource(folder_images);
    commands.insert_resource(LabelFont(font));
    commands.insert_resource(ears);
}

/// 設定と今のウィンドウから、耳の大きさを決める。
fn ear_spec(settings: &Settings, window: &Window) -> (EarSpec, EarSizes) {
    let fitted = fitted_size(
        Vec2::new(window.width(), window.height() - BAR_PX),
        settings.ear_selected_px,
    );
    let ears = EarSizes {
        fitted_width: fitted.x,
        selected_px: settings.ear_selected_px,
        preview_px: settings.ear_preview_px,
    };
    (EarSpec::new(settings, fitted.y), ears)
}

/// 右の一覧が開け閉めされたら、木を映す領域（カメラの viewport）を狭め、今のものを収め直す。
fn follow_side_panel(
    mut last: Local<(f32, UVec2)>,
    window: Single<&Window, With<PrimaryWindow>>,
    camera: Single<(&mut Camera, &mut Transform, &mut Projection), With<MainCamera>>,
    layouts: Res<Layouts>,
    nav: Res<Nav>,
    animating: Option<Res<Animating>>,
    gesture: Res<crate::input::gesture::Gesture>,
) {
    let side = side_px();
    let now = (side, window.physical_size());
    if now == *last {
        return;
    }
    *last = now;
    let (mut camera, mut transform, mut projection) = camera.into_inner();
    camera.viewport = if side > 0.0 {
        let factor = window.scale_factor();
        let size = UVec2::new(
            (view_width(&window) * factor).round() as u32,
            window.physical_height(),
        );
        Some(bevy::camera::Viewport {
            physical_position: UVec2::ZERO,
            physical_size: size.max(UVec2::ONE),
            ..default()
        })
    } else {
        None
    };
    // 動かしている最中は、そのまま（止まったあとの収め直しに任せる）。
    if animating.is_some() || gesture.active() {
        return;
    }
    let (pos, scale) = fit(nav.top().frame(&layouts), &window);
    if !(scale > 0.0 && scale.is_finite() && pos.is_finite()) {
        return;
    }
    transform.translation = pos.extend(0.0);
    if let Projection::Orthographic(ortho) = projection.as_mut() {
        ortho.scale = scale;
    }
}

/// 同期が必要なきっかけ（耳の高さ設定変更・木の増分変化・孤立検知）と、開いたフォルダの訪問番号を調べる。
#[allow(clippy::too_many_arguments)]
fn check_sync_triggers(
    settings: Res<Settings>,
    tree: Res<FileTree>,
    layouts: Res<Layouts>,
    nav: Res<Nav>,
    tree_changed: Option<Res<crate::scan::TreeChanged>>,
    mut sync: ResMut<SyncRequest>,
    mut visit: ResMut<SceneVisit>,
    mut last_key: ResMut<LastSceneKey>,
    mut laid_out_with: Local<Option<[f32; 2]>>,
    mut last_scene_node: Local<Option<NodeId>>,
) {
    let ears = [settings.ear_selected_px, settings.ear_preview_px];
    if laid_out_with.is_some_and(|h| h != ears) {
        sync.request_relayout(true);
    }
    *laid_out_with = Some(ears);

    if tree_changed.as_ref().is_some_and(|c| c.due()) {
        sync.request_relayout(false);
    }

    if !tree.is_attached(nav.top().node)
        || !tree.is_attached(layouts.anchor)
        || !layouts.contains(nav.top().node)
    {
        sync.request_reset();
    }

    let path = scene_path(&nav, &tree);
    let scene_node = path[path.len() - 1].node;
    if *last_scene_node != Some(scene_node) {
        *last_scene_node = Some(scene_node);
        let key = cache::folder_key(&tree, scene_node);
        if last_key.0 != key {
            last_key.0 = key;
            visit.0 += 1;
        }
    }
}

/// `sync_view` で場面を組み立てるために借りるアセット・印の束。
#[derive(SystemParam)]
struct SyncDrawAssets<'w> {
    meshes: ResMut<'w, Assets<Mesh>>,
    materials: ResMut<'w, Assets<ColorMaterial>>,
    images: Res<'w, FolderImages>,
    font: Res<'w, LabelFont>,
    ears: ResMut<'w, EarSizes>,
    highlight: Res<'w, crate::dups::DupHighlight>,
    catalog: Res<'w, crate::catalog::CatalogMarks>,
}

/// `sync_view` で参照する操作状態・木・DB の束。
#[derive(SystemParam)]
struct SyncCore<'w, 's> {
    tree: ResMut<'w, FileTree>,
    layouts: ResMut<'w, Layouts>,
    location: ResMut<'w, Location>,
    nav: ResMut<'w, Nav>,
    current: ResMut<'w, CurrentScene>,
    store: Option<Res<'w, crate::scan::Store>>,
    scanner: Option<ResMut<'w, crate::scan::Scanner>>,
    tree_changed: Option<ResMut<'w, crate::scan::TreeChanged>>,
    visit: ResMut<'w, SceneVisit>,
    last_key: ResMut<'w, LastSceneKey>,
    _marker: std::marker::PhantomData<&'s ()>,
}

/// 画面の同期口（コア主導・GUI 主導の双方から 1 つの道筋で配置・場面・カメラ・パンくずをそろえる）。
#[allow(clippy::too_many_arguments)]
fn sync_view(
    mut commands: Commands,
    mut sync: ResMut<SyncRequest>,
    settings: Res<Settings>,
    window: Single<&Window, With<PrimaryWindow>>,
    camera: Single<(&mut Transform, &mut Projection), With<MainCamera>>,
    animating: Option<Res<Animating>>,
    fades: Query<(), With<Fade>>,
    mut gesture: ResMut<crate::input::gesture::Gesture>,
    scenes: Query<(Entity, &RenderLayers), With<SceneRoot>>,
    mut core: SyncCore,
    mut draw: SyncDrawAssets,
) {
    let Some(mut mode) = sync.mode else {
        return;
    };
    let interacting = animating.is_some() || !fades.is_empty() || gesture.active();
    let broken = !core.tree.is_attached(core.nav.top().node)
        || !core.tree.is_attached(core.layouts.anchor)
        || !core.layouts.contains(core.nav.top().node);
    // 操作中は木・場面を壊さないよう保留する。万一ノードが孤立している場合だけ復旧を優先する。
    if interacting && !(mode == SyncMode::Reset && broken) {
        return;
    }
    sync.mode = None;
    let refit = std::mem::take(&mut sync.refit);
    if mode == SyncMode::RelayoutKeepView && broken {
        mode = SyncMode::Reset;
    }

    let main_layer = RenderLayers::layer(CONTENT_LAYER);
    let despawn_all_main = |commands: &mut Commands, warn_expected: Option<&[Entity]>| {
        let mut stray = 0usize;
        for (entity, layers) in &scenes {
            if !layers.intersects(&main_layer) {
                continue;
            }
            if let Some(expected) = warn_expected
                && !expected.contains(&entity)
            {
                stray += 1;
            }
            commands.entity(entity).despawn();
        }
        if stray > 0 {
            warn!("並べ直しで想定外の場面が {stray} 個残っていたため消しました");
        }
    };

    let (mut cam_tf, mut cam_proj) = camera.into_inner();
    match mode {
        SyncMode::Reset => {
            if animating.is_some() {
                commands.remove_resource::<Animating>();
            }
            let _ = gesture.clear_and_take_parent_scene();

            let resolved = crate::scan::resolve_location(
                &mut core.tree,
                core.store.as_deref(),
                core.scanner.as_deref_mut(),
                &core.location,
            );
            core.nav.stack = resolved.into_iter().map(|node| Level { node }).collect();
            let top_node = core.nav.top().node;
            *core.location = Location::from_tree(&core.tree, top_node);

            let path = scene_path(&core.nav, &core.tree).to_vec();
            let folder = path[path.len() - 1].node;
            let (spec, ears) = ear_spec(&settings, &window);
            *core.layouts = layout(&core.tree, folder, anchor_body(), spec);
            *draw.ears = ears;

            let (cam_pos, cam_scale) = fit(core.nav.top().frame(&core.layouts), &window);
            if cam_pos.is_finite() && cam_scale > 0.0 && cam_scale.is_finite() {
                cam_tf.translation = cam_pos.extend(0.0);
                if let Projection::Orthographic(ortho) = cam_proj.as_mut() {
                    ortho.scale = cam_scale;
                }
            }

            despawn_all_main(&mut commands, None);
            let (_, scene_scale) = fit(path[path.len() - 1].frame(&core.layouts), &window);
            core.current.0 = spawn_scene_raw(
                &mut commands,
                &mut draw,
                &core.tree,
                &core.layouts,
                &path,
                scene_scale,
                None,
            );
            show_breadcrumb(&mut commands, &core.tree, top_node);
            core.last_key.0 = cache::folder_key(&core.tree, folder);
            core.visit.0 += 1;
            if let Some(changed) = core.tree_changed.as_mut() {
                changed.pending = false;
                changed.last = Some(Instant::now());
            }
        }
        SyncMode::RelayoutKeepView => {
            if let Some(changed) = core.tree_changed.as_mut() {
                changed.pending = false;
                changed.last = Some(Instant::now());
            }
            let (spec, ears) = ear_spec(&settings, &window);
            let anchor = core.layouts.anchor;
            *core.layouts = layout(&core.tree, anchor, core.layouts[anchor].frame, spec);
            *draw.ears = ears;

            if refit {
                let (cam_pos, cam_scale) = fit(core.nav.top().frame(&core.layouts), &window);
                if cam_pos.is_finite() && cam_scale > 0.0 && cam_scale.is_finite() {
                    cam_tf.translation = cam_pos.extend(0.0);
                    if let Projection::Orthographic(ortho) = cam_proj.as_mut() {
                        ortho.scale = cam_scale;
                    }
                }
                core.visit.0 += 1;
            }

            let mut expected = vec![core.current.0];
            if let Some(p) = gesture.parent_scene_entity() {
                expected.push(p);
            }
            despawn_all_main(&mut commands, Some(&expected));

            let keep_parent = gesture.has_parent_scene()
                && core.nav.stack.len() >= 2
                && core.tree.nodes[core.nav.top().node].is_dir;
            let path = scene_path(&core.nav, &core.tree).to_vec();
            let (_, scene_scale) = fit(path[path.len() - 1].frame(&core.layouts), &window);
            if keep_parent {
                let parent_path = &core.nav.stack[..core.nav.stack.len() - 1];
                let (_, parent_scale) = fit(
                    parent_path[parent_path.len() - 1].frame(&core.layouts),
                    &window,
                );
                let parent_entity = spawn_scene_raw(
                    &mut commands,
                    &mut draw,
                    &core.tree,
                    &core.layouts,
                    parent_path,
                    parent_scale,
                    Some(core.nav.top().node),
                );
                gesture.set_parent_scene(Some(parent_entity));
                core.current.0 = spawn_scene_raw(
                    &mut commands,
                    &mut draw,
                    &core.tree,
                    &core.layouts,
                    &path,
                    scene_scale,
                    None,
                );
                commands
                    .entity(core.current.0)
                    .insert(Transform::from_xyz(0.0, 0.0, 0.5));
            } else {
                let _ = gesture.take_parent_scene();
                core.current.0 = spawn_scene_raw(
                    &mut commands,
                    &mut draw,
                    &core.tree,
                    &core.layouts,
                    &path,
                    scene_scale,
                    None,
                );
            }
            show_breadcrumb(&mut commands, &core.tree, core.nav.top().node);
        }
        SyncMode::RedrawScene => {
            let mut expected = vec![core.current.0];
            if let Some(p) = gesture.parent_scene_entity() {
                expected.push(p);
            }
            despawn_all_main(&mut commands, Some(&expected));

            let keep_parent = gesture.has_parent_scene()
                && core.nav.stack.len() >= 2
                && core.tree.nodes[core.nav.top().node].is_dir;
            let path = scene_path(&core.nav, &core.tree).to_vec();
            let (_, scene_scale) = fit(path[path.len() - 1].frame(&core.layouts), &window);
            if keep_parent {
                let parent_path = &core.nav.stack[..core.nav.stack.len() - 1];
                let (_, parent_scale) = fit(
                    parent_path[parent_path.len() - 1].frame(&core.layouts),
                    &window,
                );
                let parent_entity = spawn_scene_raw(
                    &mut commands,
                    &mut draw,
                    &core.tree,
                    &core.layouts,
                    parent_path,
                    parent_scale,
                    Some(core.nav.top().node),
                );
                gesture.set_parent_scene(Some(parent_entity));
                core.current.0 = spawn_scene_raw(
                    &mut commands,
                    &mut draw,
                    &core.tree,
                    &core.layouts,
                    &path,
                    scene_scale,
                    None,
                );
                commands
                    .entity(core.current.0)
                    .insert(Transform::from_xyz(0.0, 0.0, 0.5));
            } else {
                let _ = gesture.take_parent_scene();
                core.current.0 = spawn_scene_raw(
                    &mut commands,
                    &mut draw,
                    &core.tree,
                    &core.layouts,
                    &path,
                    scene_scale,
                    None,
                );
            }
        }
    }
}

fn spawn_scene_raw(
    commands: &mut Commands,
    draw: &mut SyncDrawAssets,
    tree: &FileTree,
    layouts: &Layouts,
    path: &[Level],
    scale: f32,
    focus: Option<NodeId>,
) -> Entity {
    let (level, outer) = path.split_last().expect("場面がある");
    let ancestors: Vec<NodeId> = outer.iter().map(|l| l.node).collect();
    let source = SceneSource {
        tree,
        layouts,
        images: &draw.images,
        font: &draw.font.0,
        ears: &draw.ears,
        highlight: Some(&draw.highlight),
        catalog: Some(&draw.catalog),
    };
    scene::spawn(
        commands,
        &mut draw.meshes,
        &mut draw.materials,
        &source,
        level.node,
        RenderLayers::layer(CONTENT_LAYER),
        scale,
        true,
        &ancestors,
        focus,
    )
}

/// ノードが Root までつながっているか（読み直しで親から外されたノードは、つながっていない）。
pub(crate) fn is_attached(tree: &FileTree, id: NodeId) -> bool {
    tree.is_attached(id)
}

/// 開いている場面のフォルダが基準から外れたまま止まったら、そのフォルダを基準にして並べ直す。
/// 深く潜っても座標が小さくなりすぎない（f32 の精度が尽きない）ようにするため。
///
/// 動いている最中（ズーム、フェード、ジェスチャー、親の場面を下に敷いているとき）は待つ。
/// カメラは同じ見え方のまま新しい座標に移し、レイヤー 0 の場面を 1 つにそろえ直す。
#[allow(clippy::too_many_arguments)]
fn reanchor(
    mut commands: Commands,
    settings: Res<Settings>,
    tree: Res<FileTree>,
    nav: Res<Nav>,
    mut layouts: ResMut<Layouts>,
    mut current: ResMut<CurrentScene>,
    mut sync: ResMut<SyncRequest>,
    window: Single<&Window, With<PrimaryWindow>>,
    camera: Single<(&mut Transform, &mut Projection), With<MainCamera>>,
    animating: Option<Res<Animating>>,
    fades: Query<(), With<Fade>>,
    gesture: Res<crate::input::gesture::Gesture>,
    scenes: Query<(Entity, &RenderLayers), With<SceneRoot>>,
    mut draw: SyncDrawAssets,
) {
    let path = scene_path(&nav, &tree);
    let folder = path[path.len() - 1].node;
    if folder == layouts.anchor
        || animating.is_some()
        || !fades.is_empty()
        || gesture.active()
        || gesture.has_parent_scene()
        || sync.mode.is_some()
    {
        return;
    }
    if !tree.is_attached(folder) {
        sync.request_reset();
        return;
    }
    // 古い座標での基準の本体 `old` が、新しい座標で `new` になる。
    let old = layouts[folder].frame;
    let new = anchor_body();
    let (spec, ears) = ear_spec(&settings, &window);
    *layouts = layout(&tree, folder, new, spec);
    *draw.ears = ears;

    let (mut transform, mut projection) = camera.into_inner();
    let mut refit = true;
    if old.height() > 0.0 {
        let k = new.height() / old.height();
        let pos = transform.translation.truncate();
        let next_pos = new.center() + (pos - old.center()) * k;
        let next_scale = match projection.as_ref() {
            Projection::Orthographic(ortho) => ortho.scale * k,
            _ => -1.0,
        };
        if k.is_finite()
            && k > 0.0
            && next_pos.is_finite()
            && next_scale.is_finite()
            && next_scale > 0.0
        {
            transform.translation = next_pos.extend(0.0);
            if let Projection::Orthographic(ortho) = projection.as_mut() {
                ortho.scale = next_scale;
            }
            refit = false;
        }
    }
    if refit {
        let (pos, scale) = fit(nav.top().frame(&layouts), &window);
        if pos.is_finite() && scale > 0.0 && scale.is_finite() {
            transform.translation = pos.extend(0.0);
            if let Projection::Orthographic(ortho) = projection.as_mut() {
                ortho.scale = scale;
            }
        }
    }

    let main_layer = RenderLayers::layer(CONTENT_LAYER);
    let mut stray = 0usize;
    for (entity, layers) in &scenes {
        if !layers.intersects(&main_layer) {
            continue;
        }
        if entity != current.0 {
            stray += 1;
        }
        commands.entity(entity).despawn();
    }
    if stray > 0 {
        warn!("基準の付け替えで想定外の場面が {stray} 個残っていたため消しました");
    }
    let (_, scene_scale) = fit(path[path.len() - 1].frame(&layouts), &window);
    current.0 = spawn_scene_raw(
        &mut commands,
        &mut draw,
        &tree,
        &layouts,
        path,
        scene_scale,
        None,
    );
    show_breadcrumb(&mut commands, &tree, nav.top().node);
}

/// 本物として描く場面までの道のり。開いているのがファイルなら、その親のフォルダまで。
pub(crate) fn scene_path<'a>(nav: &'a Nav, tree: &FileTree) -> &'a [Level] {
    scene_levels(&nav.stack, tree)
}

/// `scene_path` と同じだが、道のりを直接受け取る。
fn scene_levels<'a>(stack: &'a [Level], tree: &FileTree) -> &'a [Level] {
    let depth = stack
        .iter()
        .rposition(|l| tree.nodes[l.node].is_dir)
        .unwrap_or(0);
    &stack[..=depth]
}

/// メニューバーの上やポップアップ、ペアリングのバナーを操作しているときは、下の画面は操作しない。
pub(crate) fn ui_has_pointer(window: &Window, popup: &Popup, over_banner: bool) -> bool {
    popup.open
        || over_banner
        || window
            .cursor_position()
            .is_some_and(|p| p.y < BAR_PX || p.x > view_width(window))
}

/// 左クリックしたフォルダかファイルへズームイン、Esc で親へズームアウト（右クリックはメニュー、
/// `ui/context_menu.rs`。そこで選んだ「開く」「親へ戻る」もここで動かす）。
#[allow(clippy::too_many_arguments)]
fn navigate(
    mouse: Res<ButtonInput<MouseButton>>,
    mut keys: ResMut<ButtonInput<KeyCode>>,
    window: Single<&mut Window, With<PrimaryWindow>>,
    camera: Single<(&Camera, &GlobalTransform, &Transform, &Projection), With<MainCamera>>,
    mut location: ResMut<Location>,
    mut nav: ResMut<Nav>,
    mut current: ResMut<CurrentScene>,
    mut maker: SceneMaker,
    animating: Option<Res<Animating>>,
    fades: Query<(), With<Fade>>,
    popup: Res<Popup>,
    banners: Query<&bevy::picking::hover::Hovered, With<crate::lan::PairBanner>>,
    mut gesture: ResMut<crate::input::gesture::Gesture>,
    context_menu: Res<crate::ui::context_menu::ContextMenu>,
    request: Option<Res<crate::ui::context_menu::NavRequest>>,
    mut organize: Option<ResMut<crate::organize::OrganizeState>>,
) {
    use crate::ui::context_menu::NavRequest;
    // 右クリックのメニューで選んだ「開く」「親へ戻る」。
    let request = request.map(|r| *r);
    if request.is_some() {
        maker.commands.remove_resource::<NavRequest>();
    }
    // 右クリック「移動…」の移動先を選んでいるか。`ResMut` は書き換えるまで借りない（借りるだけで「変わった」に
    // なり、履歴パネルが毎フレーム作り直されてボタンが押せなくなるため）。
    let picking_move = request.is_none()
        && organize
            .as_ref()
            .is_some_and(|o| o.pick_move_source.is_some());
    // 移動先選び中は、カーソルの位置によらず Esc でやめられるようにする。
    if picking_move
        && keys.clear_just_pressed(KeyCode::Escape)
        && let Some(organize) = organize.as_deref_mut()
    {
        organize.pick_move_source = None;
        return;
    }
    // 切り替えの途中や、メニューやジェスチャーを操作しているときは受け付けない。
    let over_banner = banners.iter().any(|h| h.get());
    if animating.is_some()
        || !fades.is_empty()
        || gesture.active()
        || (request.is_none()
            && (context_menu.busy() || ui_has_pointer(&window, &popup, over_banner)))
    {
        return;
    }
    let window = window.into_inner();
    let (camera, camera_global, camera_transform, projection) = camera.into_inner();
    let Projection::Orthographic(ortho) = projection else {
        return;
    };

    // 右クリック「移動…」の移動先を選んでいるあいだは、Esc でやめる・左クリックで移動先フォルダを確定する。
    if picking_move && let Some(organize) = organize.as_deref_mut() {
        if keys.just_pressed(KeyCode::Escape) {
            organize.pick_move_source = None;
            return;
        }
        if mouse.just_pressed(MouseButton::Left) {
            if let Some(world) = window
                .cursor_position()
                .and_then(|p| camera.viewport_to_world_2d(camera_global, p).ok())
            {
                let path = hit_path(&maker, &nav, world);
                let picked = *path.last().unwrap_or(&FileTree::ROOT);
                let node = &maker.tree.nodes[picked];
                if node.is_dir
                    && node.remote.is_none()
                    && node.depth >= 2
                    && let Some(dest_dir) = crate::ui::tooltip::local_path(&maker.tree, picked)
                    && let Some(from) = organize.pick_move_source.take()
                {
                    organize.user_move(from, dest_dir);
                } else {
                    organize.set_notice("移動先は手元のフォルダを押してください（Esc でやめる）");
                }
            }
            return;
        }
        return;
    }
    let old = nav.stack.clone();
    let click_at = match request {
        Some(NavRequest::ClickAt(world)) => Some(world),
        Some(NavRequest::Up) => None,
        None if mouse.just_pressed(MouseButton::Left) => window
            .cursor_position()
            .and_then(|p| camera.viewport_to_world_2d(camera_global, p).ok()),
        None => None,
    };
    let go_up = matches!(request, Some(NavRequest::Up))
        || (request.is_none() && keys.just_pressed(KeyCode::Escape));
    if let Some(world) = click_at {
        // クリックした所にあるもの（今のものの中なら子、外なら隣や親の隣、何も無い所なら親）。
        let path = hit_path(&maker, &nav, world);
        if path.len() == old.len() && path.iter().zip(&old).all(|(a, b)| *a == b.node) {
            return;
        }
        nav.stack = path.into_iter().map(|node| Level { node }).collect();
    } else if go_up {
        if nav.stack.len() < 2 {
            return;
        }
        nav.stack.pop();
    } else {
        return;
    }

    let next = *nav.top();
    let (to_pos, to_scale) = fit(next.frame(&maker.layouts), &window);
    if !(to_scale > 0.0 && to_scale.is_finite() && to_pos.is_finite()) {
        // 行き先がまだ配置に無い（高さ 0）なら遷移を戻す。
        nav.stack = old;
        return;
    }

    // 正の場所を書き換える（GUI 主導の遷移）。
    *location = Location::from_tree(&maker.tree, next.node);

    // 縮めたまま止まって下に敷いてあった親の場面は、もう要らない。
    if let Some(parent) = gesture.take_parent_scene() {
        maker.commands.entity(parent).despawn();
    }
    let then_show = change_scene(&old, &nav, &mut current, &mut maker, &window);

    show_breadcrumb(&mut maker.commands, &maker.tree, next.node);
    maker.commands.insert_resource(Animating {
        from_pos: camera_transform.translation.truncate(),
        from_scale: ortho.scale,
        to_pos,
        to_scale,
        started: Instant::now(),
        then_show,
        seconds: ZOOM_SECONDS,
        ease: Ease::InOut,
    });
}

/// `world` の所にあるものまでの道のり。今開いているものへの道のりをたどり、そこから外れた最初の
/// もの（今のものの中なら子、外なら隣や、親の隣など）を選ぶ。何も無い所なら、そこを含むもの。
fn hit_path(maker: &SceneMaker, nav: &Nav, world: Vec2) -> Vec<NodeId> {
    let mut path = vec![FileTree::ROOT];
    let mut node = FileTree::ROOT;
    while let Some(child) = maker.tree.nodes[node]
        .children
        .iter()
        .copied()
        .find(|c| maker.layouts[*c].cell.contains(world))
    {
        path.push(child);
        node = child;
        let on_path = nav
            .stack
            .get(path.len() - 1)
            .is_some_and(|l| l.node == child);
        if !on_path {
            break;
        }
    }
    path
}

/// 開いている場所が `old` から今の `nav` へ変わったときに、ズームの前に場面を入れ替える。
/// ズームが終わったら本物の場面に切り替えるべきなら true を返す。
fn change_scene(
    old: &[Level],
    nav: &Nav,
    current: &mut CurrentScene,
    maker: &mut SceneMaker,
    window: &Window,
) -> bool {
    let old_scene = scene_levels(old, &maker.tree).to_vec();
    let new_scene = scene_levels(&nav.stack, &maker.tree).to_vec();
    let old_folder = old_scene[old_scene.len() - 1].node;
    let new_folder = new_scene[new_scene.len() - 1].node;
    let scale_of = |levels: &[Level], maker: &SceneMaker| {
        fit(levels[levels.len() - 1].frame(&maker.layouts), window).1
    };
    // 描く場面が同じなら（ファイルを開く・閉じるとき）、入れ替えない。
    if old_folder == new_folder {
        return false;
    }
    let outward = old_scene.len() > new_scene.len()
        && new_scene
            .iter()
            .zip(&old_scene)
            .all(|(a, b)| a.node == b.node);
    if outward {
        // 外へ戻る: 行き先の本物の場面を先に作り、今の場面を上に重ねてフェードアウトさせる
        // （重なった場面は作った順に描かれるので、去る場面は後に作り直す）。
        let focus = old_scene[new_scene.len()].node;
        maker.commands.entity(current.0).despawn();
        current.0 = maker.spawn_around(&new_scene, scale_of(&new_scene, maker), Some(focus));
        let leaving = maker.spawn(&old_scene, scale_of(&old_scene, maker));
        maker.commands.entity(leaving).insert((
            Fade {
                started: Instant::now(),
                fade_in: false,
                replaces: None,
            },
            Transform::from_xyz(0.0, 0.0, 0.5),
        ));
        return false;
    }
    // 中や隣へ進む: 行き先を含むフォルダの場面で見せ、画像のままズームして、着いてから本物に
    // 切り替える。ファイルは含むフォルダの場面にそのまま描いてあるので切り替えない。
    let target = nav.top().node;
    let container = &nav.stack[..nav.stack.len() - 1];
    if container.last().map(|l| l.node) != Some(old_folder) {
        maker.commands.entity(current.0).despawn();
        current.0 = maker.spawn(container, scale_of(container, maker));
    }
    maker.tree.nodes[target].is_dir
}

/// 環境変数 `LISA_UPDATE_MODE` から `WinitSettings` と起動ログ用の名前を決める。
/// 知らない値は既定の `reactive` にして `warn!` を出す。
pub(crate) fn update_settings_from_env() -> (WinitSettings, &'static str) {
    let raw = std::env::var("LISA_UPDATE_MODE").ok();
    let (settings, label, unknown) = parse_update_mode(raw.as_deref());
    if let Some(bad) = unknown {
        warn!("知らない LISA_UPDATE_MODE={bad:?}（reactive を使います）");
    }
    (settings, label)
}

pub(crate) fn parse_update_mode(raw: Option<&str>) -> (WinitSettings, &'static str, Option<&str>) {
    let reactive = || WinitSettings {
        // 止まっているあいだは1秒に1回。入力があればすぐ描画する。
        focused_mode: UpdateMode::reactive(Duration::from_secs(1)),
        unfocused_mode: UpdateMode::reactive_low_power(Duration::from_secs(1)),
    };
    let Some(s) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return (reactive(), "reactive (default)", None);
    };
    if s.eq_ignore_ascii_case("reactive") {
        (reactive(), "reactive", None)
    } else if s.eq_ignore_ascii_case("continuous") {
        (
            WinitSettings {
                focused_mode: UpdateMode::Continuous,
                unfocused_mode: UpdateMode::Continuous,
            },
            "continuous",
            None,
        )
    } else {
        (reactive(), "reactive (default)", Some(s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_update_mode_values() {
        let (s, label, bad) = parse_update_mode(None);
        assert_eq!(label, "reactive (default)");
        assert!(bad.is_none());
        assert!(matches!(s.focused_mode, UpdateMode::Reactive { .. }));

        let (_, label, bad) = parse_update_mode(Some(""));
        assert_eq!(label, "reactive (default)");
        assert!(bad.is_none());

        let (s, label, bad) = parse_update_mode(Some("reactive"));
        assert_eq!(label, "reactive");
        assert!(bad.is_none());
        assert!(matches!(s.focused_mode, UpdateMode::Reactive { .. }));

        let (s, label, bad) = parse_update_mode(Some("CONTINUOUS"));
        assert_eq!(label, "continuous");
        assert!(bad.is_none());
        assert!(matches!(s.focused_mode, UpdateMode::Continuous));
        assert!(matches!(s.unfocused_mode, UpdateMode::Continuous));

        let (s, label, bad) = parse_update_mode(Some("weird"));
        assert_eq!(label, "reactive (default)");
        assert_eq!(bad, Some("weird"));
        assert!(matches!(s.focused_mode, UpdateMode::Reactive { .. }));
    }

    #[test]
    fn track_window_occlusion_updates_state_and_requests_redraw_on_visible() {
        let mut app = App::new();
        app.init_resource::<WindowOccludedState>()
            .add_message::<WindowOccluded>()
            .add_message::<WindowFocused>()
            .add_message::<RequestRedraw>()
            .add_systems(Update, track_window_occlusion);

        let win = app.world_mut().spawn(PrimaryWindow).id();

        // 隠れたときは occluded=true になり、描き直しは頼まない。
        app.world_mut().write_message(WindowOccluded {
            window: win,
            occluded: true,
        });
        app.update();
        assert!(app.world().resource::<WindowOccludedState>().0);
        let mut cursor = bevy::ecs::message::MessageCursor::<RequestRedraw>::default();
        assert_eq!(
            cursor
                .read(app.world().resource::<Messages<RequestRedraw>>())
                .count(),
            0
        );

        // 再び見えたときは occluded=false になり、描き直しを 1 回頼む。
        app.world_mut().write_message(WindowOccluded {
            window: win,
            occluded: false,
        });
        app.update();
        assert!(!app.world().resource::<WindowOccludedState>().0);
        assert_eq!(
            cursor
                .read(app.world().resource::<Messages<RequestRedraw>>())
                .count(),
            1
        );

        // フォーカスが戻ったときも 1 回描き直しを頼む。
        app.world_mut().write_message(WindowFocused {
            window: win,
            focused: true,
        });
        app.update();
        assert_eq!(
            cursor
                .read(app.world().resource::<Messages<RequestRedraw>>())
                .count(),
            1
        );
    }

    #[test]
    fn location_from_tree_and_sync_request_priority() {
        let mut tree = FileTree::dummy();
        let dev = tree.nodes[FileTree::ROOT].children[0];
        let dir = tree.nodes[dev].children[0];
        // DB の entry が無い木では Named になる。
        assert_eq!(
            Location::from_tree(&tree, dir),
            Location::Named(vec![
                "Root".into(),
                tree.nodes[dev].name.clone(),
                tree.nodes[dir].name.clone()
            ])
        );
        // entry がドライブ以下でそろっていれば Local になる。
        tree.nodes[dev].entry = Some(10);
        tree.nodes[dir].entry = Some(20);
        assert_eq!(
            Location::from_tree(&tree, dir),
            Location::Local(vec![10, 20])
        );

        // 木から外れたノードでは Root にフォールバックする。
        tree.reset_children(dev);
        assert!(!tree.is_attached(dir));
        assert!(tree.path_from_root(dir).is_none());
        assert_eq!(Location::from_tree(&tree, dir), Location::root());

        // SyncRequest は優先度の高いモードへ昇格し、低いモードで上書きされない。
        let mut sync = SyncRequest::default();
        sync.request_redraw();
        assert_eq!(sync.mode, Some(SyncMode::RedrawScene));
        sync.request_relayout(true);
        assert_eq!(sync.mode, Some(SyncMode::RelayoutKeepView));
        assert!(sync.refit);
        sync.request_redraw();
        assert_eq!(sync.mode, Some(SyncMode::RelayoutKeepView));
        sync.request_reset();
        assert_eq!(sync.mode, Some(SyncMode::Reset));
        sync.request_relayout(false);
        assert_eq!(sync.mode, Some(SyncMode::Reset));
    }
}
