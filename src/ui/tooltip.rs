//! タイルの上でカーソルを少し止めたときに出す、そのファイルやフォルダの情報（ツールチップ）。
//!
//! 位置は Bevy の `Popover` に任せる。カーソルの所に大きさ 0 の目印を置き、その子として出すと、
//! 画面の端ではみ出さない側へ寄せてくれる。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bevy::ecs::system::SystemParam;
use bevy::picking::hover::HoverMap;
use bevy::picking::pointer::PointerId;
use bevy::prelude::*;
use bevy::text::{LineBreak, TextLayoutInfo};
use bevy::ui::{CalculatedClip, ComputedStackIndex, FixedNode, UiSystems};
use bevy::ui_widgets::popover::{Popover, PopoverAlign, PopoverPlacement, PopoverSide};
use bevy::window::{PrimaryWindow, RequestRedraw};
use bevy::winit::{EventLoopProxy, EventLoopProxyWrapper, WinitUserEvent};
use lisa_core::db::Db;

use crate::lan::{LanState, SavedPeer};
use crate::layout::Layouts;
use crate::model::tree::{FileTree, LoadState, NodeId};
use crate::render::palette;
use crate::render::scene::LabelFont;
use crate::scan::{Store, TreeChanged};
use crate::ui::context_menu::ContextMenu;
use crate::ui::menu::Popup;
use crate::view::{Animating, Fade, MainCamera, ViewSystems, ui_has_pointer};

/// カーソルを止めてから出すまで。
const DELAY: Duration = Duration::from_millis(500);
/// フォルダの合計を覚えておく時間。これを超えた結果と、木が変わった（`TreeChanged`）ときの
/// 結果は古くなったものとして捨てる。
const TOTAL_CACHE_TTL: Duration = Duration::from_secs(30);
/// 画面でこれより小さいタイルは、見えていないものとして選ばない（論理ピクセル）。
const MIN_TILE_PX: f32 = 6.0;
/// 画面の端から空ける余白（論理ピクセル）。`Popover::window_margin` にも渡す。
const WINDOW_MARGIN: f32 = 6.0;
/// ツールチップの枠の横幅の上限（論理ピクセル）。窓がこれより狭ければ窓に合わせる。
const MAX_WIDTH: f32 = 440.0;
/// 長い名前は途中を「…」で省く上限。幅 440 px（14 px 文字）でおよそ 2〜3 行に収まる文字数。
const MAX_NAME_CHARS: usize = 80;
/// 長いパスは途中を「…」で省く上限。幅 440 px（13 px 文字）でおよそ 3〜4 行に収まり、
/// 先頭のドライブや端末名と末尾のファイル名を残せる文字数。
const MAX_PATH_CHARS: usize = 120;
/// 長いエラー文は途中を「…」で省く上限。枠が縦に伸びすぎるのを防ぐ。
const MAX_ERROR_CHARS: usize = 120;

const PANEL_COLOR: Color = palette::base_100_alpha(0.96);
const BORDER_COLOR: Color = palette::NEUTRAL;
const TEXT_COLOR: Color = palette::BASE_CONTENT;
const DIM_COLOR: Color = palette::MUTED;

pub struct TooltipPlugin;

impl Plugin for TooltipPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Hover>()
            .init_resource::<TotalWorker>()
            .add_systems(Update, update_tooltip.after(ViewSystems))
            .add_systems(
                PostUpdate,
                clamp_popover_to_window
                    .after(UiSystems::Layout)
                    .before(UiSystems::PostLayout),
            );
        if debug_tooltip_at().is_some() {
            app.add_systems(
                PostUpdate,
                log_tooltip_layout
                    .after(UiSystems::PostLayout)
                    .after(UiSystems::Stack),
            );
        }
    }
}

/// デバッグ・確認用: 環境変数 `LISA_TOOLTIP_AT=x,y`（論理ピクセル）を指定して起動すると、
/// マウスカーソルの代わりにウィンドウ内の `(x, y)` にあるタイルへツールチップを出し、
/// レイアウトと文字の状態を `info!` ログに出す（合計が後から差し替わったときも 1 回出す）。未指定なら何もしない。
fn debug_tooltip_at() -> Option<Vec2> {
    static POS: OnceLock<Option<Vec2>> = OnceLock::new();
    *POS.get_or_init(|| parse_tooltip_at(std::env::var("LISA_TOOLTIP_AT").ok().as_deref()))
}

fn parse_tooltip_at(raw: Option<&str>) -> Option<Vec2> {
    let s = raw?.trim();
    let (x, y) = s.split_once(',')?;
    let x: f32 = x.trim().parse().ok()?;
    let y: f32 = y.trim().parse().ok()?;
    (x.is_finite() && y.is_finite()).then_some(Vec2::new(x, y))
}

/// フォルダ（端末・ドライブを含む）の下まで全部の合計。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FolderTotals {
    pub files: u64,
    pub folders: u64,
    pub bytes: u64,
}

/// フォルダの合計を裏で数えた結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TotalOutcome {
    /// DB または LAN の相手から下まで全部の合計を取れた。
    Totals(FolderTotals),
    /// DB に無い（まだ読んでいない・DB に残さないドライブ）、または LAN の相手から取れなかった。
    /// `Node.size`（木に読み出してある分だけ）を使う。
    Unavailable,
}

impl TotalOutcome {
    pub(super) fn into_display(self) -> FolderTotalDisplay {
        match self {
            Self::Totals(t) => FolderTotalDisplay::Totals(t),
            Self::Unavailable => FolderTotalDisplay::LoadedOnly,
        }
    }
}

/// ツールチップに出すフォルダ合計の状態。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FolderTotalDisplay {
    /// 裏のスレッドで計算中（「計算中…」と出す）。
    Computing,
    /// 下まで全部の合計が取れた（「大きさ」と「中身」を出す）。
    Totals(FolderTotals),
    /// DB や相手から取れないため、木に読み出してある `Node.size` に「（読んだ分だけ）」を添える。
    LoadedOnly,
}

/// フォルダの合計のキャッシュ（`NodeId` ごと）。
///
/// 捨てる決まり:
/// - 数えてから `TOTAL_CACHE_TTL`（30 秒）過ぎた項目は古くなったものとして捨てる。
/// - `TreeChanged` が立ったら `invalidate()` で覚えている項目を全部捨て、世代（`generation`）を 1 進める。
/// - 同じ `NodeId` が計算中（`in_flight`）のあいだは重ねて数えない。
/// - 計算中に `TreeChanged` が立っても進行中の問い合わせは止めず、戻ってきた結果を今のツールチップへ
///   反映して「計算中…」から抜ける。ただし世代が変わっていたとき、`Unavailable` はキャッシュに入れない
///   （スキャンが進んで DB に入ったかもしれないので、出し直せば再試行できるようにする）。`Totals` は
///   数秒かかる問い合わせのやり直しを避けるためキャッシュに入れ、次の `TreeChanged` または 30 秒で捨てる。
#[derive(Default)]
struct TotalCache {
    entries: HashMap<NodeId, (TotalOutcome, Instant)>,
    in_flight: HashSet<NodeId>,
    generation: u64,
}

impl TotalCache {
    fn new() -> Self {
        Self::default()
    }

    /// 期限切れでないキャッシュがあれば返す。期限切れならその場で捨てる。
    fn get(&mut self, node: NodeId, now: Instant) -> Option<TotalOutcome> {
        let &(outcome, computed_at) = self.entries.get(&node)?;
        if is_total_expired(computed_at, now, TOTAL_CACHE_TTL) {
            self.entries.remove(&node);
            return None;
        }
        Some(outcome)
    }

    fn is_in_flight(&self, node: NodeId) -> bool {
        self.in_flight.contains(&node)
    }

    /// まだ計算中でなければ計算中として登録し、今の世代を返す。すでに計算中なら None。
    fn try_begin(&mut self, node: NodeId) -> Option<u64> {
        if self.in_flight.insert(node) {
            Some(self.generation)
        } else {
            None
        }
    }

    /// スレッド起動に失敗したときなど、計算中の印だけ外す。
    fn cancel_in_flight(&mut self, node: NodeId) {
        self.in_flight.remove(&node);
    }

    /// 計算が終わった結果を反映する（期限切れの古い項目もついでに掃除する）。
    fn finish(&mut self, node: NodeId, generation: u64, outcome: TotalOutcome, now: Instant) {
        self.in_flight.remove(&node);
        self.entries
            .retain(|_, &mut (_, t)| !is_total_expired(t, now, TOTAL_CACHE_TTL));
        if generation == self.generation || matches!(outcome, TotalOutcome::Totals(_)) {
            self.entries.insert(node, (outcome, now));
        }
    }

    /// 木が変わったので覚えている結果をすべて捨て、世代を進める。
    fn invalidate(&mut self) {
        self.entries.clear();
        self.generation = self.generation.wrapping_add(1);
    }
}

/// 結果が `ttl` 以上古いかどうか。
fn is_total_expired(computed_at: Instant, now: Instant, ttl: Duration) -> bool {
    now.saturating_duration_since(computed_at) >= ttl
}

pub(super) struct TotalMessage {
    node: NodeId,
    generation: u64,
    pub(super) outcome: TotalOutcome,
}

/// フォルダの合計を裏のスレッドで数え、結果とキャッシュを持つリソース。
#[derive(Resource)]
struct TotalWorker {
    cache: TotalCache,
    tx: Sender<TotalMessage>,
    rx: Mutex<Receiver<TotalMessage>>,
}

impl Default for TotalWorker {
    fn default() -> Self {
        let (tx, rx) = channel();
        Self {
            cache: TotalCache::new(),
            tx,
            rx: Mutex::new(rx),
        }
    }
}

/// 出しているツールチップのエンティティ群（合計が届いたら作り直さず中身だけ書き換える）。
struct ShownTooltip {
    anchor: Entity,
    size_text: Option<Entity>,
    totals_slot: Option<Entity>,
    inner_max_w: f32,
    waiting_total: bool,
}

/// 今カーソルの下にあるものと、出しているツールチップ。
#[derive(Resource)]
struct Hover {
    node: Option<NodeId>,
    since: Instant,
    shown: Option<ShownTooltip>,
    cursor: Vec2,
}

impl Default for Hover {
    fn default() -> Self {
        Self {
            node: None,
            since: Instant::now(),
            shown: None,
            cursor: Vec2::ZERO,
        }
    }
}

#[derive(Component)]
struct TooltipAnchor;

/// 「大きさ」の値テキスト（裏で数えた合計が届いたらここを書き換える）。
#[derive(Component)]
struct TooltipSizeText;

/// 「大きさ」と「中身」を並べる入れ物（後から「中身」の行をここへ足す）。
#[derive(Component)]
struct TooltipTotalsSlot;

/// `Popover` の配置後、狭い窓などでどうしてもはみ出す場合だけウィンドウ内へ押し戻す印。
#[derive(Component)]
pub(crate) struct ClampPopover;

/// フォルダ合計の計算と反映に必要なリソース群（`update_tooltip` の引数上限を超えないようまとめる）。
#[derive(SystemParam)]
struct TotalContext<'w, 's> {
    worker: ResMut<'w, TotalWorker>,
    store: Option<Res<'w, Store>>,
    lan: Option<Res<'w, LanState>>,
    tree_changed: Option<Res<'w, TreeChanged>>,
    proxy: Option<Res<'w, EventLoopProxyWrapper>>,
    size_texts: Query<'w, 's, &'static mut Text, With<TooltipSizeText>>,
}

/// 裏のスレッドで合計を数える対象。
#[derive(Debug, Clone, PartialEq)]
pub(super) enum TotalTarget {
    LocalPath {
        db_path: PathBuf,
        path: PathBuf,
    },
    LocalDrive {
        db_path: PathBuf,
        drive_id: String,
    },
    LocalDevice {
        db_path: PathBuf,
        drive_ids: Vec<String>,
    },
    RemoteEntry {
        peer: SavedPeer,
        entry_id: i64,
    },
    RemoteDevice {
        peer: SavedPeer,
        drive_entry_ids: Vec<i64>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum TotalPlan {
    /// ファイルやダミーの木（非同期の合計は不要。従来の `Node.size` を出す）。
    NotNeeded,
    /// DB に残さないドライブや未ペアリングなど、問い合わせずに「（読んだ分だけ）」にする。
    ImmediateUnavailable,
    /// 裏のスレッドで数える。
    Fetch(TotalTarget),
}

pub(super) fn resolve_total_target(
    tree: &FileTree,
    store: Option<&Store>,
    lan: Option<&LanState>,
    db_path: Option<PathBuf>,
    node_id: NodeId,
) -> TotalPlan {
    let node = &tree.nodes[node_id];
    if !node.is_dir {
        return TotalPlan::NotNeeded;
    }
    if let Some(remote) = &node.remote {
        let Some(peer) = lan.and_then(|l| l.saved_peer(&remote.device_id)) else {
            return TotalPlan::ImmediateUnavailable;
        };
        if let Some(entry_id) = remote.entry_id {
            if node
                .remote_meta
                .as_ref()
                .is_some_and(|m| m.cloud_only || m.error.is_some())
            {
                return TotalPlan::ImmediateUnavailable;
            }
            return TotalPlan::Fetch(TotalTarget::RemoteEntry { peer, entry_id });
        }
        let drive_entry_ids: Vec<i64> = node
            .children
            .iter()
            .filter_map(|&c| tree.nodes[c].remote.as_ref()?.entry_id)
            .collect();
        return TotalPlan::Fetch(TotalTarget::RemoteDevice {
            peer,
            drive_entry_ids,
        });
    }
    let Some(store) = store else {
        // `LISA_DUMMY=1` など DB を使わないダミーの木。
        return TotalPlan::NotNeeded;
    };
    let Some(db_path) = db_path.filter(|_| store.has_db()) else {
        return TotalPlan::ImmediateUnavailable;
    };
    match node.depth {
        1 => {
            let drive_ids = store.device_drive_ids(tree, node_id);
            if drive_ids.is_empty() {
                TotalPlan::ImmediateUnavailable
            } else {
                TotalPlan::Fetch(TotalTarget::LocalDevice { db_path, drive_ids })
            }
        }
        2 => match store.drive_id_of(tree, node_id) {
            Some(drive_id) => TotalPlan::Fetch(TotalTarget::LocalDrive { db_path, drive_id }),
            None => TotalPlan::ImmediateUnavailable,
        },
        _ => {
            if store.drive_id_of(tree, node_id).is_none() || node.path.is_none() {
                return TotalPlan::ImmediateUnavailable;
            }
            let Some(path) = local_path(tree, node_id) else {
                return TotalPlan::ImmediateUnavailable;
            };
            TotalPlan::Fetch(TotalTarget::LocalPath { db_path, path })
        }
    }
}

/// 手元の 1 フォルダの合計を DB から数える（パスから `query::locate` で `entry.id` を引く）。
/// DB に無い・フォルダでない・まだ読んでいない・読めなかったときは None。
pub(crate) fn local_path_total(db: &Db, path: &Path) -> Option<FolderTotals> {
    let located = lisa_core::query::locate(db, path).ok()??;
    let entry_id = *located.chain.last()?;
    let item = lisa_core::query::item(db, entry_id).ok()??;
    if item.kind != "dir" || !item.listed || item.error.is_some() {
        return None;
    }
    let (files, folders, bytes) = lisa_core::query::folder_total(db, entry_id).ok()?;
    Some(FolderTotals {
        files,
        folders,
        bytes,
    })
}

/// 手元の 1 ドライブの合計を DB から数える（一番上の `entry` から下まで全部）。
/// DB に無い・まだ読んでいない・読めなかったときは None。
pub(crate) fn local_drive_total(db: &Db, drive_id: &str) -> Option<FolderTotals> {
    let root = db.drive_root(drive_id).ok()??;
    if !root.listed || root.error.is_some() {
        return None;
    }
    let (files, folders, bytes) = lisa_core::query::folder_total(db, root.id).ok()?;
    Some(FolderTotals {
        files,
        folders,
        bytes,
    })
}

/// 手元の端末の合計を、DB に残すドライブ全部の合計として数える。
/// 1 つも読めているドライブが無ければ None。
pub(crate) fn local_device_total(db: &Db, drive_ids: &[String]) -> Option<FolderTotals> {
    let mut total = FolderTotals {
        files: 0,
        folders: 0,
        bytes: 0,
    };
    let mut any = false;
    for drive_id in drive_ids {
        if let Some(t) = local_drive_total(db, drive_id) {
            total.files = total.files.saturating_add(t.files);
            total.folders = total.folders.saturating_add(t.folders);
            total.bytes = total.bytes.saturating_add(t.bytes);
            any = true;
        }
    }
    any.then_some(total)
}

fn open_db_if_exists(path: &Path) -> Option<Db> {
    if !path.exists() {
        return None;
    }
    match Db::open(path) {
        Ok(db) => Some(db),
        Err(error) => {
            warn!(
                "ツールチップ用に DB を開けませんでした（{}）: {error}",
                path.display()
            );
            None
        }
    }
}

fn compute_total(target: TotalTarget) -> TotalOutcome {
    match target {
        TotalTarget::LocalPath { db_path, path } => open_db_if_exists(&db_path)
            .and_then(|db| local_path_total(&db, &path))
            .map_or(TotalOutcome::Unavailable, TotalOutcome::Totals),
        TotalTarget::LocalDrive { db_path, drive_id } => open_db_if_exists(&db_path)
            .and_then(|db| local_drive_total(&db, &drive_id))
            .map_or(TotalOutcome::Unavailable, TotalOutcome::Totals),
        TotalTarget::LocalDevice { db_path, drive_ids } => open_db_if_exists(&db_path)
            .and_then(|db| local_device_total(&db, &drive_ids))
            .map_or(TotalOutcome::Unavailable, TotalOutcome::Totals),
        TotalTarget::RemoteEntry { peer, entry_id } => {
            match crate::lan::fetch_folder_total(&peer, entry_id) {
                Ok(Some(t)) => TotalOutcome::Totals(FolderTotals {
                    files: t.files,
                    folders: t.folders,
                    bytes: t.bytes,
                }),
                Ok(None) => TotalOutcome::Unavailable,
                Err((_, error)) => {
                    warn!("相手のフォルダの合計を取れませんでした（entry={entry_id}）: {error}");
                    TotalOutcome::Unavailable
                }
            }
        }
        TotalTarget::RemoteDevice {
            peer,
            drive_entry_ids,
        } => match crate::lan::fetch_device_total(&peer, &drive_entry_ids) {
            Ok(Some(t)) => TotalOutcome::Totals(FolderTotals {
                files: t.files,
                folders: t.folders,
                bytes: t.bytes,
            }),
            Ok(None) => TotalOutcome::Unavailable,
            Err((_, error)) => {
                warn!("相手の端末の合計を取れませんでした: {error}");
                TotalOutcome::Unavailable
            }
        },
    }
}

pub(super) fn spawn_total_worker(
    node: NodeId,
    generation: u64,
    target: TotalTarget,
    tx: Sender<TotalMessage>,
    proxy: Option<EventLoopProxy<WinitUserEvent>>,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("lisa-tooltip-total".into())
        .spawn(move || {
            let outcome = compute_total(target);
            let _ = tx.send(TotalMessage {
                node,
                generation,
                outcome,
            });
            if let Some(proxy) = proxy {
                let _ = proxy.send_event(WinitUserEvent::WakeUp);
            }
        })?;
    Ok(())
}

/// 下の画面（タイル）をカーソルで指せないとき（メニューやポップアップ、ジェスチャー、ズームの途中など）。
#[derive(SystemParam)]
pub(crate) struct PointerBlocked<'w, 's> {
    popup: Res<'w, Popup>,
    dialog: Option<Res<'w, super::dialog::DialogState>>,
    banners: Query<'w, 's, &'static bevy::picking::hover::Hovered, With<crate::lan::PairBanner>>,
    gesture: Res<'w, crate::input::gesture::Gesture>,
    animating: Option<Res<'w, Animating>>,
    fades: Query<'w, 's, (), With<Fade>>,
    hover_map: Res<'w, HoverMap>,
    ui_nodes: Query<'w, 's, (), With<Node>>,
}

impl PointerBlocked<'_, '_> {
    /// 右クリックのメニューが開いているか（`menu_busy`）は、呼ぶ側が渡す（メニューの側は書き換えるため、
    /// ここで読むと同じリソースの読み書きがぶつかる）。
    pub(crate) fn blocked(&self, window: &Window, menu_busy: bool) -> bool {
        let over_banner = self.banners.iter().any(|h| h.get());
        // 画面に固定の UI（読み込み中のリストなど）の上。
        let over_ui = self
            .hover_map
            .get(&PointerId::Mouse)
            .is_some_and(|hits| hits.keys().any(|e| self.ui_nodes.contains(*e)));
        ui_has_pointer(window, &self.popup, over_banner)
            || self.dialog.as_ref().is_some_and(|dialog| dialog.busy())
            || over_ui
            || self.gesture.active()
            || self.animating.is_some()
            || !self.fades.is_empty()
            || menu_busy
    }
}

/// カーソルの所にあるもの（画面で見えている中で一番深いもの）。Root しか無いなら None。
pub(crate) fn node_under_cursor(
    window: &Window,
    camera: (&Camera, &GlobalTransform, &Projection),
    tree: &FileTree,
    layouts: &Layouts,
) -> Option<(NodeId, Vec2)> {
    node_at_viewport_pos(window.cursor_position()?, camera, tree, layouts)
}

/// ウィンドウ内の論理座標 `pos` にあるもの（画面で見えている中で一番深いもの）。Root しか無いなら None。
fn node_at_viewport_pos(
    pos: Vec2,
    camera: (&Camera, &GlobalTransform, &Projection),
    tree: &FileTree,
    layouts: &Layouts,
) -> Option<(NodeId, Vec2)> {
    let (camera, global, projection) = camera;
    let Projection::Orthographic(ortho) = projection else {
        return None;
    };
    let world = camera.viewport_to_world_2d(global, pos).ok()?;
    node_at(tree, layouts, world, ortho.scale).map(|n| (n, world))
}

/// `world` を含むタイルを Root から下へたどる。画面で `MIN_TILE_PX` より小さいものの手前で止める。
pub(crate) fn node_at(
    tree: &FileTree,
    layouts: &Layouts,
    world: Vec2,
    scale: f32,
) -> Option<NodeId> {
    let mut node = FileTree::ROOT;
    while let Some(child) = tree.nodes[node].children.iter().copied().find(|&c| {
        let cell = layouts[c].cell;
        cell.contains(world) && cell.width().min(cell.height()) / scale.max(1e-6) >= MIN_TILE_PX
    }) {
        node = child;
    }
    (node != FileTree::ROOT).then_some(node)
}

#[allow(clippy::too_many_arguments)]
fn update_tooltip(
    mut commands: Commands,
    mut hover: ResMut<Hover>,
    window: Single<&Window, With<PrimaryWindow>>,
    camera: Single<(&Camera, &GlobalTransform, &Projection), With<MainCamera>>,
    tree: Res<FileTree>,
    layouts: Res<Layouts>,
    mouse: Res<ButtonInput<MouseButton>>,
    blocked: PointerBlocked,
    menu: Res<ContextMenu>,
    mut totals: TotalContext,
    font: Res<LabelFont>,
    mut anchors: Query<&mut Node, With<TooltipAnchor>>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    let hover = &mut *hover;
    let now = Instant::now();
    if totals.tree_changed.as_ref().is_some_and(|c| c.is_changed()) {
        totals.worker.cache.invalidate();
    }

    // 裏のスレッドで数え終わった合計を受け取る。ホバーが別のものに移っていたらキャッシュに入れるだけ。
    let completed: Vec<TotalMessage> = totals
        .worker
        .rx
        .lock()
        .map(|rx| rx.try_iter().collect())
        .unwrap_or_default();
    let mut current_completed: Option<TotalOutcome> = None;
    for msg in completed {
        totals
            .worker
            .cache
            .finish(msg.node, msg.generation, msg.outcome, now);
        if hover.node == Some(msg.node) {
            current_completed = Some(msg.outcome);
        }
    }

    let window = window.into_inner();
    let (cam, global, projection) = camera.into_inner();
    let debug_pos = debug_tooltip_at();
    let pressed = debug_pos.is_none() && mouse.get_pressed().next().is_some();
    let target = if let Some(pos) = debug_pos {
        node_at_viewport_pos(pos, (cam, global, projection), &tree, &layouts).map(|(n, _)| n)
    } else if pressed || blocked.blocked(window, menu.busy()) {
        None
    } else {
        node_under_cursor(window, (cam, global, projection), &tree, &layouts).map(|(n, _)| n)
    };
    let cursor = debug_pos
        .or_else(|| window.cursor_position())
        .unwrap_or_default();

    // 指すものが変わったか、ボタンを押したら、出していたものを消して数え直す。
    if target != hover.node || pressed {
        if let Some(shown) = hover.shown.take() {
            commands.entity(shown.anchor).despawn();
            redraw.write(RequestRedraw);
        }
        hover.node = target;
        hover.since = now;
        current_completed = None;
    }
    let Some(node) = target else {
        return;
    };
    let window_size = Vec2::new(window.width(), window.height());
    match hover.shown.as_mut() {
        None if hover.since.elapsed() >= DELAY => {
            let plan = resolve_total_target(
                &tree,
                totals.store.as_deref(),
                totals.lan.as_deref(),
                crate::scan::db_path(),
                node,
            );
            let folder_total = match plan {
                TotalPlan::NotNeeded => None,
                TotalPlan::ImmediateUnavailable => Some(FolderTotalDisplay::LoadedOnly),
                TotalPlan::Fetch(fetch_target) => {
                    if let Some(cached) = totals.worker.cache.get(node, now) {
                        Some(cached.into_display())
                    } else if let Some(generation) = totals.worker.cache.try_begin(node) {
                        let tx = totals.worker.tx.clone();
                        let proxy = totals.proxy.as_deref().map(|p| (**p).clone());
                        if spawn_total_worker(node, generation, fetch_target, tx, proxy).is_ok() {
                            Some(FolderTotalDisplay::Computing)
                        } else {
                            totals.worker.cache.cancel_in_flight(node);
                            Some(FolderTotalDisplay::LoadedOnly)
                        }
                    } else {
                        // すでに別の裏スレッドが同じノードを数えているので、結果を待つ。
                        Some(FolderTotalDisplay::Computing)
                    }
                }
            };
            let rows = describe(&tree, totals.store.as_deref(), node, folder_total);
            let mut shown = spawn_tooltip(&mut commands, &font, cursor, window_size, &rows);
            shown.waiting_total = matches!(folder_total, Some(FolderTotalDisplay::Computing));
            hover.shown = Some(shown);
            hover.cursor = cursor;
            redraw.write(RequestRedraw);
        }
        // 止めている間は描画が寝るので、出すときまで起こし続ける。
        None => {
            redraw.write(RequestRedraw);
        }
        Some(shown) => {
            if cursor != hover.cursor
                && let Ok(mut anchor) = anchors.get_mut(shown.anchor)
            {
                anchor.left = Val::Px(cursor.x);
                anchor.top = Val::Px(cursor.y);
                hover.cursor = cursor;
                redraw.write(RequestRedraw);
            }
            if shown.waiting_total
                && let Some(outcome) = current_completed.or_else(|| {
                    (!totals.worker.cache.is_in_flight(node))
                        .then(|| totals.worker.cache.get(node, now))
                        .flatten()
                })
            {
                apply_total_to_tooltip(
                    &mut commands,
                    shown,
                    outcome,
                    tree.nodes[node].size,
                    &font,
                    &mut totals.size_texts,
                );
                shown.waiting_total = false;
                redraw.write(RequestRedraw);
            }
        }
    }
}

/// ツールチップの 1 行（見出しと中身）。見出しが空なら名前の行。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Row {
    pub label: &'static str,
    pub value: String,
}

fn row(label: &'static str, value: impl Into<String>) -> Row {
    Row {
        label,
        value: value.into(),
    }
}

/// バイト数をツールチップ用の文字にする（1024 B 未満は「… バイト」、以上は「12.3 GB（… バイト）」）。
fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{} バイト", group_digits(bytes))
    } else {
        format!(
            "{}（{} バイト）",
            crate::ui::progress::bytes(bytes),
            group_digits(bytes)
        )
    }
}

/// 木に読み出してある分だけの大きさ表示。
fn format_loaded_only_size(bytes: u64) -> String {
    format!("{}（読んだ分だけ）", format_size(bytes))
}

/// フォルダの中身の数（「ファイル 1,234 / フォルダ 56」）。
fn format_contents(totals: FolderTotals) -> String {
    format!(
        "ファイル {} / フォルダ {}",
        group_digits(totals.files),
        group_digits(totals.folders)
    )
}

/// ノード `id` について出す行。分からないものは出さない。
/// フォルダの合計は裏で数えるため、状態に応じた `folder_total` を渡す（ファイルやダミーの木では `None`）。
pub(crate) fn describe(
    tree: &FileTree,
    store: Option<&Store>,
    id: NodeId,
    folder_total: Option<FolderTotalDisplay>,
) -> Vec<Row> {
    describe_rows(tree, store, id, folder_total, false)
}

pub(super) fn describe_details(
    tree: &FileTree,
    store: Option<&Store>,
    id: NodeId,
    folder_total: Option<FolderTotalDisplay>,
) -> Vec<Row> {
    describe_rows(tree, store, id, folder_total, true)
}

fn describe_rows(
    tree: &FileTree,
    store: Option<&Store>,
    id: NodeId,
    folder_total: Option<FolderTotalDisplay>,
    full: bool,
) -> Vec<Row> {
    let name_or_path = |value: &str, max| {
        if full { value.to_owned() } else { shorten_middle(value, max) }
    };
    let date = |ms| {
        let minute = format_local(ms);
        if full {
            let second = (ms.div_euclid(1000) + local_offset_secs()).rem_euclid(60);
            format!("{minute}:{second:02}")
        } else {
            minute
        }
    };
    let node = &tree.nodes[id];
    let path = if full { local_item_path(tree, store, id) } else { local_path(tree, id) };
    let name = match path.as_deref().and_then(Path::file_name) {
        Some(name) if full && node.depth > 2 => name.to_string_lossy(),
        _ => node.name.as_str().into(),
    };
    let mut rows = vec![row(if full { "名前" } else { "" }, name_or_path(&name, MAX_NAME_CHARS))];
    let meta = path.as_deref().and_then(|p| std::fs::symlink_metadata(p).ok().map(|m| (p, m)));

    let kind = match node.depth {
        1 => "端末".to_string(),
        2 => "ドライブ".to_string(),
        _ => {
            if let Some((_, m)) = &meta {
                if full && m.file_type().is_symlink() {
                    "シンボリックリンク".into()
                } else {
                    kind_label(m.file_type().is_symlink(), node.is_dir, false)
                }
            } else if let Some(r) = &node.remote_meta {
                if full && r.kind == "symlink" {
                    "シンボリックリンク".into()
                } else {
                    kind_label(r.kind == "symlink", node.is_dir, r.cloud_only)
                }
            } else {
                kind_label(false, node.is_dir, false)
            }
        }
    };
    rows.push(row("種類", kind));
    if !node.is_dir {
        let bytes = if full {
            meta.as_ref().map_or(node.size, |(_, m)| m.len())
        } else {
            node.size
        };
        rows.push(row("大きさ", format_size(bytes)));
    } else {
        rows.extend(folder_rows(folder_total, node.size));
    }

    if let Some((path, m)) = &meta {
        if node.depth > 2 {
            if let Some(t) = m.created().ok().and_then(system_ms) {
                rows.push(row(if full { "作成日時" } else { "作成" }, date(t)));
            }
            if let Some(t) = m.modified().ok().and_then(system_ms) {
                rows.push(row(if full { "更新日時" } else { "更新" }, date(t)));
            }
            if let Some(hash) = store
                .and_then(|s| crate::scan::stored_item(s, path))
                .and_then(|i| i.hash)
            {
                rows.push(row("ハッシュ", if full { hash } else { short_hash(&hash) }));
            }
        }
    } else if let Some(r) = &node.remote_meta
        && node.depth > 2
    {
        if let Some(t) = r.mtime {
            rows.push(row(if full { "更新日時" } else { "更新" }, date(t)));
        }
        if let Some(hash) = &r.hash {
            rows.push(row(
                "ハッシュ",
                if full { hash.clone() } else { short_hash(hash) },
            ));
        }
    }
    if node.depth >= 2 {
        let place = crate::model::display_path::tree_text(tree, id);
        rows.push(row("場所", name_or_path(&place, MAX_PATH_CHARS)));
    }

    let state = match node.state {
        LoadState::Loaded if full => Some("読み込み済み"),
        LoadState::Loaded => None,
        LoadState::Unloaded => Some("まだ読んでいない"),
        LoadState::Loading => Some("読み込み中"),
        LoadState::Stale => Some("古い（読み直し待ち）"),
        LoadState::Failed => Some("読めなかった"),
    };
    if let Some(state) = state {
        rows.push(row("状態", state));
    }
    if let Some(error) = node.remote_meta.as_ref().and_then(|r| r.error.as_deref()) {
        rows.push(row("エラー", shorten_middle(error, MAX_ERROR_CHARS)));
    }
    rows
}

pub(super) fn folder_rows(total: Option<FolderTotalDisplay>, loaded_bytes: u64) -> Vec<Row> {
    match total {
        Some(FolderTotalDisplay::Computing) => vec![row("大きさ", "計算中…")],
        Some(FolderTotalDisplay::Totals(t)) => vec![
            row("大きさ", format_size(t.bytes)),
            row("中身", format_contents(t)),
        ],
        Some(FolderTotalDisplay::LoadedOnly) => {
            vec![row("大きさ", format_loaded_only_size(loaded_bytes))]
        }
        None if loaded_bytes > 0 => vec![row("大きさ", format_size(loaded_bytes))],
        None => Vec::new(),
    }
}

/// ディスク操作には短縮・正規化された表示名ではなく、DB に記録した実パスを使う。
pub(crate) fn local_item_path(tree: &FileTree, store: Option<&Store>, id: NodeId) -> Option<PathBuf> {
    let node = &tree.nodes[id];
    if node.remote.is_some() {
        return None;
    }
    node.path.clone()
        .or_else(|| store?.stored_path(node.entry?))
        .or_else(|| local_path(tree, id))
}

fn kind_label(symlink: bool, is_dir: bool, cloud_only: bool) -> String {
    let base = if symlink {
        "リンク"
    } else if is_dir {
        "フォルダ"
    } else {
        "ファイル"
    };
    if cloud_only {
        format!("{base}（クラウドのみ）")
    } else {
        base.to_string()
    }
}

/// 手元のノードのディスク上のパス。場所を持つ一番近い祖先（自分も含む）に、その下の名前をつなぐ。
pub(crate) fn local_path(tree: &FileTree, id: NodeId) -> Option<PathBuf> {
    if tree.nodes[id].remote.is_some() {
        return None;
    }
    let mut names = Vec::new();
    let mut cur = id;
    loop {
        let node = &tree.nodes[cur];
        if let Some(path) = &node.path {
            let mut full = path.clone();
            for name in names.iter().rev() {
                full.push(name);
            }
            return Some(full);
        }
        // ドライブより上（端末や Root）までたどっても場所が無ければ、分からない。
        if node.depth <= 2 {
            return None;
        }
        names.push(node.name.clone());
        cur = node.parent?;
    }
}

/// LAN の相手のノードの場所（「端末名: ドライブ/フォルダ/…」）。端末のノードなら None。
/// ドライブ名が `/` や `\` で終わるときは区切りを重ねない。
pub(crate) fn remote_path_text(tree: &FileTree, id: NodeId) -> Option<String> {
    tree.nodes[id].remote.as_ref()?;
    let mut names = Vec::new();
    let mut cur = id;
    while tree.nodes[cur].depth > 1 {
        names.push(tree.nodes[cur].name.as_str());
        cur = tree.nodes[cur].parent?;
    }
    if names.is_empty() {
        return None;
    }
    names.reverse();
    let mut path = String::new();
    for part in names {
        if !path.is_empty() && !path.ends_with('/') && !path.ends_with('\\') {
            path.push('/');
        }
        path.push_str(part);
    }
    Some(format!("{}: {path}", tree.nodes[cur].name))
}

/// 長い文字列（パスやファイル名）を、末尾（ファイル名）を多めに残して途中を「…」で省く。
fn shorten_middle(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    if max_chars == 0 {
        return String::new();
    }
    if max_chars == 1 {
        return "…".to_string();
    }
    let keep = max_chars - 1;
    let head = keep / 3;
    let tail = keep - head;
    let mut out = String::new();
    for c in text.chars().take(head) {
        out.push(c);
    }
    out.push('…');
    for c in text.chars().skip(count - tail) {
        out.push(c);
    }
    out
}

fn system_ms(t: SystemTime) -> Option<i64> {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_millis()).ok(),
        Err(e) => i64::try_from(e.duration().as_millis()).ok().map(|ms| -ms),
    }
}

/// ハッシュの先頭だけ。
fn short_hash(hex: &str) -> String {
    if hex.len() > 16 {
        format!("{}…", &hex[..16])
    } else {
        hex.to_string()
    }
}

/// 3 桁ごとに「,」を入れる。
fn group_digits(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// UTC の Unix 時刻（ミリ秒）を、この PC の時刻で「2026-11-18 23:45」の形にする。
fn format_local(ms: i64) -> String {
    format_with_offset(ms, local_offset_secs())
}

fn format_with_offset(ms: i64, offset_secs: i64) -> String {
    let secs = ms.div_euclid(1000) + offset_secs;
    let days = secs.div_euclid(86_400);
    let rest = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        rest / 3600,
        rest % 3600 / 60
    )
}

/// 1970-01-01 からの日数を（年、月、日）にする（Howard Hinnant の civil_from_days）。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// この PC の時刻と UTC の差（秒）。起動して最初に聞かれたときに 1 回だけ調べる（夏時間の切り替えは
/// 見ない）。
fn local_offset_secs() -> i64 {
    static OFFSET: OnceLock<i64> = OnceLock::new();
    *OFFSET.get_or_init(read_local_offset)
}

#[cfg(windows)]
fn read_local_offset() -> i64 {
    use windows_sys::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
    // SAFETY: 書き込み先は呼び出しの間だけ生きている、ゼロで埋めた構造体。
    let mut info: TIME_ZONE_INFORMATION = unsafe { std::mem::zeroed() };
    let id = unsafe { GetTimeZoneInformation(&mut info) };
    // 2 は夏時間の最中（TIME_ZONE_ID_DAYLIGHT）。Bias は「UTC = 時刻 + Bias」の分。
    let bias = info.Bias + if id == 2 { info.DaylightBias } else { 0 };
    -i64::from(bias) * 60
}

#[cfg(not(windows))]
fn read_local_offset() -> i64 {
    std::process::Command::new("date")
        .arg("+%z")
        .output()
        .ok()
        .and_then(|o| parse_offset(String::from_utf8_lossy(&o.stdout).trim()))
        .unwrap_or(0)
}

/// 「+0900」「-0530」を秒にする。
#[cfg_attr(windows, allow(dead_code))]
fn parse_offset(text: &str) -> Option<i64> {
    let (sign, digits) = match text.as_bytes().first()? {
        b'+' => (1, &text[1..]),
        b'-' => (-1, &text[1..]),
        _ => return None,
    };
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let h: i64 = digits[..2].parse().ok()?;
    let m: i64 = digits[2..].parse().ok()?;
    Some(sign * (h * 3600 + m * 60))
}

/// ツールチップを作る。返すのは目印と、後から合計を書き換えるためのエンティティ群。
fn spawn_tooltip(
    commands: &mut Commands,
    font: &LabelFont,
    cursor: Vec2,
    window_size: Vec2,
    rows: &[Row],
) -> ShownTooltip {
    let text_font = TextFont::from_font_size(13.0).with_font(font.0.clone());
    let title_font = TextFont::from_font_size(14.0).with_font(font.0.clone());
    let max_w = MAX_WIDTH.min((window_size.x - WINDOW_MARGIN * 2.0).max(80.0));
    let max_h = (window_size.y - WINDOW_MARGIN * 2.0).max(60.0);
    // `panel` の横余白（padding 10*2 + border 1*2）を引いた内側の上限幅。
    // `width: Auto` の `panel` が中身を測るとき、各子にもこの上限を渡しておかないと、
    // 長い行が 1 行分の高さで測られたあと `panel` だけ `max_width` に縮み、折り返した分が下に欠ける。
    let inner_max_w = (max_w - 22.0).max(40.0);
    let anchor = commands
        .spawn((
            TooltipAnchor,
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(cursor.x),
                top: Val::Px(cursor.y),
                width: Val::Px(0.0),
                height: Val::Px(0.0),
                ..default()
            },
            GlobalZIndex(40),
            Pickable::IGNORE,
        ))
        .id();
    // `FixedNode` でレイアウトだけビューポート基準にする。大きさ 0 の `anchor` のまま子として測ると、
    // Taffy が `available_width = 0` で折り返し高さを計算してしまい、枠だけ異常に縦長になるため。
    // `ChildOf(anchor)` は維持するので、`Popover` の親基準の位置決めと `GlobalZIndex` はそのまま効く。
    let panel = commands
        .spawn((
            FixedNode,
            ClampPopover,
            Node {
                position_type: PositionType::Absolute,
                flex_direction: FlexDirection::Column,
                row_gap: Val::Px(3.0),
                padding: UiRect::axes(Val::Px(10.0), Val::Px(8.0)),
                border: UiRect::all(Val::Px(1.0)),
                border_radius: BorderRadius::all(Val::Px(6.0)),
                max_width: Val::Px(max_w),
                max_height: Val::Px(max_h),
                overflow: Overflow::clip(),
                ..default()
            },
            BackgroundColor(PANEL_COLOR),
            BorderColor::all(BORDER_COLOR),
            Popover {
                positions: [
                    (PopoverSide::Bottom, PopoverAlign::Start, 18.0),
                    (PopoverSide::Top, PopoverAlign::Start, 6.0),
                    (PopoverSide::Bottom, PopoverAlign::End, 18.0),
                    (PopoverSide::Top, PopoverAlign::End, 6.0),
                    (PopoverSide::Bottom, PopoverAlign::Center, 18.0),
                    (PopoverSide::Top, PopoverAlign::Center, 6.0),
                    (PopoverSide::Right, PopoverAlign::Center, 14.0),
                    (PopoverSide::Left, PopoverAlign::Center, 14.0),
                ]
                .into_iter()
                .map(|(side, align, gap)| PopoverPlacement { side, align, gap })
                .collect(),
                window_margin: WINDOW_MARGIN,
            },
            Pickable::IGNORE,
            ChildOf(anchor),
        ))
        .id();
    let mut size_text = None;
    let mut totals_slot = None;
    for r in rows {
        if r.label.is_empty() {
            commands.spawn((
                Node {
                    max_width: Val::Px(inner_max_w),
                    ..default()
                },
                Text::new(r.value.clone()),
                title_font.clone(),
                TextColor(TEXT_COLOR),
                TextLayout::linebreak(LineBreak::AnyCharacter),
                Pickable::IGNORE,
                ChildOf(panel),
            ));
            continue;
        }
        if r.label == "大きさ" {
            let slot = commands
                .spawn((
                    TooltipTotalsSlot,
                    Node {
                        flex_direction: FlexDirection::Column,
                        row_gap: Val::Px(3.0),
                        max_width: Val::Px(inner_max_w),
                        ..default()
                    },
                    Pickable::IGNORE,
                    ChildOf(panel),
                ))
                .id();
            let val_entity = spawn_kv_line(
                commands,
                slot,
                inner_max_w,
                &text_font,
                r.label,
                r.value.clone(),
                true,
            );
            size_text = Some(val_entity);
            totals_slot = Some(slot);
            continue;
        }
        if r.label == "中身"
            && let Some(slot) = totals_slot
        {
            spawn_kv_line(
                commands,
                slot,
                inner_max_w,
                &text_font,
                r.label,
                r.value.clone(),
                false,
            );
            continue;
        }
        spawn_kv_line(
            commands,
            panel,
            inner_max_w,
            &text_font,
            r.label,
            r.value.clone(),
            false,
        );
    }
    ShownTooltip {
        anchor,
        size_text,
        totals_slot,
        inner_max_w,
        waiting_total: false,
    }
}

/// 見出しと値の 1 行を `parent` の子として作る。値テキストのエンティティを返す。
fn spawn_kv_line(
    commands: &mut Commands,
    parent: Entity,
    inner_max_w: f32,
    text_font: &TextFont,
    label: &'static str,
    value: String,
    is_size: bool,
) -> Entity {
    let line = commands
        .spawn((
            Node {
                column_gap: Val::Px(8.0),
                max_width: Val::Px(inner_max_w),
                ..default()
            },
            Pickable::IGNORE,
            ChildOf(parent),
        ))
        .id();
    commands.spawn((
        Node {
            min_width: Val::Px(52.0),
            flex_shrink: 0.0,
            ..default()
        },
        Text::new(label),
        text_font.clone(),
        TextColor(DIM_COLOR),
        TextLayout::linebreak(LineBreak::NoWrap),
        Pickable::IGNORE,
        ChildOf(line),
    ));
    let place = label == "場所";
    let mut val_cmd = commands.spawn((
        Text::new(if place {
            lisa_core::display_path::wrap_at_separators(&value)
        } else {
            value
        }),
        text_font.clone(),
        TextColor(TEXT_COLOR),
        TextLayout::linebreak(if place {
            LineBreak::WordOrCharacter
        } else {
            LineBreak::AnyCharacter
        }),
        Pickable::IGNORE,
        ChildOf(line),
    ));
    if is_size {
        val_cmd.insert(TooltipSizeText);
    }
    val_cmd.id()
}

/// 出しているツールチップの「大きさ」を書き換え、合計が取れていれば「中身」の行を足す（枠を作り直さない）。
fn apply_total_to_tooltip(
    commands: &mut Commands,
    shown: &ShownTooltip,
    outcome: TotalOutcome,
    loaded_bytes: u64,
    font: &LabelFont,
    size_texts: &mut Query<&mut Text, With<TooltipSizeText>>,
) {
    let size_value = match outcome {
        TotalOutcome::Totals(t) => format_size(t.bytes),
        TotalOutcome::Unavailable => format_loaded_only_size(loaded_bytes),
    };
    if let Some(size_entity) = shown.size_text
        && let Ok(mut text) = size_texts.get_mut(size_entity)
    {
        text.0 = size_value;
    }
    if let TotalOutcome::Totals(t) = outcome
        && let Some(slot) = shown.totals_slot
    {
        let text_font = TextFont::from_font_size(13.0).with_font(font.0.clone());
        spawn_kv_line(
            commands,
            slot,
            shown.inner_max_w,
            &text_font,
            "中身",
            format_contents(t),
            false,
        );
    }
}

/// `Popover` の配置後でも、狭い窓の中央付近など上下左右いずれの候補でもはみ出す場合だけ、
/// 枠をウィンドウの余白（`window_margin`）の内側へ押し戻す。
///
/// 理由: `Popover` はカーソルから上・下・左・右の片側に置く候補から選ぶだけなので、
/// 例えば 420×260 の窓の中央 (210, 130) で 408×180 の枠を出すと、枠自体は窓より小さくても
/// どの候補も必ずはみ出す。`UiGlobalTransform` のみ平行移動することで、次フレームの
/// `position_popover` とのせめぎ合いを起こさずに描画・クリッピング・当たり判定を窓内へ収める。
fn clamp_popover_to_window(
    mut popovers: Query<
        (
            Entity,
            &mut UiGlobalTransform,
            &ComputedNode,
            &ComputedUiRenderTargetInfo,
            &Popover,
        ),
        With<ClampPopover>,
    >,
    mut child_transforms: Query<&mut UiGlobalTransform, Without<ClampPopover>>,
    children_q: Query<&Children>,
) {
    for (entity, mut global, computed, target, popover) in &mut popovers {
        let win = target.logical_size();
        if win.x <= 0.0 || win.y <= 0.0 {
            continue;
        }
        let isf = computed.inverse_scale_factor;
        let size = computed.size * isf;
        let half = size * 0.5;
        let center = global.translation * isf;
        let min = center - half;
        let max = center + half;
        let margin = popover.window_margin;
        let min_x = margin.min((win.x - size.x).max(0.0) * 0.5);
        let min_y = margin.min((win.y - size.y).max(0.0) * 0.5);
        let max_x = (win.x - margin).max(win.x - min_x);
        let max_y = (win.y - margin).max(win.y - min_y);

        let mut shift = Vec2::ZERO;
        if max.x > max_x {
            shift.x -= max.x - max_x;
        }
        if min.x + shift.x < min_x {
            shift.x += min_x - (min.x + shift.x);
        }
        if max.y > max_y {
            shift.y -= max.y - max_y;
        }
        if min.y + shift.y < min_y {
            shift.y += min_y - (min.y + shift.y);
        }
        if shift.abs().max_element() < 0.01 {
            continue;
        }
        let phys_shift = shift * target.scale_factor();
        let mut affine = global.affine();
        affine.translation += phys_shift;
        *global = affine.into();
        if let Ok(children) = children_q.get(entity) {
            for child in children.iter() {
                translate_descendants(child, phys_shift, &children_q, &mut child_transforms);
            }
        }
    }
}

fn translate_descendants(
    entity: Entity,
    phys_shift: Vec2,
    children_q: &Query<&Children>,
    transforms: &mut Query<&mut UiGlobalTransform, Without<ClampPopover>>,
) {
    if let Ok(mut global) = transforms.get_mut(entity) {
        let mut affine = global.affine();
        affine.translation += phys_shift;
        *global = affine.into();
    }
    if let Ok(children) = children_q.get(entity) {
        for child in children.iter() {
            translate_descendants(child, phys_shift, children_q, transforms);
        }
    }
}

#[derive(Default)]
struct TooltipLogState {
    anchor: Option<Entity>,
    frame: u32,
    first_dump: Option<String>,
    last_size_text: Option<String>,
    update_countdown: Option<u32>,
}

/// `LISA_TOOLTIP_AT` が指定されているとき、ツールチップの各ノードのレイアウトと文字の状態を
/// ログに出す（初回表示と、裏で数えた合計が差し替わったあと、それぞれ 1 回）。
#[allow(clippy::type_complexity)]
fn log_tooltip_layout(
    mut state: Local<TooltipLogState>,
    window: Single<&Window, With<PrimaryWindow>>,
    anchors: Query<Entity, With<TooltipAnchor>>,
    size_texts: Query<&Text, With<TooltipSizeText>>,
    nodes: Query<(
        &ComputedNode,
        &UiGlobalTransform,
        &UiTransform,
        Option<&Text>,
        Option<&TextLayoutInfo>,
        Option<&bevy::ui::TextNodeFlags>,
        Option<&CalculatedClip>,
        Option<&ComputedStackIndex>,
        Option<&GlobalZIndex>,
        Option<&InheritedVisibility>,
        Option<&ViewVisibility>,
        Option<&ComputedUiRenderTargetInfo>,
        Option<&Children>,
        Has<Popover>,
    )>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    let Some(anchor) = anchors.iter().next() else {
        *state = TooltipLogState::default();
        return;
    };
    if state.anchor != Some(anchor) {
        *state = TooltipLogState {
            anchor: Some(anchor),
            ..Default::default()
        };
    }
    let frame = state.frame;
    state.frame += 1;

    let current_size_text = size_texts.iter().next().map(|t| t.0.clone());
    if frame > 3 && current_size_text != state.last_size_text {
        state.last_size_text = current_size_text.clone();
        state.update_countdown = Some(2);
    }

    let need_dump = frame == 0 || frame == 3 || state.update_countdown == Some(0);
    if !need_dump {
        if frame < 3 {
            redraw.write(RequestRedraw);
        } else if let Some(left) = state.update_countdown.as_mut() {
            *left -= 1;
            redraw.write(RequestRedraw);
        }
        return;
    }

    let mut dump = String::new();
    let w = window.into_inner();
    use std::fmt::Write;
    let _ = writeln!(
        dump,
        "window=({:.1}x{:.1} logical, scale={:.2}) frame={frame}",
        w.width(),
        w.height(),
        w.scale_factor()
    );

    fn visit(
        entity: Entity,
        depth: usize,
        anchor_pos: Vec2,
        win_size: Vec2,
        out: &mut String,
        nodes: &Query<(
            &ComputedNode,
            &UiGlobalTransform,
            &UiTransform,
            Option<&Text>,
            Option<&TextLayoutInfo>,
            Option<&bevy::ui::TextNodeFlags>,
            Option<&CalculatedClip>,
            Option<&ComputedStackIndex>,
            Option<&GlobalZIndex>,
            Option<&InheritedVisibility>,
            Option<&ViewVisibility>,
            Option<&ComputedUiRenderTargetInfo>,
            Option<&Children>,
            Has<Popover>,
        )>,
    ) {
        let Ok((
            computed,
            global,
            local,
            text,
            text_info,
            flags,
            clip,
            stack,
            gz,
            inh_vis,
            view_vis,
            target_info,
            children,
            is_popover,
        )) = nodes.get(entity)
        else {
            return;
        };
        let indent = "  ".repeat(depth);
        let role = if depth == 0 {
            "anchor"
        } else if is_popover {
            "panel(Popover)"
        } else if text.is_some() {
            "text"
        } else {
            "line"
        };
        let isf = computed.inverse_scale_factor;
        let logical_size = computed.size * isf;
        let logical_pos = global.translation * isf;
        let rect_min = logical_pos - logical_size * 0.5;
        let rect_max = logical_pos + logical_size * 0.5;
        let _ = write!(
            out,
            "{indent}{role}({entity:?}) phys_size=({:.1},{:.1}) log_size=({:.1},{:.1}) log_center=({:.1},{:.1}) log_rect=[({:.1},{:.1})..({:.1},{:.1})] isf={:.3} tr=({:?},{:?})",
            computed.size.x,
            computed.size.y,
            logical_size.x,
            logical_size.y,
            logical_pos.x,
            logical_pos.y,
            rect_min.x,
            rect_min.y,
            rect_max.x,
            rect_max.y,
            isf,
            local.translation.x,
            local.translation.y,
        );
        if is_popover {
            let dmin = rect_min - anchor_pos;
            let dmax = rect_max - anchor_pos;
            let fits_win = rect_min.x >= 0.0
                && rect_min.y >= 0.0
                && rect_max.x <= win_size.x
                && rect_max.y <= win_size.y;
            let fits_margin = rect_min.x >= WINDOW_MARGIN - 0.5
                && rect_min.y >= WINDOW_MARGIN - 0.5
                && rect_max.x <= win_size.x - WINDOW_MARGIN + 0.5
                && rect_max.y <= win_size.y - WINDOW_MARGIN + 0.5;
            let _ = write!(
                out,
                " delta_anchor=[({:.1},{:.1})..({:.1},{:.1})] fits_win={fits_win} fits_margin={fits_margin}",
                dmin.x, dmin.y, dmax.x, dmax.y
            );
        }
        if let Some(t) = text {
            let _ = write!(out, " text={:?}", t.0);
        }
        if let Some(ti) = text_info {
            let _ = write!(
                out,
                " text_info=(size=({:.1},{:.1}), glyphs={}, runs={}, scale={:.2})",
                ti.size.x,
                ti.size.y,
                ti.glyphs.len(),
                ti.run_geometry.len(),
                ti.scale_factor
            );
        }
        if let Some(f) = flags {
            let _ = write!(out, " flags={f:?}");
        }
        if let Some(c) = clip {
            let _ = write!(out, " clip={c:?}");
        }
        if let Some(s) = stack {
            let _ = write!(out, " stack={}", s.0);
        }
        if let Some(z) = gz {
            let _ = write!(out, " gz={}", z.0);
        }
        if let Some(iv) = inh_vis {
            let _ = write!(out, " inh_vis={}", iv.get());
        }
        if let Some(vv) = view_vis {
            let _ = write!(out, " view_vis={}", vv.get());
        }
        if let Some(ti) = target_info {
            let _ = write!(
                out,
                " target=(scale={:.2}, phys={}x{})",
                ti.scale_factor(),
                ti.physical_size().x,
                ti.physical_size().y
            );
        }
        out.push('\n');
        if let Some(children) = children {
            for child in children.iter() {
                visit(child, depth + 1, anchor_pos, win_size, out, nodes);
            }
        }
    }

    let anchor_pos = nodes
        .get(anchor)
        .map(|(c, g, ..)| g.translation * c.inverse_scale_factor)
        .unwrap_or_default();
    let win_size = Vec2::new(w.width(), w.height());
    visit(anchor, 0, anchor_pos, win_size, &mut dump, &nodes);

    if frame == 0 {
        state.first_dump = Some(dump);
        redraw.write(RequestRedraw);
    } else if frame == 3 {
        state.last_size_text = current_size_text;
        let first = state.first_dump.take().unwrap_or_default();
        info!(
            "tooltip layout dump (frame 0 vs frame 3):\n--- frame 0 ---\n{first}--- frame 3 ---\n{dump}"
        );
    } else if state.update_countdown == Some(0) {
        state.update_countdown = None;
        info!("tooltip layout dump (after total update):\n{dump}");
    }
}

/// ディスクのパスの見た目を、そのまま文字にする（テスト用の小さな道具）。
#[cfg(test)]
fn path_text(p: &std::path::Path) -> String {
    p.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_digits_by_three() {
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(999), "999");
        assert_eq!(group_digits(1000), "1,000");
        assert_eq!(group_digits(1_234_567_890), "1,234,567,890");
    }

    #[test]
    fn formats_time_with_offset() {
        // 2026-11-18 14:44:32 UTC。
        let ms = 1_795_013_072_000;
        assert_eq!(format_with_offset(ms, 0), "2026-11-18 14:44");
        assert_eq!(format_with_offset(ms, 9 * 3600), "2026-11-18 23:44");
        assert_eq!(format_with_offset(ms, 10 * 3600), "2026-11-19 00:44");
        assert_eq!(format_with_offset(0, 0), "1970-01-01 00:00");
        assert_eq!(format_with_offset(-1, 0), "1969-12-31 23:59");
        // うるう日。
        assert_eq!(format_with_offset(951_782_400_000, 0), "2000-02-29 00:00");
    }

    #[test]
    fn parses_offsets() {
        assert_eq!(parse_offset("+0900"), Some(32_400));
        assert_eq!(parse_offset("-0530"), Some(-19_800));
        assert_eq!(parse_offset("0900"), None);
        assert_eq!(parse_offset("+9"), None);
    }

    #[test]
    fn shortens_hash() {
        assert_eq!(short_hash("abc"), "abc");
        assert_eq!(short_hash(&"a".repeat(64)), format!("{}…", "a".repeat(16)));
    }

    #[test]
    fn local_and_remote_paths() {
        let mut tree = FileTree::dummy();
        // ダミーの木はディスクの場所を持たないので、ドライブに場所を付けて確かめる。
        let device = tree.nodes[FileTree::ROOT].children[0];
        let drive = tree.nodes[device].children[0];
        tree.nodes[drive].path = Some(PathBuf::from("/Volumes/X"));
        let child = tree.nodes[drive].children[0];
        let expected = PathBuf::from("/Volumes/X").join(&tree.nodes[child].name);
        assert_eq!(
            local_path(&tree, child).map(|p| path_text(&p)),
            Some(path_text(&expected))
        );
        assert_eq!(local_path(&tree, device), None);

        let remote = tree.ensure_remote_device("peer", "Windows PC", LoadState::Unloaded);
        tree.add_remote_drives(
            remote,
            vec![crate::model::tree::RemoteDrive {
                id: None,
                mount: None,
                label: "C:".into(),
                entry_id: 1,
                listed: true,
                has_hash: false,
                has_error: false,
            }],
        );
        let c = tree.nodes[remote].children[0];
        assert_eq!(remote_path_text(&tree, remote), None);
        assert_eq!(
            remote_path_text(&tree, c).as_deref(),
            Some("Windows PC: C:")
        );
        assert_eq!(local_path(&tree, c), None);
    }

    #[test]
    fn parses_debug_tooltip_at() {
        assert_eq!(
            parse_tooltip_at(Some("400,300")),
            Some(Vec2::new(400.0, 300.0))
        );
        assert_eq!(
            parse_tooltip_at(Some(" 12.5 , 34.5 ")),
            Some(Vec2::new(12.5, 34.5))
        );
        assert_eq!(parse_tooltip_at(Some("")), None);
        assert_eq!(parse_tooltip_at(Some("400")), None);
        assert_eq!(parse_tooltip_at(Some("NaN,10")), None);
        assert_eq!(parse_tooltip_at(None), None);
    }

    fn temp_test_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lisa-tooltip-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn row_value<'a>(rows: &'a [Row], label: &str) -> Option<&'a str> {
        rows.iter()
            .find(|r| r.label == label)
            .map(|r| r.value.as_str())
    }

    fn one_local_drive(dir: &std::path::Path) -> (FileTree, NodeId, NodeId) {
        let vol = lisa_core::volumes::Volume {
            id: None,
            label: "TestDrive".into(),
            fs: "apfs".into(),
            mount_path: dir.to_path_buf(),
            capacity: None,
            available: None,
            removable: false,
            network: false,
            read_only: false,
            case_sensitive: None,
            hidden: false,
        };
        let tree = FileTree::this_device("ThisMac", &[vol]);
        let device = tree.nodes[FileTree::ROOT].children[0];
        let drive = tree.nodes[device].children[0];
        (tree, device, drive)
    }

    #[test]
    fn describe_local_file_folder_drive_and_device() {
        let dir = temp_test_dir("local");
        let file_path = dir.join("note.txt");
        let body = vec![b'x'; 2048];
        std::fs::write(&file_path, &body).unwrap();
        let sub_dir = dir.join("docs");
        std::fs::create_dir(&sub_dir).unwrap();
        std::fs::write(sub_dir.join("inner.bin"), vec![0u8; 512]).unwrap();

        let (mut tree, device, drive) = one_local_drive(&dir);
        tree.add_listing(
            drive,
            lisa_core::listing::read_dir(&dir).map_err(|e| e.to_string()),
        );
        let find = |tree: &FileTree, parent: NodeId, name: &str| {
            tree.nodes[parent]
                .children
                .iter()
                .copied()
                .find(|&c| tree.nodes[c].name == name)
                .unwrap()
        };
        let file_id = find(&tree, drive, "note.txt");
        let sub_id = find(&tree, drive, "docs");
        tree.add_listing(
            sub_id,
            lisa_core::listing::read_dir(&sub_dir).map_err(|e| e.to_string()),
        );
        tree.mark_hashed(file_id);

        // 手元のファイル
        let file_rows = describe(&tree, None, file_id, None);
        assert_eq!(row_value(&file_rows, ""), Some("note.txt"));
        assert_eq!(row_value(&file_rows, "種類"), Some("ファイル"));
        assert_eq!(
            row_value(&file_rows, "大きさ"),
            Some("2.0 KB（2,048 バイト）")
        );
        assert_eq!(
            row_value(&file_rows, "場所"),
            Some("ThisMac › TestDrive › note.txt")
        );
        assert_eq!(row_value(&file_rows, "状態"), None);
        #[cfg(target_os = "macos")]
        {
            let out = std::process::Command::new("stat")
                .args(["-f", "%SB|%Sm", "-t", "%Y-%m-%d %H:%M"])
                .arg(&file_path)
                .output()
                .unwrap();
            assert!(out.status.success());
            let stat_str = String::from_utf8(out.stdout).unwrap();
            let (birth, mtime) = stat_str.trim().split_once('|').unwrap();
            assert_eq!(row_value(&file_rows, "作成"), Some(birth));
            assert_eq!(row_value(&file_rows, "更新"), Some(mtime));
        }

        // 手元のフォルダ（512 バイト = 1024 B 未満なので「512 バイト」だけ）
        let folder_rows = describe(&tree, None, sub_id, None);
        assert_eq!(row_value(&folder_rows, ""), Some("docs"));
        assert_eq!(row_value(&folder_rows, "種類"), Some("フォルダ"));
        assert_eq!(row_value(&folder_rows, "大きさ"), Some("512 バイト"));
        assert!(row_value(&folder_rows, "作成").is_some());
        assert!(row_value(&folder_rows, "更新").is_some());
        assert_eq!(
            row_value(&folder_rows, "場所"),
            Some("ThisMac › TestDrive › docs")
        );

        // フォルダの合計の状態ごとの表示（計算中 / 合計あり / 読んだ分だけ）。
        let computing_rows = describe(&tree, None, sub_id, Some(FolderTotalDisplay::Computing));
        assert_eq!(row_value(&computing_rows, "大きさ"), Some("計算中…"));
        assert_eq!(row_value(&computing_rows, "中身"), None);

        let totals_rows = describe(
            &tree,
            None,
            sub_id,
            Some(FolderTotalDisplay::Totals(FolderTotals {
                files: 1234,
                folders: 56,
                bytes: 13_207_483_392,
            })),
        );
        assert_eq!(
            row_value(&totals_rows, "大きさ"),
            Some("12.3 GB（13,207,483,392 バイト）")
        );
        assert_eq!(
            row_value(&totals_rows, "中身"),
            Some("ファイル 1,234 / フォルダ 56")
        );

        let fallback_rows = describe(&tree, None, sub_id, Some(FolderTotalDisplay::LoadedOnly));
        assert_eq!(
            row_value(&fallback_rows, "大きさ"),
            Some("512 バイト（読んだ分だけ）")
        );
        assert_eq!(row_value(&fallback_rows, "中身"), None);

        // ドライブ（作成・更新日時は出さない）
        let drive_rows = describe(&tree, None, drive, None);
        assert_eq!(row_value(&drive_rows, ""), Some("TestDrive"));
        assert_eq!(row_value(&drive_rows, "種類"), Some("ドライブ"));
        assert_eq!(
            row_value(&drive_rows, "大きさ"),
            Some("2.5 KB（2,560 バイト）")
        );
        assert_eq!(row_value(&drive_rows, "作成"), None);
        assert_eq!(row_value(&drive_rows, "更新"), None);
        assert_eq!(row_value(&drive_rows, "場所"), Some("ThisMac › TestDrive"));

        // 端末（場所・作成・更新日時は出さない）
        let dev_rows = describe(&tree, None, device, None);
        assert_eq!(row_value(&dev_rows, ""), Some("ThisMac"));
        assert_eq!(row_value(&dev_rows, "種類"), Some("端末"));
        assert_eq!(
            row_value(&dev_rows, "大きさ"),
            Some("2.5 KB（2,560 バイト）")
        );
        assert_eq!(row_value(&dev_rows, "場所"), None);
        assert_eq!(row_value(&dev_rows, "作成"), None);
        assert_eq!(row_value(&dev_rows, "更新"), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shortens_long_names_and_paths_keeping_tail() {
        assert_eq!(shorten_middle("short.txt", 20), "short.txt");
        let path = "/Users/nozomi/Documents/very/deep/nested/folder/final_report.pdf";
        let cut = shorten_middle(path, 30);
        assert_eq!(cut.chars().count(), 30);
        assert!(cut.starts_with("/Users/no"));
        assert!(cut.contains('…'));
        assert!(cut.ends_with("final_report.pdf"));

        // 日本語（マルチバイト）でも文字数で切れ、末尾が残る。
        let jp = "あいうえおかきくけこさしすせそたちつてと.pdf";
        let cut_jp = shorten_middle(jp, 12);
        assert_eq!(cut_jp.chars().count(), 12);
        assert!(cut_jp.contains('…'));
        assert!(cut_jp.ends_with("てと.pdf"));
    }

    #[test]
    fn remote_path_does_not_double_slash_for_root_drive() {
        let mut tree = FileTree::dummy();
        let remote = tree.ensure_remote_device("peer-mac", "MacBook", LoadState::Unloaded);
        tree.add_remote_drives(
            remote,
            vec![crate::model::tree::RemoteDrive {
                id: None,
                mount: None,
                label: "/".into(),
                entry_id: 1,
                listed: true,
                has_hash: false,
                has_error: false,
            }],
        );
        let root_drive = tree.nodes[remote].children[0];
        tree.add_remote_children(
            root_drive,
            vec![lisa_core::query::Item {
                id: 2,
                name: "Users".into(),
                kind: "dir".into(),
                size: 0,
                mtime: None,
                hash: None,
                cloud_only: false,
                other_volume: false,
                listed: true,
                error: None,
            }],
        );
        let users = tree.nodes[root_drive].children[0];
        tree.add_remote_children(
            users,
            vec![lisa_core::query::Item {
                id: 3,
                name: "memo.txt".into(),
                kind: "file".into(),
                size: 42,
                mtime: None,
                hash: None,
                cloud_only: false,
                other_volume: false,
                listed: false,
                error: None,
            }],
        );
        let memo = tree.nodes[users].children[0];
        assert_eq!(
            remote_path_text(&tree, root_drive).as_deref(),
            Some("MacBook: /")
        );
        assert_eq!(
            remote_path_text(&tree, memo).as_deref(),
            Some("MacBook: /Users/memo.txt")
        );
        let rows = describe(&tree, None, memo, None);
        assert_eq!(row_value(&rows, "大きさ"), Some("42 バイト"));
        assert_eq!(
            row_value(&rows, "場所"),
            Some("MacBook (LAN) › / › Users › memo.txt")
        );
    }

    #[test]
    fn describe_remote_files_from_add_remote_children() {
        let mut tree = FileTree::dummy();
        let remote = tree.ensure_remote_device("peer-1", "Windows PC", LoadState::Unloaded);
        tree.add_remote_drives(
            remote,
            vec![crate::model::tree::RemoteDrive {
                id: None,
                mount: None,
                label: "C:".into(),
                entry_id: 1,
                listed: true,
                has_hash: false,
                has_error: false,
            }],
        );
        let drive = tree.nodes[remote].children[0];
        let mtime_ms = 1_795_013_072_000_i64;
        let full_hash = "0123456789abcdef".repeat(4);
        tree.add_remote_children(
            drive,
            vec![
                lisa_core::query::Item {
                    id: 10,
                    name: "report.pdf".into(),
                    kind: "file".into(),
                    size: 4096,
                    mtime: Some(mtime_ms),
                    hash: Some(full_hash),
                    cloud_only: false,
                    other_volume: false,
                    listed: false,
                    error: None,
                },
                lisa_core::query::Item {
                    id: 11,
                    name: "cloud.mov".into(),
                    kind: "file".into(),
                    size: 1024 * 1024,
                    mtime: None,
                    hash: None,
                    cloud_only: true,
                    other_volume: false,
                    listed: false,
                    error: None,
                },
                lisa_core::query::Item {
                    id: 12,
                    name: "broken.dat".into(),
                    kind: "file".into(),
                    size: 0,
                    mtime: None,
                    hash: None,
                    cloud_only: false,
                    other_volume: false,
                    listed: false,
                    error: Some("access denied".into()),
                },
            ],
        );

        let find = |name: &str| {
            tree.nodes[drive]
                .children
                .iter()
                .copied()
                .find(|&c| tree.nodes[c].name == name)
                .unwrap()
        };

        let report = describe(&tree, None, find("report.pdf"), None);
        assert_eq!(row_value(&report, ""), Some("report.pdf"));
        assert_eq!(row_value(&report, "種類"), Some("ファイル"));
        assert_eq!(row_value(&report, "大きさ"), Some("4.0 KB（4,096 バイト）"));
        assert_eq!(
            row_value(&report, "更新"),
            Some(format_local(mtime_ms).as_str())
        );
        assert_eq!(row_value(&report, "ハッシュ"), Some("0123456789abcdef…"));
        assert_eq!(
            row_value(&report, "場所"),
            Some("Windows PC (LAN) › C: › report.pdf")
        );
        assert_eq!(row_value(&report, "状態"), None);
        assert_eq!(row_value(&report, "エラー"), None);

        let cloud = describe(&tree, None, find("cloud.mov"), None);
        assert_eq!(row_value(&cloud, "種類"), Some("ファイル（クラウドのみ）"));
        assert_eq!(
            row_value(&cloud, "場所"),
            Some("Windows PC (LAN) › C: › cloud.mov")
        );

        let broken = describe(&tree, None, find("broken.dat"), None);
        assert_eq!(row_value(&broken, "状態"), Some("読めなかった"));
        assert_eq!(row_value(&broken, "エラー"), Some("access denied"));

        // 相手の端末とドライブ。
        let rdev = describe(&tree, None, remote, None);
        assert_eq!(row_value(&rdev, "種類"), Some("端末"));
        assert_eq!(row_value(&rdev, "場所"), None);
        let rdrive = describe(&tree, None, drive, None);
        assert_eq!(row_value(&rdrive, "種類"), Some("ドライブ"));
        assert_eq!(row_value(&rdrive, "場所"), Some("Windows PC (LAN) › C:"));
    }

    #[test]
    fn local_db_totals_sum_nested_folders_drives_and_device() {
        let dir = temp_test_dir("db-totals");
        let drive1 = dir.join("drive1");
        let deep = drive1.join("a").join("b").join("c");
        let empty = drive1.join("empty");
        let unlisted = drive1.join("unlisted");
        let drive2 = dir.join("drive2");
        let unreadable = drive1.join("unreadable");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::create_dir_all(&empty).unwrap();
        std::fs::create_dir_all(&unlisted).unwrap();
        std::fs::create_dir_all(&unreadable).unwrap();
        std::fs::create_dir_all(&drive2).unwrap();

        std::fs::write(drive1.join("root.bin"), vec![0u8; 10]).unwrap();
        std::fs::write(drive1.join("a").join("f1.bin"), vec![0u8; 20]).unwrap();
        std::fs::write(drive1.join("a").join("b").join("f2.bin"), vec![0u8; 30]).unwrap();
        std::fs::write(deep.join("f3.bin"), vec![0u8; 40]).unwrap();
        std::fs::write(deep.join("f4.bin"), vec![0u8; 50]).unwrap();
        std::fs::write(drive2.join("d2.bin"), vec![0u8; 500]).unwrap();

        let db_path = dir.join("lisa.db");
        {
            let mut db = Db::open(&db_path).unwrap();
            let dev = db.this_device("TestMac", "TestMac", "macos").unwrap();
            let make_vol = |id: &str, label: &str, mount: &Path| lisa_core::volumes::Volume {
                id: Some(id.into()),
                label: label.into(),
                fs: "apfs".into(),
                mount_path: mount.to_path_buf(),
                capacity: None,
                available: None,
                removable: false,
                network: false,
                read_only: false,
                case_sensitive: Some(false),
                hidden: false,
            };
            db.upsert_drive(&dev, &make_vol("test:d1", "D1", &drive1))
                .unwrap();
            db.upsert_drive(&dev, &make_vol("test:d2", "D2", &drive2))
                .unwrap();
            db.upsert_drive(&dev, &make_vol("test:d3", "D3", &dir.join("drive3")))
                .unwrap();

            for (did, mount, folder) in [
                ("test:d1", &drive1, drive1.as_path()),
                ("test:d1", &drive1, drive1.join("a").as_path()),
                ("test:d1", &drive1, drive1.join("a").join("b").as_path()),
                ("test:d1", &drive1, deep.as_path()),
                ("test:d1", &drive1, empty.as_path()),
                ("test:d2", &drive2, drive2.as_path()),
            ] {
                let listing = lisa_core::listing::read_dir(folder).map_err(|e| e.to_string());
                db.save_listing(did, mount, folder, &listing).unwrap();
            }
            // 読めなかったフォルダ（error あり）。
            db.save_listing(
                "test:d1",
                &drive1,
                &unreadable,
                &Err("permission denied".into()),
            )
            .unwrap();
        }

        let db = Db::open(&db_path).unwrap();

        // 3 段下のフォルダまで全部足される（a -> b -> c）。
        assert_eq!(
            local_path_total(&db, &drive1.join("a")),
            Some(FolderTotals {
                files: 4,
                folders: 2,
                bytes: 20 + 30 + 40 + 50,
            })
        );
        // 空のフォルダ（読み込み済み）は 0。
        assert_eq!(
            local_path_total(&db, &empty),
            Some(FolderTotals {
                files: 0,
                folders: 0,
                bytes: 0,
            })
        );
        // まだ読んでいないフォルダ・読めなかったフォルダ・ファイル・DB に無いパスは None。
        assert_eq!(local_path_total(&db, &unlisted), None);
        assert_eq!(local_path_total(&db, &unreadable), None);
        assert_eq!(local_path_total(&db, &drive1.join("root.bin")), None);
        assert_eq!(local_path_total(&db, &drive1.join("missing")), None);

        // ドライブ 1 の全体: ファイル 5（root, f1, f2, f3, f4）、フォルダ 6（a, b, c, empty, unlisted, unreadable）、150 バイト。
        assert_eq!(
            local_drive_total(&db, "test:d1"),
            Some(FolderTotals {
                files: 5,
                folders: 6,
                bytes: 150,
            })
        );
        // まだ読んでいないドライブ（test:d3）は None。
        assert_eq!(local_drive_total(&db, "test:d3"), None);

        // 端末全体: DB に残すドライブ全部の合計（未読の d3 は飛ばし、d1 + d2 = 6 ファイル、6 フォルダ、650 バイト）。
        assert_eq!(
            local_device_total(&db, &["test:d1".into(), "test:d2".into(), "test:d3".into()],),
            Some(FolderTotals {
                files: 6,
                folders: 6,
                bytes: 650,
            })
        );
        assert_eq!(local_device_total(&db, &["test:d3".into()]), None);
        assert_eq!(local_device_total(&db, &[]), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn total_cache_expires_invalidates_and_deduplicates_in_flight() {
        let mut cache = TotalCache::new();
        let t0 = Instant::now();
        let totals = TotalOutcome::Totals(FolderTotals {
            files: 10,
            folders: 2,
            bytes: 4096,
        });

        // 同じノードは二重に開始できない。
        let gen1 = cache.try_begin(7).unwrap();
        assert!(cache.is_in_flight(7));
        assert_eq!(cache.try_begin(7), None);

        // 完了するとキャッシュに入り、30 秒未満は返る。
        cache.finish(7, gen1, totals, t0);
        assert!(!cache.is_in_flight(7));
        assert_eq!(cache.get(7, t0), Some(totals));
        assert_eq!(cache.get(7, t0 + Duration::from_secs(29)), Some(totals));
        // 30 秒ちょうどで期限切れになり捨てられる。
        assert_eq!(cache.get(7, t0 + TOTAL_CACHE_TTL), None);

        // 木が変わった（invalidate）ら、期限内でも捨てられる。
        let gen2 = cache.try_begin(8).unwrap();
        cache.finish(8, gen2, totals, t0);
        assert_eq!(cache.get(8, t0 + Duration::from_secs(1)), Some(totals));
        cache.invalidate();
        assert_eq!(cache.get(8, t0 + Duration::from_secs(1)), None);

        // 計算中に invalidate が起きた場合:
        // - Unavailable は古い世代なのでキャッシュに入れず、再試行できるようにする。
        let gen_old = cache.try_begin(9).unwrap();
        cache.invalidate();
        cache.finish(9, gen_old, TotalOutcome::Unavailable, t0);
        assert!(!cache.is_in_flight(9));
        assert_eq!(cache.get(9, t0), None);
        // - Totals はキャッシュに入り、直後の出し直しで再計算しない。
        let gen_old2 = cache.try_begin(10).unwrap();
        cache.invalidate();
        cache.finish(10, gen_old2, totals, t0);
        assert_eq!(cache.get(10, t0), Some(totals));
    }

    #[test]
    fn tooltip_wraps_long_path_and_fits_in_window() {
        use bevy::camera::{ComputedCameraValues, RenderTargetInfo};
        use bevy::ecs::system::RunSystemOnce;
        use bevy::text::TextPlugin;
        use bevy::ui::UiPlugin;
        use bevy::ui_widgets::popover::PopoverPlugin;

        fn measure_tooltip(
            win: Vec2,
            cursor: Vec2,
            rows: &[Row],
        ) -> (Rect, Vec<(String, Rect, usize)>) {
            let mut app = App::new();
            app.add_plugins((
                MinimalPlugins,
                AssetPlugin::default(),
                WindowPlugin {
                    primary_window: None,
                    exit_condition: bevy::window::ExitCondition::DontExit,
                    ..default()
                },
                bevy::transform::TransformPlugin,
                bevy::input::InputPlugin,
                bevy::picking::DefaultPickingPlugins,
                bevy::image::ImagePlugin::default(),
                TextPlugin,
                UiPlugin,
                PopoverPlugin,
            ));
            app.add_systems(
                PostUpdate,
                clamp_popover_to_window
                    .after(UiSystems::Layout)
                    .before(UiSystems::PostLayout),
            );
            app.init_asset::<TextureAtlasLayout>();
            let font_handle = app
                .world_mut()
                .resource_mut::<Assets<Font>>()
                .add(Font::from_bytes(
                    include_bytes!("../../assets/fonts/NotoSansJP-Light.otf").to_vec(),
                ));
            app.world_mut().spawn((
                Camera2d,
                Camera {
                    computed: ComputedCameraValues {
                        target_info: Some(RenderTargetInfo {
                            physical_size: UVec2::new(win.x as u32 * 2, win.y as u32 * 2),
                            scale_factor: 2.0,
                        }),
                        ..default()
                    },
                    ..default()
                },
            ));
            let font = LabelFont(font_handle);
            let rows_owned = rows.to_vec();
            let shown = app
                .world_mut()
                .run_system_once(move |mut commands: Commands| {
                    spawn_tooltip(&mut commands, &font, cursor, win, &rows_owned)
                })
                .unwrap();
            let anchor = shown.anchor;
            for _ in 0..4 {
                app.update();
            }
            let world = app.world();
            let panel = world.get::<Children>(anchor).unwrap()[0];
            let panel_rect = {
                let c = world.get::<ComputedNode>(panel).unwrap();
                let g = world.get::<UiGlobalTransform>(panel).unwrap();
                let half = c.size * c.inverse_scale_factor * 0.5;
                let center = g.translation * c.inverse_scale_factor;
                Rect::from_center_half_size(center, half)
            };
            let mut texts = Vec::new();
            let mut stack = vec![panel];
            while let Some(e) = stack.pop() {
                if let Some(t) = world.get::<Text>(e) {
                    let c = world.get::<ComputedNode>(e).unwrap();
                    let g = world.get::<UiGlobalTransform>(e).unwrap();
                    let ti = world.get::<TextLayoutInfo>(e).unwrap();
                    let half = c.size * c.inverse_scale_factor * 0.5;
                    let center = g.translation * c.inverse_scale_factor;
                    texts.push((
                        t.0.clone(),
                        Rect::from_center_half_size(center, half),
                        ti.glyphs.len(),
                    ));
                }
                if let Some(children) = world.get::<Children>(e) {
                    stack.extend(children.iter());
                }
            }
            (panel_rect, texts)
        }

        let long_path = shorten_middle(
            "/Users/nozomi/Documents/github/lisa/crates/lisa-core/src/very/deeply/nested/directory/structure/report_2026_final_v3.pdf",
            MAX_PATH_CHARS,
        );
        let rows = vec![
            row("", "とても長い名前のファイル_report_2026_final_v3.pdf"),
            row("種類", "ファイル"),
            row("大きさ", "12.3 MB（12,897,484 バイト）"),
            row("作成", "2026-11-18 23:45"),
            row("更新", "2026-11-19 12:34"),
            row("ハッシュ", "0123456789abcdef…"),
            row("場所", long_path),
        ];

        // 通常ウィンドウ（1280x720）の四隅近くと中央。
        let win = Vec2::new(1280.0, 720.0);
        for cursor in [
            Vec2::new(10.0, 40.0),
            Vec2::new(1270.0, 40.0),
            Vec2::new(10.0, 710.0),
            Vec2::new(1270.0, 710.0),
            Vec2::new(640.0, 360.0),
        ] {
            let (rect, texts) = measure_tooltip(win, cursor, &rows);
            assert!(
                rect.width() <= MAX_WIDTH + 0.5,
                "cursor={cursor:?} rect={rect:?}"
            );
            assert!(
                rect.height() < 260.0,
                "height inflated: cursor={cursor:?} rect={rect:?}"
            );
            assert!(
                rect.min.x >= 0.0
                    && rect.min.y >= 0.0
                    && rect.max.x <= win.x
                    && rect.max.y <= win.y,
                "overflowed 1280x720: cursor={cursor:?} rect={rect:?}"
            );
            assert!(!texts.is_empty());
            for (text, trect, glyphs) in &texts {
                assert!(
                    *glyphs > 0 && trect.width() > 0.0 && trect.height() > 0.0,
                    "invisible text {text:?}: trect={trect:?} glyphs={glyphs}"
                );
                assert!(
                    trect.min.x >= rect.min.x - 0.5
                        && trect.min.y >= rect.min.y - 0.5
                        && trect.max.x <= rect.max.x + 0.5
                        && trect.max.y <= rect.max.y + 0.5,
                    "text {text:?} outside panel: trect={trect:?} panel={rect:?}"
                );
            }
        }

        // 狭いウィンドウ（420x260）でも枠・テキストともに窓内へ収まる。
        let narrow = Vec2::new(420.0, 260.0);
        for cursor in [
            Vec2::new(10.0, 40.0),
            Vec2::new(410.0, 40.0),
            Vec2::new(10.0, 250.0),
            Vec2::new(410.0, 250.0),
            Vec2::new(210.0, 130.0),
        ] {
            let (rect, texts) = measure_tooltip(narrow, cursor, &rows);
            assert!(
                rect.min.x >= 0.0
                    && rect.min.y >= 0.0
                    && rect.max.x <= narrow.x
                    && rect.max.y <= narrow.y,
                "overflowed narrow 420x260: cursor={cursor:?} rect={rect:?}"
            );
            for (text, trect, glyphs) in &texts {
                assert!(
                    *glyphs > 0
                        && trect.min.x >= rect.min.x - 0.5
                        && trect.min.y >= rect.min.y - 0.5
                        && trect.max.x <= rect.max.x + 0.5
                        && trect.max.y <= rect.max.y + 0.5,
                    "narrow text {text:?} outside panel: trect={trect:?} panel={rect:?}"
                );
            }
        }
    }

    #[test]
    fn tooltip_updates_folder_total_in_place_and_stays_inside_window() {
        use bevy::camera::{ComputedCameraValues, RenderTargetInfo};
        use bevy::ecs::system::RunSystemOnce;
        use bevy::text::TextPlugin;
        use bevy::ui::UiPlugin;
        use bevy::ui_widgets::popover::PopoverPlugin;

        fn setup_ui_app(win: Vec2) -> (App, LabelFont) {
            let mut app = App::new();
            app.add_plugins((
                MinimalPlugins,
                AssetPlugin::default(),
                WindowPlugin {
                    primary_window: None,
                    exit_condition: bevy::window::ExitCondition::DontExit,
                    ..default()
                },
                bevy::transform::TransformPlugin,
                bevy::input::InputPlugin,
                bevy::picking::DefaultPickingPlugins,
                bevy::image::ImagePlugin::default(),
                TextPlugin,
                UiPlugin,
                PopoverPlugin,
            ));
            app.add_systems(
                PostUpdate,
                clamp_popover_to_window
                    .after(UiSystems::Layout)
                    .before(UiSystems::PostLayout),
            );
            app.init_asset::<TextureAtlasLayout>();
            let font_handle = app
                .world_mut()
                .resource_mut::<Assets<Font>>()
                .add(Font::from_bytes(
                    include_bytes!("../../assets/fonts/NotoSansJP-Light.otf").to_vec(),
                ));
            app.world_mut().spawn((
                Camera2d,
                Camera {
                    computed: ComputedCameraValues {
                        target_info: Some(RenderTargetInfo {
                            physical_size: UVec2::new(win.x as u32 * 2, win.y as u32 * 2),
                            scale_factor: 2.0,
                        }),
                        ..default()
                    },
                    ..default()
                },
            ));
            (app, LabelFont(font_handle))
        }

        fn inspect_panel(app: &App, anchor: Entity) -> (Entity, Rect, Vec<(String, Rect, usize)>) {
            let world = app.world();
            let panel = world.get::<Children>(anchor).unwrap()[0];
            let panel_rect = {
                let c = world.get::<ComputedNode>(panel).unwrap();
                let g = world.get::<UiGlobalTransform>(panel).unwrap();
                let half = c.size * c.inverse_scale_factor * 0.5;
                let center = g.translation * c.inverse_scale_factor;
                Rect::from_center_half_size(center, half)
            };
            let mut texts = Vec::new();
            let mut stack = vec![panel];
            while let Some(e) = stack.pop() {
                if let Some(t) = world.get::<Text>(e) {
                    let c = world.get::<ComputedNode>(e).unwrap();
                    let g = world.get::<UiGlobalTransform>(e).unwrap();
                    let ti = world.get::<TextLayoutInfo>(e).unwrap();
                    let half = c.size * c.inverse_scale_factor * 0.5;
                    let center = g.translation * c.inverse_scale_factor;
                    texts.push((
                        t.0.clone(),
                        Rect::from_center_half_size(center, half),
                        ti.glyphs.len(),
                    ));
                }
                if let Some(children) = world.get::<Children>(e) {
                    stack.extend(children.iter());
                }
            }
            (panel, panel_rect, texts)
        }

        for (win, cursor) in [
            (Vec2::new(1280.0, 720.0), Vec2::new(1260.0, 700.0)),
            (Vec2::new(420.0, 260.0), Vec2::new(210.0, 130.0)),
        ] {
            let (mut app, font) = setup_ui_app(win);
            let initial_rows = vec![
                row("", "Macintosh HD"),
                row("種類", "ドライブ"),
                row("大きさ", "計算中…"),
                row("場所", "/System/Volumes/Data"),
            ];
            let font_spawn = LabelFont(font.0.clone());
            let shown = app
                .world_mut()
                .run_system_once(move |mut commands: Commands| {
                    spawn_tooltip(&mut commands, &font_spawn, cursor, win, &initial_rows)
                })
                .unwrap();
            for _ in 0..4 {
                app.update();
            }
            let (panel_before, _, texts_before) = inspect_panel(&app, shown.anchor);
            assert!(texts_before.iter().any(|(t, ..)| t == "計算中…"));
            assert!(!texts_before.iter().any(|(t, ..)| t == "中身"));

            // 合計が届いたら同じパネルのまま「大きさ」を書き換え「中身」を足す。
            let font_apply = LabelFont(font.0.clone());
            let shown_anchor = shown.anchor;
            let shown_size = shown.size_text;
            let shown_slot = shown.totals_slot;
            let inner_max_w = shown.inner_max_w;
            app.world_mut()
                .run_system_once(
                    move |mut commands: Commands,
                          mut size_texts: Query<&mut Text, With<TooltipSizeText>>| {
                        let s = ShownTooltip {
                            anchor: shown_anchor,
                            size_text: shown_size,
                            totals_slot: shown_slot,
                            inner_max_w,
                            waiting_total: true,
                        };
                        apply_total_to_tooltip(
                            &mut commands,
                            &s,
                            TotalOutcome::Totals(FolderTotals {
                                files: 1234,
                                folders: 56,
                                bytes: 13_207_483_392,
                            }),
                            512,
                            &font_apply,
                            &mut size_texts,
                        );
                    },
                )
                .unwrap();
            for _ in 0..4 {
                app.update();
            }
            let (panel_after, rect, texts_after) = inspect_panel(&app, shown.anchor);
            assert_eq!(panel_before, panel_after);
            assert!(
                texts_after
                    .iter()
                    .any(|(t, ..)| t == "12.3 GB（13,207,483,392 バイト）")
            );
            assert!(
                texts_after
                    .iter()
                    .any(|(t, ..)| t == "ファイル 1,234 / フォルダ 56")
            );
            assert!(!texts_after.iter().any(|(t, ..)| t == "計算中…"));
            assert!(
                rect.min.x >= 0.0
                    && rect.min.y >= 0.0
                    && rect.max.x <= win.x
                    && rect.max.y <= win.y,
                "after update overflowed {win:?}: rect={rect:?}"
            );
            for (text, trect, glyphs) in &texts_after {
                assert!(
                    *glyphs > 0
                        && trect.min.x >= rect.min.x - 0.5
                        && trect.min.y >= rect.min.y - 0.5
                        && trect.max.x <= rect.max.x + 0.5
                        && trect.max.y <= rect.max.y + 0.5,
                    "after update text {text:?} outside panel: trect={trect:?} panel={rect:?}"
                );
            }
        }

        // 取れなかったとき（Unavailable）は「（読んだ分だけ）」に戻り「中身」は足さない。
        let win = Vec2::new(1280.0, 720.0);
        let (mut app, font) = setup_ui_app(win);
        let initial_rows = vec![
            row("", "RemoteFolder"),
            row("種類", "フォルダ"),
            row("大きさ", "計算中…"),
        ];
        let font_spawn = LabelFont(font.0.clone());
        let shown = app
            .world_mut()
            .run_system_once(move |mut commands: Commands| {
                spawn_tooltip(
                    &mut commands,
                    &font_spawn,
                    Vec2::new(200.0, 200.0),
                    win,
                    &initial_rows,
                )
            })
            .unwrap();
        for _ in 0..4 {
            app.update();
        }
        let font_apply = LabelFont(font.0.clone());
        let shown_anchor = shown.anchor;
        let shown_size = shown.size_text;
        let shown_slot = shown.totals_slot;
        let inner_max_w = shown.inner_max_w;
        app.world_mut()
            .run_system_once(
                move |mut commands: Commands,
                      mut size_texts: Query<&mut Text, With<TooltipSizeText>>| {
                    let s = ShownTooltip {
                        anchor: shown_anchor,
                        size_text: shown_size,
                        totals_slot: shown_slot,
                        inner_max_w,
                        waiting_total: true,
                    };
                    apply_total_to_tooltip(
                        &mut commands,
                        &s,
                        TotalOutcome::Unavailable,
                        4096,
                        &font_apply,
                        &mut size_texts,
                    );
                },
            )
            .unwrap();
        for _ in 0..4 {
            app.update();
        }
        let (_, _, texts_after) = inspect_panel(&app, shown.anchor);
        assert!(
            texts_after
                .iter()
                .any(|(t, ..)| t == "4.0 KB（4,096 バイト）（読んだ分だけ）")
        );
        assert!(!texts_after.iter().any(|(t, ..)| t == "中身"));
    }
}
