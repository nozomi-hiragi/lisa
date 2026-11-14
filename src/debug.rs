//! デバッグ用に、FPS とフレーム数をウィンドウのタイトルに出す（デバッグビルドのみ、F3 で切り替え）。
//!
//! FPS は直前のフレームとの間隔から求めるので、止まっているときは約 1 になる。
//! これ自体は描画を頼まない。フレームが回ったときに数えるだけ。

use std::time::{Duration, Instant};

use bevy::prelude::*;

use crate::view::{Busy, TitleSuffix};

/// 動いている最中にタイトルを書き換える間隔の下限（OS に負担をかけないため）。
const MIN_UPDATE_INTERVAL: Duration = Duration::from_millis(250);

pub struct DebugTitlePlugin;

impl Plugin for DebugTitlePlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(TitleSuffix(Some(String::new())))
            .add_systems(Update, count_frames);
    }
}

#[derive(Default)]
struct Counter {
    frames: u64,
    last_frame: Option<Instant>,
    last_update: Option<Instant>,
    enabled_off: bool,
}

fn count_frames(
    mut counter: Local<Counter>,
    keys: Res<ButtonInput<KeyCode>>,
    busy: Res<Busy>,
    mut suffix: ResMut<TitleSuffix>,
) {
    let now = Instant::now();
    counter.frames += 1;
    let interval = counter.last_frame.map(|last| now - last);
    counter.last_frame = Some(now);

    if keys.just_pressed(KeyCode::F3) {
        counter.enabled_off = !counter.enabled_off;
        counter.last_update = None;
    }
    if counter.enabled_off {
        if suffix.0.is_some() {
            suffix.0 = None;
        }
        return;
    }
    // 動いている最中は書き換えを間引く。止まっているときは毎回（約 1 秒おき）。
    if busy.0
        && counter
            .last_update
            .is_some_and(|last| now - last < MIN_UPDATE_INTERVAL)
    {
        return;
    }
    counter.last_update = Some(now);

    let fps = interval.map_or(0.0, |d| 1.0 / d.as_secs_f32().max(1e-6));
    let mode = if busy.0 { "animating" } else { "idle" };
    let text = format!("{fps:.1} fps  #{}  {mode}", counter.frames);
    if suffix.0.as_deref() != Some(text.as_str()) {
        suffix.0 = Some(text);
    }
}
