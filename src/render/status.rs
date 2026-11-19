//! 読み込みの状態ごとのタイルの見た目。
//!
//! - 未読み込み: 枠だけを薄く描く（中身はまだわからない）。
//! - 読み込み中: 未読み込みと同じ枠の上に、脈打つ光を重ねる（フォルダは読めた分の中身の上に）。
//! - 読み込み済み: そのまま描く。
//! - 古い: 読み込んだ中身の上に、暗い膜を重ねる。
//! - 失敗: 赤みのある本体に × を書く。

use std::time::Instant;

use bevy::math::Rect;
use bevy::prelude::*;
use bevy::sprite::Anchor;
use bevy::window::RequestRedraw;

use crate::model::tree::LoadState;
use crate::render::preview;
use crate::render::scene::{BACKGROUND, OVERLAY_LAYER};
use crate::render::shapes::{BaseAlpha, Painter, over};

/// 失敗の赤（パレットの error）。
const ERROR: Color = Color::srgb(0.941, 0.478, 0.478);
/// 読み込み中の光の色（パレットの accent）。
const PULSE: Color = Color::srgb(0.553, 0.706, 0.918);
/// 読み込み中の光のいちばん濃いときの不透明度。
const PULSE_ALPHA: f32 = 0.28;
/// 脈打つ周期（秒）。
const PULSE_SECONDS: f32 = 1.4;
/// 古いときに重ねる暗い膜の不透明度。
const STALE_ALPHA: f32 = 0.5;

/// 脈打つ光。`pulse` が不透明度を毎フレーム変える。
#[derive(Component)]
pub struct Pulse;

/// 脈打ちの起点（全部のタイルで揃える）。
#[derive(Resource)]
pub struct PulseClock(pub Instant);

impl Default for PulseClock {
    fn default() -> Self {
        Self(Instant::now())
    }
}

/// ファイルのタイルの本体を描く。`background` は下の色。`live` なら読み込み中の光を動かす
/// （画像に焼くときは動かせないので、光を描かない）。
pub fn draw_file(
    painter: &mut Painter,
    state: LoadState,
    name: &str,
    frame: Rect,
    background: Color,
    z: f32,
    live: bool,
) {
    match state {
        LoadState::Unloaded => outline(painter, frame, background, z),
        LoadState::Loading => {
            outline(painter, frame, background, z);
            if live {
                pulse(painter, frame, z + 0.1);
            }
        }
        LoadState::Loaded => preview::draw(painter, name, frame, background, z),
        LoadState::Stale => {
            preview::draw(painter, name, frame, background, z);
            dim(painter, frame, z + 0.1);
        }
        LoadState::Failed => failed(painter, frame, background, z),
    }
}

/// フォルダのタイルの本体を描く。`image` は焼いた中身（無ければ空の本体）、`color` は本体の色。
#[allow(clippy::too_many_arguments)]
pub fn draw_folder(
    painter: &mut Painter,
    state: LoadState,
    frame: Rect,
    image: Option<Handle<Image>>,
    color: Color,
    background: Color,
    z: f32,
    live: bool,
) {
    let body = |painter: &mut Painter| match image.clone() {
        // 画像には子の本体と中身が焼いてある（耳は入っていない）。
        Some(image) => painter.image(frame, image, z),
        // まだ画像が無いときは、空の本体だけ。
        None => painter.rect(frame, color, z),
    };
    match state {
        LoadState::Unloaded => outline(painter, frame, background, z),
        LoadState::Loading => {
            body(painter);
            if live {
                pulse(painter, frame, z + 0.1);
            }
        }
        LoadState::Loaded => body(painter),
        LoadState::Stale => {
            body(painter);
            dim(painter, frame, z + 0.1);
        }
        LoadState::Failed => failed(painter, frame, background, z),
    }
}

/// 枠だけ。中は下の色のまま。
fn outline(painter: &mut Painter, frame: Rect, background: Color, z: f32) {
    let color = over(background, Color::WHITE, 0.22);
    let w = (frame.height() * 0.025).max(painter.scale * 1.5);
    let (min, max) = (frame.min, frame.max);
    for rect in [
        Rect::new(min.x, min.y, max.x, min.y + w),
        Rect::new(min.x, max.y - w, max.x, max.y),
        Rect::new(min.x, min.y + w, min.x + w, max.y - w),
        Rect::new(max.x - w, min.y + w, max.x, max.y - w),
    ] {
        painter.rect(rect, color, z);
    }
}

/// 脈打つ光。最初は透明で、`pulse` が濃さを変える（`OVERLAY_LAYER` に描き、サムネには入れない）。
fn pulse(painter: &mut Painter, frame: Rect, z: f32) {
    let parent = painter.parent;
    let layer = bevy::camera::visibility::RenderLayers::layer(OVERLAY_LAYER);
    let mut entity = painter.commands.spawn((
        ChildOf(parent),
        layer,
        // フェードでは消しておく（フェードが終われば `pulse` が描く）。
        BaseAlpha(0.0),
        Pulse,
        Sprite::from_color(PULSE.with_alpha(0.0), frame.size()),
        Transform::from_translation(frame.center().extend(z)),
    ));
    if painter.no_cull {
        entity.insert(bevy::camera::visibility::NoFrustumCulling);
    }
}

/// 古いときの暗い膜。
fn dim(painter: &mut Painter, frame: Rect, z: f32) {
    painter.rect(frame, BACKGROUND.with_alpha(STALE_ALPHA), z);
}

/// 赤みのある本体と ×。
fn failed(painter: &mut Painter, frame: Rect, background: Color, z: f32) {
    let body = over(background, ERROR, 0.22);
    painter.rect(frame, body, z);
    painter.text(
        "×".to_string(),
        frame.center(),
        Anchor::CENTER,
        frame.height() * 0.6,
        frame.width(),
        over(body, ERROR, 0.85),
        z + 0.05,
    );
}

/// 読み込み中の光を脈打たせる。光があるあいだは描き続ける（ウィンドウが隠れている間は止める）。
pub fn pulse_loading(
    clock: Res<PulseClock>,
    mut pulses: Query<&mut Sprite, With<Pulse>>,
    occluded: Res<crate::view::WindowOccludedState>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    if occluded.0 || pulses.is_empty() {
        return;
    }
    let t = clock.0.elapsed().as_secs_f32() / PULSE_SECONDS;
    let k = 0.5 - 0.5 * (t * std::f32::consts::TAU).cos();
    for mut sprite in &mut pulses {
        sprite.color.set_alpha(PULSE_ALPHA * k);
    }
    redraw.write(RequestRedraw);
}
