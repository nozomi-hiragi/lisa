//! 読み込み中のリスト（画面の左下に固定）。今読んでいるフォルダと、このあと読むフォルダ、
//! 読んだ数と速さを出す。読み終えたら少しして消す。

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
    let (head, body) = match p.finished {
        Some(at) => {
            if let Ok(mut v) = panel.single_mut() {
                *v = if at.elapsed() >= HIDE_AFTER {
                    Visibility::Hidden
                } else {
                    Visibility::Inherited
                };
            }
            (
                format!(
                    "読み終えました  {} フォルダ{failed}・{elapsed:.1} 秒",
                    p.done
                ),
                String::new(),
            )
        }
        None => {
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
