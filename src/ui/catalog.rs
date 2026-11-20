//! カタログのパネル（画面の右側）。メニューバーの「カタログ」で開け閉めする。重複の一覧と同じ出方で、
//! 木を映す領域を縮めて場所を空ける（上に重ねない）。重複の一覧とは同時に開かない。
//!
//! - アプリごと・型ごとの当たりと大きさ。押すと選んで（木の上で枠が付く）、選んだものをもう一度押すと
//!   そこへ飛ぶ。
//! - 今開いているフォルダの仕分け（アプリを選ぶと `assignment` に残す。外すこともできる）。
//! - どの型にも仕分けにも当たらない大きなフォルダ。押すとそこへ飛ぶ。
//!
//! 表示と仕分けだけで、ユーザーのファイルは何も動かさない。

use std::collections::BTreeMap;

use bevy::input::mouse::{MouseScrollUnit, MouseWheel};
use bevy::prelude::*;
use bevy::ui::ScrollPosition;
use bevy::window::RequestRedraw;
use lisa_core::catalog::{Role, State};

use crate::catalog::{CatalogView, Hit};
use crate::dups::Dups;
use crate::model::tree::FileTree;
use crate::render::palette;
use crate::render::scene::LabelFont;
use crate::scan::JumpTo;
use crate::ui::menu::BAR_PX;
use crate::view::Nav;

/// パネルの幅（論理ピクセル）。
pub const PANEL_PX: f32 = 380.0;

const PANEL_COLOR: Color = palette::BASE_100;
const ROW_COLOR: Color = palette::BASE_200;
const SELECTED_COLOR: Color = palette::NEUTRAL;
const CHIP_COLOR: Color = palette::NEUTRAL;
const TEXT_COLOR: Color = palette::BASE_CONTENT;
const DIM_COLOR: Color = palette::MUTED;
const ERROR_COLOR: Color = palette::ERROR;
/// すべり出しにかける時間（秒）。
const SLIDE_SECONDS: f32 = 0.22;
/// 1 回のホイールで動かす量（行単位のとき）。
const LINE_PX: f32 = 24.0;

pub struct CatalogPanelPlugin;

impl Plugin for CatalogPanelPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(PostStartup, spawn_panel).add_systems(
            Update,
            (only_one_panel, show_panel, fill_panel, scroll_panel).chain(),
        );
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

/// 重複の一覧とカタログのパネルは、後に開いた方だけを残す。
fn only_one_panel(mut view: ResMut<CatalogView>, mut dups: ResMut<Dups>) {
    if view.is_changed() && view.open && dups.open {
        dups.open = false;
    } else if dups.is_changed() && dups.open && view.open {
        view.open = false;
    }
}

/// 開け閉め。右からすべり出し、その幅の分だけ木を映す領域を狭める。
fn show_panel(
    view: Res<CatalogView>,
    mut shown: Local<f32>,
    mut last: Local<Option<std::time::Instant>>,
    mut panel: Single<&mut Node, With<Panel>>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    let target = if view.open { 1.0 } else { 0.0 };
    let now = std::time::Instant::now();
    let dt = last.map_or(0.0, |t| now.duration_since(t).as_secs_f32());
    *last = Some(now);
    if *shown == target {
        return;
    }
    let step = (dt.min(1.0 / 30.0)) / SLIDE_SECONDS;
    *shown = if target > *shown {
        (*shown + step).min(1.0)
    } else {
        (*shown - step).max(0.0)
    };
    let t = *shown;
    let eased = t * t * (3.0 - 2.0 * t);
    let width = PANEL_PX * eased;
    panel.display = if *shown > 0.0 {
        Display::Flex
    } else {
        Display::None
    };
    panel.right = Val::Px(width - PANEL_PX);
    crate::view::set_catalog_side_px(width);
    redraw.write(RequestRedraw);
}

fn state_text(state: State) -> &'static str {
    match state {
        State::Match => "",
        State::Maybe => "（たぶん。中身はクラウドにだけある）",
        State::Pending => "（確かめ中）",
        State::Miss => "（外れ）",
    }
}

fn role_text(role: Role) -> &'static str {
    match role {
        Role::Data => "data",
        Role::Config => "config",
        Role::Cache => "cache",
        Role::Install => "install",
        Role::Log => "log",
    }
}

/// 中身を作り直す（結果が来たとき、選んだものが変わったとき、開いているフォルダが変わったとき）。
#[allow(clippy::too_many_arguments)]
fn fill_panel(
    mut commands: Commands,
    view: Res<CatalogView>,
    remote: Res<crate::catalog::RemoteCatalogs>,
    nav: Res<Nav>,
    tree: Res<FileTree>,
    font: Res<LabelFont>,
    list: Single<Entity, With<List>>,
    mut last_open: Local<Option<usize>>,
) {
    let open_folder = nav.top().node;
    let folder_changed = *last_open != Some(open_folder);
    if !view.open || !(view.is_changed() || remote.is_changed() || folder_changed) {
        return;
    }
    *last_open = Some(open_folder);
    let list = *list;
    commands.entity(list).despawn_children();
    let big = TextFont::from_font_size(13.0).with_font(font.0.clone());
    let small = TextFont::from_font_size(11.0).with_font(font.0.clone());
    let Some(snapshot) = &view.snapshot else {
        commands.entity(list).with_children(|list| {
            list.spawn((
                Text::new("カタログを当てています…"),
                big.clone(),
                TextColor(TEXT_COLOR),
            ));
        });
        return;
    };
    // 同じフォルダに 2 つの型が当たることがあるので、合計は 1 回ずつ数える（入れ子は数え直さない）。
    let mut counted = std::collections::HashSet::new();
    let total: u64 = snapshot
        .hits
        .iter()
        .filter(|h| counted.insert(h.entry_id()))
        .filter(|h| {
            !snapshot.hits.iter().any(|o| {
                o.entry_id() != h.entry_id()
                    && h.chain.len() > o.chain.len()
                    && h.chain.contains(&o.entry_id())
            })
        })
        .map(|h| h.size)
        .sum();
    let header = format!("カタログ  {} か所・合計 {}", counted.len(), human(total));
    // アプリごとにまとめる（並びは裏のスレッドが決めた順）。
    let mut by_app: BTreeMap<&str, Vec<(usize, &Hit)>> = BTreeMap::new();
    for (index, hit) in snapshot.hits.iter().enumerate() {
        by_app.entry(&hit.app).or_default().push((index, hit));
    }
    // 開いているフォルダ（この端末の DB のもの）と、そこに付いている仕分け。
    let node = &tree.nodes[open_folder];
    let open_entry = node.entry.filter(|_| node.remote.is_none() && node.is_dir);
    let open_assignment = open_entry.and_then(|e| {
        snapshot
            .hits
            .iter()
            .find(|h| h.assignment.is_some() && h.entry_id() == e)
            .cloned()
    });
    let mut apps: Vec<String> = snapshot.apps.clone();
    apps.sort();
    apps.dedup();

    commands.entity(list).with_children(|list| {
        list.spawn((Text::new(header), big.clone(), TextColor(TEXT_COLOR)));
        list.spawn((
            Text::new(format!(
                "型 {} 個。表示と仕分けだけで、ファイルは動かしません。",
                snapshot.catalogs
            )),
            small.clone(),
            TextColor(DIM_COLOR),
        ));
        for error in &snapshot.errors {
            list.spawn((
                Text::new(format!("読めない型: {error}")),
                small.clone(),
                TextColor(ERROR_COLOR),
            ));
        }

        // 開いているフォルダの仕分け。
        if let Some(entry) = open_entry {
            section(
                list,
                &big,
                &format!("このフォルダ（{}）の仕分け", node.name),
            );
            if let Some(hit) = &open_assignment {
                let id = hit.assignment.unwrap_or(0);
                let text = format!("{} に仕分け済み", hit.app);
                chip_row(
                    list,
                    &small,
                    &[(format!("{text}  ×外す"), hit.color)],
                    move |_| crate::catalog::unassign(id),
                );
            } else if apps.is_empty() {
                list.spawn((
                    Text::new(
                        "仕分け先のアプリがまだありません（型か MCP の lisa_assign で足せます）",
                    ),
                    small.clone(),
                    TextColor(DIM_COLOR),
                ));
            } else {
                let chips: Vec<(String, Color)> = apps
                    .iter()
                    .map(|a| {
                        let rgb = lisa_core::catalog::app_color(a);
                        (a.clone(), Color::srgb_u8(rgb[0], rgb[1], rgb[2]))
                    })
                    .collect();
                let apps = apps.clone();
                chip_row(list, &small, &chips, move |i| {
                    crate::catalog::assign(entry, &apps[i], None)
                });
            }
        }

        // 当たりの一覧。
        for (app, hits) in &by_app {
            let sum: u64 = hits.iter().map(|(_, h)| h.size).sum();
            section(
                list,
                &big,
                &format!("{app}  {} か所・{}", hits.len(), human(sum)),
            );
            for &(index, hit) in hits {
                let selected = view.selected == Some(index);
                let chain = hit.chain.clone();
                let title = if hit.label == hit.name || hit.label == hit.app {
                    hit.name.clone()
                } else {
                    format!("{}  {}", hit.label, hit.name)
                };
                list.spawn((
                    Node {
                        flex_direction: FlexDirection::Column,
                        padding: UiRect::axes(Val::Px(8.0), Val::Px(4.0)),
                        border_radius: BorderRadius::all(Val::Px(4.0)),
                        border: UiRect::left(Val::Px(4.0)),
                        flex_shrink: 0.0,
                        ..default()
                    },
                    BorderColor::all(hit.color),
                    BackgroundColor(if selected { SELECTED_COLOR } else { ROW_COLOR }),
                    children![
                        (
                            Text::new(format!("{title}{}", state_text(hit.state))),
                            big.clone(),
                            TextColor(if selected { hit.color } else { TEXT_COLOR }),
                        ),
                        (
                            Text::new(format!("{}  {}", human(hit.size), hit.path.display())),
                            small.clone(),
                            TextColor(DIM_COLOR),
                        )
                    ],
                ))
                .observe(
                    move |_: On<PointerClick>,
                          mut view: ResMut<CatalogView>,
                          mut commands: Commands| {
                        if view.selected == Some(index) {
                            commands.insert_resource(JumpTo(chain.clone()));
                        } else {
                            view.selected = Some(index);
                        }
                    },
                );
            }
        }

        // どれにも当たらない大きなフォルダ。
        if !snapshot.unclassified.is_empty() {
            section(list, &big, "どの型にも当たらない大きなフォルダ");
            list.spawn((
                Text::new("押すとそこへ寄ります。開いてから上の仕分けで付けられます。"),
                small.clone(),
                TextColor(DIM_COLOR),
            ));
            for big_folder in &snapshot.unclassified {
                let chain = big_folder.chain.clone();
                list.spawn((
                    Node {
                        padding: UiRect::axes(Val::Px(8.0), Val::Px(3.0)),
                        border_radius: BorderRadius::all(Val::Px(4.0)),
                        flex_shrink: 0.0,
                        ..default()
                    },
                    BackgroundColor(ROW_COLOR),
                    children![(
                        Text::new(format!(
                            "{}  {}",
                            human(big_folder.size),
                            big_folder.path.display()
                        )),
                        small.clone(),
                        TextColor(TEXT_COLOR)
                    )],
                ))
                .observe(move |_: On<PointerClick>, mut commands: Commands| {
                    commands.insert_resource(JumpTo(chain.clone()));
                });
            }
        }
        // LAN の相手（相手の Lisa が当てた結果）と、相手にしか無い型。
        let mut devices: Vec<_> = remote.by_device.values().collect();
        devices.sort_by(|a, b| a.name.cmp(&b.name));
        for device in devices {
            section(
                list,
                &big,
                &format!("{} の当たり  {} か所", device.name, device.hits.len()),
            );
            if let Some(error) = &device.error {
                list.spawn((
                    Text::new(format!("読めませんでした: {error}")),
                    small.clone(),
                    TextColor(ERROR_COLOR),
                ));
            }
            let mut by_app: BTreeMap<&str, usize> = BTreeMap::new();
            for hit in &device.hits {
                *by_app.entry(&hit.app).or_default() += 1;
            }
            if !by_app.is_empty() {
                let text = by_app
                    .iter()
                    .map(|(a, n)| format!("{a} {n}"))
                    .collect::<Vec<_>>()
                    .join("・");
                list.spawn((Text::new(text), small.clone(), TextColor(DIM_COLOR)));
            }
            if !device.only_there.is_empty() {
                list.spawn((
                    Text::new("相手にしか無い型（押すと、こちらのカタログに取り込みます）"),
                    small.clone(),
                    TextColor(DIM_COLOR),
                ));
                let chips: Vec<(String, Color)> = device
                    .only_there
                    .iter()
                    .map(|c| {
                        let rgb = lisa_core::catalog::app_color(&c.app);
                        (
                            format!("取り込む: {}", c.name),
                            Color::srgb_u8(rgb[0], rgb[1], rgb[2]),
                        )
                    })
                    .collect();
                let texts: Vec<String> = device.only_there.iter().map(|c| c.text.clone()).collect();
                chip_row(
                    list,
                    &small,
                    &chips,
                    move |i| match crate::catalog::import(&texts[i]) {
                        Ok(path) => {
                            info!("カタログを取り込みました: {}", path.display());
                            crate::catalog::refresh_now();
                        }
                        Err(error) => warn!("カタログを取り込めませんでした: {error}"),
                    },
                );
            }
        }
        let roles: usize = snapshot.roles.len();
        if roles > 0 {
            let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
            for role in snapshot.roles.values() {
                *counts.entry(role_text(*role)).or_default() += 1;
            }
            let text = counts
                .iter()
                .map(|(r, n)| format!("{r} {n}"))
                .collect::<Vec<_>>()
                .join("・");
            list.spawn((
                Text::new(format!(
                    "中の役割: {text}（cache・install・log は暗く描きます）"
                )),
                small.clone(),
                TextColor(DIM_COLOR),
            ));
        }
    });
}

/// 見出し。
fn section(list: &mut ChildSpawnerCommands, font: &TextFont, text: &str) {
    list.spawn((
        Node {
            margin: UiRect::top(Val::Px(8.0)),
            flex_shrink: 0.0,
            ..default()
        },
        children![(Text::new(text), font.clone(), TextColor(DIM_COLOR))],
    ));
}

/// 押せる札を横に並べる。押したら `on_click(番号)`。
fn chip_row(
    list: &mut ChildSpawnerCommands,
    font: &TextFont,
    chips: &[(String, Color)],
    on_click: impl Fn(usize) + Send + Sync + Clone + 'static,
) {
    list.spawn(Node {
        flex_wrap: FlexWrap::Wrap,
        column_gap: Val::Px(6.0),
        row_gap: Val::Px(6.0),
        flex_shrink: 0.0,
        ..default()
    })
    .with_children(|row| {
        for (i, (text, color)) in chips.iter().enumerate() {
            let on_click = on_click.clone();
            row.spawn((
                Node {
                    padding: UiRect::axes(Val::Px(8.0), Val::Px(3.0)),
                    border_radius: BorderRadius::all(Val::Px(4.0)),
                    border: UiRect::left(Val::Px(4.0)),
                    ..default()
                },
                BorderColor::all(*color),
                BackgroundColor(CHIP_COLOR),
                children![(Text::new(text.clone()), font.clone(), TextColor(TEXT_COLOR))],
            ))
            .observe(move |_: On<PointerClick>| on_click(i));
        }
    });
}

/// パネルの上でホイールを回したら、一覧を動かす。
fn scroll_panel(
    view: Res<CatalogView>,
    mut wheel: MessageReader<MouseWheel>,
    window: Single<&Window>,
    list: Single<(&mut ScrollPosition, &ComputedNode), With<List>>,
) {
    let (mut scroll, node) = list.into_inner();
    // 一番下より先へは進めない（進めた分が溜まると、戻すときに動かなくなる）。中身をまだ並べていない
    // フレーム（作り直した直後）は、下の端が分からないので上だけ止める。
    let bottom = ((node.content_size.y - node.size.y + node.padding.max_inset.y).max(0.0)
        * node.inverse_scale_factor)
        .max(0.0);
    let laid_out = node.content_size.y > 0.0;
    let over = view.open
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
        let y = (scroll.y - dy).max(0.0);
        scroll.y = if laid_out { y.min(bottom) } else { y };
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
