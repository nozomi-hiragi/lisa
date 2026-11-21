//! 外（`lisa-mcp` など）からの頼みを受けて、画面を動かす（`lisa_core::remote`）。
//!
//! 起動したら `127.0.0.1` で待ち、番号と合言葉を DB と同じフォルダの `control.json` に書く。頼みは
//! 待つ係のスレッドから届くので、届いたらアプリを起こし、毎フレーム `handle` で受け取って返事をする。
//! 頼めるのは、今の様子を聞く、場所へ飛ぶ（`scan::JumpTo`）、重複の一覧の開け閉めと組を選ぶ、
//! フォルダを先に読み直す、一時停止だけ。ファイルは動かさない。

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, channel};

use bevy::camera::visibility::RenderLayers;
use bevy::ecs::system::SystemParam;
use bevy::input::gestures::PinchGesture;
use bevy::input::mouse::{MouseScrollUnit, MouseWheel};
use bevy::prelude::*;
use bevy::window::{PrimaryWindow, RequestRedraw};
use bevy::winit::{EventLoopProxyWrapper, WinitUserEvent};
use lisa_core::remote::{
    self, Command, DebugSceneInfo, DebugState, DebugTileRect, Incoming, Reply, State,
};

use crate::dups::{Dups, DupsState};
use crate::input::gesture::{DebugCursor, Gesture, TrackpadInput};
use crate::input::mac_scroll::{self, RawScroll};
use crate::input::scroll_input::RawPhase;
use crate::layout::Layouts;
use crate::model::tree::FileTree;
use crate::render::cache::{FolderImages, ThumbnailCapture};
use crate::render::scene::{CONTENT_LAYER, SceneRoot};
use crate::scan::{JumpTo, ScanControl, ScanProgress, TreeChanged};
use crate::ui::context_menu::NavRequest;
use crate::ui::menu::MenuBreadcrumb;
use crate::view::{
    Animating, Breadcrumb, CurrentScene, Fade, MainCamera, Nav, SyncRequest, ViewSystems,
    WindowOccludedState, fit, is_attached, scene_path,
};

pub struct RemotePlugin;

impl Plugin for RemotePlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, start)
            .add_systems(Update, (handle, select_pending).chain().after(ViewSystems))
            .add_systems(Last, remove_on_exit);
    }
}

/// 検証用の頼みを受けるか（環境変数 `LISA_DEBUG_REMOTE=1` のときのみ）。
fn debug_remote_enabled() -> bool {
    std::env::var("LISA_DEBUG_REMOTE").as_deref() == Ok("1")
}

type DebugCameraItem = (
    &'static Camera,
    &'static GlobalTransform,
    &'static Transform,
    &'static Projection,
);

type DebugSceneItem = (
    Entity,
    &'static SceneRoot,
    &'static RenderLayers,
    Option<&'static Fade>,
    Option<&'static Children>,
);

/// 検証用コマンドで参照する描画・入力まわりのリソース群。
#[derive(SystemParam)]
struct DebugContext<'w, 's> {
    window: Single<'w, 's, &'static Window, With<PrimaryWindow>>,
    camera: Single<'w, 's, DebugCameraItem, With<MainCamera>>,
    layouts: Res<'w, Layouts>,
    breadcrumb: Res<'w, Breadcrumb>,
    menu_breadcrumb: Option<Res<'w, MenuBreadcrumb>>,
    current_scene: Option<Res<'w, CurrentScene>>,
    scenes: Query<'w, 's, DebugSceneItem>,
    gesture: Res<'w, Gesture>,
    trackpad: Res<'w, TrackpadInput>,
    animating: Option<Res<'w, Animating>>,
    capture: Res<'w, ThumbnailCapture>,
    folder_images: Res<'w, FolderImages>,
    image_assets: Res<'w, Assets<Image>>,
    tree_changed: Option<Res<'w, TreeChanged>>,
    jump_to: Option<Res<'w, JumpTo>>,
    sync_request: Res<'w, SyncRequest>,
    pinch_writer: MessageWriter<'w, PinchGesture>,
    wheel_writer: MessageWriter<'w, MouseWheel>,
}

/// 届いた頼みを受け取る口と、書いた `control.json` の場所。
#[derive(Resource)]
struct Inbox {
    receiver: Mutex<Receiver<Incoming>>,
    file: PathBuf,
}

impl Drop for Inbox {
    fn drop(&mut self) {
        remote::remove(&self.file);
    }
}

/// 一覧ができたら選ぶ組（頼まれたときにまだ一覧が無かったもの）。
#[derive(Resource)]
struct PendingSelect(String);

fn start(mut commands: Commands, proxy: Res<EventLoopProxyWrapper>) {
    let Some(dir) = crate::scan::db_path().and_then(|p| p.parent().map(PathBuf::from)) else {
        return;
    };
    let (sender, receiver) = channel();
    let proxy = (**proxy).clone();
    let deliver = move |incoming: Incoming| {
        let sent = sender.send(incoming).is_ok();
        // 止まっている間でも、すぐ受け取れるように起こす。
        let _ = proxy.send_event(WinitUserEvent::WakeUp);
        sent
    };
    match remote::listen(&dir, deliver) {
        Ok(file) => {
            info!("外からの頼みを待っています（{}）", file.display());
            commands.insert_resource(Inbox {
                receiver: Mutex::new(receiver),
                file,
            });
        }
        Err(error) => warn!("外からの頼みを待てませんでした: {error}"),
    }
}

/// 終わるときに `control.json` を消す。
fn remove_on_exit(mut exit: MessageReader<AppExit>, inbox: Option<Res<Inbox>>) {
    if exit.read().next().is_some()
        && let Some(inbox) = inbox
    {
        remote::remove(&inbox.file);
    }
}

#[allow(clippy::too_many_arguments)]
fn handle(
    mut commands: Commands,
    inbox: Option<Res<Inbox>>,
    nav: Res<Nav>,
    tree: Res<FileTree>,
    mut dups: ResMut<Dups>,
    mut catalog: ResMut<crate::catalog::CatalogView>,
    control: Option<Res<ScanControl>>,
    progress: Option<Res<ScanProgress>>,
    occluded: Res<WindowOccludedState>,
    mut redraw: MessageWriter<RequestRedraw>,
    mut exit: MessageWriter<AppExit>,
    mut dbg: DebugContext,
) {
    let Some(inbox) = inbox else {
        return;
    };
    let incoming: Vec<Incoming> = match inbox.receiver.lock() {
        Ok(receiver) => receiver.try_iter().collect(),
        Err(_) => return,
    };
    let debug_enabled = debug_remote_enabled();
    for Incoming { command, reply } in incoming {
        let mut debug_out: Option<DebugState> = None;
        let read_only = matches!(command, Command::State | Command::DebugState);
        let result: Result<(), String> = match command {
            Command::State => Ok(()),
            Command::Show { chain } => {
                if chain.is_empty() {
                    Err("行き先が空です".into())
                } else {
                    commands.insert_resource(JumpTo(chain));
                    Ok(())
                }
            }
            Command::Dups { open, select } => {
                dups.open = open;
                if !open {
                    dups.selected = None;
                }
                if let (true, Some(hash)) = (open, select) {
                    // 一覧がまだなら、できてから選ぶ（`select_pending`）。
                    match index_of(&dups, &hash) {
                        Some(i) => dups.selected = Some(i),
                        None => commands.insert_resource(PendingSelect(hash)),
                    }
                }
                Ok(())
            }
            Command::Catalog { open, select } => {
                catalog.open = open;
                if let Some(entry) = select {
                    let index = catalog
                        .snapshot
                        .as_ref()
                        .and_then(|s| s.hits.iter().position(|h| h.entry_id() == entry));
                    catalog.selected = index;
                }
                if catalog.selected.is_some() || select.is_none() {
                    Ok(())
                } else {
                    Err(format!(
                        "entry {} はカタログに当たっていません",
                        select.unwrap_or(0)
                    ))
                }
            }
            Command::Capture { name } => {
                match validate_capture(&name, occluded.0, crate::scan::data_dir()) {
                    Ok(path) => {
                        if let Some(dir) = path.parent() {
                            let _ = std::fs::create_dir_all(dir);
                        }
                        commands
                            .spawn(bevy::render::view::screenshot::Screenshot::primary_window())
                            .observe(bevy::render::view::screenshot::save_to_disk(path));
                        Ok(())
                    }
                    Err(message) => Err(message),
                }
            }
            Command::Rescan { path } => match &control {
                Some(control) if path.is_dir() => {
                    control.visible(path);
                    Ok(())
                }
                Some(_) => Err(format!("{} はフォルダではありません", path.display())),
                None => Err("読む係がまだ動いていません".into()),
            },
            Command::Pause { paused } => match &control {
                Some(control) => {
                    control.control.paused.store(paused, Ordering::Relaxed);
                    Ok(())
                }
                None => Err("読む係がまだ動いていません".into()),
            },
            Command::Quit => {
                exit.write(AppExit::Success);
                Ok(())
            }
            Command::DebugState => {
                if !debug_enabled {
                    Err("LISA_DEBUG_REMOTE=1 が必要です".into())
                } else {
                    debug_out = Some(collect_debug_state(&nav, &tree, occluded.0, &dbg));
                    Ok(())
                }
            }
            Command::DebugChanged { path } => {
                if !debug_enabled {
                    Err("LISA_DEBUG_REMOTE=1 が必要です".into())
                } else {
                    match &control {
                        Some(control) if path.is_dir() => {
                            control.debug_force_changed(path);
                            Ok(())
                        }
                        Some(_) => Err(format!("{} はフォルダではありません", path.display())),
                        None => Err("読む係がまだ動いていません".into()),
                    }
                }
            }
            Command::DebugPinch {
                delta,
                touching,
                cursor,
            } => {
                if !debug_enabled {
                    Err("LISA_DEBUG_REMOTE=1 が必要です".into())
                } else {
                    if let Some(c) = cursor {
                        commands.insert_resource(DebugCursor(Some(Vec2::new(c[0], c[1]))));
                    } else {
                        let c = Vec2::new(dbg.window.width() * 0.5, dbg.window.height() * 0.5);
                        commands.insert_resource(DebugCursor(Some(c)));
                    }
                    if let Some(t) = touching {
                        mac_scroll::set_pinch_touching(t);
                    }
                    if delta != 0.0 {
                        dbg.pinch_writer.write(PinchGesture(delta));
                    }
                    Ok(())
                }
            }
            Command::DebugPan {
                dx,
                dy,
                phase,
                cursor,
            } => {
                if !debug_enabled {
                    Err("LISA_DEBUG_REMOTE=1 が必要です".into())
                } else {
                    if let Some(c) = cursor {
                        commands.insert_resource(DebugCursor(Some(Vec2::new(c[0], c[1]))));
                    } else {
                        let c = Vec2::new(dbg.window.width() * 0.5, dbg.window.height() * 0.5);
                        commands.insert_resource(DebugCursor(Some(c)));
                    }
                    inject_pan(&mut dbg, dx, dy, &phase)
                }
            }
            Command::DebugWheel { y, cursor } => {
                if !debug_enabled {
                    Err("LISA_DEBUG_REMOTE=1 が必要です".into())
                } else {
                    if let Some(c) = cursor {
                        commands.insert_resource(DebugCursor(Some(Vec2::new(c[0], c[1]))));
                    } else {
                        let c = Vec2::new(dbg.window.width() * 0.5, dbg.window.height() * 0.5);
                        commands.insert_resource(DebugCursor(Some(c)));
                    }
                    dbg.wheel_writer.write(MouseWheel {
                        unit: MouseScrollUnit::Line,
                        x: 0.0,
                        y,
                        window: Entity::PLACEHOLDER,
                        phase: bevy::input::touch::TouchPhase::Moved,
                    });
                    Ok(())
                }
            }
            Command::DebugNav { action, screen_pos } => {
                if !debug_enabled {
                    Err("LISA_DEBUG_REMOTE=1 が必要です".into())
                } else {
                    match action.as_str() {
                        "up" => {
                            commands.insert_resource(NavRequest::Up);
                            Ok(())
                        }
                        "click" => match screen_pos {
                            Some([x, y]) => {
                                let (camera, camera_global, _, _) = *dbg.camera;
                                match camera.viewport_to_world_2d(camera_global, Vec2::new(x, y)) {
                                    Ok(world) => {
                                        commands.insert_resource(NavRequest::ClickAt(world));
                                        Ok(())
                                    }
                                    Err(err) => Err(format!("viewport_to_world_2d 失敗: {err:?}")),
                                }
                            }
                            None => Err("click には screen_pos が必要です".into()),
                        },
                        other => Err(format!("知らない action: {other}")),
                    }
                }
            }
        };
        let answer = match result {
            Ok(()) => {
                let s = state(&nav, &tree, &dups, control.as_deref(), progress.as_deref());
                match debug_out {
                    Some(d) => Reply::ok_debug(s, d),
                    None => Reply::ok(s),
                }
            }
            Err(message) => Reply::error(message),
        };
        let _ = reply.send(answer);
        if !read_only {
            redraw.write(RequestRedraw);
        }
    }
}

/// 頼まれた組を、一覧ができたら選ぶ。一覧を閉じたら諦める。
fn select_pending(
    mut commands: Commands,
    pending: Option<Res<PendingSelect>>,
    mut dups: ResMut<Dups>,
) {
    let Some(pending) = pending else {
        return;
    };
    if !dups.open {
        commands.remove_resource::<PendingSelect>();
        return;
    }
    if let Some(i) = index_of(&dups, &pending.0) {
        dups.selected = Some(i);
        commands.remove_resource::<PendingSelect>();
    } else if matches!(dups.state, DupsState::Ready(_) | DupsState::Failed(_)) {
        warn!("頼まれた重複の組が一覧にありません");
        commands.remove_resource::<PendingSelect>();
    }
}

/// 一覧の中の、ハッシュ（16 進）が `hash` の組の番号。
fn index_of(dups: &Dups, hash: &str) -> Option<usize> {
    let hash = hash.trim().to_ascii_lowercase();
    dups.groups()
        .iter()
        .position(|g| lisa_core::query::hex(&g.hash) == hash)
}

/// 検証用の 2 本指パン入力を、プラットフォームの本物の入力経路に流す。
fn inject_pan(dbg: &mut DebugContext, dx: f32, dy: f32, phase: &str) -> Result<(), String> {
    if dbg.trackpad.uses_raw() {
        match phase {
            "began" => mac_scroll::inject_scroll(RawScroll {
                phase: RawPhase::Began,
                momentum_phase: RawPhase::None,
                delta: Vec2::new(dx, dy),
            }),
            "changed" => mac_scroll::inject_scroll(RawScroll {
                phase: RawPhase::Changed,
                momentum_phase: RawPhase::None,
                delta: Vec2::new(dx, dy),
            }),
            "ended" => mac_scroll::inject_scroll(RawScroll {
                phase: RawPhase::Ended,
                momentum_phase: RawPhase::None,
                delta: Vec2::new(dx, dy),
            }),
            "tap" => {
                mac_scroll::inject_scroll(RawScroll {
                    phase: RawPhase::Began,
                    momentum_phase: RawPhase::None,
                    delta: Vec2::ZERO,
                });
                mac_scroll::inject_scroll(RawScroll {
                    phase: RawPhase::Changed,
                    momentum_phase: RawPhase::None,
                    delta: Vec2::new(dx, dy),
                });
                mac_scroll::inject_scroll(RawScroll {
                    phase: RawPhase::Ended,
                    momentum_phase: RawPhase::None,
                    delta: Vec2::ZERO,
                });
            }
            other => return Err(format!("知らない phase: {other}")),
        }
        return Ok(());
    }
    let factor = dbg.window.scale_factor();
    let emit = |writer: &mut MessageWriter<MouseWheel>,
                touch_phase: bevy::input::touch::TouchPhase,
                x: f32,
                y: f32| {
        writer.write(MouseWheel {
            unit: MouseScrollUnit::Pixel,
            x: x * factor,
            y: y * factor,
            window: Entity::PLACEHOLDER,
            phase: touch_phase,
        });
    };
    match phase {
        "began" => emit(
            &mut dbg.wheel_writer,
            bevy::input::touch::TouchPhase::Started,
            dx,
            dy,
        ),
        "changed" => emit(
            &mut dbg.wheel_writer,
            bevy::input::touch::TouchPhase::Moved,
            dx,
            dy,
        ),
        "ended" => emit(
            &mut dbg.wheel_writer,
            bevy::input::touch::TouchPhase::Ended,
            dx,
            dy,
        ),
        "tap" => {
            emit(
                &mut dbg.wheel_writer,
                bevy::input::touch::TouchPhase::Started,
                0.0,
                0.0,
            );
            emit(
                &mut dbg.wheel_writer,
                bevy::input::touch::TouchPhase::Moved,
                dx,
                dy,
            );
            emit(
                &mut dbg.wheel_writer,
                bevy::input::touch::TouchPhase::Ended,
                0.0,
                0.0,
            );
        }
        other => return Err(format!("知らない phase: {other}")),
    }
    Ok(())
}

/// 検証用の描画・ナビ・場面・カメラ状態をまとめて返す。
fn collect_debug_state(
    nav: &Nav,
    tree: &FileTree,
    occluded: bool,
    dbg: &DebugContext,
) -> DebugState {
    let window = &*dbg.window;
    let (camera, camera_global, cam_tf, cam_proj) = *dbg.camera;
    let camera_scale = match cam_proj {
        Projection::Orthographic(o) => o.scale,
        _ => f32::NAN,
    };
    let top = *nav.top();
    let (fit_pos, fit_scale) = fit(top.frame(&dbg.layouts), window);
    let parent_fit_scale = nav
        .stack
        .len()
        .checked_sub(2)
        .map(|i| fit(nav.stack[i].frame(&dbg.layouts), window).1);
    let (_, root_fit_scale) = fit(dbg.layouts[FileTree::ROOT].fit, window);

    let main_layer = RenderLayers::layer(CONTENT_LAYER);
    let current_entity = dbg.current_scene.as_ref().map(|c| c.0);
    let parent_entity = dbg.gesture.parent_scene_entity();
    let mut scenes = Vec::new();
    let mut fades = 0usize;
    let mut current_scene_folder = None;
    let mut current_scene_folder_name = None;
    for (entity, root, layers, fade, children) in &dbg.scenes {
        if fade.is_some() {
            fades += 1;
        }
        if !layers.intersects(&main_layer) {
            continue;
        }
        let is_current = current_entity == Some(entity);
        let folder_name = tree
            .nodes
            .get(root.folder)
            .map(|n| n.name.clone())
            .unwrap_or_else(|| format!("<invalid:{}>", root.folder));
        if is_current {
            current_scene_folder = Some(root.folder);
            current_scene_folder_name = Some(folder_name.clone());
        }
        scenes.push(DebugSceneInfo {
            entity: entity.to_bits(),
            folder: root.folder,
            folder_name,
            is_current,
            is_parent_scene: parent_entity == Some(entity),
            has_fade: fade.is_some(),
            fade_in: fade.map(|f| f.fade_in),
            fade_replaces: fade.and_then(|f| f.replaces.map(|e| e.to_bits())),
            children_count: children.map_or(0, |c| c.len()),
        });
    }

    let bake_state = dbg.capture.status_label();
    let (slots, ready) = dbg.folder_images.counts();

    let scene = scene_path(nav, tree);
    let scene_folder = scene[scene.len() - 1].node;
    let mut child_tiles = Vec::new();
    for &child in tree.nodes[scene_folder].children.iter().take(512) {
        let node = &tree.nodes[child];
        let ready_handle = dbg.folder_images.ready(tree, child);
        let image_format = ready_handle
            .as_ref()
            .and_then(|h| dbg.image_assets.get(h))
            .map(|img| format!("{:?}", img.texture_descriptor.format));
        let frame = dbg.layouts[child].frame;
        let screen_rect = if frame.width() > 0.0 && frame.height() > 0.0 {
            let tl =
                camera.world_to_viewport(camera_global, Vec3::new(frame.min.x, frame.max.y, 0.0));
            let br =
                camera.world_to_viewport(camera_global, Vec3::new(frame.max.x, frame.min.y, 0.0));
            match (tl, br) {
                (Ok(tl), Ok(br)) => Some([tl.x, tl.y, br.x - tl.x, br.y - tl.y]),
                _ => None,
            }
        } else {
            None
        };
        child_tiles.push(DebugTileRect {
            node: child,
            name: node.name.clone(),
            is_dir: node.is_dir,
            has_image: ready_handle.is_some(),
            image_format,
            screen_rect,
        });
    }

    let sanitize = |v: f32| if v.is_finite() { v } else { -1.0 };
    let camera_pos_raw = [cam_tf.translation.x, cam_tf.translation.y];
    let camera_pos_finite = camera_pos_raw.iter().all(|v| v.is_finite());
    let camera_scale_finite = camera_scale.is_finite();
    let fit_scale_finite = fit_scale.is_finite();

    DebugState {
        nav_nodes: nav.stack.iter().map(|l| l.node).collect(),
        nav_names: nav
            .stack
            .iter()
            .map(|l| tree.nodes[l.node].name.clone())
            .collect(),
        nav_display_names: crate::model::display_path::tree_parts(tree, top.node),
        nav_entries: nav.stack.iter().map(|l| tree.nodes[l.node].entry).collect(),
        nav_top_path: tree.nodes[top.node].path.clone(),
        nav_top_attached: is_attached(tree, top.node),
        nav_top_in_layout: dbg.layouts.contains(top.node),
        breadcrumb: dbg.breadcrumb.0.clone(),
        menu_breadcrumb: dbg.menu_breadcrumb.as_ref().map(|m| m.text.clone()),
        anchor_node: dbg.layouts.anchor,
        anchor_name: tree.nodes[dbg.layouts.anchor].name.clone(),
        anchor_attached: is_attached(tree, dbg.layouts.anchor),
        camera_pos: [sanitize(camera_pos_raw[0]), sanitize(camera_pos_raw[1])],
        camera_pos_finite,
        camera_scale: sanitize(camera_scale),
        camera_scale_finite,
        fit_pos: [sanitize(fit_pos.x), sanitize(fit_pos.y)],
        fit_scale: sanitize(fit_scale),
        fit_scale_finite,
        parent_fit_scale: parent_fit_scale.map(sanitize),
        root_fit_scale: sanitize(root_fit_scale),
        window_logical_size: [window.width(), window.height()],
        window_scale_factor: window.scale_factor(),
        main_scenes: scenes.len(),
        scenes,
        current_scene_folder,
        current_scene_folder_name,
        gesture_active: dbg.gesture.active(),
        gesture_has_parent_scene: dbg.gesture.has_parent_scene(),
        animating: dbg.animating.is_some(),
        fades,
        bake_state,
        folder_images_slots: slots,
        folder_images_ready: ready,
        tree_changed_pending: dbg.tree_changed.as_ref().is_some_and(|c| c.pending),
        jump_pending: dbg.jump_to.is_some() || dbg.sync_request.mode.is_some(),
        occluded,
        child_tiles,
    }
}

/// 今の様子。
fn state(
    nav: &Nav,
    tree: &FileTree,
    dups: &Dups,
    control: Option<&ScanControl>,
    progress: Option<&ScanProgress>,
) -> State {
    let scene = crate::view::scene_path(nav, tree);
    let folder = scene[scene.len() - 1].node;
    let mut state = State {
        folder: tree.nodes[folder].path.clone(),
        breadcrumb: scene
            .iter()
            .map(|l| tree.nodes[l.node].name.clone())
            .collect(),
        entry: tree.nodes[folder].entry,
        dups_open: dups.open,
        dups_state: match dups.state {
            DupsState::NotYet => "not_yet",
            DupsState::Computing { .. } => "computing",
            DupsState::Ready(_) => "ready",
            DupsState::Failed(_) => "failed",
        }
        .into(),
        dups_groups: dups.groups().len(),
        dups_selected: dups
            .selected
            .and_then(|i| dups.groups().get(i))
            .map(|g| lisa_core::query::hex(&g.hash)),
        paused: control.is_some_and(|c| c.control.paused.load(Ordering::Relaxed)),
        ..default()
    };
    if let Some(progress) = progress.and_then(|p| p.0.lock().ok()) {
        state.folders_read = progress.done;
        state.folders_failed = progress.failed;
        state.folders_queued = progress.queued;
        state.reading = progress.current.clone();
        state.files_hashed = progress.hash.files;
        state.bytes_hashed = progress.hash.bytes;
        state.hashing = progress.hash.folder.clone();
        state.finished = progress.finished.is_some();
    }
    state
}

/// `Command::Capture` の前提（ウィンドウが見えているか、名前、保存先）を確かめて保存先パスを返す。
/// ウィンドウが隠れている間（最小化・他の窓の裏など）は macOS の wgpu-hal がスワップチェーンを
/// 返さず、Bevy 0.20 の `Screenshot::primary_window()` が全画素 (0,0,0) の PNG を保存してしまうため
/// エラーで断る。
fn validate_capture(
    name: &str,
    occluded: bool,
    data_dir: Option<PathBuf>,
) -> Result<PathBuf, String> {
    if occluded {
        return Err(
            "ウィンドウが隠れている（最小化や他の窓の裏など）ためキャプチャできません。前面に出してからやり直してください"
                .into(),
        );
    }
    let valid = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    match data_dir {
        Some(dir) if valid => Ok(dir.join("screenshots").join(format!("{name}.png"))),
        Some(_) => Err("name は英数字・-・_ だけで 64 文字まで".into()),
        None => Err("データのフォルダが分かりません".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_capture_rejects_when_occluded() {
        let err = validate_capture("shot1", true, Some(PathBuf::from("/tmp/lisa"))).unwrap_err();
        assert!(err.contains("隠れている"), "unexpected error: {err}");
    }

    #[test]
    fn validate_capture_checks_name_and_dir() {
        let path = validate_capture("ok_name-1", false, Some(PathBuf::from("/tmp/lisa"))).unwrap();
        assert_eq!(path, PathBuf::from("/tmp/lisa/screenshots/ok_name-1.png"));
        assert!(validate_capture("../bad", false, Some(PathBuf::from("/tmp/lisa"))).is_err());
        assert!(validate_capture("ok", false, None).is_err());
    }
}
