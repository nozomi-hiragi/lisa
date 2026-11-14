//! 設定。OS の設定フォルダの settings.json に保存し、起動時に読み込む。
//! 設定のポップアップに項目を並べ、変えたらすぐ保存する。

use std::fs;
use std::path::PathBuf;

use bevy::prelude::*;
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::menu::{PopupBody, spawn_menu};
use crate::scene::LabelFont;

const TEXT_COLOR: Color = Color::srgb(0.9, 0.9, 0.92);
const BUTTON_COLOR: Color = Color::srgb(0.22, 0.22, 0.27);

#[derive(Resource, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct Settings {
    /// リストの行の高さ。そのフォルダが画面いっぱいのときのピクセル数。
    pub row_height_px: f32,
    /// ズームして、子の枠が画面の幅のこの割合（%）以上になったら子へ移る。
    pub snap_in_percent: f32,
    /// ズームアウトして、今の枠が画面の幅のこの割合（%）より小さくなったら親へ戻る。
    pub snap_out_percent: f32,
    /// 勢いで移る感度（0 で切る、1〜10 で大きいほど敏感）。
    pub flick_level: u8,
    /// 上下に動かして、隣のフォルダが画面の高さのこの割合（%）以上見えたら、そのフォルダへ移る。
    pub sibling_percent: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            row_height_px: 40.0,
            snap_in_percent: 35.0,
            snap_out_percent: 85.0,
            flick_level: 3,
            sibling_percent: 30.0,
        }
    }
}

impl Settings {
    /// 勢いで移る速さのしきい値（倍率の対数で毎秒）。切っていれば None。
    pub fn flick_rate(&self) -> Option<f32> {
        (self.flick_level > 0).then(|| 9.0 / self.flick_level as f32)
    }

    /// 範囲の外の値を直す（手で書き換えた settings.json などのため）。
    fn sanitized(mut self) -> Self {
        for item in Item::ALL {
            let value = item.get(&self);
            item.set(&mut self, value);
        }
        self
    }
}

/// ポップアップに並べる項目。どれも −/+ で変える。
#[derive(Component, Clone, Copy, PartialEq)]
enum Item {
    RowHeight,
    SnapIn,
    SnapOut,
    Flick,
    Sibling,
}

impl Item {
    const ALL: [Item; 5] = [
        Item::RowHeight,
        Item::SnapIn,
        Item::SnapOut,
        Item::Sibling,
        Item::Flick,
    ];

    fn label(self) -> &'static str {
        match self {
            Item::RowHeight => "リストの行の高さ",
            Item::SnapIn => "子フォルダへ移るしきい値",
            Item::SnapOut => "親へ戻るしきい値",
            Item::Flick => "勢いで移る感度",
            Item::Sibling => "隣のフォルダへ移るしきい値",
        }
    }

    /// 最小、最大、刻み。
    fn range(self) -> (f32, f32, f32) {
        match self {
            Item::RowHeight => (24.0, 96.0, 4.0),
            Item::SnapIn => (10.0, 90.0, 5.0),
            Item::SnapOut => (50.0, 98.0, 1.0),
            Item::Flick => (0.0, 10.0, 1.0),
            Item::Sibling => (10.0, 80.0, 5.0),
        }
    }

    fn get(self, settings: &Settings) -> f32 {
        match self {
            Item::RowHeight => settings.row_height_px,
            Item::SnapIn => settings.snap_in_percent,
            Item::SnapOut => settings.snap_out_percent,
            Item::Flick => settings.flick_level as f32,
            Item::Sibling => settings.sibling_percent,
        }
    }

    fn set(self, settings: &mut Settings, value: f32) {
        let (min, max, _) = self.range();
        let value = value.clamp(min, max);
        match self {
            Item::RowHeight => settings.row_height_px = value,
            Item::SnapIn => settings.snap_in_percent = value,
            Item::SnapOut => settings.snap_out_percent = value,
            Item::Flick => settings.flick_level = value.round() as u8,
            Item::Sibling => settings.sibling_percent = value,
        }
    }

    fn show(self, settings: &Settings) -> String {
        match self {
            Item::RowHeight => format!("{} px", settings.row_height_px),
            Item::SnapIn => format!("{} %", settings.snap_in_percent),
            Item::SnapOut => format!("{} %", settings.snap_out_percent),
            Item::Flick if settings.flick_level == 0 => "切".to_string(),
            Item::Flick => format!("{}", settings.flick_level),
            Item::Sibling => format!("{} %", settings.sibling_percent),
        }
    }

    /// −/+ を押したとき。値が変わるときだけ書き換える（変わらなければ保存もしない）。
    fn step(self, settings: &mut Mut<Settings>, direction: f32) {
        let (_, _, step) = self.range();
        let mut next = (**settings).clone();
        let value = self.get(&next) + step * direction;
        self.set(&mut next, value);
        if next != **settings {
            **settings = next;
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

/// 保存した設定を読む。無いか読めなければ既定値。項目が足りなければその項目だけ既定値。
fn load() -> Settings {
    let Some(path) = settings_path() else {
        return Settings::default();
    };
    fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<Settings>(&text).ok())
        .unwrap_or_default()
        .sanitized()
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
struct ValueText(Item);

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
        for item in Item::ALL {
            body.spawn(Node {
                align_items: AlignItems::Center,
                column_gap: Val::Px(8.0),
                ..default()
            })
            .with_children(|row| {
                row.spawn((
                    Text::new(item.label()),
                    text_font.clone(),
                    TextColor(TEXT_COLOR),
                    Node {
                        flex_grow: 1.0,
                        margin: UiRect::right(Val::Px(16.0)),
                        ..default()
                    },
                ));
                row.spawn(button("−")).observe(
                    move |_: On<PointerClick>, mut settings: ResMut<Settings>| {
                        item.step(&mut settings.reborrow(), -1.0);
                    },
                );
                row.spawn((
                    ValueText(item),
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
                    move |_: On<PointerClick>, mut settings: ResMut<Settings>| {
                        item.step(&mut settings.reborrow(), 1.0);
                    },
                );
            });
        }
    });
}

fn show_values(
    settings: Res<Settings>,
    mut texts: Query<(&ValueText, &mut Text)>,
    mut shown: Local<bool>,
) {
    if !settings.is_changed() && *shown {
        return;
    }
    *shown = true;
    for (value, mut text) in &mut texts {
        text.0 = value.0.show(&settings);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_settings_file_still_loads() {
        // 行の高さしか無かった頃の settings.json。足りない項目は既定値になる。
        let settings: Settings = serde_json::from_str(r#"{ "row_height_px": 48.0 }"#).unwrap();
        assert_eq!(settings.row_height_px, 48.0);
        assert_eq!(
            settings.snap_in_percent,
            Settings::default().snap_in_percent
        );
        assert_eq!(
            settings.snap_out_percent,
            Settings::default().snap_out_percent
        );
        assert_eq!(settings.flick_level, Settings::default().flick_level);
        assert_eq!(
            settings.sibling_percent,
            Settings::default().sibling_percent
        );
    }

    #[test]
    fn out_of_range_values_are_clamped() {
        let settings: Settings =
            serde_json::from_str(r#"{ "row_height_px": 500.0, "snap_in_percent": 1.0 }"#).unwrap();
        let settings = settings.sanitized();
        assert_eq!(settings.row_height_px, 96.0);
        assert_eq!(settings.snap_in_percent, 10.0);
    }
}
