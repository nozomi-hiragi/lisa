//! 右クリックから開く名前変更と詳細のダイアログ。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, mpsc};

use bevy::clipboard::Clipboard;
use bevy::input_focus::{FocusCause, InputFocus};
use bevy::prelude::*;
use bevy::text::{EditableText, LineBreak, TextCursorStyle, TextReadWriteMode};
use bevy::ui_widgets::TextInput;
use bevy::window::RequestRedraw;
use bevy::winit::{EventLoopProxyWrapper, WinitUserEvent};

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
                (
                    update_dialog,
                    open_dialog,
                    open_link_delete_dialog,
                    show_refresh_error,
                )
                    .chain()
                    .in_set(MenuSystems),
            );
    }
}

#[derive(Resource, Default)]
pub(crate) struct DialogState {
    open: Option<OpenDialog>,
    just_closed: bool,
    link_cache: HashMap<LinkQuery, LinkReply>,
    reported_refresh: u64,
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
    LinkedDelete {
        prompt: crate::organize::LinkedDeletePrompt,
        scroll_area: Entity,
    },
    Rename {
        operation: NameEdit,
        input: Entity,
        error: Entity,
    },
    Notice,
    Details {
        path: Option<String>,
        scroll_area: Entity,
        rows: Entity,
        pending: Option<PendingTotal>,
        marker: Option<Box<MarkerAction>>,
        lookup: Option<LinkLookup>,
    },
}

enum NameEdit {
    Rename(PathBuf),
    Create {
        target: super::context_menu::LocalTarget,
        created: Option<PathBuf>,
        reply: Option<Mutex<mpsc::Receiver<Option<String>>>>,
    },
}

#[derive(Clone, Eq, Hash, PartialEq)]
enum LinkQuery {
    Sources {
        entry: i64,
        device: Option<String>,
        path: String,
    },
    Status {
        link_id: String,
        device: Option<String>,
    },
}

impl LinkQuery {
    fn device(&self) -> Option<&str> {
        match self {
            Self::Sources { device, .. } | Self::Status { device, .. } => device.as_deref(),
        }
    }
}

#[derive(Clone)]
struct LinkReply {
    rows: Vec<Row>,
    checked_at: i64,
}

struct LinkLookup {
    query: LinkQuery,
    receiver: Mutex<mpsc::Receiver<Result<LinkReply, String>>>,
    parent: Entity,
}

struct MarkerAction {
    marker: lisa_core::merged::Marker,
    status: Entity,
    pending: Option<Mutex<mpsc::Receiver<Result<MarkerJump, String>>>>,
}

enum MarkerJump {
    Local(Vec<i64>),
    Remote(crate::lan::RemoteJump),
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
    OpenMarkerTarget,
    DeleteLinkedActual,
    RestoreLink(usize),
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
            ContextAction::NewFolder => ("新規フォルダ", 440.),
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
        if command.action == ContextAction::Rename && tree.marker_view_read_only(target.node) {
            continue;
        }
        let (backdrop, panel) = frame(&mut commands, &font, title, width);
        let content = if matches!(
            command.action,
            ContextAction::Rename | ContextAction::NewFolder
        ) {
            let (operation, name, confirm) = if command.action == ContextAction::NewFolder {
                (
                    NameEdit::Create {
                        target: target.refresh.clone().expect("作成先"),
                        created: None,
                        reply: None,
                    },
                    String::new(),
                    "作成",
                )
            } else {
                let from = target.local_path.clone().expect("名前変更のパス");
                let name = from
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                (NameEdit::Rename(from), name, "変更")
            };
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
                confirm,
                DialogAction::Rename,
                true,
            );
            Content::Rename {
                operation,
                input,
                error,
            }
        } else {
            let plan = if node.merged.is_some() {
                TotalPlan::NotNeeded
            } else {
                tooltip::resolve_total_target(
                    &tree,
                    store.as_deref(),
                    lan.as_deref(),
                    crate::scan::db_path(),
                    target.node,
                )
            };
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
            let scroll_area = commands
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
            let rows = detail_section(&mut commands, scroll_area);
            let lookup = begin_link_lookup(
                &tree,
                target.node,
                lan.as_deref(),
                proxy.as_deref(),
                &mut commands,
                scroll_area,
                &font,
            );
            let metadata = tooltip::describe_details(&tree, store.as_deref(), target.node, display);
            show_rows(&mut commands, rows, &font, &metadata);
            let marker_status = node
                .merged
                .as_ref()
                .map(|_| label(&mut commands, panel, &font, "", 12., palette::ACCENT));
            let buttons = button_row(&mut commands, panel);
            let marker = node
                .merged
                .clone()
                .zip(marker_status)
                .map(|(marker, status)| {
                    button(
                        &mut commands,
                        buttons,
                        &font,
                        "マージ先を開く",
                        DialogAction::OpenMarkerTarget,
                        false,
                    );
                    MarkerAction {
                        marker,
                        status,
                        pending: None,
                    }
                });
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
                scroll_area,
                rows,
                lookup,
                marker: marker.map(Box::new),
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

fn open_link_delete_dialog(
    mut commands: Commands,
    mut state: ResMut<DialogState>,
    mut organize: Option<ResMut<OrganizeState>>,
    font: Res<LabelFont>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    if state.busy() {
        return;
    }
    let Some(prompt) = organize
        .as_mut()
        .and_then(|state| state.linked_delete.take())
    else {
        return;
    };
    let (backdrop, panel) = frame(&mut commands, &font, "リンク元がある実体を削除", 560.);
    let scroll_area = commands
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
    let pending = prompt.links.iter().any(|link| link.sync_state == "pending");
    for (index, link) in prompt.links.iter().enumerate() {
        let source = &link.marker.source;
        let line = format!("{} › {}", source.device_name, source.display_path);
        label(
            &mut commands,
            scroll_area,
            &font,
            &line,
            14.,
            palette::BASE_CONTENT,
        );
        if !pending {
            let restore = button_row(&mut commands, scroll_area);
            button(
                &mut commands,
                restore,
                &font,
                "この場所に戻す",
                DialogAction::RestoreLink(index),
                false,
            );
        }
    }
    if pending {
        label(
            &mut commands,
            panel,
            &font,
            "リンクの照合が未完了です。同期後に操作できます。",
            13.,
            palette::ACCENT,
        );
    } else {
        label(
            &mut commands,
            panel,
            &font,
            &format!(
                "実体を削除すると {} 件のリンクが切れます。",
                prompt.links.len()
            ),
            13.,
            palette::ACCENT,
        );
    }
    let buttons = button_row(&mut commands, panel);
    button(
        &mut commands,
        buttons,
        &font,
        "キャンセル",
        DialogAction::Close,
        true,
    );
    if !pending {
        button(
            &mut commands,
            buttons,
            &font,
            "実体を削除",
            DialogAction::DeleteLinkedActual,
            false,
        );
    }
    state.open = Some(OpenDialog {
        backdrop,
        content: Content::LinkedDelete {
            prompt,
            scroll_area,
        },
    });
    redraw.write(RequestRedraw);
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
    lan: Option<Res<crate::lan::LanState>>,
    proxy: Option<Res<EventLoopProxyWrapper>>,
    scan: Option<Res<crate::scan::ScanControl>>,
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
    let DialogState {
        open, link_cache, ..
    } = &mut *state;
    let Some(dialog) = open.as_mut() else {
        return;
    };
    let close = if let Content::Rename {
        operation,
        input,
        error,
    } = &mut dialog.content
    {
        let completed = if let NameEdit::Create {
            reply: Some(reply), ..
        } = operation
        {
            if action == Some(DialogAction::Close) || keys.just_pressed(KeyCode::Escape) {
                Some(None)
            } else {
                match reply.get_mut().expect("一覧の結果").try_recv() {
                    Ok(result) => Some(result),
                    Err(mpsc::TryRecvError::Disconnected) => {
                        Some(Some("一覧更新が中断されました".into()))
                    }
                    Err(mpsc::TryRecvError::Empty) => return,
                }
            }
        } else {
            None
        };
        if let Some(reason) = &completed {
            if let NameEdit::Create { reply, .. } = operation {
                *reply = None;
            }
            match reason {
                None => action = Some(DialogAction::Close),
                Some(reason) => {
                    if let Ok(mut text) = texts.get_mut(*error) {
                        **text = format!(
                            "フォルダは作成済みです。作成を押すと一覧だけ再試行します。\n{reason}"
                        );
                    }
                    redraw.write(RequestRedraw);
                    return;
                }
            }
        }
        let Ok(edit) = edits.get(*input) else {
            return;
        };
        if completed.is_none() && !edit.is_composing() {
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
                let result = match operation {
                    NameEdit::Rename(from) => validate_rename(from, &name).and_then(|changed| {
                        if changed {
                            let organize = organize.as_mut().ok_or("今は名前を変更できません")?;
                            organize.user_rename(from.clone(), name)?;
                        }
                        Ok(true)
                    }),
                    NameEdit::Create {
                        target,
                        created,
                        reply,
                    } => (|| {
                        if created.is_none() {
                            target.validate()?;
                            *created = Some(create_folder(&target.request.path, &name)?);
                        }
                        let path = created.as_ref().expect("作成済み");
                        let pending = scan
                            .as_ref()
                            .and_then(|scan| {
                                scan.list_created(target.request.clone(), path.clone())
                            })
                            .ok_or("フォルダは作成済みです。作成を押すと一覧だけ再試行します。")?;
                        *reply = Some(Mutex::new(pending));
                        if let Ok(mut text) = texts.get_mut(*error) {
                            **text = "フォルダを作成しました。一覧を確認しています…".into();
                        }
                        redraw.write(RequestRedraw);
                        Ok(false)
                    })(),
                };
                match result {
                    Ok(close) => close,
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
    } else if let Content::LinkedDelete {
        prompt,
        scroll_area,
    } = &dialog.content
    {
        if dy != 0.
            && let Ok((node, mut position)) = scroll.get_mut(*scroll_area)
        {
            let max = (node.content_size().y - node.size().y).max(0.) * node.inverse_scale_factor();
            position.y = (position.y - dy).clamp(0., max);
            redraw.write(RequestRedraw);
        }
        use lisa_core::organize::{DeleteChoice, DeleteRequest};
        let choice = match action {
            Some(DialogAction::RestoreLink(index)) => {
                prompt.links.get(index).map(|link| DeleteChoice::Restore {
                    link_id: link.marker.link_id.clone(),
                })
            }
            Some(DialogAction::DeleteLinkedActual) => Some(DeleteChoice::DeleteActual),
            _ => None,
        };
        if let Some(choice) = choice
            && let Some(organize) = organize.as_mut()
        {
            let links = prompt
                .links
                .iter()
                .map(|link| link.marker.link_id.clone())
                .collect();
            organize.confirm_linked_delete(prompt.path.clone(), DeleteRequest { choice, links });
            true
        } else {
            keys.just_pressed(KeyCode::Escape) || action == Some(DialogAction::Close)
        }
    } else if let Content::Details {
        path,
        scroll_area,
        rows,
        pending,
        marker,
        lookup,
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
            && let Ok((node, mut position)) = scroll.get_mut(*scroll_area)
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
        if let Some(active) = lookup.as_mut()
            && let Ok(result) = active
                .receiver
                .get_mut()
                .expect("UI がこの受信口を所有しています")
                .try_recv()
        {
            let device = active.query.device();
            let paired = device.is_none_or(|device| {
                lan.as_deref()
                    .and_then(|lan| lan.saved_peer(device))
                    .is_some()
            });
            let rows = match result {
                _ if !paired => vec![Row {
                    label: "リンク",
                    value: "ペアリングが解除されました".into(),
                }],
                Ok(reply) => {
                    if device.is_some() {
                        if link_cache.len() >= 128
                            && !link_cache.contains_key(&active.query)
                            && let Some(oldest) = link_cache
                                .iter()
                                .min_by_key(|(_, reply)| reply.checked_at)
                                .map(|(key, _)| key.clone())
                        {
                            link_cache.remove(&oldest);
                        }
                        link_cache.insert(active.query.clone(), reply.clone());
                    }
                    reply.rows
                }
                Err(reason) => {
                    if let Some(cached) = device.and_then(|_| link_cache.get(&active.query)) {
                        let mut rows = cached.rows.clone();
                        rows.push(Row {
                            label: "前回確認",
                            value: tooltip::format_local(cached.checked_at),
                        });
                        rows.push(Row {
                            label: "リンク",
                            value: reason,
                        });
                        rows
                    } else {
                        vec![Row {
                            label: "リンク",
                            value: format!("未取得／{reason}"),
                        }]
                    }
                }
            };
            commands.entity(active.parent).despawn_children();
            show_rows(&mut commands, active.parent, &font, &rows);
            *lookup = None;
            redraw.write(RequestRedraw);
        }
        let opened = marker.as_mut().is_some_and(|marker| {
            update_marker_action(
                marker,
                action == Some(DialogAction::OpenMarkerTarget),
                lan.as_deref(),
                proxy.as_deref(),
                &mut commands,
                &mut texts,
                &mut redraw,
            )
        });
        keys.just_pressed(KeyCode::Escape) || action == Some(DialogAction::Close) || opened
    } else {
        keys.just_pressed(KeyCode::Escape) || action == Some(DialogAction::Close)
    };
    if close {
        commands.entity(dialog.backdrop).despawn();
        state.open = None;
        state.just_closed = true;
        focus.clear();
        redraw.write(RequestRedraw);
    }
}

fn show_refresh_error(
    mut commands: Commands,
    mut state: ResMut<DialogState>,
    mut notice: Option<ResMut<super::context_menu::RefreshNotice>>,
    status: Option<Res<crate::scan::RefreshStatus>>,
    font: Res<LabelFont>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    if state.busy() {
        return;
    }
    let mut error = notice.as_mut().and_then(|notice| notice.error.take());
    if error.is_none()
        && let Some(completion) = status.as_ref().and_then(|status| status.last.as_ref())
        && state.reported_refresh < completion.generation
    {
        state.reported_refresh = completion.generation;
        error = completion
            .error
            .as_ref()
            .map(|error| format!("{}\n{error}", completion.target.path.display()));
    }
    let Some(error) = error else { return };
    let (backdrop, panel) = frame(&mut commands, &font, "更新できませんでした", 440.);
    label(
        &mut commands,
        panel,
        &font,
        &error,
        12.,
        palette::BASE_CONTENT,
    );
    let row = button_row(&mut commands, panel);
    button(
        &mut commands,
        row,
        &font,
        "閉じる",
        DialogAction::Close,
        true,
    );
    state.open = Some(OpenDialog {
        backdrop,
        content: Content::Notice,
    });
    redraw.write(RequestRedraw);
}

fn create_folder(parent: &std::path::Path, name: &str) -> Result<PathBuf, String> {
    lisa_core::organize::validate_entry_name(name)?;
    let path = parent.join(name);
    std::fs::create_dir(&path).map_err(|error| format!("フォルダを作成できません: {error}"))?;
    Ok(path)
}

fn detail_section(commands: &mut Commands, parent: Entity) -> Entity {
    commands
        .spawn((
            Node {
                flex_direction: FlexDirection::Column,
                flex_shrink: 0.,
                row_gap: Val::Px(6.),
                ..default()
            },
            ChildOf(parent),
        ))
        .id()
}

#[allow(clippy::too_many_arguments)]
fn begin_link_lookup(
    tree: &FileTree,
    id: crate::model::tree::NodeId,
    lan: Option<&crate::lan::LanState>,
    proxy: Option<&EventLoopProxyWrapper>,
    commands: &mut Commands,
    parent: Entity,
    font: &LabelFont,
) -> Option<LinkLookup> {
    let node = &tree.nodes[id];
    if node.depth <= 2 {
        return None;
    }
    let device = node.remote.as_ref().map(|remote| remote.device_id.clone());
    let query = if let Some(marker) = &node.merged {
        LinkQuery::Status {
            link_id: marker.link_id.clone(),
            device,
        }
    } else {
        let entry = node
            .remote
            .as_ref()
            .and_then(|r| r.entry_id)
            .or(node.entry)?;
        LinkQuery::Sources {
            entry,
            device,
            path: crate::model::display_path::tree_text(tree, id),
        }
    };
    let peer = query
        .device()
        .and_then(|device| lan.and_then(|lan| lan.saved_peer(device)));
    let requested = query.clone();
    let (tx, receiver) = mpsc::channel();
    let proxy = proxy.map(|proxy| (**proxy).clone());
    std::thread::Builder::new()
        .name("lisa-marker-details".into())
        .spawn(move || {
            let _ = tx.send(link_lookup(&requested, peer.as_ref()));
            if let Some(proxy) = proxy {
                let _ = proxy.send_event(WinitUserEvent::WakeUp);
            }
        })
        .ok()?;
    let rows = detail_section(commands, parent);
    show_rows(
        commands,
        rows,
        font,
        &[Row {
            label: "リンク",
            value: "確認中…".into(),
        }],
    );
    Some(LinkLookup {
        query,
        receiver: Mutex::new(receiver),
        parent: rows,
    })
}

fn link_lookup(
    query: &LinkQuery,
    peer: Option<&crate::lan::SavedPeer>,
) -> Result<LinkReply, String> {
    use lisa_core::merged::links;
    let call = |method: &str, arguments| {
        let peer = peer.ok_or("ペアリングされていません")?;
        crate::lan::call_mcp_tool(&peer.addr, peer.port, &peer.key, method, arguments).map_err(
            |(auth, error)| {
                if auth {
                    format!("{} とのペアリングを確認できません", peer.name)
                } else if error.starts_with("知らない道具") {
                    format!("{} はリンク元の確認に未対応です", peer.name)
                } else {
                    format!("{} に接続できません: {error}", peer.name)
                }
            },
        )
    };
    let local_db = || {
        let path = crate::scan::db_path()
            .filter(|path| path.exists())
            .ok_or("DB を確認できません")?;
        lisa_core::db::Db::open(&path).map_err(|error| error.to_string())
    };
    let (rows, checked_at) = match query {
        LinkQuery::Sources { entry, device, .. } => {
            let (sources, checked_at): (Vec<links::Link>, _) = if device.is_some() {
                let reply = call(
                    "lisa_merged_link_sources",
                    serde_json::json!({ "entry_id": entry }),
                )?;
                (
                    serde_json::from_value(reply["sources"].clone())
                        .map_err(|error| error.to_string())?,
                    reply["checked_at"].as_i64().ok_or("確認時刻がありません")?,
                )
            } else {
                (
                    links::sources_for_entry(&local_db()?, *entry)?,
                    lisa_core::db::now_ms(),
                )
            };
            let mut grouped = std::collections::BTreeMap::<(String, String), Vec<String>>::new();
            for source in sources {
                let place = source.marker.source;
                let status = match source.sync_state.as_str() {
                    "pending" => "（未同期）",
                    "failed" => "（同期できませんでした）",
                    _ => "",
                };
                grouped
                    .entry((place.device_name, place.device_id))
                    .or_default()
                    .push(format!("{}{status}", place.display_path));
            }
            let rows = grouped
                .into_iter()
                .map(|((name, _), paths)| {
                    let value = format!("{name}\n{}", paths.join("\n"));
                    Row {
                        label: "リンク元",
                        value,
                    }
                })
                .collect();
            (rows, checked_at)
        }
        LinkQuery::Status { link_id, device } => {
            let status = if device.is_some() {
                let reply = call(
                    "lisa_merged_link_status",
                    serde_json::json!({ "link_id": link_id }),
                )?;
                reply["status"].as_str().map(str::to_owned)
            } else {
                links::status(&local_db()?, link_id)?
            };
            let rows = match status.as_deref() {
                Some("pending") => vec![Row {
                    label: "リンク同期",
                    value: "未同期".into(),
                }],
                Some("missing") => vec![Row {
                    label: "マージ先",
                    value: "リンク先なし".into(),
                }],
                Some("failed") => vec![Row {
                    label: "リンク同期",
                    value: "同期できませんでした".into(),
                }],
                _ => Vec::new(),
            };
            (rows, lisa_core::db::now_ms())
        }
    };
    Ok(LinkReply { rows, checked_at })
}

fn update_marker_action(
    state: &mut MarkerAction,
    pressed: bool,
    lan: Option<&crate::lan::LanState>,
    proxy: Option<&EventLoopProxyWrapper>,
    commands: &mut Commands,
    texts: &mut Query<&mut Text>,
    redraw: &mut MessageWriter<RequestRedraw>,
) -> bool {
    let mut set_status = |message: String| {
        if let Ok(mut text) = texts.get_mut(state.status) {
            **text = message;
        }
        redraw.write(RequestRedraw);
    };
    if let Some(callback) = &mut state.pending {
        let result = match callback
            .get_mut()
            .expect("UI がこの受信口を所有しています")
            .try_recv()
        {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return false,
            Err(mpsc::TryRecvError::Disconnected) => Err("マージ先の確認が中断されました".into()),
        };
        state.pending = None;
        match result {
            Ok(MarkerJump::Local(chain)) => {
                commands.insert_resource(crate::scan::JumpTo(chain));
                return true;
            }
            Ok(MarkerJump::Remote(jump))
                if lan.and_then(|lan| lan.saved_peer(&jump.device)).is_some() =>
            {
                commands.insert_resource(jump);
                return true;
            }
            Ok(MarkerJump::Remote(_)) => set_status("ペアリングが解除されました".into()),
            Err(error) => set_status(error),
        }
    } else if pressed {
        let marker = state.marker.clone();
        let peer = lan.and_then(|lan| lan.saved_peer(&marker.target.device_id));
        let proxy = proxy.map(|proxy| (**proxy).clone());
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("lisa-marker-target".into())
            .spawn(move || {
                let result = marker_jump(&marker, peer.as_ref());
                let _ = tx.send(result);
                if let Some(proxy) = proxy {
                    let _ = proxy.send_event(WinitUserEvent::WakeUp);
                }
            });
        match thread {
            Ok(_) => {
                state.pending = Some(Mutex::new(rx));
                set_status("マージ先を照合中…".into());
            }
            Err(error) => set_status(format!("マージ先を照合できません: {error}")),
        }
    }
    false
}

fn marker_jump(
    marker: &lisa_core::merged::Marker,
    peer: Option<&crate::lan::SavedPeer>,
) -> Result<MarkerJump, String> {
    match crate::merged::verified_target(marker, peer.cloned())? {
        crate::merged::Target::Local { chain, .. } => Ok(MarkerJump::Local(chain)),
        crate::merged::Target::Remote { peer, chain } => {
            Ok(MarkerJump::Remote(crate::lan::RemoteJump {
                device: peer.device_id,
                chain,
            }))
        }
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
    fn 作成は単一成分で同名のファイルや同時作成を上書きしない() {
        let parent = std::env::temp_dir().join(format!("lisa-new-folder-{}", std::process::id()));
        std::fs::create_dir(&parent).unwrap();
        for invalid in ["", "  ", ".", "..", "/absolute", "a/b", "a\\b"] {
            assert!(create_folder(&parent, invalid).is_err(), "{invalid:?}");
        }
        std::fs::write(parent.join("taken"), b"file").unwrap();
        assert!(create_folder(&parent, "taken").is_err());
        assert_eq!(std::fs::read(parent.join("taken")).unwrap(), b"file");
        let (first, second) = std::thread::scope(|scope| {
            let first = scope.spawn(|| create_folder(&parent, "new"));
            let second = scope.spawn(|| create_folder(&parent, "new"));
            (first.join().unwrap(), second.join().unwrap())
        });
        assert_ne!(first.is_ok(), second.is_ok());
        assert!(parent.join("new").is_dir());
        std::fs::remove_dir_all(parent).unwrap();
    }

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
            refresh: None,
            blank: false,
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
