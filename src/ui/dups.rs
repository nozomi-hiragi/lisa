//! 重複の一覧（画面の右側に固定）。メニューバーの「重複」で開け閉めする。
//!
//! 組は空く大きさの順に並べ、それぞれに数・合計・空く量を出す。組を押すと選んで（木の上で光る）、
//! その下にコピーの場所を出す。場所を押すと、木の上のそこへ飛ぶ。

use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::ui::ScrollPosition;
use bevy::window::RequestRedraw;
use lisa_core::dups::Group;
use lisa_core::listing::Kind;

use crate::dups::remote::{Scope, ViewState};
use crate::dups::requests::{Item, Requests};
use crate::dups::{Dups, DupsState};
use crate::render::palette;
use crate::render::scene::LabelFont;
use crate::scan::JumpTo;
use crate::ui::menu::BAR_PX;

use super::{SidePanelLayout, SidePanelList};

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

pub struct DupsPanelPlugin;

impl Plugin for DupsPanelPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<DevicePicker>()
            .add_systems(PostStartup, spawn_panel)
            .add_systems(
                Update,
                (show_panel.in_set(SidePanelLayout), fill_panel, device_hover),
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
                SidePanelList::default(),
            ));
        });
}

/// 開け閉め。右からすべり出し、その幅の分だけ木を映す領域を狭める（`view::set_side_px`）。
fn show_panel(
    dups: Res<Dups>,
    mut shown: Local<f32>,
    mut last: Local<Option<std::time::Instant>>,
    panel: Single<(&mut Node, &mut GlobalZIndex), With<Panel>>,
    mut list: Single<&mut SidePanelList, With<List>>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    let (mut panel, mut z_index) = panel.into_inner();
    let z = if dups.open { 7 } else { 6 };
    if z_index.0 != z {
        z_index.0 = z;
        redraw.write(RequestRedraw);
    }
    if list.closing == dups.open {
        list.closing = !dups.open;
    }
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

#[derive(Default, PartialEq)]
struct RequestUi {
    notice: Option<String>,
    quote: Option<crate::dups::requests::Quote>,
    busy: bool,
    shared: bool,
}

/// 一覧の中身を作り直す（一覧ができたときと、選んだ組が変わったとき）。
fn fill_panel(
    mut commands: Commands,
    dups: Res<Dups>,
    (store, names): (
        Option<Res<crate::scan::Store>>,
        Res<crate::model::display_path::DisplayNames>,
    ),
    (requests, mut request_ui): (Res<Requests>, Local<RequestUi>),
    font: Res<LabelFont>,
    list: Single<Entity, With<List>>,
    (picker, lan, mut previous_peers): (
        Res<DevicePicker>,
        Option<Res<crate::lan::LanState>>,
        Local<DeviceOptions>,
    ),
) {
    if !dups.open {
        return;
    }
    let mut peers = vec![(None, "この端末".into())];
    if let Some(lan) = lan.as_deref() {
        peers.extend(
            lan.paired_devices()
                .map(|(id, name)| (Some(id.into()), name.into())),
        );
    }
    let shared = lan.as_ref().and_then(|lan| lan.relay_port()).is_some();
    let request_changed = request_ui.shared != shared
        || (requests.is_changed()
            && (request_ui.notice != requests.notice
                || request_ui.quote != requests.quote
                || request_ui.busy != requests.busy()));
    if request_changed {
        *request_ui = RequestUi {
            notice: requests.notice.clone(),
            quote: requests.quote.clone(),
            busy: requests.busy(),
            shared,
        };
    }
    if !(dups.is_changed()
        || names.is_changed()
        || picker.is_changed()
        || request_changed
        || *previous_peers != peers)
    {
        return;
    }
    *previous_peers = peers;
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
        device_controls(list, &dups, picker.0, &previous_peers, &small);
        if dups.scope != Scope::default() {
            remote_rows(list, &dups, &requests, lan.as_deref(), &big, &small);
            return;
        }
        list.spawn((Text::new(header), big.clone(), TextColor(TEXT_COLOR)));
        if let Some(notice) = &requests.notice {
            list.spawn((Text::new(notice), small.clone(), TextColor(DIM_COLOR)));
        }
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
                dups.select_group(index);
            });
            if !selected {
                continue;
            }
            let paths = crate::model::display_path::entry_texts(
                &names,
                store.as_deref(),
                group
                    .copies
                    .iter()
                    .map(|copy| (copy.chain.last().copied(), &copy.path)),
            );
            let items = group
                .copies
                .iter()
                .zip(&paths)
                .map(|(copy, text)| Item {
                    device: None,
                    reference: lisa_core::organize::remote_keep::KeepReference {
                        device_id: String::new(),
                        drive_id: String::new(),
                        chain: copy.chain.clone(),
                        format: lisa_core::dups::COMPARISON_FORMAT,
                        key: lisa_core::dups::ContentKey {
                            directory: group.kind == Kind::Dir,
                            size: group.size,
                            hash: group.hash,
                        },
                        identity: [0; 2],
                    },
                    path: text.clone(),
                    local_path: Some(copy.path.clone()),
                })
                .collect::<Vec<_>>();
            for (index, (copy, mut text)) in group.copies.iter().zip(paths).enumerate() {
                let chain = copy.chain.clone();
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
                    children![(
                        Text::new(lisa_core::display_path::wrap_at_separators(&text)),
                        small.clone(),
                        TextColor(TEXT_COLOR),
                        bevy::text::TextLayout::linebreak(bevy::text::LineBreak::WordOrCharacter)
                    )],
                ))
                .observe(move |_: On<PointerClick>, mut commands: Commands| {
                    commands.insert_resource(JumpTo(chain.clone()));
                });
                selection_buttons(list, &dups, index, &items, &small);
            }
            confirmation(
                list,
                &dups,
                &requests,
                lan.as_deref(),
                &items,
                group.size,
                &small,
            );
        }
    });
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

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Picker {
    #[default]
    Closed,
    Target,
    Comparison,
}

#[derive(Resource, Default)]
struct DevicePicker(Picker);

#[derive(Component)]
struct DeviceButton;

type DeviceOptions = Vec<(Option<String>, String)>;
type HoveredDeviceButton = (With<DeviceButton>, Changed<Hovered>);

fn device_hover(mut buttons: Query<(&Hovered, &mut BackgroundColor), HoveredDeviceButton>) {
    for (interaction, mut color) in &mut buttons {
        *color = if !interaction.get() {
            super::menu::menu_button().1
        } else {
            BackgroundColor(ROW_COLOR)
        };
    }
}

fn device_controls(
    list: &mut ChildSpawnerCommands,
    dups: &Dups,
    picker: Picker,
    peers: &[(Option<String>, String)],
    font: &TextFont,
) {
    let label = |id: &Option<String>| {
        peers
            .iter()
            .find(|(peer, _)| peer == id)
            .map(|(_, name)| name.clone())
            .unwrap_or_else(|| "ペアリング解除済み".into())
    };
    let values = [
        (
            Picker::Target,
            format!("対象端末　{}", label(&dups.scope.target)),
        ),
        (
            Picker::Comparison,
            format!(
                "比較する端末　{}",
                dups.scope
                    .comparison
                    .as_ref()
                    .map(label)
                    .unwrap_or_else(|| "なし".into())
            ),
        ),
    ];
    for (which, value) in values {
        list.spawn((
            DeviceButton,
            Hovered::default(),
            super::menu::menu_button(),
            children![(Text::new(value), font.clone(), TextColor(TEXT_COLOR),)],
        ))
        .observe(
            move |_: On<PointerClick>, mut picker: ResMut<DevicePicker>| {
                picker.0 = if picker.0 == which {
                    Picker::Closed
                } else {
                    which
                };
            },
        );
        if picker != which {
            continue;
        }
        let options = peers
            .iter()
            .map(|(id, name)| (Some(id.clone()), name.clone()));
        let options: Vec<_> = if which == Picker::Target {
            options.collect()
        } else {
            std::iter::once((None, "なし".into()))
                .chain(options.filter(|(id, _)| id.as_ref() != Some(&dups.scope.target)))
                .collect()
        };
        for (id, name) in options {
            list.spawn((
                DeviceButton,
                Hovered::default(),
                super::menu::menu_button(),
                children![(Text::new(name), font.clone(), TextColor(TEXT_COLOR),)],
            ))
            .observe(
                move |_: On<PointerClick>,
                      mut dups: ResMut<Dups>,
                      mut picker: ResMut<DevicePicker>,
                      mut commands: Commands| {
                    let mut scope = dups.scope.clone();
                    if which == Picker::Target {
                        scope.target = id.clone().flatten();
                        if scope.comparison.as_ref() == Some(&scope.target) {
                            scope.comparison = None;
                        }
                    } else {
                        scope.comparison.clone_from(&id);
                    }
                    commands.remove_resource::<crate::lan::RemoteJump>();
                    dups.set_scope(scope);
                    picker.0 = Picker::Closed;
                },
            );
        }
    }
}

fn remote_rows(
    list: &mut ChildSpawnerCommands,
    dups: &Dups,
    requests: &Requests,
    lan: Option<&crate::lan::LanState>,
    big: &TextFont,
    small: &TextFont,
) {
    let status = |list: &mut ChildSpawnerCommands, text: String| {
        list.spawn((Text::new(text), small.clone(), TextColor(DIM_COLOR)));
    };
    list.spawn((
        DeviceButton,
        Hovered::default(),
        super::menu::menu_button(),
        children![(Text::new("更新"), small.clone(), TextColor(TEXT_COLOR),)],
    ))
    .observe(|_: On<PointerClick>, mut dups: ResMut<Dups>| {
        dups.invalidate();
    });
    let view = match &dups.remote {
        ViewState::Local => return,
        ViewState::Computing(devices) => {
            for (device, progress) in devices {
                let progress = progress
                    .map(|(done, pages)| format!(" {done}/{pages} ページ"))
                    .unwrap_or_default();
                status(list, format!("{device}　取得中…{progress}"));
            }
            return;
        }
        ViewState::Failed(error) => {
            status(list, error.clone());
            return;
        }
        ViewState::Ready(view) => view,
    };
    let time = view
        .fetched_at
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let time = super::tooltip::format_local(time.try_into().unwrap_or(i64::MAX));
    for (device, complete) in &view.devices {
        let hash = if *complete {
            ""
        } else {
            "・ハッシュ未完了"
        };
        status(list, format!("{device}　取得 {time}{hash}"));
    }
    if let Some(notice) = &requests.notice {
        status(list, notice.clone());
    }
    if view.devices.iter().all(|(_, complete)| *complete) || !view.groups.is_empty() {
        status(list, format!("重複  {} 組", view.groups.len()));
    }
    for (index, group) in view.groups.iter().enumerate() {
        let selected = dups.selected == Some(index);
        let name = group
            .copies
            .first()
            .and_then(|copy| copy.location.parts.last())
            .cloned()
            .unwrap_or_default();
        let kind = if group.key.directory {
            "フォルダ"
        } else {
            "ファイル"
        };
        list.spawn((
            super::menu::menu_button().0,
            BackgroundColor(if selected { SELECTED_COLOR } else { ROW_COLOR }),
            children![(
                Text::new(format!(
                    "{kind}  {name}・{} か所・{}",
                    group.copies.len(),
                    human(group.key.size)
                )),
                big.clone(),
                TextColor(TEXT_COLOR)
            )],
        ))
        .observe(move |_: On<PointerClick>, mut dups: ResMut<Dups>| {
            dups.select_group(index);
        });
        if !selected {
            continue;
        }
        let items = group
            .copies
            .iter()
            .map(|copy| Item {
                device: copy.device_id.clone(),
                reference: lisa_core::organize::remote_keep::KeepReference {
                    device_id: copy.device_id.clone().unwrap_or_default(),
                    drive_id: copy.location.drive_id.clone(),
                    chain: copy.location.chain.clone(),
                    format: lisa_core::dups::COMPARISON_FORMAT,
                    key: group.key.clone(),
                    identity: [0; 2],
                },
                path: lisa_core::display_path::text(&copy.location.parts),
                local_path: None,
            })
            .collect::<Vec<_>>();
        for (index, copy) in group.copies.iter().enumerate() {
            let copy = copy.clone();
            let text = lisa_core::display_path::text(&copy.location.parts);
            list.spawn((
                DeviceButton,
                Hovered::default(),
                super::menu::menu_button(),
                children![(
                    Text::new(lisa_core::display_path::wrap_at_separators(&text)),
                    small.clone(),
                    TextColor(TEXT_COLOR),
                    bevy::text::TextLayout::linebreak(bevy::text::LineBreak::WordOrCharacter),
                )],
            ))
            .observe(move |_: On<PointerClick>, mut commands: Commands| {
                if let Some(device) = &copy.device_id {
                    commands.insert_resource(crate::lan::RemoteJump {
                        device: device.clone(),
                        chain: copy.location.chain.clone(),
                    });
                } else {
                    commands.insert_resource(JumpTo(copy.location.chain.clone()));
                }
            });
            selection_buttons(list, dups, index, &items, small);
        }
        confirmation(list, dups, requests, lan, &items, group.key.size, small);
    }
}

fn selection_buttons(
    list: &mut ChildSpawnerCommands,
    dups: &Dups,
    index: usize,
    items: &[Item],
    font: &TextFont,
) {
    let kept = dups.selection.keep == Some(index);
    let selected = dups.selection.sources.contains(&index);
    let other_target = dups
        .selection
        .sources
        .first()
        .is_some_and(|first| items[*first].device != items[index].device);
    list.spawn(Node {
        column_gap: Val::Px(6.0),
        margin: UiRect::left(Val::Px(12.0)),
        ..default()
    })
    .with_children(|row| {
        row.spawn((
            DeviceButton,
            Hovered::default(),
            super::menu::menu_button(),
            children![(
                Text::new(if kept { "残す ✓" } else { "残す" }),
                font.clone(),
                TextColor(if kept { ACCENT } else { TEXT_COLOR })
            )],
        ))
        .observe(move |_: On<PointerClick>, mut dups: ResMut<Dups>| {
            dups.selection.choose_keep(index)
        });
        let label = if other_target {
            "別端末の項目"
        } else if selected {
            "整理対象 ✓"
        } else {
            "整理対象"
        };
        if kept || other_target {
            row.spawn((Text::new(label), font.clone(), TextColor(DIM_COLOR)));
        } else {
            row.spawn((
                DeviceButton,
                Hovered::default(),
                super::menu::menu_button(),
                children![(
                    Text::new(label),
                    font.clone(),
                    TextColor(if selected { ACCENT } else { TEXT_COLOR })
                )],
            ))
            .observe(move |_: On<PointerClick>, mut dups: ResMut<Dups>| {
                dups.selection.toggle_source(index)
            });
        }
    });
}

fn confirmation(
    list: &mut ChildSpawnerCommands,
    dups: &Dups,
    requests: &Requests,
    lan: Option<&crate::lan::LanState>,
    items: &[Item],
    size: u64,
    font: &TextFont,
) {
    let keep = dups
        .selection
        .keep
        .and_then(|index| items.get(index))
        .cloned();
    let sources = dups
        .selection
        .sources
        .iter()
        .filter_map(|index| items.get(*index).cloned())
        .collect::<Vec<_>>();
    let (keep, source) = match (keep, sources.first()) {
        (Some(keep), Some(source)) => (keep, source),
        (keep, source) => {
            let guidance = match (keep.is_some(), source.is_some()) {
                (true, false) => "「整理対象」も 1 つ以上選ぶと、整理計画を確認できます",
                (false, true) => "「残す」を 1 つ選ぶと、整理計画を確認できます",
                _ => return,
            };
            list.spawn((Text::new(guidance), font.clone(), TextColor(DIM_COLOR)));
            return;
        }
    };
    let mut lines = vec![format!("残す　{}", keep.path)];
    lines.extend(sources.iter().map(|item| format!("隔離　{}", item.path)));
    lines.push(format!(
        "{} 件・合計 {}",
        sources.len(),
        human(size.saturating_mul(sources.len() as u64))
    ));
    for line in lines {
        list.spawn((
            Text::new(lisa_core::display_path::wrap_at_separators(&line)),
            font.clone(),
            TextColor(TEXT_COLOR),
            bevy::text::TextLayout::linebreak(bevy::text::LineBreak::WordOrCharacter),
        ));
    }
    let quote = requests.quote.as_ref().filter(|quote| {
        quote.revision == dups.refresh && quote.keep == keep && quote.sources == sources
    });
    let blocked_reason = if keep.device != source.device && source.device.is_none() {
        Some("この端末が隔離対象の端末間整理は比較表示のみです")
    } else if keep.device != source.device
        && lan.and_then(crate::lan::LanState::relay_port).is_none()
    {
        Some("依頼元の LAN 公開を有効にしてください")
    } else {
        None
    };
    let quote_error = quote
        .and_then(|quote| quote.result.as_ref().err())
        .map(String::as_str);
    let label = if source.device.is_some() {
        "対象PCに整理を依頼"
    } else {
        "整理計画を作る"
    };
    if let Some(reason) = blocked_reason.or(quote_error) {
        list.spawn((Text::new(reason), font.clone(), TextColor(DIM_COLOR)));
    }
    let preview = quote.is_none() || quote_error.is_some();
    let revision = dups.refresh;
    let group = dups.selected;
    if blocked_reason.is_none() && !requests.busy() {
        let label = if quote_error.is_some() {
            "もう一度確認"
        } else if preview {
            "整理内容を確認"
        } else {
            label
        };
        list.spawn((
            DeviceButton,
            Hovered::default(),
            super::menu::menu_button(),
            children![(Text::new(label), font.clone(), TextColor(TEXT_COLOR))],
        ))
        .observe(
            move |_: On<PointerClick>,
                  mut requests: ResMut<Requests>,
                  lan: Option<Res<crate::lan::LanState>>,
                  dups: Res<Dups>| {
                if dups.open && dups.refresh == revision && dups.selected == group {
                    requests.submit(
                        lan.as_deref(),
                        keep.clone(),
                        sources.clone(),
                        revision,
                        preview,
                    );
                }
            },
        );
    } else {
        list.spawn((Text::new(label), font.clone(), TextColor(DIM_COLOR)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dups::requests::Quote;
    use bevy::ecs::system::RunSystemOnce;

    fn item(name: &str, device: Option<&str>) -> Item {
        Item {
            device: device.map(String::from),
            reference: lisa_core::organize::remote_keep::KeepReference {
                device_id: device.unwrap_or("local").into(),
                drive_id: "fixture".into(),
                chain: vec![1, 2],
                format: lisa_core::dups::COMPARISON_FORMAT,
                key: lisa_core::dups::ContentKey {
                    directory: false,
                    size: 1,
                    hash: [0; 32],
                },
                identity: [0; 2],
            },
            path: name.into(),
            local_path: None,
        }
    }

    fn confirmation_world(
        keep: Option<usize>,
        source: Option<usize>,
        blocked: bool,
        failed: bool,
    ) -> World {
        let items = [
            item("keep", blocked.then_some("peer")),
            item("source", None),
        ];
        let mut dups = Dups {
            open: true,
            ..Default::default()
        };
        dups.selection.keep = keep;
        dups.selection.sources.extend(source);
        let quote = failed.then(|| Quote {
            revision: dups.refresh,
            keep: items[0].clone(),
            sources: vec![items[1].clone()],
            result: Err("一時的な確認エラー".into()),
        });

        let mut world = World::new();
        world.insert_resource(dups);
        world.insert_resource(Requests::polling_for_test(quote));
        world
            .run_system_once(
                move |mut commands: Commands, dups: Res<Dups>, requests: Res<Requests>| {
                    commands.spawn_empty().with_children(|list| {
                        confirmation(
                            list,
                            &dups,
                            &requests,
                            None,
                            &items,
                            1,
                            &TextFont::default(),
                        );
                    });
                },
            )
            .unwrap();
        world
    }

    fn texts(world: &mut World) -> Vec<String> {
        world
            .query::<&Text>()
            .iter(world)
            .map(|text| text.0.clone())
            .collect()
    }

    #[test]
    fn confirmation_guidance_matches_all_four_selection_states() {
        for (keep, source, guidance) in [
            (None, None, None),
            (
                Some(0),
                None,
                Some("「整理対象」も 1 つ以上選ぶと、整理計画を確認できます"),
            ),
            (
                None,
                Some(1),
                Some("「残す」を 1 つ選ぶと、整理計画を確認できます"),
            ),
            (Some(0), Some(1), None),
        ] {
            let mut world = confirmation_world(keep, source, false, false);
            let labels = texts(&mut world);
            match guidance {
                Some(guidance) => assert_eq!(labels, [guidance]),
                None if keep.is_some() => {
                    assert!(labels.iter().any(|label| label == "整理内容を確認"));
                    assert!(!labels.iter().any(|label| label.starts_with('「')));
                }
                None => assert!(labels.is_empty()),
            }
        }
    }

    #[test]
    fn blocked_confirmation_does_not_offer_retry_even_with_a_quote_error() {
        let mut world = confirmation_world(Some(0), Some(1), true, true);
        let labels = texts(&mut world);
        assert!(
            labels
                .iter()
                .any(|label| label == "この端末が隔離対象の端末間整理は比較表示のみです")
        );
        assert!(!labels.iter().any(|label| label == "もう一度確認"));
        assert_eq!(world.query::<&DeviceButton>().iter(&world).count(), 0);
    }

    #[test]
    fn retry_click_sends_preview_only_while_the_panel_is_open() {
        use bevy::camera::NormalizedRenderTarget;
        use bevy::picking::{
            backend::HitData,
            events::Pointer,
            pointer::{Location, PointerId},
        };

        let mut world = confirmation_world(Some(0), Some(1), false, true);
        assert!(
            texts(&mut world)
                .iter()
                .any(|label| label == "一時的な確認エラー")
        );
        let button = world
            .query::<(&Text, &ChildOf)>()
            .iter(&world)
            .find(|(text, _)| text.0 == "もう一度確認")
            .unwrap()
            .1
            .parent();
        let event = PointerClick {
            entity: button,
            pointer: Pointer::new(
                PointerId::Mouse,
                Location {
                    target: NormalizedRenderTarget::None {
                        width: 1,
                        height: 1,
                    },
                    position: Vec2::ZERO,
                },
            ),
            button: PointerButton::Primary,
            hit: HitData::new(Entity::PLACEHOLDER, 0.0, None, None),
            duration: std::time::Duration::ZERO,
            count: 1,
        };
        world.resource_mut::<Dups>().open = false;
        world.trigger(event.clone());
        assert_eq!(world.resource::<Requests>().queued_preview_for_test(), None);

        world.resource_mut::<Dups>().open = true;
        world.trigger(event);
        assert_eq!(
            world.resource::<Requests>().queued_preview_for_test(),
            Some(true)
        );
    }
}
