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
use crate::layout::{Layouts, layout};
use crate::scene::{self, BACKGROUND, LabelFont, SceneRoot, SceneSource};
use crate::shapes::BaseAlpha;
use crate::tree::{FileTree, NodeId};

/// ワールド座標での Root の高さ。幅はウィンドウの縦横比に合わせる。
const ROOT_HEIGHT: f32 = 1000.0;
/// ズームにかける時間（秒）。
const ZOOM_SECONDS: f32 = 0.4;
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
        .insert_resource(Focus(FileTree::ROOT))
        .insert_resource(Bake::Waiting)
        .add_systems(Startup, setup)
        .add_systems(
            Update,
            (
                cache::bake,
                show_baked,
                navigate,
                refit_on_resize,
                animate_camera,
                fade_scenes,
                keep_redrawing,
            )
                .chain(),
        );
    }
}

/// 今フィーチャーしている（枠が画面いっぱいに映っている）フォルダかファイル。
#[derive(Resource)]
struct Focus(NodeId);

/// 画面を映すカメラ（画像を焼くカメラと区別する）。
#[derive(Component)]
struct MainCamera;

/// 今画面に本物として描いている場面。
#[derive(Resource)]
struct CurrentScene(Entity);

/// カメラのズーム中だけ存在する。これがあるあいだはディスプレイの FPS で描画する。
#[derive(Resource)]
struct Animating {
    from_pos: Vec2,
    from_scale: f32,
    to_pos: Vec2,
    to_scale: f32,
    /// 待機中は Bevy の Time の進み方が不規則なので、実時間で測る。
    started: Instant,
    /// ズームが終わったら本物の場面に切り替えるフォルダ。
    then_show: Option<NodeId>,
}

/// 場面のフェード。終わったら、フェードアウトなら場面を消し、フェードインなら `replaces` を消す。
#[derive(Component)]
struct Fade {
    started: Instant,
    fade_in: bool,
    replaces: Option<Entity>,
}

/// 場面を作るのに要るもの。
#[derive(SystemParam)]
struct SceneMaker<'w, 's> {
    commands: Commands<'w, 's>,
    meshes: ResMut<'w, Assets<Mesh>>,
    materials: ResMut<'w, Assets<ColorMaterial>>,
    tree: Res<'w, FileTree>,
    layouts: Res<'w, Layouts>,
    images: Res<'w, FolderImages>,
    font: Res<'w, LabelFont>,
}

impl SceneMaker<'_, '_> {
    /// 画面に出す本物の場面を作る。`scale` はその場面を画面いっぱいに見るときの倍率。
    fn spawn(&mut self, folder: NodeId, scale: f32) -> Entity {
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
            folder,
            RenderLayers::layer(0),
            scale,
        )
    }
}

fn setup(
    mut commands: Commands,
    tree: Res<FileTree>,
    assets: Res<AssetServer>,
    mut images: ResMut<Assets<Image>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<ColorMaterial>>,
    window: Single<&Window, With<PrimaryWindow>>,
) {
    let font = assets.load("fonts/NotoSansJP-Light.otf");
    let aspect = window.width() / window.height();
    let root_rect =
        Rect::from_center_size(Vec2::ZERO, Vec2::new(ROOT_HEIGHT * aspect, ROOT_HEIGHT));
    let layouts = layout(&tree, root_rect, aspect);
    let folder_images = cache::create_images(&tree, aspect, &mut images);

    let (pos, scale) = fit(root_rect, &window);
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
    };
    let root_scene = scene::spawn(
        &mut commands,
        &mut meshes,
        &mut materials,
        &source,
        FileTree::ROOT,
        RenderLayers::layer(0),
        scale,
    );

    commands.insert_resource(CurrentScene(root_scene));
    commands.insert_resource(Layouts(layouts));
    commands.insert_resource(folder_images);
    commands.insert_resource(LabelFont(font));
}

/// `rect` が画面いっぱいに収まるカメラの位置と倍率。
fn fit(rect: Rect, window: &Window) -> (Vec2, f32) {
    let scale = (rect.width() / window.width()).max(rect.height() / window.height());
    (rect.center(), scale)
}

/// 画像が焼き上がったら、今の場面を作り直して画像を使う。
fn show_baked(
    finished: Option<Res<BakeFinished>>,
    mut current: ResMut<CurrentScene>,
    mut maker: SceneMaker,
    focus: Res<Focus>,
    window: Single<&Window, With<PrimaryWindow>>,
) {
    if finished.is_none() {
        return;
    }
    maker.commands.remove_resource::<BakeFinished>();
    maker.commands.entity(current.0).despawn();
    let (_, scale) = fit(maker.layouts.0[focus.0].frame, &window);
    current.0 = maker.spawn(focus.0, scale);
}

/// 左クリックしたフォルダかファイルへズームイン、右クリックか Esc で親へズームアウト。
#[allow(clippy::too_many_arguments)]
fn navigate(
    mouse: Res<ButtonInput<MouseButton>>,
    keys: Res<ButtonInput<KeyCode>>,
    window: Single<&mut Window, With<PrimaryWindow>>,
    camera: Single<(&Camera, &GlobalTransform, &Transform, &Projection), With<MainCamera>>,
    mut focus: ResMut<Focus>,
    mut current: ResMut<CurrentScene>,
    mut maker: SceneMaker,
    animating: Option<Res<Animating>>,
    fades: Query<(), With<Fade>>,
) {
    // 切り替えの途中は受け付けない。
    if animating.is_some() || !fades.is_empty() {
        return;
    }
    let mut window = window.into_inner();
    let (camera, camera_global, camera_transform, projection) = camera.into_inner();
    let Projection::Orthographic(ortho) = projection else {
        return;
    };

    let zoom_in = mouse.just_pressed(MouseButton::Left);
    let next = if zoom_in {
        // 今の場面の直下で、クリックした行（名前か枠）。
        window
            .cursor_position()
            .and_then(|p| camera.viewport_to_world_2d(camera_global, p).ok())
            .and_then(|world| {
                maker.tree.nodes[focus.0]
                    .children
                    .iter()
                    .copied()
                    .find(|c| maker.layouts.0[*c].row.contains(world))
            })
    } else if mouse.just_pressed(MouseButton::Right) || keys.just_pressed(KeyCode::Escape) {
        maker.tree.nodes[focus.0].parent
    } else {
        None
    };
    let Some(next) = next else {
        return;
    };
    window.title = format!("Lisa — {}", maker.tree.path(next));
    let (to_pos, to_scale) = fit(maker.layouts.0[next].frame, &window);
    let then_show = if zoom_in {
        // 画像のままズームして、終わってから本物に切り替える。
        // ファイルは今の場面にそのまま描いてあるので切り替えない。
        maker.tree.nodes[next].is_dir.then_some(next)
    } else {
        // 親の本物の場面を先に作り、今の場面を上に重ねてフェードアウトさせる。
        // 開いていたのがファイルなら、今の場面がもう親の場面なので何もしない。
        if maker.tree.nodes[focus.0].is_dir {
            let old = current.0;
            current.0 = maker.spawn(next, to_scale);
            maker.commands.entity(old).insert((
                Fade {
                    started: Instant::now(),
                    fade_in: false,
                    replaces: None,
                },
                Transform::from_xyz(0.0, 0.0, 0.5),
            ));
        }
        None
    };

    focus.0 = next;
    maker.commands.insert_resource(Animating {
        from_pos: camera_transform.translation.truncate(),
        from_scale: ortho.scale,
        to_pos,
        to_scale,
        started: Instant::now(),
        then_show,
    });
}

/// ウィンドウの大きさが変わったら、フィーチャー中の枠に合わせ直す。
fn refit_on_resize(
    mut resized: MessageReader<WindowResized>,
    window: Single<&Window, With<PrimaryWindow>>,
    camera: Single<(&mut Transform, &mut Projection), With<MainCamera>>,
    layouts: Res<Layouts>,
    focus: Res<Focus>,
    animating: Option<Res<Animating>>,
) {
    if resized.read().last().is_none() || animating.is_some() {
        return;
    }
    let (mut transform, mut projection) = camera.into_inner();
    let (pos, scale) = fit(layouts.0[focus.0].frame, &window);
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
) {
    let Some(anim) = animating else {
        return;
    };
    let t = (anim.started.elapsed().as_secs_f32() / ZOOM_SECONDS).min(1.0);
    let e = ease_in_out(t);

    // 倍率は対数で補間すると、ズームの速さが一定に見える。
    let scale = (anim.from_scale.ln() + (anim.to_scale.ln() - anim.from_scale.ln()) * e).exp();
    let pos = anim.from_pos.lerp(anim.to_pos, e);

    let (mut transform, mut projection) = camera.into_inner();
    transform.translation = pos.extend(0.0);
    if let Projection::Orthographic(ortho) = projection.as_mut() {
        ortho.scale = scale;
    }

    if t < 1.0 {
        return;
    }
    maker.commands.remove_resource::<Animating>();
    if let Some(folder) = anim.then_show {
        // 画像の上に本物の場面を重ねてフェードインし、終わったら古い場面を消す。
        let old = current.0;
        current.0 = maker.spawn(folder, anim.to_scale);
        maker.commands.entity(current.0).insert((
            Fade {
                started: Instant::now(),
                fade_in: true,
                replaces: Some(old),
            },
            Transform::from_xyz(0.0, 0.0, 0.5),
        ));
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
    scenes: Query<(Entity, &Fade, &Children), With<SceneRoot>>,
    mut parts: Query<ScenePart>,
    mut materials: ResMut<Assets<ColorMaterial>>,
) {
    for (entity, fade, children) in &scenes {
        let k = (fade.started.elapsed().as_secs_f32() / FADE_SECONDS).min(1.0);
        let alpha = if fade.fade_in { k } else { 1.0 - k };
        for child in children {
            let Ok((base, sprite, text, material)) = parts.get_mut(*child) else {
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
    mut redraw: MessageWriter<RequestRedraw>,
) {
    if animating.is_some() || !fades.is_empty() {
        redraw.write(RequestRedraw);
    }
}
