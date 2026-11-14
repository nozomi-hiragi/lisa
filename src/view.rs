//! 木の描画、カメラのズーム、描画頻度の切り替え。

use std::time::{Duration, Instant};

use bevy::math::Rect;
use bevy::prelude::*;
use bevy::sprite::Anchor;
use bevy::window::{PrimaryWindow, RequestRedraw, WindowResized};
use bevy::winit::{UpdateMode, WinitSettings};

use crate::layout::{NodeLayout, folder_body, folder_tab, layout};
use crate::preview::{self, PreviewKind};
use crate::shapes::{LodShape, Painter};
use crate::tree::{FileTree, NodeId, format_size};

/// ワールド座標での Root の高さ。幅はウィンドウの縦横比に合わせる。
const ROOT_HEIGHT: f32 = 1000.0;
/// ズームにかける時間（秒）。
const ZOOM_SECONDS: f32 = 0.4;
/// 画面上でこれより小さい図形は描かない（論理ピクセル）。
const MIN_SHAPE_PX: f32 = 2.0;
/// 画面上でこれより小さい文字は描かない（論理ピクセル）。
const MIN_LABEL_PX: f32 = 8.0;
/// 文字の最大の大きさ（論理ピクセル）。
const MAX_LABEL_PX: f32 = 36.0;

const BACKGROUND: Color = Color::srgb(0.08, 0.08, 0.1);

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
        .add_systems(Startup, setup)
        .add_systems(
            Update,
            (
                navigate,
                refit_on_resize,
                animate_camera,
                keep_redrawing,
                update_lod,
            )
                .chain(),
        );
    }
}

/// 今フィーチャーしている（枠が画面いっぱいに映っている）フォルダかファイル。
#[derive(Resource)]
struct Focus(NodeId);

/// ノードの配置。
#[derive(Resource)]
struct Layouts(Vec<NodeLayout>);

/// カメラのズーム中だけ存在する。これがあるあいだはディスプレイの FPS で描画する。
#[derive(Resource)]
struct Animating {
    from_pos: Vec2,
    from_scale: f32,
    to_pos: Vec2,
    to_scale: f32,
    /// 待機中は Bevy の Time の進み方が不規則なので、実時間で測る。
    started: Instant,
}

/// 文字を作り直す必要がある（ズームが止まったときなど）。
#[derive(Resource)]
struct LabelsDirty;

#[derive(Component)]
struct NodeLabel {
    /// ワールド座標での文字の高さ。
    world_height: f32,
    /// 文字を置ける幅（ワールド座標）。
    world_width: f32,
    /// 文字の大きさを 1 としたときの文字列の幅（見積もり）。
    em_width: f32,
}

fn setup(
    mut commands: Commands,
    tree: Res<FileTree>,
    assets: Res<AssetServer>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<ColorMaterial>>,
    window: Single<&Window, With<PrimaryWindow>>,
) {
    let font = assets.load("fonts/NotoSansJP-Light.otf");
    let aspect = window.width() / window.height();
    let root_rect =
        Rect::from_center_size(Vec2::ZERO, Vec2::new(ROOT_HEIGHT * aspect, ROOT_HEIGHT));
    let layouts = layout(&tree, root_rect, aspect);

    let (pos, scale) = fit(root_rect, &window);
    commands.spawn((
        Camera2d,
        Projection::Orthographic(OrthographicProjection {
            scale,
            ..OrthographicProjection::default_2d()
        }),
        Transform::from_translation(pos.extend(0.0)),
    ));

    for (id, node) in tree.nodes.iter().enumerate().skip(1) {
        let l = &layouts[id];
        let z = node.depth as f32;
        let hue = device_hue(node.device);

        let mut painter = Painter {
            commands: &mut commands,
            meshes: &mut meshes,
            materials: &mut materials,
        };

        // 名前の右の枠。フォルダは薄いフォルダの形（中身はこの上に描かれる）、
        // ファイルは紙の形にプレビュー。
        let mut extension_area = None;
        if node.is_dir {
            let fill = Color::hsla(hue, 0.35, 0.55, 0.10);
            painter.rect(folder_tab(l.frame), fill, z);
            painter.rect(folder_body(l.frame), fill, z);
        } else {
            extension_area = preview::draw(
                &mut painter,
                l.frame,
                &PreviewKind::from_name(&node.name),
                z,
            );
        }

        // 種類のわからないファイルは、プレビューに拡張子を大きく書く。
        if let Some(area) = extension_area {
            let ext = preview::extension(&node.name).to_uppercase();
            let ext = if ext.is_empty() {
                "FILE".to_string()
            } else {
                ext
            };
            commands.spawn((
                NodeLabel {
                    world_height: area.height() * 0.4,
                    world_width: area.width(),
                    em_width: em_width(&ext),
                },
                Text2d::new(ext),
                TextLayout::no_wrap(),
                TextFont::from_font_size(16.0).with_font(font.clone()),
                TextColor(Color::srgba(0.85, 0.87, 0.95, 0.5)),
                Anchor::CENTER,
                Transform::from_translation(area.center().extend(z + 0.2)),
                Visibility::Hidden,
            ));
        }

        // 名前。ファイルは大きさも添える。
        let text = if node.is_dir {
            node.name.clone()
        } else {
            format!("{}  {}", node.name, format_size(node.size))
        };
        let left = l.label.min.x + l.label.height() * 0.1;
        commands.spawn((
            NodeLabel {
                world_height: l.label.height() * 0.35,
                world_width: l.label.max.x - left,
                em_width: em_width(&text),
            },
            Text2d::new(text),
            TextLayout::no_wrap(),
            TextFont::from_font_size(16.0).with_font(font.clone()),
            TextColor(Color::srgba(1.0, 1.0, 1.0, 0.9)),
            Anchor::CENTER_LEFT,
            Transform::from_translation(Vec2::new(left, l.label.center().y).extend(z + 0.2)),
            Visibility::Hidden,
        ));
    }

    commands.insert_resource(Layouts(layouts));
    commands.insert_resource(LabelsDirty);
}

/// 端末ごとの色相。
fn device_hue(device: usize) -> f32 {
    [210.0, 140.0, 30.0, 290.0][device % 4]
}

/// 文字列の幅の見積もり。日本語はほぼ正方形、英数字はその半分強。
fn em_width(text: &str) -> f32 {
    text.chars()
        .map(|c| if c.is_ascii() { 0.6 } else { 1.0 })
        .sum()
}

/// `rect` が画面いっぱいに収まるカメラの位置と倍率。
fn fit(rect: Rect, window: &Window) -> (Vec2, f32) {
    let scale = (rect.width() / window.width()).max(rect.height() / window.height()) * 1.02;
    (rect.center(), scale)
}

/// フィーチャー中のノードの枠。
fn focus_rect(layouts: &Layouts, focus: NodeId) -> Rect {
    layouts.0[focus].frame
}

/// 左クリックしたフォルダかファイルへズームイン、右クリックか Esc で親へズームアウト。
#[allow(clippy::too_many_arguments)]
fn navigate(
    mut commands: Commands,
    mouse: Res<ButtonInput<MouseButton>>,
    keys: Res<ButtonInput<KeyCode>>,
    window: Single<&mut Window, With<PrimaryWindow>>,
    camera: Single<(&Camera, &GlobalTransform, &Transform, &Projection)>,
    tree: Res<FileTree>,
    layouts: Res<Layouts>,
    mut focus: ResMut<Focus>,
) {
    let mut window = window.into_inner();
    let (camera, camera_global, camera_transform, projection) = camera.into_inner();

    let next = if mouse.just_pressed(MouseButton::Left) {
        // フィーチャー中のフォルダの直下で、クリックした行（名前か枠）。
        window
            .cursor_position()
            .and_then(|p| camera.viewport_to_world_2d(camera_global, p).ok())
            .and_then(|world| {
                tree.nodes[focus.0]
                    .children
                    .iter()
                    .copied()
                    .find(|c| layouts.0[*c].row.contains(world))
            })
    } else if mouse.just_pressed(MouseButton::Right) || keys.just_pressed(KeyCode::Escape) {
        tree.nodes[focus.0].parent
    } else {
        None
    };
    let Some(next) = next else {
        return;
    };

    focus.0 = next;
    window.title = format!("Lisa — {}", tree.path(next));

    let Projection::Orthographic(ortho) = projection else {
        return;
    };
    let (to_pos, to_scale) = fit(focus_rect(&layouts, next), &window);
    commands.insert_resource(Animating {
        from_pos: camera_transform.translation.truncate(),
        from_scale: ortho.scale,
        to_pos,
        to_scale,
        started: Instant::now(),
    });
}

/// ウィンドウの大きさが変わったら、フィーチャー中の枠に合わせ直す。
fn refit_on_resize(
    mut commands: Commands,
    mut resized: MessageReader<WindowResized>,
    window: Single<&Window, With<PrimaryWindow>>,
    camera: Single<(&mut Transform, &mut Projection), With<Camera2d>>,
    layouts: Res<Layouts>,
    focus: Res<Focus>,
    animating: Option<Res<Animating>>,
) {
    if resized.read().last().is_none() || animating.is_some() {
        return;
    }
    let (mut transform, mut projection) = camera.into_inner();
    let (pos, scale) = fit(focus_rect(&layouts, focus.0), &window);
    transform.translation = pos.extend(0.0);
    if let Projection::Orthographic(ortho) = projection.as_mut() {
        ortho.scale = scale;
    }
    commands.insert_resource(LabelsDirty);
}

fn animate_camera(
    mut commands: Commands,
    animating: Option<Res<Animating>>,
    camera: Single<(&mut Transform, &mut Projection), With<Camera2d>>,
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

    if t >= 1.0 {
        commands.remove_resource::<Animating>();
        commands.insert_resource(LabelsDirty);
    }
}

fn ease_in_out(t: f32) -> f32 {
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
    }
}

/// アニメーション中は毎フレーム次の描画を頼む。垂直同期でディスプレイの FPS になる。
fn keep_redrawing(animating: Option<Res<Animating>>, mut redraw: MessageWriter<RequestRedraw>) {
    if animating.is_some() {
        redraw.write(RequestRedraw);
    }
}

/// 小さすぎる図形を隠し、文字の大きさを画面に合わせる。
fn update_lod(
    mut commands: Commands,
    projection: Single<&Projection, (With<Camera2d>, Changed<Projection>)>,
    labels_dirty: Option<Res<LabelsDirty>>,
    mut shapes: Query<(&LodShape, &mut Visibility), Without<NodeLabel>>,
    mut labels: Query<(&NodeLabel, &mut TextFont, &mut Transform, &mut Visibility)>,
) {
    let Projection::Orthographic(ortho) = *projection else {
        return;
    };
    let scale = ortho.scale;

    // 図形は毎フレーム（ズーム中も）判定する。軽い処理なので問題ない。
    for (shape, mut visibility) in &mut shapes {
        visibility.set_if_neq(if shape.min_size / scale >= MIN_SHAPE_PX {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        });
    }

    // 文字は大きさを変えるたびにフォントアトラスが作られるので、止まったときだけ更新する。
    if labels_dirty.is_none() {
        return;
    }
    commands.remove_resource::<LabelsDirty>();
    for (label, mut font, mut transform, mut visibility) in &mut labels {
        // 行の高さと欄の幅の両方に収まる大きさにする。
        let fit_width = label.world_width / scale * 0.95 / label.em_width;
        let px = (label.world_height / scale)
            .min(fit_width)
            .min(MAX_LABEL_PX)
            .floor();
        let fits = px >= MIN_LABEL_PX;
        visibility.set_if_neq(if fits {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        });
        if fits {
            font.font_size = FontSize::Px(px);
            // ワールドでは常に `scale` 倍して、画面上で `px` の大きさに見せる。
            transform.scale = Vec3::splat(scale);
        }
    }
}
