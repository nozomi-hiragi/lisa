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

use crate::cache::{self, Bake, BakeFinished, FolderImages};
use crate::layout::{Layouts, column_items, column_of, layout};
use crate::menu::{BAR_PX, MenuBreadcrumb, MenuSystems, Popup};
use crate::scene::{self, BACKGROUND, LabelFont, Rows, SceneRoot, SceneSource};
use crate::settings::Settings;
use crate::shapes::BaseAlpha;
use crate::tree::{FileTree, NodeId};

/// ワールド座標での Root の高さ。幅はウィンドウの縦横比に合わせる。
const ROOT_HEIGHT: f32 = 1000.0;
/// ズームにかける時間（秒）。
pub(crate) const ZOOM_SECONDS: f32 = 0.4;
/// 画像から本物の描画へ切り替えるときのフェードの時間（秒）。
const FADE_SECONDS: f32 = 0.1;

pub struct TreeViewPlugin;

impl Plugin for TreeViewPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(WinitSettings {
            // 止まっているあいだは1秒に1回。入力があればすぐ描画する。
            focused_mode: UpdateMode::reactive(Duration::from_secs(1)),
            unfocused_mode: UpdateMode::reactive_low_power(Duration::from_secs(1)),
        })
        .insert_resource(ClearColor(BACKGROUND))
        .insert_resource(FileTree::dummy())
        .insert_resource(Nav {
            stack: vec![Level {
                node: FileTree::ROOT,
                shift: 0.0,
                scroll: [0.0, 0.0],
            }],
        })
        .insert_resource(Bake::Waiting)
        .insert_resource(Breadcrumb("Root".into()))
        .init_resource::<Busy>()
        .init_resource::<crate::gesture::Gesture>()
        .init_resource::<crate::gesture::ScrollPhase>()
        .add_systems(Startup, setup)
        .add_systems(
            Update,
            (
                cache::bake,
                show_baked,
                relayout,
                show_relayout,
                navigate,
                crate::gesture::gesture,
                refit_on_resize,
                animate_camera,
                fade_scenes,
                keep_redrawing,
                update_title,
            )
                .chain()
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
    /// 場面全体を上下にずらす量（親がスクロールしていた分）。
    pub(crate) shift: f32,
    /// 列（ファイル、フォルダ）ごとに、何行目から見せているか（小数で行の途中も）。
    pub(crate) scroll: [f32; 2],
}

impl Level {
    /// 画面に映すべき枠（ワールド座標）。
    pub(crate) fn frame(&self, layouts: &Layouts) -> Rect {
        let frame = layouts.0[self.node].frame;
        Rect {
            min: frame.min + Vec2::new(0.0, self.shift),
            max: frame.max + Vec2::new(0.0, self.shift),
        }
    }

    /// その列の行を描いている位置のずれ（自分のずれ＋その列のスクロールの分）。
    pub(crate) fn column_shift(&self, layouts: &Layouts, column: usize) -> f32 {
        self.shift + self.scroll[column] * layouts.0[self.node].list.row_height
    }

    /// 今見えている、その列の子。
    pub(crate) fn visible(&self, tree: &FileTree, layouts: &Layouts, column: usize) -> Vec<NodeId> {
        let items = column_items(tree, self.node, column);
        let fit = layouts.0[self.node].list.fit;
        let first = (self.scroll[column].floor() as usize).min(items.len());
        let last = ((self.scroll[column] + fit as f32).ceil() as usize).min(items.len());
        items[first..last].to_vec()
    }
}

/// 今いる場所（Root からの道のり）。
#[derive(Resource)]
pub(crate) struct Breadcrumb(pub(crate) String);

/// ズームやフェードの最中なら true（毎フレーム描画している）。
#[derive(Resource, Default)]
pub struct Busy(pub bool);

/// タイトルの後ろに添える文字（デバッグ用）。None なら何も添えない。
#[derive(Resource, Default)]
pub struct TitleSuffix(pub Option<String>);

/// 画面を映すカメラ（画像を焼くカメラと区別する）。
#[derive(Component)]
pub(crate) struct MainCamera;

/// 今画面に本物として描いている場面。
#[derive(Resource)]
pub(crate) struct CurrentScene(pub(crate) Entity);

/// カメラのズーム中だけ存在する。これがあるあいだはディスプレイの FPS で描画する。
#[derive(Resource)]
pub(crate) struct Animating {
    pub(crate) from_pos: Vec2,
    pub(crate) from_scale: f32,
    pub(crate) to_pos: Vec2,
    pub(crate) to_scale: f32,
    /// 待機中は Bevy の Time の進み方が不規則なので、実時間で測る。
    pub(crate) started: Instant,
    /// ズームが終わったら、今の位置（Nav の最後）の本物の場面に切り替える。
    pub(crate) then_show: bool,
    /// かける時間（秒）と、速さの緩急。
    pub(crate) seconds: f32,
    pub(crate) ease: Ease,
}

/// 場面のフェード。終わったら、フェードアウトなら場面を消し、フェードインなら `replaces` を消す。
#[derive(Component)]
pub(crate) struct Fade {
    pub(crate) started: Instant,
    pub(crate) fade_in: bool,
    pub(crate) replaces: Option<Entity>,
}

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
}

impl SceneMaker<'_, '_> {
    /// 画面に出す本物の場面を作る。`path` の最後がその場面で、手前は外側のフォルダ。
    /// `scale` はその場面を画面いっぱいに見るときの倍率。
    pub(crate) fn spawn(&mut self, path: &[Level], scale: f32) -> Entity {
        let level = path.last().expect("場面がある");
        self.spawn_rows(path, scale, Rows::Scroll(level.scroll))
    }

    /// `spawn` と同じだが、行の見せ方を指定する。
    pub(crate) fn spawn_rows(&mut self, path: &[Level], scale: f32, rows: Rows) -> Entity {
        let (level, outer) = path.split_last().expect("場面がある");
        let ancestors: Vec<(NodeId, f32)> = outer.iter().map(|l| (l.node, l.shift)).collect();
        let source = SceneSource {
            tree: &self.tree,
            layouts: &self.layouts.0,
            images: &self.images,
            font: &self.font.0,
        };
        scene::spawn(
            &mut self.commands,
            &mut self.meshes,
            &mut self.materials,
            &source,
            level.node,
            RenderLayers::layer(0),
            scale,
            level.shift,
            rows,
            &ancestors,
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
    // 枠はメニューバーの下の領域と同じ縦横比にする。
    let view_height = window.height() - BAR_PX;
    let aspect = window.width() / view_height;
    let root_rect =
        Rect::from_center_size(Vec2::ZERO, Vec2::new(ROOT_HEIGHT * aspect, ROOT_HEIGHT));
    let layouts = layout(
        &tree,
        root_rect,
        aspect,
        settings.row_height_px / view_height,
    );
    commands.insert_resource(Geometry {
        root_rect,
        aspect,
        view_height,
    });
    let folder_images = cache::create_images(&tree, aspect, &mut images);

    let (pos, scale) = fit(root_rect, &window);
    commands.spawn((
        MainCamera,
        IsDefaultUiCamera,
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
    };
    let root_scene = scene::spawn(
        &mut commands,
        &mut meshes,
        &mut materials,
        &source,
        FileTree::ROOT,
        RenderLayers::layer(0),
        scale,
        0.0,
        Rows::Scroll([0.0, 0.0]),
        &[],
    );

    commands.insert_resource(CurrentScene(root_scene));
    commands.insert_resource(Layouts(layouts));
    commands.insert_resource(folder_images);
    commands.insert_resource(LabelFont(font));
}

/// 木を配置したときの条件（設定が変わったら、これで配置し直す）。
#[derive(Resource)]
struct Geometry {
    root_rect: Rect,
    aspect: f32,
    view_height: f32,
}

/// 配置し直したので、今の場面を作り直す。
#[derive(Resource)]
struct Relaid;

/// 行の高さの設定が変わったら、木を配置し直し、画像を焼き直す。
#[allow(clippy::too_many_arguments)]
fn relayout(
    mut commands: Commands,
    settings: Res<Settings>,
    geometry: Res<Geometry>,
    tree: Res<FileTree>,
    mut layouts: ResMut<Layouts>,
    mut nav: ResMut<Nav>,
    mut bake: ResMut<Bake>,
    mut pending: Local<bool>,
    mut laid_out_with: Local<Option<f32>>,
    animating: Option<Res<Animating>>,
    fades: Query<(), With<Fade>>,
) {
    // 行の高さが変わったときだけ（ほかの設定では配置は変わらない）。
    let row_height = settings.row_height_px;
    if laid_out_with.is_some_and(|h| h != row_height) {
        *pending = true;
    }
    *laid_out_with = Some(row_height);
    // ズームや画像を焼いている途中なら、終わってからやる。
    if !*pending || animating.is_some() || !fades.is_empty() || !matches!(*bake, Bake::Done) {
        return;
    }
    *pending = false;

    layouts.0 = layout(
        &tree,
        geometry.root_rect,
        geometry.aspect,
        settings.row_height_px / geometry.view_height,
    );

    // 開いてきた道のりの位置を、新しい配置で求め直す。開いている子が見えるようにスクロールも直す。
    for i in 0..nav.stack.len() {
        let level = nav.stack[i];
        let list = layouts.0[level.node].list;
        let mut scroll = level.scroll;
        for (column, value) in scroll.iter_mut().enumerate() {
            let count = column_items(&tree, level.node, column).len();
            *value = value.min(count.saturating_sub(list.fit) as f32);
        }
        if let Some(next) = nav.stack.get(i + 1).map(|l| l.node) {
            let column = column_of(&tree, next);
            let index = column_items(&tree, level.node, column)
                .iter()
                .position(|c| *c == next)
                .unwrap_or(0) as f32;
            if index < scroll[column] {
                scroll[column] = index;
            } else if index + 1.0 > scroll[column] + list.fit as f32 {
                scroll[column] = index + 1.0 - list.fit as f32;
            }
            nav.stack[i].scroll = scroll;
            nav.stack[i + 1].shift = nav.stack[i].column_shift(&layouts, column);
        } else {
            nav.stack[i].scroll = scroll;
        }
    }

    *bake = Bake::Waiting;
    commands.insert_resource(Relaid);
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
    if relaid.is_none() {
        return;
    }
    maker.commands.remove_resource::<Relaid>();
    let (pos, scale) = fit(nav.top().frame(&maker.layouts), &window);
    let (mut transform, mut projection) = camera.into_inner();
    transform.translation = pos.extend(0.0);
    if let Projection::Orthographic(ortho) = projection.as_mut() {
        ortho.scale = scale;
    }
    let path = scene_path(&nav, &maker.tree);
    let (_, scene_scale) = fit(path[path.len() - 1].frame(&maker.layouts), &window);
    maker.commands.entity(current.0).despawn();
    current.0 = maker.spawn(path, scene_scale);
}

/// 本物として描く場面までの道のり。開いているのがファイルなら、その親のフォルダまで。
pub(crate) fn scene_path<'a>(nav: &'a Nav, tree: &FileTree) -> &'a [Level] {
    let depth = nav
        .stack
        .iter()
        .rposition(|l| tree.nodes[l.node].is_dir)
        .unwrap_or(0);
    &nav.stack[..=depth]
}

/// 今いる場所を、ウィンドウのタイトルとメニューバーに出す。
pub(crate) fn show_breadcrumb(commands: &mut Commands, tree: &FileTree, node: NodeId) {
    commands.insert_resource(Breadcrumb(tree.path(node)));
    commands.insert_resource(MenuBreadcrumb(tree.path(node)));
}

/// `rect` がメニューバーの下の領域いっぱいに収まるカメラの位置と倍率。
pub(crate) fn fit(rect: Rect, window: &Window) -> (Vec2, f32) {
    let view_height = window.height() - BAR_PX;
    let scale = (rect.width() / window.width()).max(rect.height() / view_height);
    // 画面の中心より、バーの高さの半分だけ下に枠の中心が来るようにする。
    (rect.center() + Vec2::new(0.0, BAR_PX / 2.0 * scale), scale)
}

/// メニューバーの上やポップアップを操作しているときは、下の画面は操作しない。
pub(crate) fn ui_has_pointer(window: &Window, popup: &Popup) -> bool {
    popup.open || window.cursor_position().is_some_and(|p| p.y < BAR_PX)
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
    gesture: Res<crate::gesture::Gesture>,
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
    let top = *nav.top();

    let then_show = if mouse.just_pressed(MouseButton::Left) {
        // 今の場面に見えている行のうち、クリックした行（名前か枠）。
        let Some(world) = window
            .cursor_position()
            .and_then(|p| camera.viewport_to_world_2d(camera_global, p).ok())
        else {
            return;
        };
        let hit = (0..2).find_map(|column| {
            let shift = top.column_shift(&maker.layouts, column);
            let local = world - Vec2::new(0.0, shift);
            top.visible(&maker.tree, &maker.layouts, column)
                .into_iter()
                .find(|c| maker.layouts.0[*c].row.contains(local))
                .map(|c| (c, shift))
        });
        let Some((child, shift)) = hit else {
            return;
        };
        nav.stack.push(Level {
            node: child,
            shift,
            scroll: [0.0, 0.0],
        });
        // 画像のままズームして、終わってから本物に切り替える。
        // ファイルは今の場面にそのまま描いてあるので切り替えない。
        maker.tree.nodes[child].is_dir
    } else if mouse.just_pressed(MouseButton::Right) || keys.just_pressed(KeyCode::Escape) {
        if nav.stack.len() < 2 {
            return;
        }
        nav.stack.pop();
        // 親の本物の場面を先に作り、今の場面を上に重ねてフェードアウトさせる。
        // 開いていたのがファイルなら、今の場面がもう親の場面なので何もしない。
        if maker.tree.nodes[top.node].is_dir {
            let (_, scale) = fit(nav.top().frame(&maker.layouts), &window);
            let (_, own_scale) = fit(top.frame(&maker.layouts), &window);
            let mut leaving = nav.stack.clone();
            leaving.push(top);
            maker.commands.entity(current.0).despawn();
            current.0 = maker.spawn(&nav.stack, scale);
            // 重なった場面は作った順に描かれるので、去る場面は親の後に作り直して上に重ねる。
            let old = maker.spawn(&leaving, own_scale);
            maker.commands.entity(old).insert((
                Fade {
                    started: Instant::now(),
                    fade_in: false,
                    replaces: None,
                },
                Transform::from_xyz(0.0, top.shift, 0.5),
            ));
        }
        false
    } else {
        return;
    };

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

/// ウィンドウの大きさが変わったら、フィーチャー中の枠に合わせ直す。
fn refit_on_resize(
    mut resized: MessageReader<WindowResized>,
    window: Single<&Window, With<PrimaryWindow>>,
    camera: Single<(&mut Transform, &mut Projection), With<MainCamera>>,
    layouts: Res<Layouts>,
    nav: Res<Nav>,
    animating: Option<Res<Animating>>,
) {
    if resized.read().last().is_none() || animating.is_some() {
        return;
    }
    let (mut transform, mut projection) = camera.into_inner();
    let (pos, scale) = fit(nav.top().frame(&layouts), &window);
    transform.translation = pos.extend(0.0);
    if let Projection::Orthographic(ortho) = projection.as_mut() {
        ortho.scale = scale;
    }
}

fn animate_camera(
    animating: Option<Res<Animating>>,
    camera: Single<(&mut Transform, &mut Projection), With<MainCamera>>,
    mut current: ResMut<CurrentScene>,
    mut maker: SceneMaker,
    nav: Res<Nav>,
) {
    let Some(anim) = animating else {
        return;
    };
    let t = (anim.started.elapsed().as_secs_f32() / anim.seconds).min(1.0);
    let e = anim.ease.apply(t);

    // 倍率は対数で補間すると、ズームの速さが一定に見える。
    let scale = (anim.from_scale.ln() + (anim.to_scale.ln() - anim.from_scale.ln()) * e).exp();
    let pos = zoom_position(&anim, scale, e);

    let (mut transform, mut projection) = camera.into_inner();
    transform.translation = pos.extend(0.0);
    if let Projection::Orthographic(ortho) = projection.as_mut() {
        ortho.scale = scale;
    }

    if t < 1.0 {
        return;
    }
    maker.commands.remove_resource::<Animating>();
    if anim.then_show {
        // 画像の上に本物の場面を重ねてフェードインし、終わったら古い場面を消す。
        let level = *nav.top();
        let old = current.0;
        current.0 = maker.spawn(&nav.stack, anim.to_scale);
        maker.commands.entity(current.0).insert((
            Fade {
                started: Instant::now(),
                fade_in: true,
                replaces: Some(old),
            },
            Transform::from_xyz(0.0, level.shift, 0.5),
        ));
    }
}

/// ズーム中のカメラの位置。
///
/// 始めと終わりの見え方は、ある1点（ズームの中心）を中心に拡大縮小した関係になっている。
/// その点を中心に倍率だけを変えれば、画面上のどの点も中心から（へ）まっすぐ動く。
/// 速さの緩急は倍率の補間（イージング）でつける。
fn zoom_position(anim: &Animating, scale: f32, e: f32) -> Vec2 {
    let ratio = anim.to_scale / anim.from_scale;
    // 倍率がほとんど変わらない（ただの移動）ときは、まっすぐ動かすだけ。
    if (ratio - 1.0).abs() < 1e-3 {
        return anim.from_pos.lerp(anim.to_pos, e);
    }
    let center = (anim.to_pos - anim.from_pos * ratio) / (1.0 - ratio);
    center + (anim.from_pos - center) * (scale / anim.from_scale)
}

/// 速さの緩急。
#[derive(Clone, Copy)]
pub(crate) enum Ease {
    /// ゆっくり始まり、ゆっくり止まる（ズーム）。
    InOut,
    /// すぐ動き出し、ゆっくり止まる（ばねで戻る）。
    Out,
}

impl Ease {
    fn apply(self, t: f32) -> f32 {
        match self {
            Ease::InOut => ease_in_out(t),
            Ease::Out => 1.0 - (1.0 - t).powi(3),
        }
    }
}

fn ease_in_out(t: f32) -> f32 {
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
    }
}

/// 場面の部品のうち、フェードで不透明度を変えるもの。
type ScenePart = (
    &'static BaseAlpha,
    Option<&'static mut Sprite>,
    Option<&'static mut TextColor>,
    Option<&'static MeshMaterial2d<ColorMaterial>>,
);

fn fade_scenes(
    mut commands: Commands,
    scenes: Query<(Entity, &Fade), With<SceneRoot>>,
    hierarchy: Query<&Children>,
    mut parts: Query<ScenePart>,
    mut materials: ResMut<Assets<ColorMaterial>>,
) {
    for (entity, fade) in &scenes {
        let k = (fade.started.elapsed().as_secs_f32() / FADE_SECONDS).min(1.0);
        let alpha = if fade.fade_in { k } else { 1.0 - k };
        for part in hierarchy.iter_descendants(entity) {
            let Ok((base, sprite, text, material)) = parts.get_mut(part) else {
                continue;
            };
            let a = base.0 * alpha;
            if let Some(mut sprite) = sprite {
                sprite.color.set_alpha(a);
            }
            if let Some(mut text) = text {
                text.0.set_alpha(a);
            }
            if let Some(mut material) = material.and_then(|m| materials.get_mut(&m.0)) {
                material.color.set_alpha(a);
            }
        }
        if k < 1.0 {
            continue;
        }
        if fade.fade_in {
            commands.entity(entity).remove::<Fade>();
            if let Some(old) = fade.replaces {
                commands.entity(old).despawn();
            }
        } else {
            commands.entity(entity).despawn();
        }
    }
}

/// ズームやフェードの最中は毎フレーム次の描画を頼む。垂直同期でディスプレイの FPS になる。
fn keep_redrawing(
    animating: Option<Res<Animating>>,
    fades: Query<(), With<Fade>>,
    gesture: Res<crate::gesture::Gesture>,
    mut redraw: MessageWriter<RequestRedraw>,
    mut busy: ResMut<Busy>,
) {
    // ジェスチャーの最中も、指を離したのを（入力が止まったことで）見分けるために回し続ける。
    busy.0 = animating.is_some() || !fades.is_empty() || gesture.active();
    if busy.0 {
        redraw.write(RequestRedraw);
    }
}

/// ウィンドウのタイトル。今いる場所に、あればデバッグ情報を添える。変わったときだけ書き換える。
fn update_title(
    breadcrumb: Res<Breadcrumb>,
    debug: Option<Res<TitleSuffix>>,
    mut window: Single<&mut Window, With<PrimaryWindow>>,
) {
    let title = match debug.as_deref() {
        Some(TitleSuffix(Some(suffix))) => format!("Lisa — {}  |  {suffix}", breadcrumb.0),
        _ => format!("Lisa — {}", breadcrumb.0),
    };
    if window.title != title {
        window.title = title;
    }
}
