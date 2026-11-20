//! 整理の UI（許可のポップアップ、移動先を選ぶ案内、右から出る履歴パネル）。
//!
//! - 許可のポップアップは `src/lan.rs` のペアリングのポップアップと同じ見た目・配置（上部左寄せ）で、
//!   複数あれば古い順に 1 つずつ出す。ボタンは「許可」「危ないものを除いて許可」（危ないものがあるときだけ）
//!   「断る」「場所を見る」（対象を木の上で光らせる）。
//! - 履歴のパネルはメニューバーの「整理」で開け閉めし、重複・カタログのパネルと同じく右からすべり出す
//!   （同時には開かない）。

use bevy::input::mouse::{MouseScrollUnit, MouseWheel};
use bevy::prelude::*;
use bevy::ui::ScrollPosition;
use bevy::window::RequestRedraw;
use lisa_core::organize::{OpKind, OpState, Plan, PlanState};

use crate::catalog::CatalogView;
use crate::dups::Dups;
use crate::lan::PairBanner;
use crate::organize::{ConfirmPurge, OrganizeState, PendingPlan, PlanDetails};
use crate::render::scene::LabelFont;
use crate::scan::JumpTo;
use crate::ui::menu::BAR_PX;

/// 履歴パネルの幅（論理ピクセル。重複・カタログと同じ）。
pub const PANEL_PX: f32 = 380.0;

/// 許可ポップアップに出す危ない操作の最大件数。
const MAX_SHOWN_RISKY: usize = 20;

const PANEL_COLOR: Color = Color::srgb(0.11, 0.11, 0.14);
const POPUP_COLOR: Color = Color::srgba(0.13, 0.13, 0.16, 0.96);
const POPUP_BORDER: Color = Color::srgb(0.28, 0.28, 0.35);
const ROW_COLOR: Color = Color::srgb(0.16, 0.16, 0.2);
const SELECTED_COLOR: Color = Color::srgb(0.22, 0.22, 0.28);
const BUTTON_COLOR: Color = Color::srgb(0.22, 0.22, 0.27);
const ALLOW_COLOR: Color = Color::srgb(0.22, 0.38, 0.28);
const WARN_BTN_COLOR: Color = Color::srgb(0.42, 0.33, 0.18);
const REJECT_COLOR: Color = Color::srgb(0.42, 0.22, 0.24);
const ACTIVE_BTN_COLOR: Color = Color::srgb(0.32, 0.28, 0.18);
const TEXT_COLOR: Color = Color::srgb(0.9, 0.9, 0.92);
const DIM_COLOR: Color = Color::srgb(0.6, 0.6, 0.66);
const RISKY_COLOR: Color = Color::srgb(0.95, 0.45, 0.42);
const OK_COLOR: Color = Color::srgb(0.55, 0.85, 0.62);
const ACCENT: Color = Color::srgb(0.949, 0.784, 0.443);

/// すべり出しにかける時間（秒）。
const SLIDE_SECONDS: f32 = 0.22;
/// 1 回のホイールで動かす量（行単位のとき）。
const LINE_PX: f32 = 24.0;

pub struct OrganizePanelPlugin;

impl Plugin for OrganizePanelPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(PostStartup, spawn_ui).add_systems(
            Update,
            (
                only_one_panel,
                show_panel,
                fill_panel,
                scroll_panel,
                update_banners,
            )
                .chain(),
        );
    }
}

#[derive(Component)]
struct Panel;

#[derive(Component)]
struct List;

#[derive(Component)]
struct ApprovalBanner;

#[derive(Component)]
struct MovePickBanner;

#[derive(Component)]
struct NoticeBanner;

fn spawn_ui(mut commands: Commands) {
    // 1. 上部の案内・許可ポップアップの入れ物（ペアリングのポップアップと同じ位置）。
    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                top: Val::Px(BAR_PX + 12.0),
                left: Val::Px(12.0),
                right: Val::Px(12.0),
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::FlexStart,
                row_gap: Val::Px(8.0),
                ..default()
            },
            GlobalZIndex(16),
            Pickable::IGNORE,
        ))
        .with_children(|stack| {
            // 移動先を選ぶときの案内バナー。
            stack
                .spawn((
                    PairBanner,
                    MovePickBanner,
                    bevy::picking::hover::Hovered::default(),
                    Node {
                        padding: UiRect::axes(Val::Px(14.0), Val::Px(8.0)),
                        column_gap: Val::Px(10.0),
                        align_items: AlignItems::Center,
                        border: UiRect::all(Val::Px(1.0)),
                        border_radius: BorderRadius::all(Val::Px(6.0)),
                        display: Display::None,
                        ..default()
                    },
                    BackgroundColor(POPUP_COLOR),
                    BorderColor::all(ACCENT),
                ))
                .observe(|mut click: On<PointerClick>| click.propagate(false));

            // 許可待ちの計画のポップアップ（複数あれば古い順に 1 件ずつ）。
            stack
                .spawn((
                    PairBanner,
                    ApprovalBanner,
                    bevy::picking::hover::Hovered::default(),
                    Node {
                        flex_direction: FlexDirection::Column,
                        padding: UiRect::axes(Val::Px(14.0), Val::Px(10.0)),
                        row_gap: Val::Px(6.0),
                        max_width: Val::Px(640.0),
                        max_height: Val::Px(520.0),
                        overflow: Overflow::clip(),
                        border: UiRect::all(Val::Px(1.0)),
                        border_radius: BorderRadius::all(Val::Px(6.0)),
                        display: Display::None,
                        ..default()
                    },
                    BackgroundColor(POPUP_COLOR),
                    BorderColor::all(POPUP_BORDER),
                ))
                .observe(|mut click: On<PointerClick>| click.propagate(false));

            // 一時的な結果・エラーのお知らせバナー。
            stack.spawn((
                NoticeBanner,
                Node {
                    padding: UiRect::axes(Val::Px(12.0), Val::Px(6.0)),
                    border_radius: BorderRadius::all(Val::Px(6.0)),
                    display: Display::None,
                    ..default()
                },
                BackgroundColor(POPUP_COLOR),
                Pickable::IGNORE,
            ));
        });

    // 2. 右からすべり出す履歴パネル（重複・カタログと同じ形）。
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
                    row_gap: Val::Px(6.0),
                    overflow: Overflow::scroll_y(),
                    ..default()
                },
                ScrollPosition::default(),
            ));
        });
}

/// 重複・カタログ・整理の 3 つの右パネルは、同時に 1 つだけ開く。
fn only_one_panel(
    mut organize: ResMut<OrganizeState>,
    mut catalog: ResMut<CatalogView>,
    mut dups: ResMut<Dups>,
) {
    if organize.is_changed() && organize.panel_open {
        if dups.open {
            dups.open = false;
        }
        if catalog.open {
            catalog.open = false;
        }
    } else if organize.panel_open
        && ((catalog.is_changed() && catalog.open) || (dups.is_changed() && dups.open))
    {
        organize.panel_open = false;
    }
}

/// 履歴パネルの開け閉め。右からすべり出し、その幅の分だけ木を映す領域を狭める。
fn show_panel(
    organize: Res<OrganizeState>,
    mut shown: Local<f32>,
    mut last: Local<Option<std::time::Instant>>,
    mut panel: Single<&mut Node, With<Panel>>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    let target = if organize.panel_open { 1.0 } else { 0.0 };
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
    crate::view::set_organize_side_px(width);
    redraw.write(RequestRedraw);
}

/// 上部の案内（移動先選び・許可ポップアップ・お知らせ）を状態に合わせて更新する。
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn update_banners(
    mut commands: Commands,
    state: Res<OrganizeState>,
    font: Res<LabelFont>,
    mut move_banner: Single<
        (Entity, &mut Node),
        (
            With<MovePickBanner>,
            Without<ApprovalBanner>,
            Without<NoticeBanner>,
        ),
    >,
    mut approval_banner: Single<
        (Entity, &mut Node),
        (
            With<ApprovalBanner>,
            Without<MovePickBanner>,
            Without<NoticeBanner>,
        ),
    >,
    mut notice_banner: Single<
        (Entity, &mut Node),
        (
            With<NoticeBanner>,
            Without<MovePickBanner>,
            Without<ApprovalBanner>,
        ),
    >,
    mut rendered_key: Local<Option<BannerKey>>,
) {
    let key = BannerKey::from_state(&state);
    if rendered_key.as_ref() == Some(&key) {
        return;
    }
    *rendered_key = Some(key);

    let text_font = TextFont::from_font_size(14.0).with_font(font.0.clone());
    let small_font = TextFont::from_font_size(12.0).with_font(font.0.clone());

    // 1. 切り取り中の案内。
    commands.entity(move_banner.0).despawn_children();
    if let Some(src) = &state.cut_source {
        move_banner.1.display = Display::Flex;
        let name = src
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| src.display().to_string());
        commands.entity(move_banner.0).with_children(|b| {
            b.spawn((
                Text::new(format!(
                    "「{name}」を切り取り中 — 移動先で右クリックして「ここに貼り付け」"
                )),
                text_font.clone(),
                TextColor(ACCENT),
            ));
            b.spawn((
                PairBanner,
                bevy::picking::hover::Hovered::default(),
                Node {
                    padding: UiRect::axes(Val::Px(10.0), Val::Px(3.0)),
                    border_radius: BorderRadius::all(Val::Px(4.0)),
                    ..default()
                },
                BackgroundColor(BUTTON_COLOR),
                children![(
                    Text::new("やめる"),
                    small_font.clone(),
                    TextColor(TEXT_COLOR)
                )],
            ))
            .observe(
                |mut click: On<PointerClick>, mut state: ResMut<OrganizeState>| {
                    click.propagate(false);
                    state.cut_source = None;
                },
            );
        });
    } else {
        move_banner.1.display = Display::None;
    }

    // 2. 許可待ちの計画のポップアップ（古い順に 1 件）。
    commands.entity(approval_banner.0).despawn_children();
    if let Some(active) = state.pending.first() {
        approval_banner.1.display = Display::Flex;
        build_approval_popup(
            &mut commands,
            approval_banner.0,
            active,
            state.pending.len().saturating_sub(1),
            state.highlighted_plan == Some(active.plan.id),
            state.busy,
            &text_font,
            &small_font,
        );
    } else {
        approval_banner.1.display = Display::None;
    }

    // 3. 一時的なお知らせ（履歴パネルが閉じているときだけ上に出す）。
    commands.entity(notice_banner.0).despawn_children();
    if !state.panel_open
        && let Some(msg) = state.current_notice()
    {
        notice_banner.1.display = Display::Flex;
        commands.entity(notice_banner.0).with_children(|b| {
            b.spawn((
                Text::new(msg.to_string()),
                small_font,
                TextColor(TEXT_COLOR),
            ));
        });
    } else {
        notice_banner.1.display = Display::None;
    }
}

#[derive(PartialEq, Eq)]
struct BannerKey {
    cut_source: Option<std::path::PathBuf>,
    pending_id: Option<i64>,
    pending_count: usize,
    highlighted: bool,
    busy: bool,
    notice: Option<String>,
    panel_open: bool,
}

impl BannerKey {
    fn from_state(state: &OrganizeState) -> Self {
        let pending_id = state.pending.first().map(|p| p.plan.id);
        Self {
            cut_source: state.cut_source.clone(),
            pending_id,
            pending_count: state.pending.len(),
            highlighted: pending_id.is_some() && state.highlighted_plan == pending_id,
            busy: state.busy,
            notice: state.current_notice().map(str::to_string),
            panel_open: state.panel_open,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn build_approval_popup(
    commands: &mut Commands,
    banner_entity: Entity,
    active: &PendingPlan,
    more_pending: usize,
    highlighted: bool,
    busy: bool,
    text_font: &TextFont,
    small_font: &TextFont,
) {
    let plan = &active.plan;
    let plan_id = plan.id;
    let s = &plan.summary;
    let risky_ops: Vec<_> = active.ops.iter().filter(|o| o.risky.is_some()).collect();
    let skip_count = s.refused
        + active
            .ops
            .iter()
            .filter(|o| o.state == OpState::Skipped)
            .count();

    let mut title = format!("整理の計画 #{plan_id}（{}）", plan.by);
    if more_pending > 0 {
        title.push_str(&format!("  ほか {more_pending} 件待ち"));
    }

    let mut parts = Vec::new();
    if s.mkdir > 0 {
        parts.push(format!("作成 {}", s.mkdir));
    }
    if s.move_ops > 0 {
        parts.push(format!("移動 {}", s.move_ops));
    }
    if s.trash > 0 {
        parts.push(format!("隔離 {}", s.trash));
    }
    let breakdown = if parts.is_empty() {
        String::new()
    } else {
        format!("（{}）", parts.join("・"))
    };
    let summary_line = format!(
        "全 {} 件{breakdown}  動く {}・空く {}・危ない {} 件・飛ばす {} 件",
        s.total,
        human(s.move_bytes),
        human(s.free_bytes),
        risky_ops.len(),
        skip_count,
    );

    // 危ないものが 1 件以上あるときだけ「危ないものを除いて許可」を出す
    // （右クリック 1 件の確認のときは危ない 1 件だけなので省く）。
    let show_skip_risky = !risky_ops.is_empty() && !(plan.by == "user" && s.total == 1);
    let first_chain = active.chains.first().cloned();

    commands.entity(banner_entity).with_children(|banner| {
        banner.spawn((Text::new(title), text_font.clone(), TextColor(TEXT_COLOR)));
        if let Some(note) = plan.note.as_deref().filter(|n| !n.trim().is_empty()) {
            banner.spawn((
                Text::new(note.to_string()),
                small_font.clone(),
                TextColor(TEXT_COLOR),
            ));
        }
        banner.spawn((
            Text::new(summary_line),
            small_font.clone(),
            TextColor(DIM_COLOR),
        ));

        // 危ないものの一覧（赤字で上から 20 件と残りの数）。
        if !risky_ops.is_empty() {
            banner
                .spawn((
                    Node {
                        flex_direction: FlexDirection::Column,
                        row_gap: Val::Px(2.0),
                        padding: UiRect::all(Val::Px(6.0)),
                        border_radius: BorderRadius::all(Val::Px(4.0)),
                        align_self: AlignSelf::Stretch,
                        ..default()
                    },
                    BackgroundColor(Color::srgba(0.32, 0.12, 0.13, 0.65)),
                ))
                .with_children(|box_node| {
                    box_node.spawn((
                        Text::new(format!("危ない操作（{} 件）:", risky_ops.len())),
                        small_font.clone(),
                        TextColor(RISKY_COLOR),
                    ));
                    for op in risky_ops.iter().take(MAX_SHOWN_RISKY) {
                        let reason = op.risky.as_deref().unwrap_or("");
                        let line = format!(
                            "・[{}] {} — {}",
                            op_label(op.op),
                            shorten_path(&op.src.display().to_string(), 64),
                            reason
                        );
                        box_node.spawn((
                            Text::new(line),
                            small_font.clone(),
                            TextColor(RISKY_COLOR),
                        ));
                    }
                    if risky_ops.len() > MAX_SHOWN_RISKY {
                        box_node.spawn((
                            Text::new(format!(
                                "…ほか {} 件の危ない操作があります",
                                risky_ops.len() - MAX_SHOWN_RISKY
                            )),
                            small_font.clone(),
                            TextColor(RISKY_COLOR),
                        ));
                    }
                });
        }

        // 操作ボタンの並び。
        banner
            .spawn(Node {
                flex_wrap: FlexWrap::Wrap,
                column_gap: Val::Px(8.0),
                row_gap: Val::Px(6.0),
                margin: UiRect::top(Val::Px(2.0)),
                ..default()
            })
            .with_children(|row| {
                // 許可
                row.spawn((
                    PairBanner,
                    bevy::picking::hover::Hovered::default(),
                    Node {
                        padding: UiRect::axes(Val::Px(10.0), Val::Px(4.0)),
                        border_radius: BorderRadius::all(Val::Px(4.0)),
                        ..default()
                    },
                    BackgroundColor(ALLOW_COLOR),
                    children![(
                        Text::new(if busy { "実行中…" } else { "許可" }),
                        text_font.clone(),
                        TextColor(TEXT_COLOR)
                    )],
                ))
                .observe(
                    move |mut click: On<PointerClick>, mut state: ResMut<OrganizeState>| {
                        click.propagate(false);
                        if !state.busy {
                            state.approve(plan_id, false);
                        }
                    },
                );

                // 危ないものを除いて許可（危ないものがあるときだけ）
                if show_skip_risky {
                    row.spawn((
                        PairBanner,
                        bevy::picking::hover::Hovered::default(),
                        Node {
                            padding: UiRect::axes(Val::Px(10.0), Val::Px(4.0)),
                            border_radius: BorderRadius::all(Val::Px(4.0)),
                            ..default()
                        },
                        BackgroundColor(WARN_BTN_COLOR),
                        children![(
                            Text::new("危ないものを除いて許可"),
                            text_font.clone(),
                            TextColor(TEXT_COLOR)
                        )],
                    ))
                    .observe(
                        move |mut click: On<PointerClick>, mut state: ResMut<OrganizeState>| {
                            click.propagate(false);
                            if !state.busy {
                                state.approve(plan_id, true);
                            }
                        },
                    );
                }

                // 断る
                row.spawn((
                    PairBanner,
                    bevy::picking::hover::Hovered::default(),
                    Node {
                        padding: UiRect::axes(Val::Px(10.0), Val::Px(4.0)),
                        border_radius: BorderRadius::all(Val::Px(4.0)),
                        ..default()
                    },
                    BackgroundColor(REJECT_COLOR),
                    children![(Text::new("断る"), text_font.clone(), TextColor(TEXT_COLOR))],
                ))
                .observe(
                    move |mut click: On<PointerClick>, mut state: ResMut<OrganizeState>| {
                        click.propagate(false);
                        state.reject(plan_id);
                    },
                );

                // 場所を見る（対象を木の上で光らせる）
                row.spawn((
                    PairBanner,
                    bevy::picking::hover::Hovered::default(),
                    Node {
                        padding: UiRect::axes(Val::Px(10.0), Val::Px(4.0)),
                        border_radius: BorderRadius::all(Val::Px(4.0)),
                        ..default()
                    },
                    BackgroundColor(if highlighted {
                        ACTIVE_BTN_COLOR
                    } else {
                        BUTTON_COLOR
                    }),
                    children![(
                        Text::new(if highlighted {
                            "場所を見る（光らせ中）"
                        } else {
                            "場所を見る"
                        }),
                        text_font.clone(),
                        TextColor(if highlighted { ACCENT } else { TEXT_COLOR })
                    )],
                ))
                .observe(
                    move |mut click: On<PointerClick>,
                          mut state: ResMut<OrganizeState>,
                          mut commands: Commands| {
                        click.propagate(false);
                        if state.highlighted_plan == Some(plan_id) {
                            state.highlighted_plan = None;
                        } else {
                            state.highlighted_plan = Some(plan_id);
                            if let Some(chain) = &first_chain {
                                commands.insert_resource(JumpTo(chain.clone()));
                            }
                        }
                    },
                );
            });
    });
}

/// 履歴パネルの中身を作り直す。
fn fill_panel(
    mut commands: Commands,
    state: Res<OrganizeState>,
    font: Res<LabelFont>,
    list: Single<Entity, With<List>>,
    mut rendered_key: Local<Option<PanelKey>>,
) {
    let key = PanelKey::from_state(&state);
    if rendered_key.as_ref() == Some(&key) {
        return;
    }
    *rendered_key = Some(key);
    let list = *list;
    commands.entity(list).despawn_children();
    if !state.panel_open {
        return;
    }
    let big = TextFont::from_font_size(13.0).with_font(font.0.clone());
    let small = TextFont::from_font_size(11.0).with_font(font.0.clone());

    let unpurged_trash = state
        .history
        .iter()
        .filter(|p| {
            matches!(
                p.state,
                PlanState::Done | PlanState::Partial | PlanState::Undone
            ) && p.purged_at.is_none()
                && p.summary.trash > 0
        })
        .count();

    commands.entity(list).with_children(|list| {
        // 見出しと「すべてゴミ箱へ送る」ボタン。
        list.spawn(Node {
            justify_content: JustifyContent::SpaceBetween,
            align_items: AlignItems::Center,
            width: Val::Percent(100.0),
            ..default()
        })
        .with_children(|hdr| {
            hdr.spawn((
                Text::new(format!("整理の履歴  {} 件", state.history.len())),
                big.clone(),
                TextColor(TEXT_COLOR),
            ));
            if unpurged_trash > 0 {
                hdr.spawn((
                    Node {
                        padding: UiRect::axes(Val::Px(8.0), Val::Px(3.0)),
                        border_radius: BorderRadius::all(Val::Px(4.0)),
                        ..default()
                    },
                    BackgroundColor(BUTTON_COLOR),
                    children![(
                        Text::new("すべてゴミ箱へ送る"),
                        small.clone(),
                        TextColor(TEXT_COLOR)
                    )],
                ))
                .observe(
                    |mut click: On<PointerClick>, mut state: ResMut<OrganizeState>| {
                        click.propagate(false);
                        if !state.busy {
                            state.confirming_purge = Some(ConfirmPurge::All);
                        }
                    },
                );
            }
        });

        list.spawn((
            Text::new("計画を押すと操作ごとの結果が出ます。消した分は隔離フォルダから取り消せます。"),
            small.clone(),
            TextColor(DIM_COLOR),
        ));

        if let Some(msg) = state.current_notice() {
            list.spawn((
                Node {
                    padding: UiRect::axes(Val::Px(8.0), Val::Px(5.0)),
                    border_radius: BorderRadius::all(Val::Px(4.0)),
                    flex_shrink: 0.0,
                    ..default()
                },
                BackgroundColor(SELECTED_COLOR),
                children![(Text::new(msg.to_string()), small.clone(), TextColor(ACCENT))],
            ));
        }

        if let Some(confirming) = state.confirming_purge {
            let message = match confirming {
                ConfirmPurge::Plan(id) => format!(
                    "計画 #{id} の隔離ファイルを OS のゴミ箱へ送りますか？（取り消せなくなります）"
                ),
                ConfirmPurge::All => {
                    "すべての隔離ファイルを OS のゴミ箱へ送りますか？（取り消せなくなります）".to_string()
                }
            };
            list.spawn(Node {
                flex_direction: FlexDirection::Column,
                row_gap: Val::Px(6.0),
                padding: UiRect::all(Val::Px(7.0)),
                border_radius: BorderRadius::all(Val::Px(4.0)),
                border: UiRect::all(Val::Px(1.0)),
                flex_shrink: 0.0,
                ..default()
            })
            .insert((
                BackgroundColor(Color::srgba(0.32, 0.12, 0.13, 0.75)),
                BorderColor::all(RISKY_COLOR),
            ))
            .with_children(|box_node| {
                box_node.spawn((Text::new(message), small.clone(), TextColor(RISKY_COLOR)));
                box_node
                    .spawn(Node {
                        column_gap: Val::Px(8.0),
                        ..default()
                    })
                    .with_children(|buttons| {
                        buttons
                            .spawn((
                                Node {
                                    padding: UiRect::axes(Val::Px(8.0), Val::Px(2.0)),
                                    border_radius: BorderRadius::all(Val::Px(4.0)),
                                    ..default()
                                },
                                BackgroundColor(REJECT_COLOR),
                                children![(Text::new("送る"), small.clone(), TextColor(TEXT_COLOR))],
                            ))
                            .observe(
                                move |mut click: On<PointerClick>,
                                      mut state: ResMut<OrganizeState>| {
                                    click.propagate(false);
                                    if state.confirming_purge == Some(confirming) && !state.busy {
                                        state.confirming_purge = None;
                                        match confirming {
                                            ConfirmPurge::Plan(id) => state.purge_plan(id),
                                            ConfirmPurge::All => state.purge_all(),
                                        }
                                    }
                                },
                            );
                        buttons
                            .spawn((
                                Node {
                                    padding: UiRect::axes(Val::Px(8.0), Val::Px(2.0)),
                                    border_radius: BorderRadius::all(Val::Px(4.0)),
                                    ..default()
                                },
                                BackgroundColor(BUTTON_COLOR),
                                children![(
                                    Text::new("やめる"),
                                    small.clone(),
                                    TextColor(TEXT_COLOR)
                                )],
                            ))
                            .observe(
                                move |mut click: On<PointerClick>,
                                      mut state: ResMut<OrganizeState>| {
                                    click.propagate(false);
                                    if state.confirming_purge == Some(confirming) {
                                        state.confirming_purge = None;
                                    }
                                },
                            );
                });
            });
        }

        if state.history.is_empty() {
            list.spawn((
                Node {
                    padding: UiRect::axes(Val::Px(8.0), Val::Px(10.0)),
                    ..default()
                },
                children![(
                    Text::new("まだ整理の計画はありません。右クリックの「切り取り」「ここに貼り付け」「削除…」や MCP から計画を作れます。"),
                    small.clone(),
                    TextColor(DIM_COLOR),
                )],
            ));
            return;
        }

        for plan in &state.history {
            let selected = state.selected_plan == Some(plan.id);
            let details = state
                .selected_details
                .as_ref()
                .filter(|d| d.plan_id == plan.id);
            spawn_plan_card(list, plan, selected, details, state.busy, &big, &small);
        }
    });
}

#[derive(PartialEq, Eq)]
struct PanelKey {
    panel_open: bool,
    busy: bool,
    selected_plan: Option<i64>,
    notice: Option<String>,
    history: Vec<(i64, PlanState, Option<i64>)>,
    details_id: Option<i64>,
    details_ops: Option<Vec<(usize, OpState)>>,
    confirming_purge: Option<ConfirmPurge>,
}

impl PanelKey {
    fn from_state(state: &OrganizeState) -> Self {
        Self {
            panel_open: state.panel_open,
            busy: state.busy,
            selected_plan: state.selected_plan,
            notice: state.current_notice().map(str::to_string),
            history: state
                .history
                .iter()
                .map(|p| (p.id, p.state, p.purged_at))
                .collect(),
            details_id: state.selected_details.as_ref().map(|d| d.plan_id),
            details_ops: state.selected_details.as_ref().map(|d| {
                d.ops.iter().map(|o| (o.seq as usize, o.state)).collect()
            }),
            confirming_purge: state.confirming_purge,
        }
    }
}

fn spawn_plan_card(
    list: &mut ChildSpawnerCommands,
    plan: &Plan,
    selected: bool,
    details: Option<&PlanDetails>,
    busy: bool,
    big: &TextFont,
    small: &TextFont,
) {
    let plan_id = plan.id;
    let can_undo = matches!(plan.state, PlanState::Done | PlanState::Partial)
        && plan.purged_at.is_none();
    let undo_busy = busy;
    let can_purge = matches!(
        plan.state,
        PlanState::Done | PlanState::Partial | PlanState::Undone
    ) && plan.purged_at.is_none()
        && plan.summary.trash > 0;
    let state_color = plan_state_color(plan.state);

    list.spawn((
        Node {
            flex_direction: FlexDirection::Column,
            padding: UiRect::axes(Val::Px(8.0), Val::Px(6.0)),
            row_gap: Val::Px(3.0),
            border_radius: BorderRadius::all(Val::Px(4.0)),
            border: UiRect::left(Val::Px(3.0)),
            flex_shrink: 0.0,
            ..default()
        },
        BorderColor::all(state_color),
        BackgroundColor(if selected { SELECTED_COLOR } else { ROW_COLOR }),
    ))
    .observe(
        move |_: On<PointerClick>, mut state: ResMut<OrganizeState>| {
            if state.selected_plan == Some(plan_id) {
                state.select_plan(None);
            } else {
                state.select_plan(Some(plan_id));
            }
        },
    )
    .with_children(|card| {
        card.spawn((
            Text::new(format!(
                "#{}  {}  {}  {}",
                plan.id,
                plan_state_label(plan.state),
                plan.by,
                format_epoch_ms(plan.created_at),
            )),
            big.clone(),
            TextColor(state_color),
        ));
        if let Some(note) = plan.note.as_deref().filter(|n| !n.trim().is_empty()) {
            card.spawn((
                Text::new(note.to_string()),
                small.clone(),
                TextColor(TEXT_COLOR),
            ));
        }
        let mut meta = format!(
            "{} 件（移動 {}・空く {}）",
            plan.summary.total,
            human(plan.summary.move_bytes),
            human(plan.summary.free_bytes),
        );
        if plan.purged_at.is_some() {
            meta.push_str("  ゴミ箱へ送り済み");
        }
        card.spawn((Text::new(meta), small.clone(), TextColor(DIM_COLOR)));

        if can_undo || can_purge {
            card.spawn(Node {
                column_gap: Val::Px(6.0),
                margin: UiRect::top(Val::Px(3.0)),
                ..default()
            })
            .with_children(|actions| {
                if can_undo {
                    actions
                        .spawn((
                            Node {
                                padding: UiRect::axes(Val::Px(8.0), Val::Px(2.0)),
                                border_radius: BorderRadius::all(Val::Px(4.0)),
                                ..default()
                            },
                            BackgroundColor(if undo_busy { ROW_COLOR } else { BUTTON_COLOR }),
                            children![(
                                Text::new(if undo_busy { "取り消し中…" } else { "取り消す" }),
                                small.clone(),
                                TextColor(if undo_busy { DIM_COLOR } else { TEXT_COLOR })
                            )],
                        ))
                        .observe(
                            move |mut click: On<PointerClick>, mut state: ResMut<OrganizeState>| {
                                click.propagate(false);
                                if !busy && !state.busy {
                                    state.undo(plan_id);
                                }
                            },
                        );
                }
                if can_purge {
                    actions
                        .spawn((
                            Node {
                                padding: UiRect::axes(Val::Px(8.0), Val::Px(2.0)),
                                border_radius: BorderRadius::all(Val::Px(4.0)),
                                ..default()
                            },
                            BackgroundColor(REJECT_COLOR),
                            children![(
                                Text::new("ゴミ箱へ送る"),
                                small.clone(),
                                TextColor(TEXT_COLOR)
                            )],
                        ))
                        .observe(
                            move |mut click: On<PointerClick>, mut state: ResMut<OrganizeState>| {
                                click.propagate(false);
                                if !busy && !state.busy {
                                    state.confirming_purge = Some(ConfirmPurge::Plan(plan_id));
                                }
                            },
                        );
                }
            });
        }
    });

    if !selected {
        return;
    }

    let Some(details) = details else {
        list.spawn((
            Node {
                margin: UiRect::left(Val::Px(10.0)),
                padding: UiRect::axes(Val::Px(6.0), Val::Px(3.0)),
                flex_shrink: 0.0,
                ..default()
            },
            children![(
                Text::new("読み込み中…"),
                small.clone(),
                TextColor(DIM_COLOR)
            )],
        ));
        return;
    };

    for (op, chain) in details.ops.iter().zip(&details.chains) {
        let mut line = format!(
            "#{} {} [{}]  {}",
            op.seq,
            op_label(op.op),
            op_state_label(op.state),
            shorten_path(&op.src.display().to_string(), 48),
        );
        if op.op == OpKind::Move
            && let Some(dst) = &op.dst
        {
            line.push_str(&format!(
                " -> {}",
                shorten_path(&dst.display().to_string(), 40)
            ));
        }
        let color = match op.state {
            OpState::Done | OpState::Undone => TEXT_COLOR,
            OpState::Skipped => ACCENT,
            OpState::Failed => RISKY_COLOR,
            OpState::Pending | OpState::Running => DIM_COLOR,
        };
        let mut row_cmd = list.spawn((
            Node {
                flex_direction: FlexDirection::Column,
                margin: UiRect::left(Val::Px(10.0)),
                padding: UiRect::axes(Val::Px(8.0), Val::Px(4.0)),
                row_gap: Val::Px(2.0),
                border_radius: BorderRadius::all(Val::Px(4.0)),
                flex_shrink: 0.0,
                ..default()
            },
            BackgroundColor(ROW_COLOR),
        ));
        row_cmd.with_children(|row| {
            row.spawn((Text::new(line), small.clone(), TextColor(color)));
            if let Some(risky) = &op.risky {
                row.spawn((
                    Text::new(format!("危ない: {risky}")),
                    small.clone(),
                    TextColor(RISKY_COLOR),
                ));
            }
            if let Some(reason) = &op.reason {
                row.spawn((
                    Text::new(format!("理由: {reason}")),
                    small.clone(),
                    TextColor(DIM_COLOR),
                ));
            }
        });
        if let Some(chain) = chain.clone() {
            row_cmd.observe(move |_: On<PointerClick>, mut commands: Commands| {
                commands.insert_resource(JumpTo(chain.clone()));
            });
        }
    }
}

/// 履歴パネルの上でホイールを回したらスクロールする。
fn scroll_panel(
    state: Res<OrganizeState>,
    mut wheel: MessageReader<MouseWheel>,
    window: Single<&Window>,
    mut list: Single<&mut ScrollPosition, With<List>>,
) {
    let over = state.panel_open
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

fn plan_state_label(state: PlanState) -> &'static str {
    match state {
        PlanState::Pending => "許可待ち",
        PlanState::Approved => "許可済み",
        PlanState::Running => "実行中",
        PlanState::Done => "完了",
        PlanState::Partial => "一部完了",
        PlanState::Rejected => "断った",
        PlanState::Cancelled => "取り下げ",
        PlanState::Expired => "期限切れ",
        PlanState::Undone => "取り消し済み",
    }
}

fn plan_state_color(state: PlanState) -> Color {
    match state {
        PlanState::Pending | PlanState::Approved | PlanState::Running => ACCENT,
        PlanState::Done => OK_COLOR,
        PlanState::Partial => Color::srgb(0.95, 0.72, 0.4),
        PlanState::Rejected | PlanState::Cancelled | PlanState::Expired => DIM_COLOR,
        PlanState::Undone => Color::srgb(0.65, 0.78, 0.95),
    }
}

fn op_label(op: OpKind) -> &'static str {
    match op {
        OpKind::Mkdir => "作成",
        OpKind::Move => "移動",
        OpKind::Trash => "隔離",
    }
}

fn op_state_label(state: OpState) -> &'static str {
    match state {
        OpState::Pending => "待ち",
        OpState::Running => "実行中",
        OpState::Done => "完了",
        OpState::Skipped => "飛ばした",
        OpState::Failed => "失敗",
        OpState::Undone => "戻した",
    }
}

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

fn shorten_path(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    let tail: String = s.chars().skip(count - keep).collect();
    format!("…{tail}")
}

/// UTC の Unix ミリ秒を「YYYY-MM-DD HH:MM」に整形する（ローカル時刻オフセット付き）。
fn format_epoch_ms(ms: i64) -> String {
    let secs = ms.div_euclid(1000) + local_offset_secs();
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400) as u32;
    let hour = sod / 3600;
    let min = (sod % 3600) / 60;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = i64::from(yoe) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    format!("{year:04}-{m:02}-{d:02} {hour:02}:{min:02}")
}

fn local_offset_secs() -> i64 {
    #[cfg(unix)]
    {
        #[repr(C)]
        struct Tm {
            tm_sec: i32,
            tm_min: i32,
            tm_hour: i32,
            tm_mday: i32,
            tm_mon: i32,
            tm_year: i32,
            tm_wday: i32,
            tm_yday: i32,
            tm_isdst: i32,
            tm_gmtoff: isize,
            tm_zone: *const std::ffi::c_char,
        }
        unsafe extern "C" {
            fn time(t: *mut isize) -> isize;
            fn localtime_r(timep: *const isize, result: *mut Tm) -> *mut Tm;
        }
        unsafe {
            let now = time(std::ptr::null_mut());
            let mut tm: Tm = std::mem::zeroed();
            if !localtime_r(&now, &mut tm).is_null() {
                return tm.tm_gmtoff as i64;
            }
        }
        0
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
        unsafe {
            let mut tzi: TIME_ZONE_INFORMATION = std::mem::zeroed();
            let state = GetTimeZoneInformation(&mut tzi);
            let bias_min = match state {
                1 => tzi.Bias + tzi.StandardBias,
                2 => tzi.Bias + tzi.DaylightBias,
                _ => tzi.Bias,
            };
            -i64::from(bias_min) * 60
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shorten_path_keeps_tail() {
        assert_eq!(shorten_path("/a/b", 10), "/a/b");
        assert_eq!(
            shorten_path("/Users/alice/Documents/file.txt", 12),
            "…ts/file.txt"
        );
    }

    #[test]
    fn format_epoch_ms_produces_readable_timestamp() {
        let s = format_epoch_ms(1_700_000_000_000);
        assert_eq!(s.len(), 16);
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[7..8], "-");
        assert_eq!(&s[10..11], " ");
        assert_eq!(&s[13..14], ":");
    }
}
