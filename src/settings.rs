//! 設定。OS の設定フォルダの settings.json に保存し、起動時に読み込む。
//! 設定のポップアップに項目を並べ、変えたらすぐ保存する。

use std::fs;
use std::path::PathBuf;

use bevy::prelude::*;
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::menu::{PopupBody, spawn_menu};
use crate::scene::LabelFont;

/// リストの行の高さの範囲と刻み（ピクセル）。
const ROW_HEIGHT_MIN: f32 = 24.0;
const ROW_HEIGHT_MAX: f32 = 96.0;
const ROW_HEIGHT_STEP: f32 = 4.0;

const TEXT_COLOR: Color = Color::srgb(0.9, 0.9, 0.92);
const BUTTON_COLOR: Color = Color::srgb(0.22, 0.22, 0.27);

#[derive(Resource, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct Settings {
    /// リストの行の高さ。そのフォルダが画面いっぱいのときのピクセル数。
    pub row_height_px: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            row_height_px: 40.0,
        }
    }
}

pub struct SettingsPlugin;

impl Plugin for SettingsPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(load())
            .add_systems(PostStartup, spawn_settings_ui.after(spawn_menu))
            .add_systems(Update, (save_on_change, show_values));
    }
}

fn settings_path() -> Option<PathBuf> {
    ProjectDirs::from("", "", "Lisa").map(|dirs| dirs.config_dir().join("settings.json"))
}

/// 保存した設定を読む。無いか読めなければ既定値。
fn load() -> Settings {
    let Some(path) = settings_path() else {
        return Settings::default();
    };
    let mut settings: Settings = fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    settings.row_height_px = settings.row_height_px.clamp(ROW_HEIGHT_MIN, ROW_HEIGHT_MAX);
    settings
}

fn save_on_change(settings: Res<Settings>) {
    if !settings.is_changed() || settings.is_added() {
        return;
    }
    let Some(path) = settings_path() else {
        return;
    };
    let result = path
        .parent()
        .map_or(Ok(()), fs::create_dir_all)
        .and_then(|_| {
            let text = serde_json::to_string_pretty(&*settings).map_err(std::io::Error::other)?;
            fs::write(&path, text)
        });
    if let Err(error) = result {
        warn!("設定を保存できませんでした（{}）: {error}", path.display());
    }
}

/// 今の値を表示する文字。
#[derive(Component)]
struct RowHeightValue;

fn spawn_settings_ui(
    mut commands: Commands,
    body: Single<Entity, With<PopupBody>>,
    font: Res<LabelFont>,
) {
    let text_font = TextFont::from_font_size(14.0).with_font(font.0.clone());
    let button = |label: &str| {
        (
            Node {
                width: Val::Px(28.0),
                height: Val::Px(24.0),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                border_radius: BorderRadius::all(Val::Px(4.0)),
                ..default()
            },
            BackgroundColor(BUTTON_COLOR),
            children![(Text::new(label), text_font.clone(), TextColor(TEXT_COLOR))],
        )
    };

    commands.entity(*body).with_children(|body| {
        body.spawn(Node {
            align_items: AlignItems::Center,
            column_gap: Val::Px(8.0),
            ..default()
        })
        .with_children(|row| {
            row.spawn((
                Text::new("リストの行の高さ"),
                text_font.clone(),
                TextColor(TEXT_COLOR),
                Node {
                    flex_grow: 1.0,
                    ..default()
                },
            ));
            row.spawn(button("−")).observe(
                |_: On<PointerClick>, mut settings: ResMut<Settings>| {
                    change_row_height(&mut settings, -ROW_HEIGHT_STEP);
                },
            );
            row.spawn((
                RowHeightValue,
                Text::new(""),
                text_font.clone(),
                TextColor(TEXT_COLOR),
                Node {
                    width: Val::Px(56.0),
                    justify_content: JustifyContent::Center,
                    ..default()
                },
                TextLayout::justify(Justify::Center),
            ));
            row.spawn(button("+")).observe(
                |_: On<PointerClick>, mut settings: ResMut<Settings>| {
                    change_row_height(&mut settings, ROW_HEIGHT_STEP);
                },
            );
        });
    });
}

fn change_row_height(settings: &mut Settings, delta: f32) {
    let value = (settings.row_height_px + delta).clamp(ROW_HEIGHT_MIN, ROW_HEIGHT_MAX);
    if value != settings.row_height_px {
        settings.row_height_px = value;
    }
}

fn show_values(
    settings: Res<Settings>,
    mut text: Single<&mut Text, With<RowHeightValue>>,
    mut shown: Local<bool>,
) {
    if !settings.is_changed() && *shown {
        return;
    }
    *shown = true;
    text.0 = format!("{} px", settings.row_height_px);
}
