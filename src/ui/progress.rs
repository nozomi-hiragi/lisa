//! 読み込み中のリスト（画面の左下に固定）。今読んでいるフォルダと、このあと読むフォルダ、
//! 読んだ数と速さを出す。読み終えたらハッシュの進み具合（今のフォルダ、数、大きさ、速さ）を出し、
//! それも終えたら少しして消す。

use std::time::{Duration, Instant};

use bevy::picking::hover::Hovered;
use bevy::prelude::*;

use crate::render::palette;
use crate::render::scene::LabelFont;
use crate::scan::ScanProgress;

const PANEL_COLOR: Color = palette::base_100_alpha(0.92);
const TEXT_COLOR: Color = palette::BASE_CONTENT;
const DIM_COLOR: Color = palette::MUTED;
/// 書き換える間隔の下限（毎フレーム文字を組み直さないように）。
const UPDATE_EVERY: Duration = Duration::from_millis(200);
/// 読み終えてから消すまで。
const HIDE_AFTER: Duration = Duration::from_secs(4);
/// 場所をこの文字数より長ければ、中央を「…」で省く。
const MAX_CHARS: usize = 70;

pub struct ProgressPlugin;

impl Plugin for ProgressPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(PostStartup, spawn_panel)
            .add_systems(Update, (show_progress, boost_hover));
    }
}

#[derive(Component)]
struct Panel;

#[derive(bevy::ecs::system::SystemParam)]
pub(crate) struct PanelPointer<'w, 's> {
    panel: Query<
        'w,
        's,
        (
            &'static ComputedNode,
            &'static bevy::ui::UiGlobalTransform,
            &'static Visibility,
        ),
        With<Panel>,
    >,
}

impl PanelPointer<'_, '_> {
    pub(crate) fn contains(&self, window: &Window, cursor: Option<Vec2>) -> bool {
        let Some(point) = cursor.map(|point| point * window.scale_factor()) else {
            return false;
        };
        self.panel.iter().any(|(node, transform, visibility)| {
            *visibility != Visibility::Hidden
                && node.size().min_element() > 0.0
                && node.contains_point(*transform, point)
        })
    }
}

#[derive(Component)]
enum ProgressLine {
    Summary,
    Details,
    Paths,
    BoostLabel,
}

#[derive(Component)]
struct BoostButton;

fn spawn_panel(mut commands: Commands, font: Res<LabelFont>, progress: Option<Res<ScanProgress>>) {
    if progress.is_none() {
        return;
    }
    let font = TextFont::from_font_size(12.0).with_font(font.0.clone());
    commands
        .spawn((
            Panel,
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(12.0),
                bottom: Val::Px(12.0),
                padding: UiRect::axes(Val::Px(10.0), Val::Px(6.0)),
                flex_direction: FlexDirection::Column,
                row_gap: Val::Px(2.0),
                border_radius: BorderRadius::all(Val::Px(6.0)),
                ..default()
            },
            BackgroundColor(PANEL_COLOR),
            GlobalZIndex(5),
            Pickable::default(),
        ))
        .with_children(|panel| {
            panel
                .spawn((
                    Node {
                        align_items: AlignItems::Center,
                        column_gap: Val::Px(8.0),
                        ..default()
                    },
                    Pickable::IGNORE,
                ))
                .with_children(|row| {
                    row.spawn((
                        ProgressLine::Summary,
                        // 走査からハッシュへ移るときや連打したときにも、ボタンを同じ場所に置く。
                        Node {
                            min_width: Val::Px(144.0),
                            ..default()
                        },
                        Text::new("読み込み中"),
                        font.clone(),
                        TextColor(TEXT_COLOR),
                        Pickable::IGNORE,
                    ));
                    let (mut button, color) = super::menu::menu_button();
                    button.display = Display::None;
                    button.width = Val::Px(82.0);
                    button.justify_content = JustifyContent::Center;
                    button.border = UiRect::all(Val::Px(1.0));
                    row.spawn((
                        BoostButton,
                        button,
                        color,
                        BorderColor::all(palette::NEUTRAL),
                        Hovered::default(),
                        children![(
                            ProgressLine::BoostLabel,
                            Text::new("ブースト"),
                            font.clone(),
                            TextColor(TEXT_COLOR),
                            Pickable::IGNORE,
                        )],
                    ))
                    .observe(toggle_boost);
                    row.spawn((
                        ProgressLine::Details,
                        Text::new(""),
                        font.clone(),
                        TextColor(TEXT_COLOR),
                        Pickable::IGNORE,
                    ));
                });
            panel.spawn((
                ProgressLine::Paths,
                Text::new(""),
                font,
                TextColor(DIM_COLOR),
                Pickable::IGNORE,
            ));
        });
}

fn toggle_boost(click: On<PointerClick>, control: Option<Res<crate::scan::ScanControl>>) {
    if click.button == PointerButton::Primary
        && let Some(control) = control
    {
        control.control.set_boost(!control.control.boost_status().1);
    }
}

type ChangedBoostHover = (With<BoostButton>, Changed<Hovered>);

fn boost_hover(mut buttons: Query<(&Hovered, &mut BackgroundColor), ChangedBoostHover>) {
    for (hovered, mut color) in &mut buttons {
        color.0 = if hovered.get() {
            palette::BASE_100
        } else {
            palette::NEUTRAL
        };
    }
}

fn show_progress(
    progress: Option<Res<ScanProgress>>,
    tree: Res<crate::model::tree::FileTree>,
    control: Option<Res<crate::scan::ScanControl>>,
    mut last: Local<Option<Instant>>,
    mut panel: Query<&mut Visibility, With<Panel>>,
    mut button: Query<&mut Node, With<BoostButton>>,
    mut text: Query<(&ProgressLine, &mut Text)>,
) {
    let Some(progress) = progress else {
        return;
    };
    if last.is_some_and(|t| t.elapsed() < UPDATE_EVERY) {
        return;
    }
    *last = Some(Instant::now());
    let Ok(p) = progress.0.lock() else {
        return;
    };
    let elapsed = p
        .finished
        .unwrap_or_else(Instant::now)
        .duration_since(p.started.unwrap_or_else(Instant::now))
        .as_secs_f64();
    let rate = p.done as f64 / elapsed.max(1e-3);
    let failed = if p.failed > 0 {
        format!("（失敗 {}）", p.failed)
    } else {
        String::new()
    };
    let hash = &p.hash;
    let (working, boosted, priority_failed) = control
        .as_ref()
        .map_or((false, false, false), |c| c.control.boost_status());
    let mode = if boosted { "・ブースト中" } else { "" };
    let display = if working {
        Display::Flex
    } else {
        Display::None
    };
    if let Ok(mut node) = button.single_mut()
        && node.display != display
    {
        node.display = display;
    }
    let (head, details, body) = match (p.finished, hash.started, hash.finished) {
        // 読み終えて、ハッシュも終えた（か、ハッシュしない）。
        (Some(at), started, finished) if !working && (started.is_none() || finished.is_some()) => {
            let at = finished.unwrap_or(at);
            if let Ok(mut v) = panel.single_mut() {
                *v = if at.elapsed() >= HIDE_AFTER {
                    Visibility::Hidden
                } else {
                    Visibility::Inherited
                };
            }
            let head = match started {
                Some(_) => format!(
                    "読み終えました  {} フォルダ{failed}・ハッシュ {} ファイル（{}）",
                    p.done,
                    hash.files,
                    bytes(hash.bytes)
                ),
                None => format!(
                    "読み終えました  {} フォルダ{failed}・{elapsed:.1} 秒",
                    p.done
                ),
            };
            (head, String::new(), String::new())
        }
        // ハッシュしている。
        (Some(_), Some(started), None) => {
            if let Ok(mut v) = panel.single_mut() {
                *v = Visibility::Inherited;
            }
            let seconds = started.elapsed().as_secs_f64().max(1e-3);
            let speed = hash.bytes as f64 / seconds;
            let mut body = String::new();
            if let Some(folder) = &hash.folder {
                body = format!(
                    "▶ {}",
                    crate::model::display_path::mounted_tree_text(&tree, folder, MAX_CHARS)
                );
            }
            (
                format!("ハッシュ中{mode}"),
                format!(
                    "{} ファイル・{}・1 秒に {}",
                    hash.files,
                    bytes(hash.bytes),
                    bytes(speed as u64)
                ),
                body,
            )
        }
        _ => {
            if let Ok(mut v) = panel.single_mut() {
                *v = Visibility::Inherited;
            }
            let mut body = Vec::new();
            if let Some(current) = &p.current {
                body.push(format!(
                    "▶ {}",
                    crate::model::display_path::mounted_tree_text(&tree, current, MAX_CHARS)
                ));
            }
            body.extend(p.next.iter().map(|path| {
                format!(
                    "   {}",
                    crate::model::display_path::mounted_tree_text(&tree, path, MAX_CHARS)
                )
            }));
            (
                format!("読み込み中{mode}"),
                format!(
                    "{} フォルダ{failed}・あと {}・1 秒に {rate:.0}",
                    p.done, p.queued
                ),
                body.join("\n"),
            )
        }
    };
    // 裏の分を休んでいるなら、その理由を添える。
    let rest = control.map_or(crate::scan::Rest::No, |c| c.control.rest());
    let mut details = match rest {
        _ if !working => details,
        crate::scan::Rest::No => details,
        crate::scan::Rest::Paused => format!("{details}（一時停止中）"),
        crate::scan::Rest::Battery => format!("{details}（電池なので休み中）"),
        crate::scan::Rest::Cpu => format!("{details}（CPU が混んでいるので休み中）"),
        crate::scan::Rest::Disk => format!("{details}（ディスクが混んでいるので休み中）"),
    };
    if working && priority_failed {
        details.push_str("（OS優先度の変更に失敗）");
    }
    for (line, mut text) in &mut text {
        let next = match line {
            ProgressLine::Summary => head.as_str(),
            ProgressLine::Details => details.as_str(),
            ProgressLine::Paths => body.as_str(),
            ProgressLine::BoostLabel if boosted => "通常に戻す",
            ProgressLine::BoostLabel => "ブースト",
        };
        if text.0 != next {
            text.0 = next.into();
        }
    }
}

/// 読みやすい大きさ（1024 区切り）。
pub(crate) fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::ecs::system::RunSystemOnce;
    use bevy::math::DVec2;
    use bevy::ui::UiGlobalTransform;

    #[test]
    fn パネルの実寸と非表示に追従する() {
        let mut world = World::new();
        let window_id = world.spawn(Window::default()).id();
        let panel = world
            .spawn((
                Panel,
                Visibility::Inherited,
                ComputedNode::default(),
                UiGlobalTransform::from_xy(160., 400.),
            ))
            .id();
        for (size, point, visible, expected) in [
            (Vec2::new(200., 40.), DVec2::new(160., 410.), true, true),
            (Vec2::new(200., 40.), DVec2::new(160., 430.), true, false),
            (Vec2::new(400., 100.), DVec2::new(160., 430.), true, true),
            (Vec2::new(400., 100.), DVec2::new(160., 430.), false, false),
        ] {
            world.get_mut::<ComputedNode>(panel).unwrap().size = size;
            *world.get_mut::<Visibility>(panel).unwrap() = if visible {
                Visibility::Inherited
            } else {
                Visibility::Hidden
            };
            world
                .get_mut::<Window>(window_id)
                .unwrap()
                .set_physical_cursor_position(Some(point));
            let hit = world
                .run_system_once(|panel: PanelPointer, window: Single<&Window>| {
                    panel.contains(&window, window.cursor_position())
                })
                .unwrap();
            assert_eq!(hit, expected);
        }
    }
}
