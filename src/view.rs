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
use bevy::window::{PrimaryWindow, RequestRedraw, WindowResized};
use bevy::winit::{UpdateMode, WinitSettings};

use crate::layout::{EarSpec, Layouts, anchor_body, fitted_size, layout};
use crate::model::tree::{FileTree, NodeId};
use crate::render::cache::{self, Bake, BakeFinished, FolderImages};
use crate::render::ear::{EarSizes, update_ears};
use crate::render::scene::{self, BACKGROUND, LabelFont, SceneRoot, SceneSource};
use crate::render::shapes::BaseAlpha;
use crate::settings::Settings;
use crate::ui::menu::{BAR_PX, MenuBreadcrumb, MenuSystems, Popup};

mod camera;
mod fade;
mod title;

pub use camera::Busy;
pub(crate) use camera::{
    Animating, Ease, MainCamera, ZOOM_SECONDS, animate_camera, fit, keep_redrawing,
    refit_on_resize, set_side_px, side_px, view_width,
};
pub(crate) use fade::{Fade, fade_scenes};
pub use title::TitleSuffix;
pub(crate) use title::{Breadcrumb, show_breadcrumb, update_title};

pub struct TreeViewPlugin;

/// 画面の処理（木を読む処理 `scan.rs` は、この前に動く）。
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct ViewSystems;

impl Plugin for TreeViewPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(WinitSettings {
            // 止まっているあいだは1秒に1回。入力があればすぐ描画する。
            focused_mode: UpdateMode::reactive(Duration::from_secs(1)),
            unfocused_mode: UpdateMode::reactive_low_power(Duration::from_secs(1)),
        })
        .insert_resource(ClearColor(BACKGROUND))
        .insert_resource(Nav {
            stack: vec![Level {
                node: FileTree::ROOT,
            }],
        })
        .insert_resource(Bake::Waiting)
        .insert_resource(cache::ImageCache::new())
        .insert_resource(Breadcrumb("Root".into()))
        .init_resource::<Busy>()
        .init_resource::<crate::render::status::PulseClock>()
        .init_resource::<crate::input::gesture::Gesture>()
        .init_resource::<crate::input::gesture::TrackpadInput>()
        .add_systems(
            Startup,
            (setup, crate::input::gesture::install_scroll_monitor),
        )
        .add_systems(
            Update,
            (
                cache::bake,
                show_baked,
                jump,
                relayout,
                reanchor,
                show_relayout,
                navigate,
                crate::input::gesture::gesture,
                refit_on_resize,
                follow_side_panel,
                animate_camera,
                update_ears,
                fade_scenes,
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

/// 開いてきた道のり。最後が今フィーチャーしている（枠が画面いっぱいに映っている）もの。
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
        };
        scene::spawn(
            &mut self.commands,
            &mut self.meshes,
            &mut self.materials,
            &source,
            level.node,
            RenderLayers::layer(0),
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
    assets: Res<AssetServer>,
    mut images: ResMut<Assets<Image>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<ColorMaterial>>,
    window: Single<&Window, With<PrimaryWindow>>,
) {
    let font = assets.load("fonts/NotoSansJP-Light.otf");
    // 本体はどれも √2:1（A4 の横長）。耳の高さの設定は、本体を画面に収めたときのピクセル数で決める。
    // 最初は Root を基準にして並べる。
    let (spec, ears) = ear_spec(&settings, &window);
    let layouts = layout(&tree, FileTree::ROOT, anchor_body(), spec);
    let mut folder_images = FolderImages::default();
    folder_images.prepare(&tree, &layouts, &mut images);

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
    ));

    // 画像が焼き上がるまでは、子フォルダは空の枠で見せる。
    let source = SceneSource {
        tree: &tree,
        layouts: &layouts,
        images: &folder_images,
        font: &font,
        ears: &ears,
        highlight: None,
    };
    let root_scene = scene::spawn(
        &mut commands,
        &mut meshes,
        &mut materials,
        &source,
        FileTree::ROOT,
        RenderLayers::layer(0),
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

/// 配置し直したので、今の場面を作り直す。`refit` なら、カメラも開いているものに合わせ直す
/// （基準を付け替えただけのときは、カメラはもう新しい座標に移してあるので合わせ直さない）。
#[derive(Resource)]
struct Relaid {
    refit: bool,
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
    transform.translation = pos.extend(0.0);
    if let Projection::Orthographic(ortho) = projection.as_mut() {
        ortho.scale = scale;
    }
}

/// 一覧から選んだ場所（`scan::JumpPath`）へ飛ぶ。その場所を含むフォルダを基準に並べ直し、
/// カメラをその場所に合わせる（ズームの動きはしない）。動いている最中は、終わるまで待つ。
#[allow(clippy::too_many_arguments)]
fn jump(
    mut commands: Commands,
    jump: Option<Res<crate::scan::JumpPath>>,
    settings: Res<Settings>,
    tree: Res<FileTree>,
    mut nav: ResMut<Nav>,
    mut layouts: ResMut<Layouts>,
    mut folder_images: ResMut<FolderImages>,
    mut images: ResMut<Assets<Image>>,
    mut bake: ResMut<Bake>,
    window: Single<&Window, With<PrimaryWindow>>,
    animating: Option<Res<Animating>>,
    fades: Query<(), With<Fade>>,
    gesture: Res<crate::input::gesture::Gesture>,
) {
    let Some(jump) = jump else {
        return;
    };
    if animating.is_some()
        || !fades.is_empty()
        || gesture.active()
        || gesture.has_parent_scene()
        || !matches!(*bake, Bake::Done)
    {
        return;
    }
    commands.remove_resource::<crate::scan::JumpPath>();
    let Some(&target) = jump.0.last() else {
        return;
    };
    nav.stack = jump.0.iter().map(|&node| Level { node }).collect();
    let scene = scene_path(&nav, &tree);
    let anchor = scene[scene.len() - 1].node;
    let (spec, ears) = ear_spec(&settings, &window);
    *layouts = layout(&tree, anchor, anchor_body(), spec);
    folder_images.prepare(&tree, &layouts, &mut images);
    commands.insert_resource(ears);
    show_breadcrumb(&mut commands, &tree, target);
    *bake = Bake::Waiting;
    commands.insert_resource(Relaid { refit: true });
}

/// 大きさの設定が変わったら、木を配置し直し、画像を焼き直す。
#[allow(clippy::too_many_arguments)]
fn relayout(
    mut commands: Commands,
    settings: Res<Settings>,
    tree: Res<FileTree>,
    mut layouts: ResMut<Layouts>,
    mut folder_images: ResMut<FolderImages>,
    mut images: ResMut<Assets<Image>>,
    mut bake: ResMut<Bake>,
    mut pending: Local<bool>,
    mut laid_out_with: Local<Option<[f32; 2]>>,
    window: Single<&Window, With<PrimaryWindow>>,
    animating: Option<Res<Animating>>,
    fades: Query<(), With<Fade>>,
    gesture: Res<crate::input::gesture::Gesture>,
    tree_changed: Option<ResMut<crate::scan::TreeChanged>>,
) {
    // 耳の高さが変わったとき（ほかの設定では配置は変わらない）か、木が変わったとき。
    let ears = [settings.ear_selected_px, settings.ear_preview_px];
    if laid_out_with.is_some_and(|h| h != ears) {
        *pending = true;
    }
    *laid_out_with = Some(ears);
    let mut tree_changed = tree_changed;
    let tree_due = tree_changed.as_ref().is_some_and(|c| c.due());
    // ズームや画像を焼いている途中、指で動かしている途中なら、終わってからやる。
    let busy = animating.is_some()
        || !fades.is_empty()
        || gesture.active()
        || gesture.has_parent_scene()
        || !matches!(*bake, Bake::Done);
    if !(*pending || tree_due) || busy {
        return;
    }
    // 耳の高さを変えたときは、開いているものに合わせ直す。木が変わっただけなら、見え方はそのまま。
    let refit = *pending;
    *pending = false;
    if let Some(changed) = tree_changed.as_mut() {
        changed.pending = false;
        changed.last = Some(Instant::now());
    }

    let (spec, ears) = ear_spec(&settings, &window);
    // 基準のフォルダは同じ場所に置いたまま、耳の大きさだけ変えて並べ直す。
    let anchor = layouts.anchor;
    *layouts = layout(&tree, anchor, layouts[anchor].frame, spec);
    folder_images.prepare(&tree, &layouts, &mut images);
    commands.insert_resource(ears);

    *bake = Bake::Waiting;
    commands.insert_resource(Relaid { refit });
}

/// 開いている場面のフォルダが基準から外れたまま止まったら、そのフォルダを基準にして並べ直す。
/// 深く潜っても座標が小さくなりすぎない（f32 の精度が尽きない）ようにするため。
///
/// 動いている最中（ズーム、フェード、ジェスチャー、画像を焼いている途中、親の場面を下に敷いて
/// いるとき）は待つ。カメラは同じ見え方のまま新しい座標に移し、場面を作り直して画像を焼き直す。
#[allow(clippy::too_many_arguments)]
fn reanchor(
    mut commands: Commands,
    settings: Res<Settings>,
    tree: Res<FileTree>,
    nav: Res<Nav>,
    mut layouts: ResMut<Layouts>,
    mut folder_images: ResMut<FolderImages>,
    mut images: ResMut<Assets<Image>>,
    mut bake: ResMut<Bake>,
    window: Single<&Window, With<PrimaryWindow>>,
    camera: Single<(&mut Transform, &mut Projection), With<MainCamera>>,
    animating: Option<Res<Animating>>,
    relaid: Option<Res<Relaid>>,
    fades: Query<(), With<Fade>>,
    gesture: Res<crate::input::gesture::Gesture>,
) {
    let path = scene_path(&nav, &tree);
    let folder = path[path.len() - 1].node;
    if folder == layouts.anchor
        || animating.is_some()
        || relaid.is_some()
        || !fades.is_empty()
        || gesture.active()
        || gesture.has_parent_scene()
        || !matches!(*bake, Bake::Done)
    {
        return;
    }
    // 古い座標での基準の本体 `old` が、新しい座標で `new` になる。
    let old = layouts[folder].frame;
    let new = anchor_body();
    let k = new.height() / old.height();
    let (spec, _) = ear_spec(&settings, &window);
    *layouts = layout(&tree, folder, new, spec);
    folder_images.prepare(&tree, &layouts, &mut images);

    let (mut transform, mut projection) = camera.into_inner();
    let pos = transform.translation.truncate();
    transform.translation = (new.center() + (pos - old.center()) * k).extend(0.0);
    if let Projection::Orthographic(ortho) = projection.as_mut() {
        ortho.scale *= k;
    }
    *bake = Bake::Waiting;
    commands.insert_resource(Relaid { refit: false });
}

/// 配置し直したあと、カメラを合わせ直して今の場面を作り直す。
fn show_relayout(
    relaid: Option<Res<Relaid>>,
    mut current: ResMut<CurrentScene>,
    mut maker: SceneMaker,
    nav: Res<Nav>,
    window: Single<&Window, With<PrimaryWindow>>,
    camera: Single<(&mut Transform, &mut Projection), With<MainCamera>>,
) {
    let Some(relaid) = relaid else {
        return;
    };
    maker.commands.remove_resource::<Relaid>();
    if relaid.refit {
        let (pos, scale) = fit(nav.top().frame(&maker.layouts), &window);
        let (mut transform, mut projection) = camera.into_inner();
        transform.translation = pos.extend(0.0);
        if let Projection::Orthographic(ortho) = projection.as_mut() {
            ortho.scale = scale;
        }
    }
    let path = scene_path(&nav, &maker.tree);
    let (_, scene_scale) = fit(path[path.len() - 1].frame(&maker.layouts), &window);
    maker.commands.entity(current.0).despawn();
    current.0 = maker.spawn(path, scene_scale);
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

/// メニューバーの上やポップアップを操作しているときは、下の画面は操作しない。
pub(crate) fn ui_has_pointer(window: &Window, popup: &Popup) -> bool {
    popup.open
        || window
            .cursor_position()
            .is_some_and(|p| p.y < BAR_PX || p.x > view_width(window))
}

/// 画像が焼き上がったら、今の場面を作り直して画像を使う。
fn show_baked(
    finished: Option<Res<BakeFinished>>,
    mut current: ResMut<CurrentScene>,
    mut maker: SceneMaker,
    nav: Res<Nav>,
    window: Single<&Window, With<PrimaryWindow>>,
) {
    if finished.is_none() {
        return;
    }
    maker.commands.remove_resource::<BakeFinished>();
    maker.commands.entity(current.0).despawn();
    let path = scene_path(&nav, &maker.tree);
    let (_, scale) = fit(path[path.len() - 1].frame(&maker.layouts), &window);
    current.0 = maker.spawn(path, scale);
}

/// 左クリックしたフォルダかファイルへズームイン、右クリックか Esc で親へズームアウト。
#[allow(clippy::too_many_arguments)]
fn navigate(
    mouse: Res<ButtonInput<MouseButton>>,
    keys: Res<ButtonInput<KeyCode>>,
    window: Single<&mut Window, With<PrimaryWindow>>,
    camera: Single<(&Camera, &GlobalTransform, &Transform, &Projection), With<MainCamera>>,
    mut nav: ResMut<Nav>,
    mut current: ResMut<CurrentScene>,
    mut maker: SceneMaker,
    animating: Option<Res<Animating>>,
    fades: Query<(), With<Fade>>,
    popup: Res<Popup>,
    mut gesture: ResMut<crate::input::gesture::Gesture>,
) {
    // 切り替えの途中や、メニューやジェスチャーを操作しているときは受け付けない。
    if animating.is_some()
        || !fades.is_empty()
        || gesture.active()
        || ui_has_pointer(&window, &popup)
    {
        return;
    }
    let window = window.into_inner();
    let (camera, camera_global, camera_transform, projection) = camera.into_inner();
    let Projection::Orthographic(ortho) = projection else {
        return;
    };
    let old = nav.stack.clone();
    if mouse.just_pressed(MouseButton::Left) {
        // クリックした所にあるもの（今のものの中なら子、外なら隣や親の隣、何も無い所なら親）。
        let Some(world) = window
            .cursor_position()
            .and_then(|p| camera.viewport_to_world_2d(camera_global, p).ok())
        else {
            return;
        };
        let path = hit_path(&maker, &nav, world);
        if path.len() == old.len() && path.iter().zip(&old).all(|(a, b)| *a == b.node) {
            return;
        }
        nav.stack = path.into_iter().map(|node| Level { node }).collect();
    } else if mouse.just_pressed(MouseButton::Right) || keys.just_pressed(KeyCode::Escape) {
        if nav.stack.len() < 2 {
            return;
        }
        nav.stack.pop();
    } else {
        return;
    }

    // 縮めたまま止まって下に敷いてあった親の場面は、もう要らない。
    if let Some(parent) = gesture.take_parent_scene() {
        maker.commands.entity(parent).despawn();
    }
    let then_show = change_scene(&old, &nav, &mut current, &mut maker, &window);

    let next = *nav.top();
    show_breadcrumb(&mut maker.commands, &maker.tree, next.node);
    let (to_pos, to_scale) = fit(next.frame(&maker.layouts), &window);
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
