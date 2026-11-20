//! 読み込みの状態ごとのタイルの見た目。
//!
//! - 未読み込み: 枠の中央に 3 つの点を並べる。
//! - 読み込み中: 本体の中央に回転するスピナーを重ねる。
//! - 読み込み済み: そのまま描く。
//! - 古い: 読み込んだ中身の上に、暗い膜を重ねる。
//! - 失敗: 赤みのある本体に × を書く。

use std::f32::consts::{FRAC_PI_2, TAU};
use std::time::Instant;

use bevy::camera::visibility::RenderLayers;

use bevy::math::Rect;
use bevy::prelude::*;
use bevy::sprite::Anchor;
use bevy::window::RequestRedraw;

use crate::model::tree::LoadState;
use crate::preview::{self, PreviewRegistry};
use crate::render::ear::{self, Size, Slot};
use crate::render::scene::{BACKGROUND, FolderPreview, OVERLAY_LAYER};
use crate::render::shapes::{Painter, over};

/// 失敗の赤（パレットの error）。
const ERROR: Color = Color::srgb(0.941, 0.478, 0.478);
/// 読み込み中の点の色（パレットの accent）。
const SPINNER_COLOR: Color = Color::srgb_u8(168, 230, 255);
const SPINNER_SECONDS: f64 = 0.95;
const SPINNER_DOTS: usize = 12;
/// 古いときに重ねる暗い膜の不透明度。
const STALE_ALPHA: f32 = 0.5;

/// スピナーの点をぶら下げる回転軸。
#[derive(Component)]
pub struct Spinner;

#[derive(Resource)]
pub struct SpinnerClock(Instant);

impl Default for SpinnerClock {
    fn default() -> Self {
        Self(Instant::now())
    }
}

/// ファイルのタイルの本体を描く。`background` は下の色。`live` なら読み込み中のスピナーを出す。
#[allow(clippy::too_many_arguments)]
pub fn draw_file(
    painter: &mut Painter,
    previews: &PreviewRegistry,
    state: LoadState,
    name: &str,
    frame: Rect,
    background: Color,
    z: f32,
    live: bool,
    loaded: Option<crate::preview::Preview>,
) {
    match state {
        LoadState::Unloaded => {
            outline(painter, frame, background, z);
            ellipsis(painter, frame, background, z + 0.1);
        }
        LoadState::Loading => {
            outline(painter, frame, background, z);
            if live {
                spinner(painter, frame, z + 0.1);
            }
        }
        LoadState::Loaded => preview::draw(previews, painter, name, frame, background, z, loaded),
        LoadState::Stale => {
            preview::draw(previews, painter, name, frame, background, z, loaded);
            dim(painter, frame, z + 0.1);
        }
        LoadState::Failed => failed(painter, frame, background, z),
    }
}

/// フォルダの枠と耳を描く。キャッシュ画像は枠の中央に載せる。
#[allow(clippy::too_many_arguments)]
pub fn draw_folder(
    painter: &mut Painter,
    state: LoadState,
    known_empty: bool,
    slot: Slot,
    size: Size,
    name: &str,
    image: Option<Handle<Image>>,
    color: Color,
    z: f32,
    live: bool,
) {
    let (color, image) = match state {
        LoadState::Unloaded => (color.with_alpha(0.65), None),
        LoadState::Failed => (ERROR, None),
        _ => (color, image),
    };
    let has_image = image.is_some();
    let show_placeholder = folder_needs_ellipsis(state, has_image, known_empty);
    ear::draw_folder(painter, slot, size, Some(name), color, image, z);
    if show_placeholder {
        let parent = painter.parent;
        let placeholder = ear::body_group(painter, slot, size);
        painter.commands.entity(placeholder).insert(FolderPreview);
        painter.parent = placeholder;
        // 古いフォルダの暗い膜より手前に置く。
        ellipsis(painter, slot.body, color, z + 0.11);
        painter.parent = parent;
    }
    match state {
        LoadState::Loading if folder_needs_spinner(live, has_image) => {
            let parent = painter.parent;
            let placeholder = ear::body_group(painter, slot, size);
            painter.commands.entity(placeholder).insert(FolderPreview);
            painter.parent = placeholder;
            spinner(painter, slot.body, z + 0.1);
            painter.parent = parent;
        }
        LoadState::Stale => {
            ear::draw_folder_overlay(
                painter,
                slot,
                size,
                name,
                BACKGROUND.with_alpha(STALE_ALPHA),
                z + 0.1,
            );
        }
        LoadState::Failed => {
            let parent = painter.parent;
            painter.parent = ear::body_group(painter, slot, size);
            painter.text(
                "×".to_string(),
                slot.body.center(),
                Anchor::CENTER,
                slot.body.height() * 0.6,
                slot.body.width(),
                ERROR,
                z + 0.1,
            );
            painter.parent = parent;
        }
        _ => {}
    }
}

fn folder_needs_ellipsis(state: LoadState, has_image: bool, known_empty: bool) -> bool {
    state == LoadState::Unloaded
        || (matches!(state, LoadState::Loaded | LoadState::Stale) && !has_image && !known_empty)
}

fn folder_needs_spinner(live: bool, has_image: bool) -> bool {
    live && !has_image
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

fn ellipsis_geometry(frame: Rect) -> ([Vec2; 3], f32) {
    let short_side = frame.size().min_element();
    let step = Vec2::X * (short_side * 0.16);
    let center = frame.center();
    ([center - step, center, center + step], short_side * 0.045)
}

fn ellipsis(painter: &mut Painter, frame: Rect, color: Color, z: f32) {
    let (dots, radius) = ellipsis_geometry(frame);
    let color = over(color, Color::WHITE, 0.55).with_alpha(0.7);
    for dot in dots {
        painter.circle(dot, radius, color, z);
    }
}

/// 先頭から尾にかけて薄い点を回転軸に並べる。軸と点は撮影に映らないレイヤーに置く。
fn spinner(painter: &mut Painter, frame: Rect, z: f32) {
    let parent = painter.parent;
    let overlay = RenderLayers::layer(OVERLAY_LAYER);
    let layer = std::mem::replace(&mut painter.layer, overlay.clone());
    painter.parent = painter
        .commands
        .spawn((
            ChildOf(parent),
            overlay,
            Spinner,
            Transform::from_translation(frame.center().extend(z)),
            Visibility::default(),
        ))
        .id();
    let short_side = frame.size().min_element();
    for index in 0..SPINNER_DOTS {
        let fraction = index as f32 / SPINNER_DOTS as f32;
        let angle = FRAC_PI_2 + fraction * TAU;
        let offset = Vec2::from_angle(angle) * (short_side * 0.16);
        let alpha = 0.95 - 0.8 * index as f32 / (SPINNER_DOTS - 1) as f32;
        painter.circle(
            offset,
            short_side * 0.025,
            SPINNER_COLOR.with_alpha(alpha),
            0.0,
        );
    }
    painter.parent = parent;
    painter.layer = layer;
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

fn spinner_angle(seconds: f64) -> f32 {
    -TAU * (seconds.rem_euclid(SPINNER_SECONDS) / SPINNER_SECONDS) as f32
}

/// 読み込み中のスピナーがある間だけ描き直す。
pub fn animate_spinners(
    clock: Res<SpinnerClock>,
    mut spinners: Query<&mut Transform, With<Spinner>>,
    occluded: Res<crate::view::WindowOccludedState>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    if occluded.0 || spinners.is_empty() {
        return;
    }
    let rotation = Quat::from_rotation_z(spinner_angle(clock.0.elapsed().as_secs_f64()));
    for mut transform in &mut spinners {
        transform.rotation = rotation;
    }
    redraw.write(RequestRedraw);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loading_folder_spins_only_without_a_thumbnail_in_the_live_scene() {
        assert!(folder_needs_spinner(true, false));
        assert!(!folder_needs_spinner(true, true));
        assert!(!folder_needs_spinner(false, false));
    }

    #[test]
    fn folder_without_a_preview_shows_dots_only_when_appropriate() {
        for state in [LoadState::Loaded, LoadState::Stale] {
            assert!(folder_needs_ellipsis(state, false, false));
            assert!(!folder_needs_ellipsis(state, false, true));
            assert!(!folder_needs_ellipsis(state, true, false));
        }
        assert!(folder_needs_ellipsis(LoadState::Unloaded, false, false));
        assert!(!folder_needs_ellipsis(LoadState::Loading, false, false));
        assert!(!folder_needs_ellipsis(LoadState::Failed, false, false));
    }

    #[test]
    fn ellipsis_is_centered_with_spacing_based_on_the_short_side() {
        for size in [Vec2::new(200.0, 100.0), Vec2::new(100.0, 200.0)] {
            let center = Vec2::new(45.0, -12.0);
            let (dots, radius) = ellipsis_geometry(Rect::from_center_size(center, size));
            assert_eq!(
                dots,
                [center - Vec2::X * 16.0, center, center + Vec2::X * 16.0]
            );
            assert!((radius - 4.5).abs() < 0.001);
        }
    }

    #[test]
    fn spinner_turns_clockwise_once_per_period() {
        let assert_near = |actual: f32, expected: f32| assert!((actual - expected).abs() < 0.00001);
        let quarter = SPINNER_SECONDS / 4.0;
        assert_near(spinner_angle(0.0), 0.0);
        assert_near(spinner_angle(quarter), -FRAC_PI_2);
        assert_near(spinner_angle(SPINNER_SECONDS), 0.0);
        assert_near(spinner_angle(SPINNER_SECONDS + quarter), -FRAC_PI_2);
    }
}
