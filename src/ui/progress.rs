//! 読み込み中のリスト（画面の左下に固定）。今読んでいるフォルダと、このあと読むフォルダ、
//! 読んだ数と速さを出す。読み終えたらハッシュの進み具合（今のフォルダ、数、大きさ、速さ）を出し、
//! それも終えたら少しして消す。

use std::path::Path;
use std::time::{Duration, Instant};

use bevy::prelude::*;

use crate::render::scene::LabelFont;
use crate::scan::ScanProgress;

const PANEL_COLOR: Color = Color::srgba(0.13, 0.13, 0.16, 0.88);
const TEXT_COLOR: Color = Color::srgb(0.9, 0.9, 0.92);
const DIM_COLOR: Color = Color::srgb(0.6, 0.6, 0.66);
/// 書き換える間隔の下限（毎フレーム文字を組み直さないように）。
const UPDATE_EVERY: Duration = Duration::from_millis(200);
/// 読み終えてから消すまで。
const HIDE_AFTER: Duration = Duration::from_secs(4);
/// パスをこの文字数より長ければ、前を「…」で切る。
const MAX_CHARS: usize = 70;

pub struct ProgressPlugin;

impl Plugin for ProgressPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(PostStartup, spawn_panel)
            .add_systems(Update, show_progress);
    }
}

#[derive(Component)]
struct Panel;

#[derive(Component)]
struct Summary;

#[derive(Component)]
struct Lines;

fn spawn_panel(mut commands: Commands, font: Res<LabelFont>, progress: Option<Res<ScanProgress>>) {
    if progress.is_none() {
        return;
    }
    let font = TextFont::from_font_size(12.0).with_font(font.0.clone());
    commands
        .spawn((
            Panel,
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(12.0),
                bottom: Val::Px(12.0),
                padding: UiRect::axes(Val::Px(10.0), Val::Px(6.0)),
                flex_direction: FlexDirection::Column,
                row_gap: Val::Px(2.0),
                border_radius: BorderRadius::all(Val::Px(6.0)),
                ..default()
            },
            BackgroundColor(PANEL_COLOR),
            GlobalZIndex(5),
            Pickable::IGNORE,
        ))
        .with_children(|panel| {
            panel.spawn((
                Summary,
                Text::new("読み込み中"),
                font.clone(),
                TextColor(TEXT_COLOR),
            ));
            panel.spawn((Lines, Text::new(""), font, TextColor(DIM_COLOR)));
        });
}

fn show_progress(
    progress: Option<Res<ScanProgress>>,
    control: Option<Res<crate::scan::ScanControl>>,
    mut last: Local<Option<Instant>>,
    mut panel: Query<&mut Visibility, With<Panel>>,
    mut summary: Query<&mut Text, (With<Summary>, Without<Lines>)>,
    mut lines: Query<&mut Text, (With<Lines>, Without<Summary>)>,
) {
    let Some(progress) = progress else {
        return;
    };
    if last.is_some_and(|t| t.elapsed() < UPDATE_EVERY) {
        return;
    }
    *last = Some(Instant::now());
    let Ok(p) = progress.0.lock() else {
        return;
    };
    let elapsed = p
        .finished
        .unwrap_or_else(Instant::now)
        .duration_since(p.started.unwrap_or_else(Instant::now))
        .as_secs_f64();
    let rate = p.done as f64 / elapsed.max(1e-3);
    let failed = if p.failed > 0 {
        format!("（失敗 {}）", p.failed)
    } else {
        String::new()
    };
    let hash = &p.hash;
    let (head, body) = match (p.finished, hash.started, hash.finished) {
        // 読み終えて、ハッシュも終えた（か、ハッシュしない）。
        (Some(at), started, finished) if started.is_none() || finished.is_some() => {
            let at = finished.unwrap_or(at);
            if let Ok(mut v) = panel.single_mut() {
                *v = if at.elapsed() >= HIDE_AFTER {
                    Visibility::Hidden
                } else {
                    Visibility::Inherited
                };
            }
            let head = match started {
                Some(_) => format!(
                    "読み終えました  {} フォルダ{failed}・ハッシュ {} ファイル（{}）",
                    p.done,
                    hash.files,
                    bytes(hash.bytes)
                ),
                None => format!(
                    "読み終えました  {} フォルダ{failed}・{elapsed:.1} 秒",
                    p.done
                ),
            };
            (head, String::new())
        }
        // ハッシュしている。
        (Some(_), Some(started), None) => {
            if let Ok(mut v) = panel.single_mut() {
                *v = Visibility::Inherited;
            }
            let seconds = started.elapsed().as_secs_f64().max(1e-3);
            let speed = hash.bytes as f64 / seconds;
            let mut body = String::new();
            if let Some(folder) = &hash.folder {
                body = format!("▶ {}", shorten(folder));
            }
            (
                format!(
                    "ハッシュ中  {} ファイル・{}・1 秒に {}",
                    hash.files,
                    bytes(hash.bytes),
                    bytes(speed as u64)
                ),
                body,
            )
        }
        _ => {
            let mut body = Vec::new();
            if let Some(current) = &p.current {
                body.push(format!("▶ {}", shorten(current)));
            }
            body.extend(p.next.iter().map(|path| format!("   {}", shorten(path))));
            (
                format!(
                    "読み込み中  {} フォルダ{failed}・あと {}・1 秒に {rate:.0}",
                    p.done, p.queued
                ),
                body.join("\n"),
            )
        }
    };
    // 裏の分を休んでいるなら、その理由を添える。
    let rest = control.map_or(crate::scan::Rest::No, |c| c.control.rest());
    let working = p.finished.is_none() || (hash.started.is_some() && hash.finished.is_none());
    let head = match rest {
        _ if !working => head,
        crate::scan::Rest::No => head,
        crate::scan::Rest::Paused => format!("{head}（一時停止中）"),
        crate::scan::Rest::Battery => format!("{head}（電池なので休み中）"),
        crate::scan::Rest::Cpu => format!("{head}（CPU が混んでいるので休み中）"),
    };
    if let Ok(mut text) = summary.single_mut()
        && text.0 != head
    {
        text.0 = head;
    }
    if let Ok(mut text) = lines.single_mut()
        && text.0 != body
    {
        text.0 = body;
    }
}

/// 読みやすい大きさ（1024 区切り）。
fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// 長いパスは前を切って、後ろ（今いる場所に近いほう）を残す。
fn shorten(path: &Path) -> String {
    let text = path.display().to_string();
    let count = text.chars().count();
    if count <= MAX_CHARS {
        return text;
    }
    let tail: String = text.chars().skip(count - (MAX_CHARS - 1)).collect();
    format!("…{tail}")
}
