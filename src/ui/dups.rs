//! 重複の一覧（画面の右側に固定）。メニューバーの「重複」で開け閉めする。
//!
//! 組は空く大きさの順に並べ、それぞれに数・合計・空く量を出す。組を押すと選んで（木の上で光る）、
//! その下にコピーの場所を出す。場所を押すと、木の上のそこへ飛ぶ。表示だけで、何も動かさない。

use bevy::input::mouse::{MouseScrollUnit, MouseWheel};
use bevy::prelude::*;
use bevy::ui::ScrollPosition;
use bevy::window::RequestRedraw;
use lisa_core::dups::Group;
use lisa_core::listing::Kind;

use crate::dups::{Dups, DupsState};
use crate::render::palette;
use crate::render::scene::LabelFont;
use crate::scan::JumpTo;
use crate::ui::menu::BAR_PX;

/// 一覧の幅（論理ピクセル）。
pub const PANEL_PX: f32 = 380.0;

const PANEL_COLOR: Color = palette::BASE_100;
const ROW_COLOR: Color = palette::BASE_200;
const SELECTED_COLOR: Color = palette::NEUTRAL;
const TEXT_COLOR: Color = palette::BASE_CONTENT;
const DIM_COLOR: Color = palette::MUTED;
/// 選んだ組の色（木の上の重複枠と同じ warning）。
const ACCENT: Color = palette::WARNING;
/// すべり出しにかける時間（秒）。
const SLIDE_SECONDS: f32 = 0.22;
/// 1 回のホイールで動かす量（行単位のとき）。
const LINE_PX: f32 = 24.0;

pub struct DupsPanelPlugin;

impl Plugin for DupsPanelPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(PostStartup, spawn_panel)
            .add_systems(Update, (show_panel, fill_panel, scroll_panel));
    }
}

#[derive(Component)]
struct Panel;

#[derive(Component)]
struct List;

fn spawn_panel(mut commands: Commands) {
    commands
        .spawn((
            Panel,
            Node {
                position_type: PositionType::Absolute,
                top: Val::Px(BAR_PX),
                right: Val::Px(-PANEL_PX),
                bottom: Val::Px(0.0),
                width: Val::Px(PANEL_PX),
                display: Display::None,
                ..default()
            },
            BackgroundColor(PANEL_COLOR),
            GlobalZIndex(6),
        ))
        .with_children(|panel| {
            panel.spawn((
                List,
                Node {
                    flex_direction: FlexDirection::Column,
                    width: Val::Percent(100.0),
                    height: Val::Percent(100.0),
                    padding: UiRect::all(Val::Px(8.0)),
                    row_gap: Val::Px(4.0),
                    overflow: Overflow::scroll_y(),
                    ..default()
                },
                ScrollPosition::default(),
            ));
        });
}

/// 開け閉め。右からすべり出し、その幅の分だけ木を映す領域を狭める（`view::set_side_px`）。
fn show_panel(
    dups: Res<Dups>,
    mut shown: Local<f32>,
    mut last: Local<Option<std::time::Instant>>,
    mut panel: Single<&mut Node, With<Panel>>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    let target = if dups.open { 1.0 } else { 0.0 };
    let now = std::time::Instant::now();
    let dt = last.map_or(0.0, |t| now.duration_since(t).as_secs_f32());
    *last = Some(now);
    if *shown == target {
        return;
    }
    // 少し前に開け閉めを始めたばかりなら、最初の 1 歩は小さく。
    let step = (dt.min(1.0 / 30.0)) / SLIDE_SECONDS;
    *shown = if target > *shown {
        (*shown + step).min(1.0)
    } else {
        (*shown - step).max(0.0)
    };
    // 始めと終わりをゆっくりにする。
    let t = *shown;
    let eased = t * t * (3.0 - 2.0 * t);
    let width = PANEL_PX * eased;
    panel.display = if *shown > 0.0 {
        Display::Flex
    } else {
        Display::None
    };
    panel.right = Val::Px(width - PANEL_PX);
    crate::view::set_side_px(width);
    redraw.write(RequestRedraw);
}

/// 一覧の中身を作り直す（一覧ができたときと、選んだ組が変わったとき）。
fn fill_panel(
    mut commands: Commands,
    dups: Res<Dups>,
    font: Res<LabelFont>,
    list: Single<Entity, With<List>>,
) {
    if !dups.is_changed() || !dups.open {
        return;
    }
    let list = *list;
    commands.entity(list).despawn_children();
    let big = TextFont::from_font_size(13.0).with_font(font.0.clone());
    let small = TextFont::from_font_size(11.0).with_font(font.0.clone());
    let header = match &dups.state {
        DupsState::NotYet | DupsState::Computing { previous: None, .. } => {
            "重複を探しています…（数十秒かかります）".to_string()
        }
        DupsState::Computing {
            previous: Some(groups),
            ..
        } => {
            let wasted: u64 = groups.iter().map(Group::wasted).sum();
            format!(
                "重複  {} 組・空く {}（前の一覧。作り直しています…）",
                groups.len(),
                human(wasted)
            )
        }
        DupsState::Failed(error) => format!("重複の一覧を作れませんでした: {error}"),
        DupsState::Ready(groups) => {
            let wasted: u64 = groups.iter().map(Group::wasted).sum();
            format!("重複  {} 組・空く {}", groups.len(), human(wasted))
        }
    };
    commands.entity(list).with_children(|list| {
        list.spawn((Text::new(header), big.clone(), TextColor(TEXT_COLOR)));
        list.spawn((
            Text::new("表示だけです。ファイルは動かしません。"),
            small.clone(),
            TextColor(DIM_COLOR),
        ));
        for (index, group) in dups.groups().iter().enumerate() {
            let selected = dups.selected == Some(index);
            let kind = if group.kind == Kind::Dir {
                "フォルダ"
            } else {
                "ファイル"
            };
            list.spawn((
                Node {
                    flex_direction: FlexDirection::Column,
                    padding: UiRect::axes(Val::Px(8.0), Val::Px(5.0)),
                    border_radius: BorderRadius::all(Val::Px(4.0)),
                    flex_shrink: 0.0,
                    ..default()
                },
                BackgroundColor(if selected { SELECTED_COLOR } else { ROW_COLOR }),
                children![
                    (
                        Text::new(format!("{kind}  {}", group.name)),
                        big.clone(),
                        TextColor(if selected { ACCENT } else { TEXT_COLOR }),
                    ),
                    (
                        Text::new(format!(
                            "{} か所・合計 {}・空く {}",
                            group.copies.len(),
                            human(group.total()),
                            human(group.wasted())
                        )),
                        small.clone(),
                        TextColor(DIM_COLOR),
                    )
                ],
            ))
            .observe(move |_: On<PointerClick>, mut dups: ResMut<Dups>| {
                dups.selected = if dups.selected == Some(index) {
                    None
                } else {
                    Some(index)
                };
            });
            if !selected {
                continue;
            }
            for copy in &group.copies {
                let chain = copy.chain.clone();
                let mut text = copy.path.display().to_string();
                if !copy.hard_links.is_empty() {
                    text.push_str(&format!(
                        "（ハードリンク {} か所も同じ実体）",
                        copy.hard_links.len()
                    ));
                }
                list.spawn((
                    Node {
                        margin: UiRect::left(Val::Px(12.0)),
                        padding: UiRect::axes(Val::Px(8.0), Val::Px(3.0)),
                        border_radius: BorderRadius::all(Val::Px(4.0)),
                        flex_shrink: 0.0,
                        ..default()
                    },
                    BackgroundColor(ROW_COLOR),
                    children![(Text::new(text), small.clone(), TextColor(TEXT_COLOR))],
                ))
                .observe(move |_: On<PointerClick>, mut commands: Commands| {
                    commands.insert_resource(JumpTo(chain.clone()));
                });
            }
        }
    });
}

/// 一覧の上でホイールを回したら、一覧を動かす。
fn scroll_panel(
    dups: Res<Dups>,
    mut wheel: MessageReader<MouseWheel>,
    window: Single<&Window>,
    mut list: Single<&mut ScrollPosition, With<List>>,
) {
    let over = dups.open
        && window
            .cursor_position()
            .is_some_and(|p| p.x > crate::view::view_width(&window) && p.y > BAR_PX);
    for event in wheel.read() {
        if !over {
            continue;
        }
        let dy = match event.unit {
            MouseScrollUnit::Line => event.y * LINE_PX,
            MouseScrollUnit::Pixel => event.y,
        };
        list.y = (list.y - dy).max(0.0);
    }
}

/// 読みやすい大きさ（1024 区切り）。
fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
