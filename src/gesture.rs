//! トラックパッドの操作（2本指のスクロールとピンチ）。
//!
//! - 2本指で動かすと、カメラが指について動く。今のフォルダの枠からはみ出す方向には、
//!   はみ出すほど重くなり（ラバーバンド）、指を離すとばねのように戻る。
//! - リストの上で縦に動かすと、まずその列をスクロールする。端まで行ったら同じように重くなる。
//! - ピンチでカーソルの位置を中心に拡大縮小する。離したとき、子の枠が画面の大部分を占めて
//!   いればその子へ移り、今の枠が小さくなっていれば親へ戻る。どちらでもなければ元に戻る。
//! - マウスはホイールでポインタを中心に拡大縮小し、止まったらピンチと同じように行き先を決める。
//!   中ボタンで引っぱるとトラックパッドの2本指と同じように動く（リストの上なら縦はスクロール）。
//!
//! マウスとトラックパッドはスクロールの単位で見分ける（行単位がマウス、ピクセル単位が
//! トラックパッド。Magic Mouse はピクセル単位なのでトラックパッド扱いになる）。
//! Bevy にはピンチの始まりと終わりが来ないので、入力が少し途切れたら指を離したとみなす。

use std::time::{Duration, Instant};

use bevy::input::gestures::PinchGesture;
use bevy::input::mouse::{MouseScrollUnit, MouseWheel};
use bevy::input::touch::TouchPhase;
use bevy::math::Rect;
use bevy::prelude::*;
use bevy::window::PrimaryWindow;

use crate::layout::{DIRS, FILES, column_items};
use crate::menu::{BAR_PX, Popup};
use crate::settings::Settings;
use crate::view::{
    Animating, CurrentScene, Ease, Fade, Level, MainCamera, Nav, SceneMaker, ZOOM_SECONDS, fit,
    show_breadcrumb, ui_has_pointer,
};

/// 入力がこれだけ途切れたら、指を離したとみなす（トラックパッド）。
const RELEASE_AFTER: Duration = Duration::from_millis(100);
/// マウスのホイールがこれだけ止まったら、行き先を決める。
const WHEEL_RELEASE_AFTER: Duration = Duration::from_millis(150);
/// true にすると、トラックパッドの2本指の縦スクロールでも（マウスのホイールのように）拡大縮小する。
/// false なら、2本指は移動とリストのスクロール。
const TRACKPAD_SCROLL_ZOOMS: bool = false;
/// TRACKPAD_SCROLL_ZOOMS のとき、縦に 1px 動かしたぶんの拡大縮小の割合。
const TRACKPAD_ZOOM_PER_PX: f32 = 0.004;
/// マウスのホイール1段で拡大縮小する割合。
const WHEEL_ZOOM_STEP: f32 = 0.12;
/// ラバーバンドの硬さ。はみ出しは画面の大きさ × これ に近づいていく。
const RUBBER_LIMIT: f32 = 0.35;
/// Root でさらに縮小しようとしたときの重さ（対数の倍率に対する上限）。
const ROOT_ZOOM_OUT_LIMIT: f32 = 0.25;
/// 子の枠が画面の幅のこれ以上を占めて離したら、その子へ移る。
const SNAP_IN_COVERAGE: f32 = 0.55;
/// 今の枠が画面の幅のこれより小さくなって離したら、親へ戻る。
const SNAP_OUT_COVERAGE: f32 = 0.7;
/// 拡大できる上限（今の枠に合わせた倍率に対して）。
const MAX_ZOOM_IN: f32 = 40.0;
/// 縮小できる上限（今の枠に合わせた倍率に対して）。
const MAX_ZOOM_OUT: f32 = 4.0;
/// ばねで戻るのにかける時間（秒）。
const SPRING_SECONDS: f32 = 0.3;

#[derive(Resource, Default)]
pub struct Gesture {
    /// 指で動かしている最中なら、最後に入力が来た時刻。
    last_input: Option<Instant>,
    /// 2本指のスクロールで指を離した（このあとは慣性の入力が続くことがある）。
    lifted: bool,
    /// 制限をかける前のカメラの位置と倍率（指の動きそのまま）。
    raw_pos: Vec2,
    raw_scale: f32,
    /// リストのスクロールの、1行に満たない端数。
    pending_rows: f32,
    /// 最後の入力がマウスのホイールか。
    by_wheel: bool,
    /// 中ボタンで引っぱっているなら、前のフレームのカーソルの位置。
    middle: Option<Vec2>,
    /// 縮小して親が見えてきたときに、下に描いておく親の場面。
    parent_scene: Option<Entity>,
}

impl Gesture {
    pub fn active(&self) -> bool {
        self.last_input.is_some()
    }
}

#[allow(clippy::too_many_arguments)]
pub fn gesture(
    mut state: ResMut<Gesture>,
    mut wheel: MessageReader<MouseWheel>,
    mut pinch: MessageReader<PinchGesture>,
    mut nav: ResMut<Nav>,
    mut current: ResMut<CurrentScene>,
    mut maker: SceneMaker,
    window: Single<&Window, With<PrimaryWindow>>,
    camera: Single<(&Camera, &GlobalTransform, &mut Transform, &mut Projection), With<MainCamera>>,
    animating: Option<Res<Animating>>,
    fades: Query<(), With<Fade>>,
    popup: Res<Popup>,
    settings: Res<Settings>,
    buttons: Res<ButtonInput<MouseButton>>,
) {
    let wheel: Vec<MouseWheel> = wheel.read().cloned().collect();
    let pinch: f32 = pinch.read().map(|p| p.0).sum();
    // 切り替えの途中やメニューの操作中は、入力を捨てる。
    if animating.is_some() || !fades.is_empty() || (popup.open && !state.active()) {
        return;
    }
    let (camera, camera_global, mut transform, mut projection) = camera.into_inner();
    let Projection::Orthographic(ortho) = projection.as_mut() else {
        return;
    };
    let top = *nav.top();
    let (rest_pos, rest_scale) = fit(top.frame(&maker.layouts), &window);
    let cursor = window.cursor_position();
    let cursor_world = cursor.and_then(|p| camera.viewport_to_world_2d(camera_global, p).ok());
    let over_ui = ui_has_pointer(&window, &popup);

    let mut touched = false;
    let mut zoom = 0.0;
    let mut pan: Vec<Vec2> = Vec::new();
    for event in &wheel {
        if over_ui {
            continue;
        }
        match event.unit {
            // マウスのホイールは、ポインタを中心にした拡大縮小。
            MouseScrollUnit::Line => {
                zoom += event.y * WHEEL_ZOOM_STEP;
                state.by_wheel = true;
            }
            MouseScrollUnit::Pixel => {
                if event.phase == TouchPhase::Ended || event.phase == TouchPhase::Canceled {
                    state.lifted = true;
                    continue;
                }
                if event.phase == TouchPhase::Started {
                    state.lifted = false;
                }
                // 離したあとの慣性で、はみ出す方向へは動かさない（そのまま戻す）。
                if state.lifted && overshooting(&state, top, &maker, &window, rest_scale) {
                    continue;
                }
                state.by_wheel = false;
                if TRACKPAD_SCROLL_ZOOMS {
                    zoom += event.y * TRACKPAD_ZOOM_PER_PX;
                    pan.push(Vec2::new(event.x, 0.0));
                } else {
                    pan.push(Vec2::new(event.x, event.y));
                }
            }
        }
    }

    // 中ボタンで引っぱる。
    if buttons.just_pressed(MouseButton::Middle) && !over_ui {
        state.middle = cursor;
        state.by_wheel = false;
        begin(&mut state, transform.translation.truncate(), ortho.scale);
        touched = true;
    }
    if let (Some(last), Some(now)) = (state.middle, cursor) {
        if now != last {
            pan.push(now - last);
        }
        state.middle = Some(now);
    }
    let middle_released = state.middle.is_some() && !buttons.pressed(MouseButton::Middle);
    if middle_released {
        state.middle = None;
    }

    for delta in pan {
        begin(&mut state, transform.translation.truncate(), ortho.scale);
        touched = true;
        let displaced = (state.raw_pos - rest_pos).length() > rest_scale * 0.5
            || (state.raw_scale - rest_scale).abs() > rest_scale * 1e-3;
        // 縦の動きで、リストの上にいて、カメラが元の位置なら、まずリストを動かす。
        if !displaced && delta.y.abs() > delta.x.abs() && maker.tree.nodes[top.node].is_dir {
            state.pending_rows -= delta.y / settings.row_height_px;
            let rows = state.pending_rows.trunc();
            if rows != 0.0 {
                state.pending_rows -= rows;
                let left = scroll_list(
                    &mut nav,
                    &mut current,
                    &mut maker,
                    &window,
                    cursor_world,
                    rows,
                );
                // 端まで行って動かせなかった分は、カメラを引っぱる（重くなって戻る）。
                state.raw_pos.y -= left * settings.row_height_px * ortho.scale;
            }
            continue;
        }
        // 指についてカメラを動かす（中身が指と同じ向きに動く）。
        state.raw_pos += Vec2::new(-delta.x, delta.y) * ortho.scale;
    }

    // ピンチ（指を広げると正）とホイール（上へ回すと正）は、どちらも正で拡大。
    // ピンチしながら指を動かすと、移動（上）と拡大縮小（下）が同じフレームで両方かかる。
    let pinch = pinch + zoom;
    if pinch != 0.0 && !over_ui {
        begin(&mut state, transform.translation.truncate(), ortho.scale);
        touched = true;
        // カーソルの下の点が動かないように拡大縮小する。
        let factor = (-pinch).exp();
        let new_scale =
            (state.raw_scale * factor).clamp(rest_scale / MAX_ZOOM_IN, rest_scale * MAX_ZOOM_OUT);
        let anchor = cursor_world.unwrap_or(state.raw_pos);
        state.raw_pos = anchor + (state.raw_pos - anchor) * (new_scale / state.raw_scale);
        state.raw_scale = new_scale;
    }

    let Some(last_input) = state.last_input else {
        return;
    };
    if touched {
        state.last_input = Some(Instant::now());
    }

    // 制限をかけた見え方にする。
    let at_root = nav.stack.len() < 2;
    let (pos, scale) = constrain(&state, top, &maker, &window, rest_scale, at_root);
    transform.translation = pos.extend(0.0);
    ortho.scale = scale;

    // 縮小して親が見えてきたら、親の場面を下に描いておく。
    if !at_root
        && scale > rest_scale * 1.01
        && state.parent_scene.is_none()
        && maker.tree.nodes[top.node].is_dir
    {
        let parent_path = &nav.stack[..nav.stack.len() - 1];
        let (_, parent_scale) = fit(
            parent_path[parent_path.len() - 1].frame(&maker.layouts),
            &window,
        );
        state.parent_scene = Some(maker.spawn(parent_path, parent_scale));
        maker
            .commands
            .entity(current.0)
            .insert(Transform::from_xyz(0.0, top.shift, 0.5));
    }

    // 指を離したら、子へ移るか、親へ戻るか、元に戻る。
    let wait = if state.by_wheel {
        WHEEL_RELEASE_AFTER
    } else {
        RELEASE_AFTER
    };
    let released = if state.middle.is_some() {
        false
    } else {
        middle_released || (!touched && last_input.elapsed() >= wait)
    };
    let bounced = state.lifted && overshooting(&state, top, &maker, &window, rest_scale);
    if released || (bounced && !touched) {
        settle(
            &mut state,
            &mut nav,
            &mut current,
            &mut maker,
            &window,
            camera,
            camera_global,
            pos,
            scale,
            rest_pos,
            rest_scale,
        );
    }
}

/// ジェスチャーを始める。
fn begin(state: &mut Gesture, pos: Vec2, scale: f32) {
    if state.last_input.is_none() {
        state.last_input = Some(Instant::now());
        state.raw_pos = pos;
        state.raw_scale = scale;
        state.pending_rows = 0.0;
    }
}

/// 今の枠に対して、カメラが動いてよい範囲（メニューバーの下の領域の中心の範囲）。
fn bounds(frame: Rect, window: &Window, scale: f32) -> Rect {
    let half = Vec2::new(window.width(), window.height() - BAR_PX) / 2.0 * scale;
    let min = frame.min + half;
    let max = frame.max - half;
    // 枠より画面のほうが広ければ、真ん中に固定する。
    let center = frame.center();
    Rect {
        min: Vec2::new(
            if min.x <= max.x { min.x } else { center.x },
            if min.y <= max.y { min.y } else { center.y },
        ),
        max: Vec2::new(
            if min.x <= max.x { max.x } else { center.x },
            if min.y <= max.y { max.y } else { center.y },
        ),
    }
}

/// はみ出し `d` を、重さをかけた量にする。`limit` に近づいていき、超えない。
fn rubber(d: f32, limit: f32) -> f32 {
    if d == 0.0 || limit <= 0.0 {
        return 0.0;
    }
    d.signum() * limit * (1.0 - 1.0 / (d.abs() / limit + 1.0))
}

/// 指の動きそのままの位置と倍率に、はみ出しの重さをかける。
fn constrain(
    state: &Gesture,
    top: Level,
    maker: &SceneMaker,
    window: &Window,
    rest_scale: f32,
    at_root: bool,
) -> (Vec2, f32) {
    // Root でさらに縮小しようとすると重くなる。
    let mut scale = state.raw_scale;
    if at_root && scale > rest_scale {
        let over = (scale / rest_scale).ln();
        scale = rest_scale * rubber(over, ROOT_ZOOM_OUT_LIMIT).exp();
    }
    // カメラの中心（バーの下の領域の中心）を、動いてよい範囲に重さつきで収める。
    let offset = Vec2::new(0.0, BAR_PX / 2.0 * scale);
    let view_center = state.raw_pos - offset;
    let area = bounds(top.frame(&maker.layouts), window, scale);
    let size = Vec2::new(window.width(), window.height() - BAR_PX) * scale;
    let clamped = view_center.clamp(area.min, area.max);
    let over = view_center - clamped;
    let shown = clamped
        + Vec2::new(
            rubber(over.x, size.x * RUBBER_LIMIT),
            rubber(over.y, size.y * RUBBER_LIMIT),
        );
    (shown + offset, scale)
}

/// 動いてよい範囲からはみ出しているか。
fn overshooting(
    state: &Gesture,
    top: Level,
    maker: &SceneMaker,
    window: &Window,
    rest_scale: f32,
) -> bool {
    if state.last_input.is_none() {
        return false;
    }
    let offset = Vec2::new(0.0, BAR_PX / 2.0 * state.raw_scale);
    let view_center = state.raw_pos - offset;
    let area = bounds(top.frame(&maker.layouts), window, state.raw_scale);
    let over = view_center - view_center.clamp(area.min, area.max);
    over.length() > rest_scale
}

/// 指を離したあと、行き先を決めてそこへ動かす。
#[allow(clippy::too_many_arguments)]
fn settle(
    state: &mut Gesture,
    nav: &mut Nav,
    current: &mut CurrentScene,
    maker: &mut SceneMaker,
    window: &Window,
    camera: &Camera,
    camera_global: &GlobalTransform,
    pos: Vec2,
    scale: f32,
    rest_pos: Vec2,
    rest_scale: f32,
) {
    let top = *nav.top();
    let parent_scene = state.parent_scene.take();
    *state = Gesture::default();

    // 拡大して、カーソル（無ければ画面の中心）の下の子が画面の大部分を占めていれば、その子へ。
    let view_width = window.width() * scale;
    let point = window
        .cursor_position()
        .and_then(|p| camera.viewport_to_world_2d(camera_global, p).ok())
        .unwrap_or(pos - Vec2::new(0.0, BAR_PX / 2.0 * scale));
    let child = (0..2).find_map(|column| {
        let shift = top.column_shift(&maker.layouts, column);
        top.visible(&maker.tree, &maker.layouts, column)
            .into_iter()
            .find(|c| {
                maker.layouts.0[*c]
                    .row
                    .contains(point - Vec2::new(0.0, shift))
            })
            .map(|c| (c, shift))
    });
    if scale < rest_scale
        && let Some((child, shift)) = child
        && maker.layouts.0[child].frame.width() / view_width >= SNAP_IN_COVERAGE
    {
        if let Some(parent) = parent_scene {
            maker.commands.entity(parent).despawn();
        }
        nav.stack.push(Level {
            node: child,
            shift,
            scroll: [0, 0],
        });
        let then_show = maker.tree.nodes[child].is_dir;
        go(
            maker,
            window,
            nav,
            pos,
            scale,
            then_show,
            ZOOM_SECONDS,
            Ease::Out,
        );
        return;
    }

    // 縮小して、今の枠が画面のわずかしか占めていなければ、親へ。
    let coverage = top.frame(&maker.layouts).width() / view_width;
    if nav.stack.len() >= 2 && coverage < SNAP_OUT_COVERAGE {
        nav.stack.pop();
        if let Some(parent) = parent_scene {
            // 親の場面はもう下に描いてある。今の場面をフェードアウトさせる。
            let old = current.0;
            current.0 = parent;
            maker.commands.entity(old).insert(Fade {
                started: Instant::now(),
                fade_in: false,
                replaces: None,
            });
        }
        go(
            maker,
            window,
            nav,
            pos,
            scale,
            false,
            ZOOM_SECONDS,
            Ease::Out,
        );
        return;
    }

    // どちらでもなければ、ばねで元に戻る。
    if let Some(parent) = parent_scene {
        maker.commands.entity(parent).despawn();
    }
    maker.commands.insert_resource(Animating {
        from_pos: pos,
        from_scale: scale,
        to_pos: rest_pos,
        to_scale: rest_scale,
        started: Instant::now(),
        then_show: false,
        seconds: SPRING_SECONDS,
        ease: Ease::Out,
    });
}

/// 今の位置（Nav の最後）へ動かす。
#[allow(clippy::too_many_arguments)]
fn go(
    maker: &mut SceneMaker,
    window: &Window,
    nav: &Nav,
    from_pos: Vec2,
    from_scale: f32,
    then_show: bool,
    seconds: f32,
    ease: Ease,
) {
    let next = *nav.top();
    show_breadcrumb(&mut maker.commands, &maker.tree, next.node);
    let (to_pos, to_scale) = fit(next.frame(&maker.layouts), window);
    maker.commands.insert_resource(Animating {
        from_pos,
        from_scale,
        to_pos,
        to_scale,
        started: Instant::now(),
        then_show,
        seconds,
        ease,
    });
}

/// カーソルのある列を `rows` 行スクロールする。端で動かせなかった行数を返す。
fn scroll_list(
    nav: &mut Nav,
    current: &mut CurrentScene,
    maker: &mut SceneMaker,
    window: &Window,
    cursor_world: Option<Vec2>,
    rows: f32,
) -> f32 {
    let top = *nav.top();
    if !maker.tree.nodes[top.node].is_dir {
        return rows;
    }
    let Some(world) = cursor_world else {
        return rows;
    };
    let list = maker.layouts.0[top.node].list;
    let middle = (list.columns[0].max.x + list.columns[1].min.x) / 2.0;
    let column = if world.x < middle { FILES } else { DIRS };
    let count = column_items(&maker.tree, top.node, column).len();
    let max = count.saturating_sub(list.fit) as f32;
    let before = top.scroll[column] as f32;
    let after = (before + rows).clamp(0.0, max);
    if after == before {
        return rows;
    }
    let level = nav.stack.last_mut().expect("Root は常にある");
    level.scroll[column] = after as usize;
    // 見えている行だけを作り直す。
    let (_, scale) = fit(level.frame(&maker.layouts), window);
    maker.commands.entity(current.0).despawn();
    current.0 = maker.spawn(&nav.stack, scale);
    rows - (after - before)
}
