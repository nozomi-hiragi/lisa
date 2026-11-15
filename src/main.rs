#[cfg(debug_assertions)]
mod debug;
mod input;
mod layout;
mod model;
mod render;
mod settings;
mod ui;
mod view;

use bevy::log::{DEFAULT_FILTER, LogPlugin};
use bevy::prelude::*;

fn main() {
    let mut app = App::new();
    app.add_plugins(
        DefaultPlugins
            .set(WindowPlugin {
                primary_window: Some(Window {
                    title: "Lisa — Root".into(),
                    ..default()
                }),
                ..default()
            })
            .set(LogPlugin {
                // TODO: 日本語・中国語の改行位置データ（ICU4X のセグメンテーションモデル）を同梱して
                // いないため、その警告（icu_provider::error）を止めている。ラベルを折り返すように
                // なったら、Bevy の `complex_script_segmentation` 機能でデータを入れ、この絞り込みと
                // Cargo.toml の icu_provider の logging 機能を外す（README の「既知の課題」を参照）。
                filter: format!("{DEFAULT_FILTER},icu_provider=off"),
                ..default()
            }),
    )
    .add_plugins((
        settings::SettingsPlugin,
        view::TreeViewPlugin,
        ui::menu::MenuPlugin,
    ));
    #[cfg(debug_assertions)]
    app.add_plugins(debug::DebugTitlePlugin);
    app.run();
}
