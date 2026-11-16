//! トラックパッドとマウスの操作（2本指、ピンチ、ホイール、中ボタン）。
//!
//! - 2本指（と中ボタン）で動かすと、カメラが指について動く。ピンチとホイールは、カーソルの位置を
//!   中心に拡大縮小する。
//! - フォルダやファイルを開いているとき、周りには同じ階層の隣（親のタイル）が見えている。隣がある
//!   範囲は縦も横も斜めも重さなしで動ける。行き先の無い方向は重くなる（ラバーバンド）。
//! - 指を離したら、離す直前の速さを持ったまま動き続け、いちばん近い行き先（今のもの、中の子、親、
//!   隣）へ重力で引き寄せられて止まる。移動もズームも同じ決まり。動いている途中で触れるとつかめる。
//!
//! マウスとトラックパッドはスクロールの単位で見分ける（行単位がマウス、ピクセル単位が
//! トラックパッド。Magic Mouse はピクセル単位なのでトラックパッド扱いになる）。
//!
//! 2本指のスクロールには「指を置いた・動かした・離した」が付いてくる。macOS が指を離したあとに
//! 送ってくる慣性は使わない（離したあとの動きは重力で作る）。ピンチは macOS なら指が乗っているかを
//! NSEvent で見て、ほかでは入力が少し途切れたら離したとみなす。

use std::time::{Duration, Instant};

use bevy::ecs::system::NonSendMarker;
use bevy::input::gestures::PinchGesture;
use bevy::input::mouse::{MouseScrollUnit, MouseWheel};
use bevy::math::Rect;
use bevy::prelude::*;
use bevy::window::PrimaryWindow;

use crate::input::mac_scroll;
use crate::input::scroll_input::{Action, ScrollMachine};
use crate::model::tree::{FileTree, NodeId};
use crate::ui::menu::{BAR_PX, Popup};
use crate::view::{
    Animating, CurrentScene, Fade, MainCamera, Nav, SceneMaker, fit, scene_path, ui_has_pointer,
};

use super::settle::{Coast, choose_pan, coast_step, refocus, show_parent_if_zoomed_out};
use super::zoom_levels::{choose_zoom, zoom_ladder};

/// ピンチの指が乗っているか分からないとき（macOS 以外）、入力がこれだけ途切れたら離したとみなす。
const PINCH_RELEASE_AFTER: Duration = Duration::from_millis(40);
/// 入力がこれだけ途切れたら、指を離したとみなす（トラックパッド）。
const RELEASE_AFTER: Duration = Duration::from_millis(100);
/// マウスのホイールがこれだけ止まったら、行き先を決める。
const WHEEL_RELEASE_AFTER: Duration = Duration::from_millis(150);
/// true にすると、トラックパッドの2本指の縦スクロールでも（マウスのホイールのように）拡大縮小する。
/// false なら、2本指は移動。
const TRACKPAD_SCROLL_ZOOMS: bool = false;
/// TRACKPAD_SCROLL_ZOOMS のとき、縦に 1px 動かしたぶんの拡大縮小の割合。
const TRACKPAD_ZOOM_PER_PX: f32 = 0.004;
/// マウスのホイール1段で拡大縮小する割合。
const WHEEL_ZOOM_STEP: f32 = 0.12;
/// ラバーバンドの硬さ。はみ出しは画面の大きさ × これ に近づいていく。
const RUBBER_LIMIT: f32 = 0.35;
/// Root でさらに縮小しようとしたときの重さ（対数の倍率に対する上限）。
const ROOT_ZOOM_OUT_LIMIT: f32 = 0.25;
/// 離す直前のこの時間の動きから、離したときの速さを求める。
const VELOCITY_WINDOW: Duration = Duration::from_millis(80);
/// 拡大できる上限（今の枠に合わせた倍率に対して）。
const MAX_ZOOM_IN: f32 = 40.0;
/// 縮小できる上限（Root を収める倍率に対して）。
const MAX_ZOOM_OUT: f32 = 4.0;
/// 離したときの拡大縮小の勢いを、これだけ先（秒）まで見込んで、ピンチの幅に足す。
const ZOOM_LOOKAHEAD: f32 = 0.1;
#[derive(Resource, Default)]
pub struct Gesture {
    /// 指で動かしている最中なら、最後に入力が来た時刻。
    last_input: Option<Instant>,
    /// 制限をかける前のカメラの位置と倍率（指の動きそのまま）。
    raw_pos: Vec2,
    raw_scale: f32,
    /// 最後の入力がマウスのホイールか、2本指のスクロールか。
    by_wheel: bool,
    by_scroll: bool,
    /// ピンチで拡大縮小した。
    by_pinch: bool,
    /// ピンチ（ホイール）で拡大縮小を始めたときの、開いていたものと倍率の対数。
    zoom_start: Option<(NodeId, f32)>,
    /// 中ボタンで引っぱっているなら、前のフレームのカーソルの位置。
    middle: Option<Vec2>,
    /// 縮小して親が見えてきたときに、下に描いておく親の場面。
    parent_scene: Option<Entity>,
    /// 見せた倍率と位置の記録（時刻、倍率の対数、位置）。離す直前の速さを求める。
    history: Vec<(Instant, f32, Vec2)>,
    /// 離したあと、近くのタイルへ引き寄せられて動いている最中。
    coast: Option<Coast>,
}
impl Gesture {
    /// 指で動かしているか、離したあと動いている最中なら true（毎フレーム描画する）。
    pub fn active(&self) -> bool {
        self.last_input.is_some() || self.coast.is_some()
    }

    /// 縮めたまま止まって、親の場面を下に敷いている。
    pub fn has_parent_scene(&self) -> bool {
        self.parent_scene.is_some()
    }

    /// 下に敷いたままの親の場面を返して手放す（クリックなどで場面を作り直すとき、消すため）。
    pub fn take_parent_scene(&mut self) -> Option<Entity> {
        self.parent_scene.take()
    }
}
/// 2本指のスクロールの状態（`scroll_input` の状態機械）。ジェスチャーが終わっても残す。
#[derive(Resource)]
pub struct TrackpadInput {
    machine: ScrollMachine,
    /// 時刻の起点（状態機械には、ここからの秒数を渡す）。
    epoch: Instant,
    /// デバッグ用の入力の記録先（LISA_INPUT_LOG=1 のときだけ）。
    log: Option<std::fs::File>,
    /// macOS の NSEvent を直接見ているか（見ていれば winit のピクセル単位のスクロールは使わない）。
    raw: bool,
}
impl Default for TrackpadInput {
    fn default() -> Self {
        Self {
            machine: ScrollMachine::default(),
            epoch: Instant::now(),
            log: input_log(),
            raw: false,
        }
    }
}
impl TrackpadInput {
    fn now(&self) -> f64 {
        self.epoch.elapsed().as_secs_f64()
    }

    fn write_log(&mut self, line: std::fmt::Arguments) {
        let ms = self.now() * 1000.0;
        if let Some(file) = &mut self.log {
            use std::io::Write;
            let _ = writeln!(file, "{ms:.1} {line}");
        }
    }

    fn drop_momentum(&mut self) {
        let before = self.machine.state;
        self.machine.drop_momentum();
        if before != self.machine.state {
            let after = self.machine.state;
            self.write_log(format_args!("drop_momentum {before:?} -> {after:?}"));
        }
    }
}
/// macOS で NSEvent のスクロールを直接見るモニタを付ける。メインスレッドで動かす。
pub fn install_scroll_monitor(_main_thread: NonSendMarker, mut input: ResMut<TrackpadInput>) {
    input.raw = mac_scroll::install();
    let raw = input.raw;
    input.write_log(format_args!("nsevent monitor installed={raw}"));
}
/// デバッグビルドで LISA_INPUT_LOG=1 のとき、入力を /tmp/lisa-input.log に書き足す。
fn input_log() -> Option<std::fs::File> {
    if !cfg!(debug_assertions) || std::env::var("LISA_INPUT_LOG").as_deref() != Ok("1") {
        return None;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/lisa-input.log")
        .ok()
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
    mut fades: Query<&mut Fade>,
    popup: Res<Popup>,
    buttons: Res<ButtonInput<MouseButton>>,
    mut input: ResMut<TrackpadInput>,
) {
    let wheel: Vec<MouseWheel> = wheel.read().cloned().collect();
    let pinch: f32 = pinch.read().map(|p| p.0).sum();

    // 2本指のスクロールは、まず状態機械に通して「指」「離した」「慣性」に分ける。
    // アニメーション中でも通すので、状態がずれない。
    let mut touches: Vec<Vec2> = Vec::new();
    let mut momentum: Vec<(Vec2, f32)> = Vec::new();
    let mut lifted_now = false;
    let mut line_zoom = 0.0;
    let mut touch_began = false;
    let now = input.now();
    let mut actions = Vec::new();
    // 触れたと推し量ったまま何も来なかったら、離したことにする。
    let tick = input.machine.tick(now);
    if tick != Action::None {
        input.write_log(format_args!("tick -> {tick:?}"));
        actions.push(tick);
    }
    // macOS では NSEvent を直接見た記録を使う（指と慣性を確実に見分けられる）。
    // ほかでは winit のスクロールから推し量る。winit の動きは物理ピクセルなので論理に直す。
    let raw = mac_scroll::drain();
    let scale_factor = window.scale_factor();
    for event in &wheel {
        match event.unit {
            MouseScrollUnit::Line => line_zoom += event.y,
            MouseScrollUnit::Pixel if !input.raw => {
                let before = input.machine.state;
                let delta = Vec2::new(event.x, event.y) / scale_factor;
                let action = input.machine.event(now, event.phase, delta);
                let after = input.machine.state;
                input.write_log(format_args!(
                    "wheel px phase={:?} d=({}, {}) -> {action:?} state={after:?}{}",
                    event.phase,
                    delta.x,
                    delta.y,
                    if before != after { " (changed)" } else { "" }
                ));
                actions.push(action);
            }
            MouseScrollUnit::Pixel => {}
        }
    }
    for record in raw {
        let before = input.machine.state;
        let action =
            input
                .machine
                .event_raw(now, record.phase, record.momentum_phase, record.delta);
        let after = input.machine.state;
        input.write_log(format_args!(
            "nsevent phase={:?} momentum={:?} d=({}, {}) -> {action:?} state={after:?}{}",
            record.phase,
            record.momentum_phase,
            record.delta.x,
            record.delta.y,
            if before != after { " (changed)" } else { "" }
        ));
        actions.push(action);
    }
    for action in actions {
        match action {
            Action::TouchBegin(d) => {
                touch_began = true;
                if d != Vec2::ZERO {
                    touches.push(d);
                }
            }
            Action::Touch(d) => touches.push(d),
            Action::Lift => lifted_now = true,
            Action::Momentum { delta, since_lift } => momentum.push((delta, since_lift)),
            Action::None => {}
        }
    }
    if pinch != 0.0 {
        input.write_log(format_args!("pinch {pinch}"));
    }

    // 戻ったり移ったりしている最中に、新しく指で触れて動かしたら（ピンチ、中ボタンも）、
    // すぐにそれをやめて、今の位置から指で動かせるようにする。
    let new_input = touch_began
        || !touches.is_empty()
        || pinch != 0.0
        || line_zoom != 0.0
        || buttons.just_pressed(MouseButton::Middle);
    let busy = animating.is_some() || !fades.is_empty();
    if busy && new_input && !popup.open {
        if let Some(anim) = &animating {
            maker.commands.remove_resource::<Animating>();
            // 着いたら本物に切り替える予定だったなら、今すぐ切り替える。
            if anim.then_show {
                let path = scene_path(&nav, &maker.tree);
                let (_, scene_scale) = fit(path[path.len() - 1].frame(&maker.layouts), &window);
                maker.commands.entity(current.0).despawn();
                current.0 = maker.spawn(path, scene_scale);
            }
        }
        // フェードはすぐ終わらせる（このあとのフェードの処理で片付く）。
        for mut fade in &mut fades {
            fade.started = Instant::now() - Duration::from_secs(1);
        }
        input.write_log(format_args!("animation interrupted by new input"));
        // このフレームの動きは使わない。今の位置で止めて（指で持った状態にして）、
        // 次のフレームから動かし始める。指を離したら、そこから戻るか移るかを決める。
        let pos = camera.2.translation.truncate();
        let scale = match &*camera.3 {
            Projection::Orthographic(ortho) => ortho.scale,
            _ => 1.0,
        };
        let parent_scene = state.parent_scene.take();
        *state = Gesture::default();
        state.parent_scene = parent_scene;
        begin(&mut state, pos, scale);
        state.by_scroll = touch_began || !touches.is_empty();
        if buttons.just_pressed(MouseButton::Middle) {
            state.middle = window.cursor_position();
        }
        return;
    } else if busy || (popup.open && !state.active()) {
        // 切り替えの途中やメニューの操作中は、入力を捨てる。慣性はこの先も使わない。
        if !momentum.is_empty() || lifted_now {
            input.drop_momentum();
        }
        return;
    }
    let (camera, camera_global, mut transform, mut projection) = camera.into_inner();
    let Projection::Orthographic(ortho) = projection.as_mut() else {
        return;
    };
    let top = *nav.top();
    let (_, rest_scale) = fit(top.frame(&maker.layouts), &window);
    let cursor = window.cursor_position();
    let cursor_world = cursor.and_then(|p| camera.viewport_to_world_2d(camera_global, p).ok());
    let over_ui = ui_has_pointer(&window, &popup);

    // macOS の慣性は使わない（離したあとの動きは、こちらで重力をかけて作る）。
    if !momentum.is_empty() || lifted_now {
        input.drop_momentum();
    }

    // 離したあと動いている最中。新しく触れたら、その場でつかむ。そうでなければ 1 歩進める。
    if let Some(mut coast) = state.coast.take() {
        if new_input && !over_ui {
            state.parent_scene = coast.parent_scene;
            begin(&mut state, transform.translation.truncate(), ortho.scale);
            input.write_log(format_args!("coast grabbed"));
        } else {
            let arrived = coast_step(
                &mut coast,
                &mut nav,
                &mut current,
                &mut maker,
                &window,
                &mut transform,
                ortho,
            );
            if arrived {
                // 縮めたまま止まったときの親の場面は、次に動かすときまで残す。
                state.parent_scene = coast.parent_scene;
            } else {
                state.coast = Some(coast);
            }
            return;
        }
    }

    let mut touched = false;
    let mut zoom = 0.0;
    let mut pan: Vec<Vec2> = Vec::new();
    if !over_ui {
        // マウスのホイールは、ポインタを中心にした拡大縮小。
        if line_zoom != 0.0 {
            zoom += line_zoom * WHEEL_ZOOM_STEP;
            state.by_wheel = true;
        }
        for delta in &touches {
            state.by_wheel = false;
            state.by_scroll = true;
            if TRACKPAD_SCROLL_ZOOMS {
                zoom += delta.y * TRACKPAD_ZOOM_PER_PX;
                pan.push(Vec2::new(delta.x, 0.0));
            } else {
                pan.push(*delta);
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
        // 指についてカメラを動かす（中身が指と同じ向きに動く）。
        state.raw_pos += Vec2::new(-delta.x, delta.y) * ortho.scale;
    }

    // ピンチ（指を広げると正）とホイール（上へ回すと正）は、どちらも正で拡大。
    // ピンチしながら指を動かすと、移動（上）と拡大縮小（下）が同じフレームで両方かかる。
    let pinched = pinch != 0.0;
    let pinch = pinch + zoom;
    if pinch != 0.0 && !over_ui {
        begin(&mut state, transform.translation.truncate(), ortho.scale);
        touched = true;
        if state.zoom_start.is_none() {
            state.zoom_start = Some((nav.top().node, ortho.scale.ln()));
        }
        if pinched {
            state.by_pinch = true;
        }
        // カーソルの下の点が動かないように拡大縮小する。
        let factor = (-pinch).exp();
        // 縮める上限は Root を基準にする（開いている場所が途中で変わっても、急に止まらない）。
        let root_scale = fit(maker.layouts[FileTree::ROOT].fit, &window).1;
        let new_scale =
            (state.raw_scale * factor).clamp(rest_scale / MAX_ZOOM_IN, root_scale * MAX_ZOOM_OUT);
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
    let siblings = Siblings::of(&nav, &maker);
    let (pos, scale) = constrain(&state, &nav, &maker, &window, rest_scale, siblings.as_ref());
    transform.translation = pos.extend(0.0);
    ortho.scale = scale;
    if touched {
        let now = Instant::now();
        state.history.push((now, scale.ln(), pos));
        state
            .history
            .retain(|(t, _, _)| now - *t <= VELOCITY_WINDOW * 2);
    }

    // 縮小して親が見えてきたら、親の場面を下に描いておく（隣は今の場面に描いてある）。
    show_parent_if_zoomed_out(
        &mut state.parent_scene,
        &nav,
        &mut current,
        &mut maker,
        &window,
        scale,
    );
    // 動かしている最中も、いちばん近いものが変わったら、開いている場所をそちらへ切り替える。
    // ピンチの最中も切り替える（行き先は、ピンチを始めたときのものから決める）。
    if touched {
        refocus(
            &mut state.parent_scene,
            &mut nav,
            &mut current,
            &mut maker,
            &window,
            pos,
            scale,
        );
    }

    // 指を離したら、その速さのまま、いちばん近い行き先へ引き寄せられて動く。
    // 指が乗っている間は、止まっていても、はみ出していても、そのまま保つ。
    // macOS ではピンチの指が乗っているかも分かるので、離した瞬間に動き出す。ほかとホイールは
    // 入力の途切れで離したとみなす。
    let pinch_known = state.by_pinch && input.raw;
    let wait = if state.by_wheel {
        WHEEL_RELEASE_AFTER
    } else if state.by_pinch {
        PINCH_RELEASE_AFTER
    } else {
        RELEASE_AFTER
    };
    let holding = state.middle.is_some()
        || (state.by_scroll && input.machine.touching())
        || (pinch_known && mac_scroll::pinch_touching());
    let released = !holding
        && (middle_released
            || (state.by_scroll && lifted_now)
            || (pinch_known && !touched)
            || (!touched && last_input.elapsed() >= wait));
    if released {
        // ホイールは段ごとの動きなので、速さは引き継がない。
        let (velocity_ln, velocity) = if state.by_wheel {
            (0.0, Vec2::ZERO)
        } else {
            release_velocity(&state.history)
        };
        // ピンチ（ホイール）なら、使った幅（と勢い）で、次の段へ進むか戻るかを決める。
        let zoom_goal = state.zoom_start.map(|(from, start_ln)| {
            let amount = scale.ln() - start_ln + velocity_ln * ZOOM_LOOKAHEAD;
            let point = cursor_world.unwrap_or(pos - Vec2::new(0.0, BAR_PX / 2.0 * scale));
            choose_zoom(&zoom_ladder(from, &maker, &window, point), start_ln, amount)
        });
        // 2本指で動かしただけなら、勢いの先を見込んで行き先を決める（勢いがあれば隣へ進む）。
        let pan_goal = if zoom_goal.is_none() {
            choose_pan(&nav, &maker, &window, pos, scale, velocity)
        } else {
            None
        };
        let parent_scene = state.parent_scene.take();
        *state = Gesture::default();
        state.coast = Some(Coast {
            velocity,
            velocity_ln,
            last: Instant::now(),
            parent_scene,
            zoom_goal,
            pan_goal,
        });
        input.write_log(format_args!(
            "release velocity=({}, {}) ln={velocity_ln}",
            velocity.x, velocity.y
        ));
    }
}
/// 開いているものと同じ階層の隣（親の中のほかのタイル）がある範囲。
#[derive(Clone)]
struct Siblings {
    /// 隣へそのまま動いてよい範囲（それぞれを画面に収めたときの、枠の真ん中が並ぶ範囲）。
    area: Rect,
}
impl Siblings {
    fn of(nav: &Nav, maker: &SceneMaker) -> Option<Self> {
        if nav.stack.len() < 2 {
            return None;
        }
        let parent = nav.stack[nav.stack.len() - 2];
        let mut centers = maker.tree.nodes[parent.node]
            .children
            .iter()
            .map(|c| maker.layouts[*c].fit.center());
        let first = centers.next()?;
        let area = centers.fold(Rect::from_corners(first, first), |r, c| r.union_point(c));
        Some(Self { area })
    }
}
/// ジェスチャーを始める。
fn begin(state: &mut Gesture, pos: Vec2, scale: f32) {
    if state.last_input.is_none() {
        state.last_input = Some(Instant::now());
        state.raw_pos = pos;
        state.raw_scale = scale;
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
    nav: &Nav,
    maker: &SceneMaker,
    window: &Window,
    rest_scale: f32,
    siblings: Option<&Siblings>,
) -> (Vec2, f32) {
    let top = *nav.top();
    let at_root = nav.stack.len() < 2;
    let parent = nav.stack.len().checked_sub(2).map(|i| nav.stack[i]);
    // Root でさらに縮小しようとすると重くなる。
    let mut scale = state.raw_scale;
    if at_root && scale > rest_scale {
        let over = (scale / rest_scale).ln();
        scale = rest_scale * rubber(over, ROOT_ZOOM_OUT_LIMIT).exp();
    }
    // カメラの中心（バーの下の領域の中心）を、動いてよい範囲に重さつきで収める。
    let offset = Vec2::new(0.0, BAR_PX / 2.0 * scale);
    let view_center = state.raw_pos - offset;
    // 親があれば、親の枠の中は動いてよい（行き先が変わっても、動ける範囲が急に変わらない）。
    let mut area = bounds(top.frame(&maker.layouts), window, scale);
    if let Some(parent) = parent {
        area = area.union(bounds(parent.frame(&maker.layouts), window, scale));
    }
    let size = Vec2::new(window.width(), window.height() - BAR_PX) * scale;
    // 隣があれば、その範囲へは重さなしで動ける。無い方向だけ重くなる。
    let area = siblings.map_or(area, |s| area.union(s.area));
    let clamped = view_center.clamp(area.min, area.max);
    let over = view_center - clamped;
    let shown = clamped
        + Vec2::new(
            rubber(over.x, size.x * RUBBER_LIMIT),
            rubber(over.y, size.y * RUBBER_LIMIT),
        );
    (shown + offset, scale)
}
/// 離す直前の速さ（倍率の対数で毎秒、ワールド座標で毎秒）。
fn release_velocity(history: &[(Instant, f32, Vec2)]) -> (f32, Vec2) {
    let Some(&(last_time, last_ln, last_pos)) = history.last() else {
        return (0.0, Vec2::ZERO);
    };
    let Some(&(first_time, first_ln, first_pos)) = history
        .iter()
        .find(|(t, _, _)| last_time - *t <= VELOCITY_WINDOW)
    else {
        return (0.0, Vec2::ZERO);
    };
    let seconds = (last_time - first_time).as_secs_f32();
    // 2つの入力が同じくらいの時刻に来たときは、速さを求められない。
    if seconds < 0.004 {
        return (0.0, Vec2::ZERO);
    }
    // 離す直前にしばらく止まっていたなら、速さは 0（指を止めてから離した）。
    if Instant::now() - last_time > Duration::from_millis(200) {
        return (0.0, Vec2::ZERO);
    }
    (
        (last_ln - first_ln) / seconds,
        (last_pos - first_pos) / seconds,
    )
}
