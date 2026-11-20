//! 右クリックから開く名前変更と詳細のダイアログ。

use std::path::PathBuf;
use std::sync::{Mutex, mpsc};

use bevy::clipboard::Clipboard;
use bevy::input_focus::{FocusCause, InputFocus};
use bevy::prelude::*;
use bevy::text::{EditableText, LineBreak, TextCursorStyle, TextReadWriteMode};
use bevy::ui_widgets::TextInput;
use bevy::window::RequestRedraw;
use bevy::winit::EventLoopProxyWrapper;

use crate::model::tree::FileTree;
use crate::organize::OrganizeState;
use crate::render::{palette, scene::LabelFont};
use crate::scan::Store;

use super::context_menu::{ContextAction, ContextCommand, availability};
use super::menu::MenuSystems;
use super::tooltip::{self, FolderTotalDisplay, Row, TotalMessage, TotalPlan};

pub(super) struct DialogPlugin;

impl Plugin for DialogPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<DialogState>()
            .add_message::<DialogAction>()
            .add_systems(First, |mut dialog: ResMut<DialogState>| {
                if dialog.just_closed {
                    dialog.just_closed = false;
                }
            })
            .add_systems(
                Update,
                (update_dialog, open_dialog).chain().in_set(MenuSystems),
            );
    }
}

#[derive(Resource, Default)]
pub(crate) struct DialogState {
    open: Option<OpenDialog>,
    just_closed: bool,
}

impl DialogState {
    pub(crate) fn busy(&self) -> bool {
        self.open.is_some() || self.just_closed
    }
}

struct OpenDialog {
    backdrop: Entity,
    content: Content,
}

enum Content {
    Rename {
        from: PathBuf,
        input: Entity,
        error: Entity,
    },
    Details {
        path: Option<String>,
        rows: Entity,
        pending: Option<PendingTotal>,
    },
}

struct PendingTotal {
    receiver: Mutex<mpsc::Receiver<TotalMessage>>,
    rows: Vec<Row>,
    loaded_bytes: u64,
}

#[derive(Message, Clone, Copy, PartialEq, Eq)]
enum DialogAction {
    Close,
    Rename,
    CopyPath,
}

#[allow(clippy::too_many_arguments)]
fn open_dialog(
    mut commands: Commands,
    mut messages: MessageReader<ContextCommand>,
    mut state: ResMut<DialogState>,
    mut focus: ResMut<InputFocus>,
    font: Res<LabelFont>,
    tree: Res<FileTree>,
    store: Option<Res<Store>>,
    lan: Option<Res<crate::lan::LanState>>,
    proxy: Option<Res<EventLoopProxyWrapper>>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    for command in messages.read() {
        if state.busy() {
            continue;
        }
        let (title, width) = match command.action {
            ContextAction::Rename => ("名前の変更", 440.),
            ContextAction::Details => ("詳細", 520.),
            _ => continue,
        };
        let target = &command.target;
        if !availability(command.action, target) {
            continue;
        }
        let Some(node) = tree.nodes.get(target.node) else {
            continue;
        };
        let (backdrop, panel) = frame(&mut commands, &font, title, width);
        let content = if command.action == ContextAction::Rename {
            let from = target
                .local_path
                .as_ref()
                .expect("available rename has a path");
            let name = from.file_name().unwrap_or_default().to_string_lossy();
            let input = commands
                .spawn((
                    Node {
                        height: Val::Px(36.),
                        flex_shrink: 0.,
                        overflow: Overflow::clip(),
                        border: UiRect::all(Val::Px(1.)),
                        border_radius: BorderRadius::all(Val::Px(4.)),
                        padding: UiRect::all(Val::Px(8.)),
                        ..default()
                    },
                    BackgroundColor(palette::BASE_200),
                    BorderColor::all(palette::PRIMARY),
                    TextInput,
                    EditableText::new(name),
                    TextCursorStyle::default(),
                    TextLayout::no_wrap(),
                    TextFont {
                        font: font.0.clone().into(),
                        font_size: 14.0.into(),
                        ..default()
                    },
                    TextColor(palette::BASE_CONTENT),
                    ChildOf(panel),
                ))
                .id();
            focus.set(input, FocusCause::Navigated);
            let error = label(&mut commands, panel, &font, "", 12., palette::ACCENT);
            let buttons = button_row(&mut commands, panel);
            button(
                &mut commands,
                buttons,
                &font,
                "キャンセル",
                DialogAction::Close,
                false,
            );
            button(
                &mut commands,
                buttons,
                &font,
                "変更",
                DialogAction::Rename,
                true,
            );
            Content::Rename {
                from: from.clone(),
                input,
                error,
            }
        } else {
            let plan = tooltip::resolve_total_target(
                &tree,
                store.as_deref(),
                lan.as_deref(),
                crate::scan::db_path(),
                target.node,
            );
            let (display, total) = match plan {
                TotalPlan::NotNeeded if !node.is_dir => (None, None),
                TotalPlan::Fetch(target_total) => {
                    let (tx, rx) = mpsc::channel();
                    let proxy = proxy.as_deref().map(|p| (**p).clone());
                    if tooltip::spawn_total_worker(target.node, 0, target_total, tx, proxy).is_ok()
                    {
                        (Some(FolderTotalDisplay::Computing), Some(Mutex::new(rx)))
                    } else {
                        (Some(FolderTotalDisplay::LoadedOnly), None)
                    }
                }
                _ => (Some(FolderTotalDisplay::LoadedOnly), None),
            };
            let rows = commands
                .spawn((
                    Node {
                        flex_direction: FlexDirection::Column,
                        row_gap: Val::Px(6.),
                        overflow: Overflow::scroll_y(),
                        min_height: Val::Px(0.),
                        ..default()
                    },
                    ScrollPosition::default(),
                    ChildOf(panel),
                ))
                .id();
            let metadata = tooltip::describe_details(&tree, store.as_deref(), target.node, display);
            show_rows(&mut commands, rows, &font, &metadata);
            let buttons = button_row(&mut commands, panel);
            let path = target.path_text.clone();
            if path.is_some() {
                button(
                    &mut commands,
                    buttons,
                    &font,
                    "パスをコピー",
                    DialogAction::CopyPath,
                    false,
                );
            }
            button(
                &mut commands,
                buttons,
                &font,
                "閉じる",
                DialogAction::Close,
                true,
            );
            Content::Details {
                path,
                rows,
                pending: total.map(|receiver| PendingTotal {
                    receiver,
                    rows: metadata,
                    loaded_bytes: node.size,
                }),
            }
        };
        state.open = Some(OpenDialog { backdrop, content });
        redraw.write(RequestRedraw);
    }
}

#[allow(clippy::too_many_arguments)]
fn update_dialog(
    mut commands: Commands,
    mut actions: MessageReader<DialogAction>,
    mut wheel: MessageReader<bevy::input::mouse::MouseWheel>,
    keys: Res<ButtonInput<KeyCode>>,
    mut state: ResMut<DialogState>,
    mut focus: ResMut<InputFocus>,
    edits: Query<&EditableText>,
    mut texts: Query<&mut Text>,
    mut scroll: Query<(&ComputedNode, &mut ScrollPosition)>,
    mut clipboard: Option<ResMut<Clipboard>>,
    mut organize: Option<ResMut<OrganizeState>>,
    font: Res<LabelFont>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    let mut action = actions.read().last().copied();
    let dy = wheel
        .read()
        .map(|event| {
            use bevy::input::mouse::MouseScrollUnit;
            event.y
                * if event.unit == MouseScrollUnit::Line {
                    24.
                } else {
                    1.
                }
        })
        .sum::<f32>();
    let Some(dialog) = state.open.as_mut() else {
        return;
    };
    let close = if let Content::Rename { from, input, error } = &dialog.content {
        let Ok(edit) = edits.get(*input) else {
            return;
        };
        if !edit.is_composing() {
            if keys.just_pressed(KeyCode::Escape) {
                action = Some(DialogAction::Close);
            } else if keys.just_pressed(KeyCode::Enter) || keys.just_pressed(KeyCode::NumpadEnter) {
                action = Some(DialogAction::Rename);
            }
        }
        match action {
            Some(DialogAction::Close) => true,
            Some(DialogAction::Rename) => {
                let name = edit.value().to_string();
                let result = validate_rename(from, &name).and_then(|changed| {
                    if changed {
                        let organize = organize.as_mut().ok_or("今は名前を変更できません")?;
                        organize.user_rename(from.clone(), name)?;
                    }
                    Ok(())
                });
                match result {
                    Ok(()) => true,
                    Err(message) => {
                        if let Ok(mut text) = texts.get_mut(*error) {
                            **text = message;
                        }
                        redraw.write(RequestRedraw);
                        false
                    }
                }
            }
            _ => false,
        }
    } else if let Content::Details {
        path,
        rows,
        pending,
    } = &mut dialog.content
    {
        if let Some(total) = pending.as_mut()
            && let Ok(receiver) = total.receiver.get_mut()
            && let Ok(msg) = receiver.try_recv()
        {
            if let Some(index) = total.rows.iter().position(|r| r.label == "大きさ") {
                let display = msg.outcome.into_display();
                total.rows.splice(
                    index..=index,
                    tooltip::folder_rows(Some(display), total.loaded_bytes),
                );
            }
            commands.entity(*rows).despawn_children();
            show_rows(&mut commands, *rows, &font, &total.rows);
            *pending = None;
            focus.clear();
            redraw.write(RequestRedraw);
        }
        if dy != 0.
            && let Ok((node, mut position)) = scroll.get_mut(*rows)
        {
            let max = (node.content_size().y - node.size().y).max(0.) * node.inverse_scale_factor();
            position.y = (position.y - dy).clamp(0., max);
            redraw.write(RequestRedraw);
        }
        if action == Some(DialogAction::CopyPath)
            && let (Some(path), Some(clipboard)) = (path, clipboard.as_mut())
            && let Err(error) = clipboard.set_text(path.clone())
        {
            warn!("パスをコピーできませんでした: {error:?}");
        }
        keys.just_pressed(KeyCode::Escape) || action == Some(DialogAction::Close)
    } else {
        false
    };
    if close {
        commands.entity(dialog.backdrop).despawn();
        state.open = None;
        state.just_closed = true;
        focus.clear();
        redraw.write(RequestRedraw);
    }
}

fn validate_rename(from: &std::path::Path, name: &str) -> Result<bool, String> {
    if from
        .file_name()
        .is_some_and(|old| old.to_string_lossy() == name)
    {
        return Ok(false);
    }
    let to = lisa_core::organize::rename_destination(from, name)?;
    match lisa_core::organize::move_destination_exists(from, &to) {
        Ok(true) => Err("同じ名前のファイルやフォルダがすでにあります".into()),
        Ok(false) => Ok(true),
        Err(error) => Err(format!("変更先を確認できません: {error}")),
    }
}

fn frame(commands: &mut Commands, font: &LabelFont, title: &str, width: f32) -> (Entity, Entity) {
    let backdrop = commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                width: Val::Percent(100.),
                height: Val::Percent(100.),
                align_items: AlignItems::Center,
                justify_content: JustifyContent::Center,
                ..default()
            },
            GlobalZIndex(20),
            BackgroundColor(palette::BASE_300.with_alpha(0.65)),
        ))
        .observe(
            |event: On<PointerClick>, mut actions: MessageWriter<DialogAction>| {
                if event.button == bevy::picking::pointer::PointerButton::Primary {
                    actions.write(DialogAction::Close);
                }
            },
        )
        .id();
    let panel = commands
        .spawn((
            Node {
                width: Val::Px(width),
                max_width: Val::Percent(95.),
                max_height: Val::Percent(95.),
                padding: UiRect::all(Val::Px(20.)),
                border: UiRect::all(Val::Px(1.)),
                border_radius: BorderRadius::all(Val::Px(6.)),
                flex_direction: FlexDirection::Column,
                row_gap: Val::Px(10.),
                ..default()
            },
            BackgroundColor(palette::BASE_100),
            BorderColor::all(palette::NEUTRAL),
            ChildOf(backdrop),
        ))
        .observe(|mut event: On<PointerClick>| event.propagate(false))
        .id();
    label(commands, panel, font, title, 16., palette::BASE_CONTENT);
    (backdrop, panel)
}

fn button_row(commands: &mut Commands, panel: Entity) -> Entity {
    commands
        .spawn((
            Node {
                align_items: AlignItems::Center,
                justify_content: JustifyContent::FlexEnd,
                column_gap: Val::Px(8.),
                flex_shrink: 0.,
                ..default()
            },
            ChildOf(panel),
        ))
        .id()
}

fn button(
    commands: &mut Commands,
    parent: Entity,
    font: &LabelFont,
    title: &str,
    action: DialogAction,
    primary: bool,
) {
    let entity = commands
        .spawn((
            Node {
                padding: UiRect::axes(Val::Px(if primary { 14. } else { 10. }), Val::Px(4.)),
                border_radius: BorderRadius::all(Val::Px(4.)),
                ..default()
            },
            BackgroundColor(if primary {
                palette::PRIMARY
            } else {
                palette::NEUTRAL
            }),
            ChildOf(parent),
        ))
        .observe(
            move |mut event: On<PointerClick>, mut actions: MessageWriter<DialogAction>| {
                event.propagate(false);
                if event.button == bevy::picking::pointer::PointerButton::Primary {
                    actions.write(action);
                }
            },
        )
        .id();
    label(
        commands,
        entity,
        font,
        title,
        14.,
        if primary {
            palette::BASE_100
        } else {
            palette::BASE_CONTENT
        },
    );
}

fn show_rows(commands: &mut Commands, parent: Entity, font: &LabelFont, rows: &[Row]) {
    for row in rows {
        let line = commands
            .spawn((
                Node {
                    column_gap: Val::Px(10.),
                    flex_shrink: 0.,
                    ..default()
                },
                ChildOf(parent),
                Pickable::IGNORE,
            ))
            .id();
        let key = label(commands, line, font, row.label, 13., palette::MUTED);
        commands.entity(key).insert(Node {
            width: Val::Px(76.),
            flex_shrink: 0.,
            ..default()
        });
        let value = label(commands, line, font, &row.value, 13., palette::BASE_CONTENT);
        commands.entity(value).insert((
            Node {
                min_width: Val::Px(0.),
                flex_grow: 1.,
                ..default()
            },
            TextLayout::linebreak(LineBreak::AnyCharacter),
        ));
        if row.label == "名前" {
            // Text で折り返し後の高さを計り、同じ領域に選択できる読み取り専用入力を重ねる。
            let field = commands
                .spawn((
                    Node {
                        min_width: Val::Px(0.),
                        flex_grow: 1.,
                        ..default()
                    },
                    ChildOf(line),
                ))
                .id();
            commands
                .entity(value)
                .insert((ChildOf(field), Visibility::Hidden));
            commands.spawn((
                Node {
                    position_type: PositionType::Absolute,
                    width: Val::Percent(100.),
                    height: Val::Percent(100.),
                    ..default()
                },
                TextInput,
                EditableText::new(&row.value),
                TextReadWriteMode::ReadOnly,
                TextFont::from_font_size(13.).with_font(font.0.clone()),
                TextColor(palette::BASE_CONTENT),
                TextLayout::linebreak(LineBreak::AnyCharacter),
                ChildOf(field),
            ));
        }
    }
}

fn label(
    commands: &mut Commands,
    parent: Entity,
    font: &LabelFont,
    text: &str,
    size: f32,
    color: Color,
) -> Entity {
    commands
        .spawn((
            Text::new(text),
            TextFont {
                font: font.0.clone().into(),
                font_size: size.into(),
                ..default()
            },
            TextColor(color),
            Pickable::IGNORE,
            ChildOf(parent),
        ))
        .id()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::context_menu::Target;

    #[test]
    fn rename_prefills_and_keeps_the_modal_open_on_validation_errors() {
        let mut app = App::new();
        app.insert_resource(FileTree::dummy())
            .insert_resource(LabelFont(Handle::default()))
            .init_resource::<ButtonInput<KeyCode>>()
            .init_resource::<InputFocus>()
            .add_message::<bevy::input::mouse::MouseWheel>()
            .add_message::<RequestRedraw>()
            .add_message::<ContextCommand>()
            .add_plugins(DialogPlugin);
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let target = Target {
            node: FileTree::ROOT,
            is_open: false,
            at_root: false,
            is_dir: false,
            local_path: Some(path),
            local_movable: true,
            cut_source: None,
            path_text: None,
        };
        let open = ContextCommand {
            action: ContextAction::Rename,
            target,
            world: Vec2::ZERO,
        };
        app.world_mut().write_message(open.clone());
        // メニュー選択に使った Enter は、新しく開いたモーダルには渡さない。
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::Enter);
        app.update();
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .reset(KeyCode::Enter);
        let entities = |world: &World| {
            let dialog = world.resource::<DialogState>().open.as_ref().unwrap();
            let Content::Rename { input, error, .. } = dialog.content else {
                panic!("名前変更ダイアログが必要です");
            };
            (input, error)
        };
        let (input, error) = entities(app.world());
        assert_eq!(app.world().resource::<InputFocus>().get(), Some(input));
        assert_eq!(
            app.world()
                .get::<EditableText>(input)
                .unwrap()
                .value()
                .to_string(),
            "Cargo.toml"
        );
        for (name, message) in [
            ("", "入力"),
            ("a/b", "使えません"),
            ("Cargo.lock", "すでにあります"),
            ("lisa-unavailable-test.txt", "今は名前を変更できません"),
        ] {
            app.world_mut()
                .entity_mut(input)
                .insert(EditableText::new(name));
            app.world_mut().write_message(DialogAction::Rename);
            app.update();
            assert!(app.world().get::<Text>(error).unwrap().contains(message));
            assert!(app.world().resource::<DialogState>().open.is_some());
        }
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::Escape);
        app.update();
        let state = app.world().resource::<DialogState>();
        assert!(state.open.is_none());
        assert!(state.busy(), "Esc を背面に渡さない");
        assert!(app.world().resource::<InputFocus>().get().is_none());
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .reset(KeyCode::Escape);
        app.world_mut().write_message(open);
        app.update();
        app.world_mut().write_message(DialogAction::Rename);
        app.update();
        assert!(app.world().resource::<DialogState>().open.is_none());
    }
}
