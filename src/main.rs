mod layout;
mod tree;
mod view;

use bevy::prelude::*;

fn main() {
    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "Lisa — Root".into(),
                ..default()
            }),
            ..default()
        }))
        .add_plugins(view::TreeViewPlugin)
        .run();
}
