//! 画面上端のメニューバーと、設定のポップアップ（Bevy UI、カメラのズームとは関係なく画面に固定）。

use bevy::input::mouse::{MouseScrollUnit, MouseWheel};
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::text::{LineBreak, TextLayout, Underline};

use crate::model::display_path::Step;
use lisa_core::display_path::DEVICES;

use crate::render::palette;
use crate::render::scene::LabelFont;

/// メニューバーの高さ（論理ピクセル）。ズームの目標はこの下の領域に合わせる。
pub const BAR_PX: f32 = 32.0;

const BAR_COLOR: Color = palette::BASE_300;
const POPUP_COLOR: Color = palette::BASE_100;
const TEXT_COLOR: Color = palette::BASE_CONTENT;
const BUTTON_COLOR: Color = palette::NEUTRAL;

pub struct MenuPlugin;

impl Plugin for MenuPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Popup>()
            .add_systems(PostStartup, spawn_menu)
            .add_systems(
                Update,
                (
                    close_with_escape,
                    show_popup,
                    show_breadcrumb,
                    show_breadcrumb_error,
                    highlight_breadcrumb,
                    scroll_breadcrumb,
                    show_pause,
                    show_organize_badge,
                )
                    .in_set(MenuSystems),
            )
            .add_systems(
                PostUpdate,
                follow_breadcrumb_end.after(bevy::ui::UiSystems::Layout),
            );
    }
}

/// メニューの処理。画面の操作（クリックや Esc）より先に動かす。
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct MenuSystems;

/// 設定のポップアップが開いているか。
#[derive(Resource, Default)]
pub struct Popup {
    pub open: bool,
}

/// メニューバーに出す、今いる場所。
#[derive(Resource, PartialEq, Eq, Clone)]
pub struct MenuBreadcrumb {
    pub(crate) text: String,
    pub(crate) parts: Vec<String>,
    pub(crate) steps: Vec<Step>,
}

#[derive(Resource)]
pub(crate) struct BreadcrumbJump(pub Vec<Step>);

#[derive(Resource)]
pub(crate) struct BreadcrumbError(pub std::time::Instant);

/// ポップアップの中身を足す場所（設定の項目はここに並べる）。
#[derive(Component)]
pub struct PopupBody;

#[derive(Component)]
struct Backdrop;

#[derive(Component)]
struct BreadcrumbStrip;

#[derive(Component, Default)]
struct BreadcrumbScroll {
    follow_end: bool,
    max: f32,
}

#[derive(Component)]
struct BreadcrumbTarget(Vec<Step>);

#[derive(Component)]
struct AncestorBreadcrumb;

#[derive(Component)]
struct BreadcrumbErrorText;

/// 一時停止のボタンの文字（止めているあいだは「再開」）。
#[derive(Component)]
struct PauseText;

/// 整理ボタンの文字（許可待ちがあれば件数を添える）。
#[derive(Component)]
struct OrganizeText;

pub fn spawn_menu(mut commands: Commands, font: Res<LabelFont>) {
    let text_font = TextFont::from_font_size(14.0).with_font(font.0.clone());

    // メニューバー。
    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                top: Val::Px(0.0),
                left: Val::Px(0.0),
                width: Val::Percent(100.0),
                height: Val::Px(BAR_PX),
                padding: UiRect::horizontal(Val::Px(12.0)),
                align_items: AlignItems::Center,
                justify_content: JustifyContent::SpaceBetween,
                ..default()
            },
            BackgroundColor(BAR_COLOR),
            GlobalZIndex(10),
        ))
        .with_children(|bar| {
            bar.spawn(Node {
                min_width: Val::Px(0.0),
                flex_basis: Val::Px(0.0),
                flex_grow: 1.0,
                align_items: AlignItems::Center,
                column_gap: Val::Px(8.0),
                margin: UiRect::right(Val::Px(8.0)),
                ..default()
            })
            .with_children(|left| {
                left.spawn((
                    menu_button(),
                    BreadcrumbTarget(Vec::new()),
                    children![(Text::new(DEVICES), text_font.clone(), TextColor(TEXT_COLOR))],
                ))
                .observe(request_breadcrumb);
                left.spawn((
                    BreadcrumbStrip,
                    BreadcrumbScroll::default(),
                    Node {
                        min_width: Val::Px(0.0),
                        flex_basis: Val::Px(0.0),
                        flex_grow: 1.0,
                        align_items: AlignItems::Center,
                        column_gap: Val::Px(8.0),
                        overflow: Overflow::scroll_x(),
                        ..default()
                    },
                    ScrollPosition::default(),
                    Hovered::default(),
                ));
                left.spawn((
                    BreadcrumbErrorText,
                    Node {
                        position_type: PositionType::Absolute,
                        top: Val::Px(BAR_PX),
                        display: Display::None,
                        padding: UiRect::all(Val::Px(6.0)),
                        ..default()
                    },
                    BackgroundColor(BAR_COLOR),
                    Text::new("この場所へ移動できませんでした"),
                    text_font.clone(),
                    TextColor(TEXT_COLOR),
                    Pickable::IGNORE,
                ));
            });
            bar.spawn(Node {
                column_gap: Val::Px(8.0),
                flex_shrink: 0.0,
                ..default()
            })
            .with_children(|buttons| {
                buttons
                    .spawn((
                        menu_button(),
                        children![(
                            PauseText,
                            Text::new("一時停止"),
                            text_font.clone(),
                            TextColor(TEXT_COLOR)
                        )],
                    ))
                    .observe(
                        |_: On<PointerClick>, control: Option<Res<crate::scan::ScanControl>>| {
                            if let Some(control) = control {
                                let paused = &control.control.paused;
                                paused.fetch_xor(true, std::sync::atomic::Ordering::Relaxed);
                            }
                        },
                    );
                buttons
                    .spawn((
                        menu_button(),
                        children![(Text::new("重複"), text_font.clone(), TextColor(TEXT_COLOR))],
                    ))
                    .observe(|_: On<PointerClick>, mut dups: ResMut<crate::dups::Dups>| {
                        dups.open = !dups.open;
                    });
                buttons
                    .spawn((
                        menu_button(),
                        children![(
                            Text::new("カタログ"),
                            text_font.clone(),
                            TextColor(TEXT_COLOR)
                        )],
                    ))
                    .observe(
                        |_: On<PointerClick>, mut view: ResMut<crate::catalog::CatalogView>| {
                            view.open = !view.open;
                        },
                    );
                buttons
                    .spawn((
                        menu_button(),
                        children![(
                            OrganizeText,
                            Text::new("整理"),
                            text_font.clone(),
                            TextColor(TEXT_COLOR)
                        )],
                    ))
                    .observe(
                        |_: On<PointerClick>,
                         mut organize: ResMut<crate::organize::OrganizeState>| {
                            organize.panel_open = !organize.panel_open;
                        },
                    );
                buttons
                    .spawn((
                        menu_button(),
                        children![(Text::new("設定"), text_font.clone(), TextColor(TEXT_COLOR))],
                    ))
                    .observe(|_: On<PointerClick>, mut popup: ResMut<Popup>| {
                        popup.open = !popup.open;
                    });
            });
        });

    // ポップアップ。後ろを暗くし、外側を押すと閉じる。
    commands
        .spawn((
            Backdrop,
            Node {
                position_type: PositionType::Absolute,
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                align_items: AlignItems::Center,
                justify_content: JustifyContent::Center,
                display: Display::None,
                ..default()
            },
            BackgroundColor(palette::BASE_300.with_alpha(0.65)),
            GlobalZIndex(20),
        ))
        .observe(|_: On<PointerClick>, mut popup: ResMut<Popup>| {
            popup.open = false;
        })
        .with_children(|backdrop| {
            backdrop
                .spawn((
                    Node {
                        flex_direction: FlexDirection::Column,
                        row_gap: Val::Px(12.0),
                        padding: UiRect::all(Val::Px(20.0)),
                        min_width: Val::Px(320.0),
                        border_radius: BorderRadius::all(Val::Px(8.0)),
                        ..default()
                    },
                    BackgroundColor(POPUP_COLOR),
                ))
                // パネルの中を押しても、後ろ（外側）を押したことにはしない。
                .observe(|mut click: On<PointerClick>| click.propagate(false))
                .with_children(|panel| {
                    panel.spawn((
                        Text::new("設定"),
                        TextFont::from_font_size(18.0).with_font(font.0.clone()),
                        TextColor(TEXT_COLOR),
                    ));
                    panel.spawn((
                        PopupBody,
                        Node {
                            flex_direction: FlexDirection::Column,
                            row_gap: Val::Px(8.0),
                            ..default()
                        },
                    ));
                    panel
                        .spawn((
                            Node {
                                align_self: AlignSelf::End,
                                padding: UiRect::axes(Val::Px(12.0), Val::Px(4.0)),
                                border_radius: BorderRadius::all(Val::Px(4.0)),
                                ..default()
                            },
                            BackgroundColor(BUTTON_COLOR),
                            children![(
                                Text::new("閉じる"),
                                text_font.clone(),
                                TextColor(TEXT_COLOR)
                            )],
                        ))
                        .observe(|mut click: On<PointerClick>, mut popup: ResMut<Popup>| {
                            click.propagate(false);
                            popup.open = false;
                        });
                });
        });
}

/// ポップアップが開いていれば、Esc はまずポップアップを閉じる（ズームアウトはしない）。
fn close_with_escape(mut keys: ResMut<ButtonInput<KeyCode>>, mut popup: ResMut<Popup>) {
    if popup.open && keys.clear_just_pressed(KeyCode::Escape) {
        popup.open = false;
    }
}

fn show_popup(popup: Res<Popup>, mut backdrop: Single<&mut Node, With<Backdrop>>) {
    if !popup.is_changed() {
        return;
    }
    backdrop.display = if popup.open {
        Display::Flex
    } else {
        Display::None
    };
}

pub(super) fn menu_button() -> (Node, BackgroundColor) {
    (
        Node {
            padding: UiRect::axes(Val::Px(10.0), Val::Px(3.0)),
            border_radius: BorderRadius::all(Val::Px(4.0)),
            flex_shrink: 0.0,
            ..default()
        },
        BackgroundColor(BUTTON_COLOR),
    )
}

fn request_breadcrumb(
    mut event: On<PointerClick>,
    targets: Query<&BreadcrumbTarget>,
    mut commands: Commands,
) {
    if event.button == PointerButton::Primary
        && let Ok(target) = targets.get(event.entity)
    {
        event.propagate(false);
        commands.insert_resource(BreadcrumbJump(target.0.clone()));
    }
}

fn breadcrumb_item(
    strip: &mut ChildSpawnerCommands,
    font: &TextFont,
    label: &str,
    target: Option<Vec<Step>>,
    color: Color,
) {
    let mut item = strip.spawn((
        Node {
            flex_shrink: 0.0,
            ..default()
        },
        Text::new(label),
        font.clone(),
        TextColor(color),
        TextLayout::linebreak(LineBreak::NoWrap),
    ));
    if let Some(target) = target {
        item.insert((
            AncestorBreadcrumb,
            BreadcrumbTarget(target),
            Hovered::default(),
        ))
        .observe(request_breadcrumb);
    } else {
        item.insert(Pickable::IGNORE);
    }
}

type ChangedAncestorHover = (With<AncestorBreadcrumb>, Changed<Hovered>);

fn highlight_breadcrumb(
    mut commands: Commands,
    mut ancestors: Query<(Entity, &Hovered, &mut TextColor), ChangedAncestorHover>,
) {
    for (entity, hovered, mut color) in &mut ancestors {
        if hovered.get() {
            color.0 = TEXT_COLOR;
            commands.entity(entity).insert(Underline);
        } else {
            color.0 = palette::MUTED;
            commands.entity(entity).remove::<Underline>();
        }
    }
}

fn show_breadcrumb(
    mut commands: Commands,
    breadcrumb: Option<Res<MenuBreadcrumb>>,
    font: Res<LabelFont>,
    strip: Single<(Entity, &mut BreadcrumbScroll), With<BreadcrumbStrip>>,
    mut last: Local<Option<MenuBreadcrumb>>,
) {
    let Some(breadcrumb) = breadcrumb.filter(|b| b.is_changed()) else {
        return;
    };
    if last.as_ref() == Some(&breadcrumb) {
        return;
    }
    *last = Some(breadcrumb.as_ref().clone());
    let (strip, mut scroll) = strip.into_inner();
    let font = TextFont::from_font_size(14.0).with_font(font.0.clone());
    let mut entity = commands.entity(strip);
    entity.despawn_children().with_children(|strip| {
        for (index, part) in breadcrumb.parts.iter().enumerate() {
            if index > 0 {
                breadcrumb_item(strip, &font, "›", None, palette::MUTED);
            }
            let target =
                (index + 1 < breadcrumb.parts.len()).then(|| breadcrumb.steps[..=index].to_vec());
            let color = if target.is_some() {
                palette::MUTED
            } else {
                TEXT_COLOR
            };
            breadcrumb_item(strip, &font, part, target, color);
        }
    });
    scroll.follow_end = true;
}

fn follow_breadcrumb_end(
    strip: Single<
        (&ComputedNode, &mut ScrollPosition, &mut BreadcrumbScroll),
        With<BreadcrumbStrip>,
    >,
) {
    let (node, mut position, mut scroll) = strip.into_inner();
    let max = (node.content_size.x - node.size.x).max(0.0) * node.inverse_scale_factor;
    if scroll.follow_end || position.x > max {
        scroll.follow_end = false;
        position.x = max;
    }
    scroll.max = max;
}

fn scroll_breadcrumb(
    mut wheel: MessageReader<MouseWheel>,
    strip: Single<(&Hovered, &mut ScrollPosition, &BreadcrumbScroll), With<BreadcrumbStrip>>,
) {
    let (hovered, mut position, scroll) = strip.into_inner();
    for event in wheel.read() {
        if !hovered.get() {
            continue;
        }
        let delta = if event.x != 0.0 { event.x } else { event.y };
        let scale = if event.unit == MouseScrollUnit::Line {
            32.0
        } else {
            1.0
        };
        position.x = (position.x - scale * delta).clamp(0.0, scroll.max);
    }
}

fn show_breadcrumb_error(
    error: Option<Res<BreadcrumbError>>,
    mut text: Single<&mut Node, With<BreadcrumbErrorText>>,
) {
    let display = if error.is_some_and(|error| error.0.elapsed().as_secs() < 4) {
        Display::Flex
    } else {
        Display::None
    };
    if text.display != display {
        text.display = display;
    }
}

fn show_pause(
    control: Option<Res<crate::scan::ScanControl>>,
    mut text: Single<&mut Text, With<PauseText>>,
) {
    let Some(control) = control else {
        return;
    };
    let paused = control
        .control
        .paused
        .load(std::sync::atomic::Ordering::Relaxed);
    let label = if paused { "再開" } else { "一時停止" };
    if text.0 != label {
        text.0 = label.to_string();
    }
}

fn show_organize_badge(
    organize: Option<Res<crate::organize::OrganizeState>>,
    mut text: Single<&mut Text, With<OrganizeText>>,
) {
    let Some(organize) = organize else {
        return;
    };
    if !organize.is_changed() {
        return;
    }
    let n = organize.pending.len();
    let label = if n > 0 {
        format!("整理 ({n})")
    } else {
        "整理".to_string()
    };
    if text.0 != label {
        text.0 = label;
    }
}
