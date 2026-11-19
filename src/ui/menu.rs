//! 画面上端のメニューバーと、設定のポップアップ（Bevy UI、カメラのズームとは関係なく画面に固定）。

use bevy::prelude::*;

use crate::render::scene::LabelFont;

/// メニューバーの高さ（論理ピクセル）。ズームの目標はこの下の領域に合わせる。
pub const BAR_PX: f32 = 32.0;

const BAR_COLOR: Color = Color::srgb(0.13, 0.13, 0.16);
const TEXT_COLOR: Color = Color::srgb(0.9, 0.9, 0.92);
const BUTTON_COLOR: Color = Color::srgb(0.22, 0.22, 0.27);

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
                    show_pause,
                    show_organize_badge,
                )
                    .in_set(MenuSystems),
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
#[derive(Resource)]
pub struct MenuBreadcrumb(pub String);

/// ポップアップの中身を足す場所（設定の項目はここに並べる）。
#[derive(Component)]
pub struct PopupBody;

#[derive(Component)]
struct Backdrop;

#[derive(Component)]
struct BreadcrumbText;

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
            bar.spawn((
                BreadcrumbText,
                Text::new("Root"),
                text_font.clone(),
                TextColor(TEXT_COLOR),
            ));
            let button = || {
                (
                    Node {
                        padding: UiRect::axes(Val::Px(10.0), Val::Px(3.0)),
                        border_radius: BorderRadius::all(Val::Px(4.0)),
                        ..default()
                    },
                    BackgroundColor(BUTTON_COLOR),
                )
            };
            bar.spawn(Node {
                column_gap: Val::Px(8.0),
                ..default()
            })
            .with_children(|buttons| {
                buttons
                    .spawn((
                        button(),
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
                        button(),
                        children![(Text::new("重複"), text_font.clone(), TextColor(TEXT_COLOR))],
                    ))
                    .observe(|_: On<PointerClick>, mut dups: ResMut<crate::dups::Dups>| {
                        dups.open = !dups.open;
                    });
                buttons
                    .spawn((
                        button(),
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
                        button(),
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
                        button(),
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
            BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.5)),
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
                    BackgroundColor(BAR_COLOR),
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

fn show_breadcrumb(
    breadcrumb: Option<Res<MenuBreadcrumb>>,
    mut text: Single<&mut Text, With<BreadcrumbText>>,
) {
    if let Some(breadcrumb) = breadcrumb
        && breadcrumb.is_changed()
    {
        text.0 = breadcrumb.0.clone();
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
