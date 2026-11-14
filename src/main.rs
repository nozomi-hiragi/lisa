mod cache;
#[cfg(debug_assertions)]
mod debug;
mod layout;
mod preview;
mod scene;
mod shapes;
mod tree;
mod view;

use bevy::prelude::*;

fn main() {
    let mut app = App::new();
    app.add_plugins(DefaultPlugins.set(WindowPlugin {
        primary_window: Some(Window {
            title: "Lisa — Root".into(),
            ..default()
        }),
        ..default()
    }))
    .add_plugins(view::TreeViewPlugin);
    #[cfg(debug_assertions)]
    app.add_plugins(debug::DebugTitlePlugin);
    app.run();
}
