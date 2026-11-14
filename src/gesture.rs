//! トラックパッドの操作（2本指のスクロールとピンチ）。
//!
//! - 2本指で動かすと、カメラが指について動く。今のフォルダの枠からはみ出す方向には、
//!   はみ出すほど重くなり（ラバーバンド）、指を離すとばねのように戻る。
//! - リストの上で縦に動かすと、まずその列をスクロールする。端まで行ったら同じように重くなる。
//! - フォルダを開いているとき、上下には同じ階層の隣のフォルダがある。そちらへは軽い重さで動き、
//!   隣のフォルダが画面の一定の割合まで見えて離すと（または勢いよく動かすと）、そのフォルダへ移る。
//! - ピンチでカーソルの位置を中心に拡大縮小する。離したとき、子の枠が画面の一定の割合を占めて
//!   いればその子へ移り、今の枠が小さくなっていれば親へ戻る（割合と、勢いで移る感度は設定で変えられる）。
//!   どちらでもなければ元に戻る。
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
use crate::scene::Rows;
use crate::settings::Settings;
use crate::tree::NodeId;
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
/// 離す直前のこの時間の拡大縮小の速さを見る。
const FLICK_WINDOW: Duration = Duration::from_millis(80);
/// 拡大できる上限（今の枠に合わせた倍率に対して）。
const MAX_ZOOM_IN: f32 = 40.0;
/// 縮小できる上限（今の枠に合わせた倍率に対して）。
const MAX_ZOOM_OUT: f32 = 4.0;
/// 隣のフォルダの方向へ動かすときの軽い重さ（指の動きに対して、これだけ動く）。
const SIBLING_FOLLOW: f32 = 0.85;
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
    /// 入力があったときの倍率の記録（時刻、倍率の対数）。離す直前の速さを求める。
    history: Vec<(Instant, f32)>,
    /// 入力があったときの縦の位置の記録（時刻、カメラの y）。離す直前の速さを求める。
    pan_history: Vec<(Instant, f32)>,
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
        // 縦の動きで、カーソルの下の列がその向きにまだスクロールでき、カメラが元の位置なら、
        // まずリストを動かす。スクロールできなければ、カメラを動かす（端の重さや隣のフォルダへ）。
        if !displaced
            && delta.y.abs() > delta.x.abs()
            && list_can_scroll(&nav, &maker, cursor_world, -delta.y)
        {
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
        let now = Instant::now();
        state.last_input = Some(now);
        let sample = (now, state.raw_scale.ln());
        state.history.push(sample);
        state.history.retain(|(t, _)| now - *t <= FLICK_WINDOW * 2);
        let sample = (now, state.raw_pos.y);
        state.pan_history.push(sample);
        state
            .pan_history
            .retain(|(t, _)| now - *t <= FLICK_WINDOW * 2);
    }

    // 制限をかけた見え方にする。
    let at_root = nav.stack.len() < 2;
    let siblings = Siblings::of(&nav, &maker);
    let (pos, scale) = constrain(
        &state,
        top,
        &maker,
        &window,
        rest_scale,
        at_root,
        siblings.as_ref(),
    );
    transform.translation = pos.extend(0.0);
    ortho.scale = scale;

    // 縮小したり上下に動かしたりして親（隣のフォルダ）が見えてきたら、親の場面を下に描いておく。
    let moved_vertically = (pos.y - rest_pos.y).abs() > rest_scale;
    if !at_root
        && (scale > rest_scale * 1.01 || moved_vertically)
        && state.parent_scene.is_none()
        && maker.tree.nodes[top.node].is_dir
    {
        let parent = nav.stack[nav.stack.len() - 2];
        state.parent_scene = Some(spawn_parent_preview(
            &nav,
            &mut maker,
            &window,
            parent.scroll,
            top.node,
            top.node,
            Some(top.node),
        ));
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
            &settings,
            siblings,
        );
    }
}

/// 開いているフォルダの、同じ階層の隣のフォルダ（親のフォルダの列で上と下）。
#[derive(Clone)]
struct Siblings {
    /// 親のフォルダの列に並ぶフォルダ（名前順）と、その中での今のフォルダの位置。
    items: Vec<NodeId>,
    index: usize,
    /// 親の行の高さ（隣のフォルダまでの距離）。
    row_height: f32,
}

impl Siblings {
    fn of(nav: &Nav, maker: &SceneMaker) -> Option<Self> {
        let top = nav.top();
        if nav.stack.len() < 2 || !maker.tree.nodes[top.node].is_dir {
            return None;
        }
        let parent = nav.stack[nav.stack.len() - 2];
        let items = column_items(&maker.tree, parent.node, DIRS);
        let index = items.iter().position(|c| *c == top.node)?;
        Some(Self {
            items,
            index,
            row_height: maker.layouts.0[parent.node].list.row_height,
        })
    }

    /// 上の隣（名前順で前）。
    fn above(&self) -> Option<NodeId> {
        self.index.checked_sub(1).map(|i| self.items[i])
    }

    /// 下の隣（名前順で次）。
    fn below(&self) -> Option<NodeId> {
        self.items.get(self.index + 1).copied()
    }
}

/// 親の場面を、`a` と `b`（とその上下）の行も描いて作る。開いているフォルダの下に敷く。
fn spawn_parent_preview(
    nav: &Nav,
    maker: &mut SceneMaker,
    window: &Window,
    parent_scroll: [usize; 2],
    a: NodeId,
    b: NodeId,
    skip: Option<NodeId>,
) -> Entity {
    let mut path = nav.stack[..nav.stack.len() - 1].to_vec();
    let parent = path.last_mut().expect("親がある");
    parent.scroll = parent_scroll;
    let items = column_items(&maker.tree, parent.node, DIRS);
    let ia = items.iter().position(|c| *c == a).unwrap_or(0);
    let ib = items.iter().position(|c| *c == b).unwrap_or(0);
    let rows = Rows::Around {
        scroll: parent.scroll,
        column: DIRS,
        from: ia.min(ib).saturating_sub(1),
        to: ia.max(ib) + 2,
        skip,
    };
    let (_, scale) = fit(parent.frame(&maker.layouts), window);
    maker.spawn_rows(&path, scale, rows)
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
    siblings: Option<&Siblings>,
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
    // 上下に隣のフォルダがあれば、そこまでは軽い重さで動く（その先は強い重さ）。
    let room = |up: bool| {
        siblings
            .filter(|s| {
                if up {
                    s.above().is_some()
                } else {
                    s.below().is_some()
                }
            })
            .map_or(0.0, |s| s.row_height)
    };
    let over_y = if over.y > 0.0 {
        follow(over.y, room(true), size.y)
    } else {
        -follow(-over.y, room(false), size.y)
    };
    let shown = clamped + Vec2::new(rubber(over.x, size.x * RUBBER_LIMIT), over_y);
    (shown + offset, scale)
}

/// はみ出し `d`（正）を、`room` までは軽い重さで、その先は強い重さで動かした量にする。
fn follow(d: f32, room: f32, size: f32) -> f32 {
    let light = (room / SIBLING_FOLLOW).min(d);
    light * SIBLING_FOLLOW + rubber(d - light, size * RUBBER_LIMIT)
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
    settings: &Settings,
    siblings: Option<Siblings>,
) {
    // しきい値は設定から（子へ移る・親へ戻る割合と、勢いで移る速さ）。
    let snap_in = settings.snap_in_percent / 100.0;
    let snap_out = settings.snap_out_percent / 100.0;
    let flick = settings.flick_rate().unwrap_or(f32::INFINITY);
    let top = *nav.top();
    let parent_scene = state.parent_scene.take();
    let rate = zoom_rate(&state.history);
    let pan_rate = zoom_rate(&state.pan_history);
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
    // 大きさが足りなくても、勢いよく拡大して離したら移る。
    if scale < rest_scale
        && let Some((child, shift)) = child
        && (maker.layouts.0[child].frame.width() / view_width >= snap_in || rate < -flick)
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
    if nav.stack.len() >= 2 && (coverage < snap_out || rate > flick) {
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

    // 上下に動かして、隣のフォルダが十分見えていれば（または勢いよく動かしたら）、そちらへ。
    if let Some(siblings) = siblings
        && (scale / rest_scale - 1.0).abs() < 0.05
        && let Some(target) = sibling_target(
            &siblings, maker, window, pos, scale, rest_pos, pan_rate, settings,
        )
    {
        move_to_sibling(
            nav,
            current,
            maker,
            window,
            parent_scene,
            top,
            target,
            pos,
            scale,
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

/// 離したときに移るべき隣のフォルダ。
#[allow(clippy::too_many_arguments)]
fn sibling_target(
    siblings: &Siblings,
    maker: &SceneMaker,
    window: &Window,
    pos: Vec2,
    scale: f32,
    rest_pos: Vec2,
    pan_rate: f32,
    settings: &Settings,
) -> Option<NodeId> {
    let moved = pos.y - rest_pos.y;
    let (target, toward) = if moved > 0.0 {
        (siblings.above()?, 1.0)
    } else if moved < 0.0 {
        (siblings.below()?, -1.0)
    } else {
        return None;
    };
    // 隣のフォルダの枠が、画面の高さのどれだけを占めているか。
    let view_height = (window.height() - BAR_PX) * scale;
    let view_center = pos.y - BAR_PX / 2.0 * scale;
    let frame_height = maker.layouts.0[target].frame.height();
    let target_center = view_center - moved + toward * siblings.row_height;
    let overlap = (view_center + view_height / 2.0).min(target_center + frame_height / 2.0)
        - (view_center - view_height / 2.0).max(target_center - frame_height / 2.0);
    let coverage = overlap.max(0.0) / view_height;
    // 勢い：離す直前に、画面の高さの何倍の速さで隣の方へ動いていたか。
    let speed = pan_rate * toward / view_height;
    let flick = settings.flick_rate().unwrap_or(f32::INFINITY);
    (coverage >= settings.sibling_percent / 100.0 || speed > flick).then_some(target)
}

/// 隣のフォルダへ移る。親の列のスクロールを、そのフォルダが見える位置に直してから動かす。
#[allow(clippy::too_many_arguments)]
fn move_to_sibling(
    nav: &mut Nav,
    current: &mut CurrentScene,
    maker: &mut SceneMaker,
    window: &Window,
    parent_scene: Option<Entity>,
    top: Level,
    target: NodeId,
    pos: Vec2,
    scale: f32,
) {
    let parent_index = nav.stack.len() - 2;
    let parent = nav.stack[parent_index];
    let list = maker.layouts.0[parent.node].list;
    let items = column_items(&maker.tree, parent.node, DIRS);
    let index = items.iter().position(|c| *c == target).unwrap_or(0);
    let mut scroll = parent.scroll;
    if index < scroll[DIRS] {
        scroll[DIRS] = index;
    } else if index >= scroll[DIRS] + list.fit {
        scroll[DIRS] = index + 1 - list.fit;
    }
    // スクロールが変わると、親の列の中のフォルダはまとめて上下にずれる。カメラも同じだけずらす。
    let delta = (scroll[DIRS] as f32 - parent.scroll[DIRS] as f32) * list.row_height;

    // 今のフォルダは画像で見せる（親の場面に描いてある）。隣へ着いたら本物に切り替える。
    if let Some(preview) = parent_scene {
        maker.commands.entity(preview).despawn();
    }
    maker.commands.entity(current.0).despawn();
    current.0 = spawn_parent_preview(nav, maker, window, scroll, top.node, target, None);

    nav.stack[parent_index].scroll = scroll;
    let shift = nav.stack[parent_index].column_shift(&maker.layouts, DIRS);
    *nav.stack.last_mut().expect("今のフォルダ") = Level {
        node: target,
        shift,
        scroll: [0, 0],
    };
    go(
        maker,
        window,
        nav,
        pos + Vec2::new(0.0, delta),
        scale,
        true,
        ZOOM_SECONDS,
        Ease::Out,
    );
}

/// 離す直前の拡大縮小の速さ（倍率の対数で毎秒。正が縮小、負が拡大）。
fn zoom_rate(history: &[(Instant, f32)]) -> f32 {
    let Some(&(last_time, last_value)) = history.last() else {
        return 0.0;
    };
    let Some(&(first_time, first_value)) =
        history.iter().find(|(t, _)| last_time - *t <= FLICK_WINDOW)
    else {
        return 0.0;
    };
    let seconds = (last_time - first_time).as_secs_f32();
    // 2つの入力が同じくらいの時刻に来たときは、速さを求められない。
    if seconds < 0.004 {
        return 0.0;
    }
    (last_value - first_value) / seconds
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

/// カーソルのある列。今がフォルダでなければ None。
fn column_under(nav: &Nav, maker: &SceneMaker, cursor_world: Option<Vec2>) -> Option<usize> {
    let top = nav.top();
    if !maker.tree.nodes[top.node].is_dir {
        return None;
    }
    let world = cursor_world?;
    let list = maker.layouts.0[top.node].list;
    let middle = (list.columns[0].max.x + list.columns[1].min.x) / 2.0;
    Some(if world.x < middle { FILES } else { DIRS })
}

/// カーソルのある列が、`direction` の向き（正で下の行へ）にまだスクロールできるか。
fn list_can_scroll(
    nav: &Nav,
    maker: &SceneMaker,
    cursor_world: Option<Vec2>,
    direction: f32,
) -> bool {
    let Some(column) = column_under(nav, maker, cursor_world) else {
        return false;
    };
    let top = nav.top();
    let fit = maker.layouts.0[top.node].list.fit;
    let count = column_items(&maker.tree, top.node, column).len();
    let scroll = top.scroll[column];
    if direction > 0.0 {
        scroll + fit < count
    } else {
        scroll > 0
    }
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
    let Some(column) = column_under(nav, maker, cursor_world) else {
        return rows;
    };
    let list = maker.layouts.0[top.node].list;
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
