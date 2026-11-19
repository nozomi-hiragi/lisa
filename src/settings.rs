//! 設定。OS の設定フォルダの settings.json に保存し、起動時に読み込む。
//! 設定のポップアップに項目を並べ、変えたらすぐ保存する。

use std::fs;
use std::path::PathBuf;

use bevy::prelude::*;
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::render::scene::LabelFont;
use crate::ui::menu::{PopupBody, spawn_menu};

const TEXT_COLOR: Color = Color::srgb(0.9, 0.9, 0.92);
const BUTTON_COLOR: Color = Color::srgb(0.22, 0.22, 0.27);

#[derive(Resource, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct Settings {
    /// 開いたフォルダの耳の高さ（画面上のピクセル数）。
    pub ear_selected_px: f32,
    /// 一覧の中のフォルダの耳の高さ（その一覧のフォルダを開いたときの、画面上のピクセル数）。
    pub ear_preview_px: f32,
    /// 裏で読む・ハッシュする速さの上限（MB 毎秒、0 なら上限なし）。見ているところは除く。
    pub background_mb_per_sec: f32,
    /// 電池で動いているときは、裏の分を休む（1 なら休む、0 なら休まない）。
    pub rest_on_battery: f32,
    /// CPU の混み具合（%）がこれを超えたら、裏の分を休む（100 なら休まない）。
    pub rest_cpu_percent: f32,
    /// ほかのアプリのディスクの読み書き（MB 毎秒）がこれを超えたら、裏の分を休む（0 なら休まない）。
    pub rest_disk_mb_per_sec: f32,
    /// LAN の他の PC にこの端末の DB を公開する（1 なら公開する、0 なら公開しない）。
    pub lan_share: f32,
    /// カタログを読むフォルダを足す（後のものほど優先。`docs/catalog.md`）。ポップアップには出さない。
    pub catalog_dirs: Vec<String>,
    /// この型（ID）の中は、重複の一覧に出さない。ポップアップには出さない。
    pub catalog_hide_duplicates: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            ear_selected_px: 28.0,
            ear_preview_px: 16.0,
            background_mb_per_sec: 200.0,
            rest_on_battery: 1.0,
            rest_cpu_percent: 80.0,
            rest_disk_mb_per_sec: 100.0,
            lan_share: 0.0,
            catalog_dirs: Vec::new(),
            catalog_hide_duplicates: Vec::new(),
        }
    }
}

impl Settings {
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
    EarSelected,
    EarPreview,
    BackgroundSpeed,
    RestOnBattery,
    RestCpu,
    RestDisk,
    LanShare,
}

impl Item {
    const ALL: [Item; 7] = [
        Item::EarSelected,
        Item::EarPreview,
        Item::BackgroundSpeed,
        Item::RestOnBattery,
        Item::RestCpu,
        Item::RestDisk,
        Item::LanShare,
    ];

    fn label(self) -> &'static str {
        match self {
            Item::EarSelected => "耳の高さ（開いたとき）",
            Item::EarPreview => "耳の高さ（一覧の中）",
            Item::BackgroundSpeed => "裏で読む速さの上限",
            Item::RestOnBattery => "電池のときは裏で読まない",
            Item::RestCpu => "CPU が混んだら裏で読まない",
            Item::RestDisk => "ディスクが混んだら裏で読まない",
            Item::LanShare => "LAN に公開",
        }
    }

    /// 最小、最大、刻み。
    fn range(self) -> (f32, f32, f32) {
        match self {
            Item::EarSelected => (12.0, 64.0, 2.0),
            Item::EarPreview => (6.0, 48.0, 2.0),
            Item::BackgroundSpeed => (0.0, 2000.0, 50.0),
            Item::RestOnBattery => (0.0, 1.0, 1.0),
            Item::RestCpu => (20.0, 100.0, 10.0),
            Item::RestDisk => (0.0, 1000.0, 25.0),
            Item::LanShare => (0.0, 1.0, 1.0),
        }
    }

    fn get(self, settings: &Settings) -> f32 {
        match self {
            Item::EarSelected => settings.ear_selected_px,
            Item::EarPreview => settings.ear_preview_px,
            Item::BackgroundSpeed => settings.background_mb_per_sec,
            Item::RestOnBattery => settings.rest_on_battery,
            Item::RestCpu => settings.rest_cpu_percent,
            Item::RestDisk => settings.rest_disk_mb_per_sec,
            Item::LanShare => settings.lan_share,
        }
    }

    fn set(self, settings: &mut Settings, value: f32) {
        let (min, max, _) = self.range();
        let value = value.clamp(min, max);
        match self {
            Item::EarSelected => settings.ear_selected_px = value,
            Item::EarPreview => settings.ear_preview_px = value,
            Item::BackgroundSpeed => settings.background_mb_per_sec = value,
            Item::RestOnBattery => settings.rest_on_battery = value.round(),
            Item::RestCpu => settings.rest_cpu_percent = value,
            Item::RestDisk => settings.rest_disk_mb_per_sec = value,
            Item::LanShare => settings.lan_share = value.round(),
        }
    }

    fn show(self, settings: &Settings) -> String {
        match self {
            Item::EarSelected => format!("{} px", settings.ear_selected_px),
            Item::EarPreview => format!("{} px", settings.ear_preview_px),
            Item::BackgroundSpeed if settings.background_mb_per_sec <= 0.0 => "上限なし".into(),
            Item::BackgroundSpeed => format!("{} MB/秒", settings.background_mb_per_sec),
            Item::RestOnBattery if settings.rest_on_battery >= 0.5 => "休む".into(),
            Item::RestOnBattery => "休まない".into(),
            Item::RestCpu if settings.rest_cpu_percent >= 100.0 => "休まない".into(),
            Item::RestCpu => format!("{} % 超", settings.rest_cpu_percent),
            Item::RestDisk if settings.rest_disk_mb_per_sec <= 0.0 => "休まない".into(),
            Item::RestDisk => format!("{} MB/秒超", settings.rest_disk_mb_per_sec),
            Item::LanShare if settings.lan_share >= 0.5 => "公開する".into(),
            Item::LanShare => "しない".into(),
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

pub(crate) fn settings_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("LISA_DATA_DIR").filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(dir).join("settings.json"));
    }
    ProjectDirs::from("", "", "Lisa").map(|dirs| dirs.config_dir().join("settings.json"))
}

/// 保存した設定を読む。無いか読めなければ既定値。項目が足りなければその項目だけ既定値。
pub fn load() -> Settings {
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
                        width: Val::Px(88.0),
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
        // 今は使わない項目（行の高さ、しきい値など）が入った古い settings.json。
        // 足りない項目は既定値になる。
        let settings: Settings = serde_json::from_str(
            r#"{ "row_height_px": 48.0, "snap_in_percent": 40.0, "ear_preview_px": 20.0 }"#,
        )
        .unwrap();
        assert_eq!(settings.ear_preview_px, 20.0);
        assert_eq!(
            settings.ear_selected_px,
            Settings::default().ear_selected_px
        );
    }

    #[test]
    fn out_of_range_values_are_clamped() {
        let settings: Settings =
            serde_json::from_str(r#"{ "ear_selected_px": 500.0, "ear_preview_px": 1.0 }"#).unwrap();
        let settings = settings.sanitized();
        assert_eq!(settings.ear_selected_px, 64.0);
        assert_eq!(settings.ear_preview_px, 6.0);
    }
}
