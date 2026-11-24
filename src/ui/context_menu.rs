//! タイルを右クリックしたときのメニュー。
//!
//! 部品は Bevy の `MenuPopup`・`MenuItem`（フォーカスが外れる、Esc、項目を選ぶで閉じる）と、位置決めの
//! `Popover`。項目は `ContextAction` に並べ、押せるかは `availability` で決め、押したら
//! `ContextCommand` を流して `run_command` で処理する。項目を足すときは、この 3 か所に足す。

use std::path::{Path, PathBuf};

use bevy::clipboard::Clipboard;
use bevy::input_focus::InputFocus;
use bevy::input_focus::tab_navigation::{NavAction, TabIndex};
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::text::LineBreak;
use bevy::ui::{FixedNode, InteractionDisabled};
use bevy::ui_widgets::popover::{Popover, PopoverAlign, PopoverPlacement, PopoverSide};
use bevy::ui_widgets::{Activate, MenuAction, MenuEvent, MenuFocusState, MenuItem, MenuPopup};
use bevy::window::{PrimaryWindow, RequestRedraw};

use crate::layout::Layouts;
use crate::model::tree::{FileTree, NodeId};
use crate::render::palette;
use crate::render::scene::LabelFont;
use crate::ui::menu::MenuSystems;
use crate::ui::tooltip::{
    ClampPopover, PointerBlocked, local_item_path, node_at_viewport_pos, remote_path_text,
};
use crate::view::{MainCamera, Nav};

const PANEL_COLOR: Color = palette::BASE_100;
const BORDER_COLOR: Color = palette::NEUTRAL;
const HOVER_COLOR: Color = palette::NEUTRAL;
const TEXT_COLOR: Color = palette::BASE_CONTENT;
const DISABLED_COLOR: Color = palette::MUTED;

pub struct ContextMenuPlugin;

impl Plugin for ContextMenuPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ContextMenu>()
            .init_resource::<RefreshNotice>()
            .add_message::<ContextCommand>()
            .add_plugins(super::dialog::DialogPlugin)
            .add_systems(First, |mut menu: ResMut<ContextMenu>| {
                if menu.just_closed {
                    menu.just_closed = false;
                }
            })
            .add_systems(
                Update,
                (open_menu, close_while_moving, highlight_items, run_command)
                    .chain()
                    .in_set(MenuSystems),
            );
    }
}

#[derive(Resource, Default)]
pub(super) struct RefreshNotice {
    pub error: Option<String>,
}

/// メニューが開いているか（開いている間と閉じたフレームは、下の画面のクリックや Esc を受けない）。
#[derive(Resource, Default)]
pub struct ContextMenu {
    open: Option<Entity>,
    /// このフレームで閉じた（閉じるのに使ったクリックや Esc を、下の画面に渡さないため）。
    just_closed: bool,
}

impl ContextMenu {
    pub fn busy(&self) -> bool {
        self.open.is_some() || self.just_closed
    }
}

/// メニューの項目。上から並べる順。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextAction {
    /// 開く（左クリックと同じ）。
    Open,
    /// 親へ戻る（Esc と同じ）。
    Up,
    CopyPath,
    Reveal,
    Rename,
    NewFolder,
    Refresh,
    Cut,
    Paste,
    Delete,
    Details,
}

/// メニューに並べるもの。`None` は区切りの線。
const ITEMS: &[Option<ContextAction>] = &[
    Some(ContextAction::Open),
    Some(ContextAction::Up),
    None,
    Some(ContextAction::CopyPath),
    Some(ContextAction::Reveal),
    None,
    Some(ContextAction::NewFolder),
    Some(ContextAction::Refresh),
    None,
    Some(ContextAction::Rename),
    Some(ContextAction::Cut),
    Some(ContextAction::Paste),
    Some(ContextAction::Delete),
    Some(ContextAction::Details),
];

impl ContextAction {
    fn label(self) -> &'static str {
        match self {
            Self::Open => "開く",
            Self::Up => "親へ戻る",
            Self::CopyPath => "パスをコピー",
            Self::Reveal => {
                if cfg!(windows) {
                    "エクスプローラーで表示"
                } else {
                    "Finder で表示"
                }
            }
            Self::NewFolder => "新規フォルダ",
            Self::Refresh => "更新",
            Self::Rename => "名前の変更…",
            Self::Cut => "切り取り",
            Self::Paste => "ここに貼り付け",
            Self::Delete => "削除…",
            Self::Details => "詳細…",
        }
    }
}

/// メニューを開いたものについて、項目を押せるかを決めるのに要ること。
#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    pub node: NodeId,
    /// 今開いている（画面いっぱいに映している）もの。
    pub is_open: bool,
    /// 開いているのが Root（それより上へ戻れない）。
    pub at_root: bool,
    /// 対象がフォルダか。
    pub is_dir: bool,
    /// 手元のディスク上のパス。
    pub local_path: Option<PathBuf>,
    /// 手元のドライブの中のファイル・フォルダか（Root・端末・ドライブの一番上や LAN の相手は false）。
    pub local_movable: bool,
    /// 切り取り中の手元パス。
    pub cut_source: Option<PathBuf>,
    /// コピーするパス（手元なら絶対パス、LAN の相手なら「端末名: パス」）。
    pub path_text: Option<String>,
    pub(crate) refresh: Option<LocalTarget>,
    pub(super) blank: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LocalTarget {
    pub(crate) request: crate::scan::RefreshTarget,
    mount: PathBuf,
}

impl LocalTarget {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if !crate::scan::scan_volumes().iter().any(|volume| {
            volume.id.as_ref() == Some(&self.request.drive_id) && volume.mount_path == self.mount
        }) {
            return Err("ドライブが接続されていません".into());
        }
        let path = &self.request.path;
        for parent in path
            .ancestors()
            .skip(1)
            .take_while(|parent| parent.starts_with(&self.mount))
        {
            if !std::fs::symlink_metadata(parent).is_ok_and(|metadata| metadata.is_dir()) {
                return Err("親フォルダを確認できません".into());
            }
        }
        let meta = std::fs::symlink_metadata(path)
            .map_err(|error| format!("対象を確認できません: {error}"))?;
        if (self.request.subtree && meta.is_dir() || !self.request.subtree && meta.is_file())
            && !lisa_core::listing::cloud_only(&meta)
        {
            Ok(())
        } else {
            Err("対象の種類が変わりました".into())
        }
    }
}

/// ノード `node` を右クリックしたときと同じ対象（目印を開いたときの詳細にも使う）。
pub(crate) fn target_of(
    node: NodeId,
    tree: &FileTree,
    store: Option<&crate::scan::Store>,
    nav: &Nav,
    organize: Option<&crate::organize::OrganizeState>,
) -> Target {
    let local = if tree.marker_view_read_only(node) {
        None
    } else {
        local_item_path(tree, store, node)
    };
    let local_movable =
        tree.nodes[node].remote.is_none() && tree.nodes[node].depth >= 3 && local.is_some();
    let path_text = local
        .as_ref()
        .map(|p| p.display().to_string())
        .or_else(|| remote_path_text(tree, node));
    let refresh = (|| {
        let item = &tree.nodes[node];
        if !is_refreshable(tree, node) {
            return None;
        }
        let (drive_id, mount) = crate::scan::drive_of(tree, store?, node)?;
        let path = local.clone().filter(|path| path.starts_with(&mount))?;
        let candidate = LocalTarget {
            request: crate::scan::RefreshTarget {
                drive_id,
                path,
                subtree: item.is_dir,
            },
            mount,
        };
        candidate.validate().ok()?;
        Some(candidate)
    })();
    Target {
        node,
        is_open: nav.top().node == node,
        at_root: nav.stack.len() < 2,
        local_path: local,
        is_dir: tree.nodes[node].is_dir,
        local_movable,
        cut_source: organize.and_then(|o| o.cut_source.clone()),
        path_text,
        refresh,
        blank: false,
    }
}

fn is_refreshable(tree: &FileTree, node: NodeId) -> bool {
    let item = &tree.nodes[node];
    item.remote.is_none()
        && item.merged.is_none()
        && !tree.in_marker_view(node)
        && !tree.marker_view_read_only(node)
        && (!item.is_dir || item.path.is_some())
}

/// 項目を押せるか。
pub fn availability(action: ContextAction, target: &Target) -> bool {
    match action {
        ContextAction::NewFolder => target.is_dir && target.refresh.is_some(),
        ContextAction::Refresh => target.refresh.is_some(),
        ContextAction::Open => !target.is_open,
        ContextAction::Up => !target.at_root,
        ContextAction::CopyPath => target.path_text.is_some(),
        ContextAction::Reveal => target.local_path.as_ref().is_some_and(|p| p.exists()),
        ContextAction::Cut | ContextAction::Delete | ContextAction::Rename => {
            target.local_movable && target.local_path.as_ref().is_some_and(|p| p.exists())
        }
        ContextAction::Paste => {
            let Some(from) = target.cut_source.as_ref().filter(|p| p.exists()) else {
                return false;
            };
            let dest_dir = if target.is_dir {
                target.local_path.clone()
            } else {
                target
                    .local_path
                    .as_ref()
                    .and_then(|p| p.parent().map(Path::to_path_buf))
            };
            let Some(dest_dir) = dest_dir else {
                return false;
            };
            dest_dir.is_dir()
                && from.parent() != Some(dest_dir.as_path())
                && !(from.is_dir() && dest_dir.starts_with(from))
        }
        ContextAction::Details => true,
    }
}

/// 押された項目。
#[derive(Message, Clone, Debug)]
pub struct ContextCommand {
    pub action: ContextAction,
    pub target: Target,
    /// 右クリックした所（ワールド座標）。「開く」は、ここを左クリックしたのと同じにする。
    pub world: Vec2,
}

/// 「開く」「親へ戻る」を、画面の操作（`view.rs` の `navigate`）に頼む。
#[derive(Resource, Clone, Copy, Debug)]
pub enum NavRequest {
    ClickAt(Vec2),
    Up,
}

#[derive(Component)]
struct MenuRoot;

#[derive(Component, Clone)]
struct ItemOf {
    action: ContextAction,
}

/// 右クリックで、カーソルの所のものに対するメニューを開く。
#[allow(clippy::too_many_arguments)]
fn open_menu(
    mut commands: Commands,
    mouse: Res<ButtonInput<MouseButton>>,
    window: Single<&Window, With<PrimaryWindow>>,
    camera: Single<(&Camera, &GlobalTransform, &Projection), With<MainCamera>>,
    tree: Res<FileTree>,
    layouts: Res<Layouts>,
    nav: Res<Nav>,
    organize: Option<Res<crate::organize::OrganizeState>>,
    store: Option<Res<crate::scan::Store>>,
    blocked: PointerBlocked,
    mut menu: ResMut<ContextMenu>,
    font: Res<LabelFont>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    if !mouse.just_pressed(MouseButton::Right) {
        return;
    }
    let window = window.into_inner();
    // 開いているメニューの外で右クリックしたら、そこで開き直す。
    if let Some(old) = menu.open.take() {
        commands.entity(old).despawn();
    }
    if blocked.blocked(window, false) {
        return;
    }
    let (cam, global, projection) = camera.into_inner();
    let Some(cursor) = blocked.cursor(window) else {
        return;
    };
    let Some(world) = cam.viewport_to_world_2d(global, cursor).ok() else {
        return;
    };
    let hit = node_at_viewport_pos(cursor, (cam, global, projection), &tree, &layouts);
    let scene = crate::view::scene_path(&nav, &tree)
        .last()
        .expect("現在地")
        .node;
    let node = hit.map_or(scene, |(node, _)| node);
    let mut target = target_of(node, &tree, store.as_deref(), &nav, organize.as_deref());
    target.blank = hit.is_none();
    let window_size = Vec2::new(window.width(), window.height());
    menu.open = Some(spawn_menu(
        &mut commands,
        &font,
        cursor,
        window_size,
        target,
        world,
    ));
    redraw.write(RequestRedraw);
}

/// ジェスチャーやズームで画面が動き出したら、メニューを閉じる（指していたものから離れるため）。
fn close_while_moving(
    mut commands: Commands,
    mut menu: ResMut<ContextMenu>,
    gesture: Res<crate::input::gesture::Gesture>,
    animating: Option<Res<crate::view::Animating>>,
    mut focus: ResMut<InputFocus>,
) {
    if (gesture.active() || animating.is_some())
        && let Some(open) = menu.open.take()
    {
        commands.entity(open).despawn();
        menu.just_closed = true;
        focus.clear();
    }
}

/// カーソルの下の項目を明るくする。
#[allow(clippy::type_complexity)]
fn highlight_items(
    mut items: Query<
        (&Hovered, Has<InteractionDisabled>, &mut BackgroundColor),
        (With<ItemOf>, Changed<Hovered>),
    >,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    for (hovered, disabled, mut bg) in &mut items {
        bg.0 = if hovered.get() && !disabled {
            HOVER_COLOR
        } else {
            Color::NONE
        };
        redraw.write(RequestRedraw);
    }
}

/// 押された項目を処理する。
fn run_command(
    mut commands: Commands,
    mut reader: MessageReader<ContextCommand>,
    mut clipboard: Option<ResMut<Clipboard>>,
    mut organize: Option<ResMut<crate::organize::OrganizeState>>,
    tree: Res<FileTree>,
    scan: Option<Res<crate::scan::ScanControl>>,
    mut refresh: ResMut<RefreshNotice>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    for command in reader.read() {
        let target = &command.target;
        if (target.node >= tree.nodes.len() || tree.marker_view_read_only(target.node))
            && matches!(
                command.action,
                ContextAction::Reveal
                    | ContextAction::Cut
                    | ContextAction::Paste
                    | ContextAction::Delete
            )
        {
            continue;
        }
        match command.action {
            ContextAction::Open => {
                commands.insert_resource(NavRequest::ClickAt(command.world));
            }
            ContextAction::Up => {
                commands.insert_resource(NavRequest::Up);
            }
            ContextAction::CopyPath => {
                let (Some(text), Some(clipboard)) = (&target.path_text, clipboard.as_mut()) else {
                    continue;
                };
                if let Err(error) = clipboard.set_text(text.clone()) {
                    warn!("パスをコピーできませんでした: {error:?}");
                }
            }
            ContextAction::Reveal => {
                if let Some(path) = &target.local_path {
                    reveal(path);
                }
            }
            ContextAction::Cut => {
                if let (Some(path), Some(organize)) = (&target.local_path, organize.as_mut()) {
                    organize.cut_source = Some(path.clone());
                }
            }
            ContextAction::Paste => {
                let from = organize.as_mut().and_then(|o| o.cut_source.take());
                if let (Some(organize), Some(from)) = (organize.as_mut(), from) {
                    let dest_dir = if target.is_dir {
                        target.local_path.clone()
                    } else {
                        target
                            .local_path
                            .as_ref()
                            .and_then(|p| p.parent().map(Path::to_path_buf))
                    };
                    if let Some(dest_dir) = dest_dir {
                        organize.user_move(from, dest_dir);
                    }
                }
            }
            ContextAction::Delete => {
                if let (Some(path), Some(organize)) = (&target.local_path, organize.as_mut()) {
                    organize.user_trash(path.clone());
                }
            }
            // ダイアログ側が ContextCommand を受け取る。
            ContextAction::Rename | ContextAction::NewFolder | ContextAction::Details => {}
            ContextAction::Refresh => {
                let Some(target) = &target.refresh else {
                    continue;
                };
                if let Err(error) = target.validate().and_then(|_| {
                    scan.as_ref()
                        .and_then(|scan| scan.refresh(target.request.clone()))
                        .map(|_| ())
                        .ok_or_else(|| "更新要求を受け付けられませんでした".into())
                }) {
                    warn!(
                        "更新できませんでした（{}）: {error}",
                        target.request.path.display()
                    );
                    refresh.error = Some(error);
                }
            }
        }
        redraw.write(RequestRedraw);
    }
}

/// Finder（Windows はエクスプローラー）で、そのファイルやフォルダを選んだ状態で見せる。
fn reveal(path: &std::path::Path) {
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open")
        .arg("-R")
        .arg(path)
        .spawn();
    #[cfg(windows)]
    let result = {
        use std::os::windows::process::CommandExt;
        let mut arg = std::ffi::OsString::from("/select,\"");
        arg.push(path.as_os_str());
        arg.push("\"");
        std::process::Command::new("explorer").raw_arg(arg).spawn()
    };
    #[cfg(not(any(target_os = "macos", windows)))]
    let result = std::process::Command::new("xdg-open")
        .arg(path.parent().unwrap_or(path))
        .spawn();
    if let Err(error) = result {
        warn!("場所を開けませんでした（{}）: {error}", path.display());
    }
}

fn spawn_menu(
    commands: &mut Commands,
    font: &LabelFont,
    cursor: Vec2,
    window_size: Vec2,
    target: Target,
    world: Vec2,
) -> Entity {
    let text_font = TextFont::from_font_size(13.0).with_font(font.0.clone());
    let margin = 6.0;
    let max_w = (window_size.x - margin * 2.0).max(120.0);
    let max_h = (window_size.y - margin * 2.0).max(80.0);
    let root = commands
        .spawn((
            MenuRoot,
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(cursor.x),
                top: Val::Px(cursor.y),
                width: Val::Px(0.0),
                height: Val::Px(0.0),
                ..default()
            },
            GlobalZIndex(50),
        ))
        .observe(
            |event: On<MenuEvent>,
             mut commands: Commands,
             mut menu: ResMut<ContextMenu>,
             mut focus: ResMut<InputFocus>,
             mut redraw: MessageWriter<RequestRedraw>| {
                if !matches!(event.action, MenuAction::CloseAll) {
                    return;
                }
                if let Some(open) = menu.open.take() {
                    commands.entity(open).despawn();
                    menu.just_closed = true;
                }
                focus.clear();
                redraw.write(RequestRedraw);
            },
        )
        .id();
    // `FixedNode` でレイアウトだけビューポート基準にする（大きさ 0 の `root` の中で
    // `available_width` が潰れて項目テキストが意図せず折り返すのを防ぐ）。
    let popup = commands
        .spawn((
            FixedNode,
            ClampPopover,
            MenuPopup::default(),
            MenuFocusState::Opening(NavAction::First),
            Node {
                position_type: PositionType::Absolute,
                flex_direction: FlexDirection::Column,
                min_width: Val::Px(200.0_f32.min(max_w)),
                max_width: Val::Px(max_w),
                max_height: Val::Px(max_h),
                overflow: Overflow::clip(),
                padding: UiRect::all(Val::Px(4.0)),
                border: UiRect::all(Val::Px(1.0)),
                border_radius: BorderRadius::all(Val::Px(6.0)),
                ..default()
            },
            BackgroundColor(PANEL_COLOR),
            BorderColor::all(BORDER_COLOR),
            Popover {
                positions: [
                    (PopoverSide::Bottom, PopoverAlign::Start),
                    (PopoverSide::Top, PopoverAlign::Start),
                    (PopoverSide::Bottom, PopoverAlign::End),
                    (PopoverSide::Top, PopoverAlign::End),
                    (PopoverSide::Bottom, PopoverAlign::Center),
                    (PopoverSide::Top, PopoverAlign::Center),
                ]
                .into_iter()
                .map(|(side, align)| PopoverPlacement {
                    side,
                    align,
                    gap: 2.0,
                })
                .collect(),
                window_margin: margin,
            },
            ChildOf(root),
        ))
        .id();
    let blank_items = [Some(ContextAction::NewFolder), Some(ContextAction::Refresh)];
    for item in if target.blank { &blank_items } else { ITEMS } {
        let Some(action) = *item else {
            commands.spawn((
                Node {
                    height: Val::Px(1.0),
                    margin: UiRect::vertical(Val::Px(4.0)),
                    ..default()
                },
                BackgroundColor(BORDER_COLOR),
                Pickable::IGNORE,
                ChildOf(popup),
            ));
            continue;
        };
        let enabled = availability(action, &target);
        let mut entity = commands.spawn((
            MenuItem,
            ItemOf { action },
            Hovered::default(),
            Node {
                padding: UiRect::axes(Val::Px(10.0), Val::Px(5.0)),
                border_radius: BorderRadius::all(Val::Px(4.0)),
                ..default()
            },
            BackgroundColor(Color::NONE),
            ChildOf(popup),
            children![(
                Text::new(action.label()),
                text_font.clone(),
                TextColor(if enabled { TEXT_COLOR } else { DISABLED_COLOR }),
                TextLayout::linebreak(LineBreak::NoWrap),
                Pickable::IGNORE,
            )],
        ));
        if enabled {
            entity.insert(TabIndex(0));
        } else {
            entity.insert(InteractionDisabled);
        }
        let target = target.clone();
        entity.observe(
            move |activate: On<Activate>,
                  items: Query<&ItemOf>,
                  mut writer: MessageWriter<ContextCommand>| {
                let Ok(item) = items.get(activate.entity) else {
                    return;
                };
                writer.write(ContextCommand {
                    action: item.action,
                    target: target.clone(),
                    world,
                });
            },
        );
    }
    root
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> Target {
        Target {
            node: 5,
            is_open: false,
            at_root: false,
            is_dir: false,
            local_path: None,
            local_movable: false,
            cut_source: None,
            path_text: None,
            refresh: None,
            blank: false,
        }
    }

    #[test]
    fn 統合表示配下は中間フォルダにパスがあっても更新対象にしない() {
        let mut tree = FileTree::dummy();
        let (child, parent) = tree
            .nodes
            .iter()
            .enumerate()
            .find_map(|(id, node)| {
                (node.is_dir && node.depth >= 3).then(|| (id, node.parent.unwrap()))
            })
            .unwrap();
        tree.nodes[child].path = Some(std::env::temp_dir().join("intermediate"));
        assert!(is_refreshable(&tree, child));
        let location = serde_json::json!({
            "device_id": "fixture", "device_name": "fixture",
            "volume_id": "test", "volume_name": "test",
            "relative_path": ["parent"], "display_path": "parent"
        });
        tree.nodes[parent].merged = Some(
            serde_json::from_value(serde_json::json!({
                "format": "fixture", "version": 1, "link_id": "fixture",
                "source": location, "target": location, "kind": lisa_core::listing::Kind::Dir, "size": 0,
                "hash": {"algorithm": "blake3", "version": 1, "value": "0".repeat(64)},
                "executed_at": {"unix_ms": 0, "timezone": "UTC"}, "plan_id": 1, "item_id": 1
            }))
            .unwrap(),
        );
        assert!(!is_refreshable(&tree, child));
        let nav = Nav {
            stack: vec![crate::view::Level { node: child }],
        };
        let target = target_of(child, &tree, None, &nav, None);
        assert!(!availability(ContextAction::NewFolder, &target));
        assert!(!availability(ContextAction::Refresh, &target));
    }

    #[test]
    fn open_and_up_follow_navigation() {
        let mut t = target();
        assert!(availability(ContextAction::Open, &t));
        assert!(availability(ContextAction::Up, &t));
        t.is_open = true;
        t.at_root = true;
        assert!(!availability(ContextAction::Open, &t));
        assert!(!availability(ContextAction::Up, &t));
    }

    #[test]
    fn copy_and_reveal_need_a_path() {
        let mut t = target();
        assert!(!availability(ContextAction::CopyPath, &t));
        assert!(!availability(ContextAction::Reveal, &t));
        assert!(!availability(ContextAction::Cut, &t));
        assert!(!availability(ContextAction::Delete, &t));
        // LAN の相手のもの: コピーはできるが、手元のディスクに無いので表示・移動・削除はできない。
        t.path_text = Some("Windows PC: C:/Users".into());
        assert!(availability(ContextAction::CopyPath, &t));
        assert!(!availability(ContextAction::Reveal, &t));
        assert!(!availability(ContextAction::Cut, &t));
        assert!(!availability(ContextAction::Delete, &t));
        let here = std::env::current_dir().unwrap();
        t.local_path = Some(here.clone());
        t.path_text = Some(here.display().to_string());
        assert!(availability(ContextAction::Reveal, &t));
        // ドライブの一番上（local_movable = false）では移動・削除はできない。
        assert!(!availability(ContextAction::Cut, &t));
        assert!(!availability(ContextAction::Delete, &t));
        // ドライブの中のファイル・フォルダなら移動・削除できる。
        t.local_movable = true;
        assert!(availability(ContextAction::Cut, &t));
        assert!(availability(ContextAction::Delete, &t));
    }

    #[test]
    fn rename_requires_an_existing_local_item_and_details_is_always_available() {
        let mut t = target();
        assert!(!availability(ContextAction::Rename, &t));
        assert!(availability(ContextAction::Details, &t));
        t.local_path = Some(std::env::current_dir().unwrap());
        assert!(!availability(ContextAction::Rename, &t));
        assert!(availability(ContextAction::Details, &t));
        t.local_movable = true;
        assert!(availability(ContextAction::Rename, &t));
        t.local_path = Some(t.local_path.unwrap().join("missing-lisa-test-item"));
        assert!(!availability(ContextAction::Rename, &t));
        assert_eq!(ContextAction::Rename.label(), "名前の変更…");
        assert_eq!(ContextAction::Details.label(), "詳細…");
    }

    #[test]
    fn paste_requires_a_valid_different_destination() {
        let root = std::env::temp_dir().join(format!("lisa-paste-test-{}", std::process::id()));
        let from = root.join("source");
        let child = from.join("child");
        std::fs::create_dir_all(&child).unwrap();
        let dest = root.join("destination");
        std::fs::create_dir_all(&dest).unwrap();
        let mut t = target();
        t.cut_source = Some(from.clone());
        t.is_dir = true;
        t.local_path = Some(dest.clone());
        assert!(availability(ContextAction::Paste, &t));
        t.local_path = Some(child.clone());
        assert!(!availability(ContextAction::Paste, &t));
        t.local_path = Some(root.clone());
        assert!(!availability(ContextAction::Paste, &t));
        t.is_dir = false;
        t.local_path = Some(dest.join("file.txt"));
        assert!(availability(ContextAction::Paste, &t));
        t.cut_source = None;
        assert!(!availability(ContextAction::Paste, &t));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn every_action_is_listed_once() {
        let listed: Vec<_> = ITEMS.iter().flatten().copied().collect();
        for action in [
            ContextAction::Open,
            ContextAction::Up,
            ContextAction::CopyPath,
            ContextAction::Reveal,
            ContextAction::Rename,
            ContextAction::Cut,
            ContextAction::Paste,
            ContextAction::Delete,
            ContextAction::Details,
        ] {
            assert_eq!(listed.iter().filter(|a| **a == action).count(), 1);
        }
    }
}
