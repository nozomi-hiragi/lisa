//! 描画状態の診断ログ（環境変数 `LISA_RENDER_DIAG=1` のときだけ有効）。
//!
//! 画面が真っ黒になる・描画が止まるといった不具合を追うため、1 秒ごとにメインワールドと
//! レンダーワールドのウィンドウ・カメラ・Bake・スワップチェーン取得状態をログへ出す。
//! デバッグ・リリースどちらのビルドでも環境変数で切り替えられる。

use std::time::{Duration, Instant};

use bevy::camera::{NormalizedRenderTarget, RenderTarget};
use bevy::prelude::*;
use bevy::render::camera::{ExtractedCamera, SortedCameras};
use bevy::render::gpu_readback::ReadbackOnce;
use bevy::render::sync_world::MainEntity;
use bevy::render::view::screenshot::Screenshot;
use bevy::render::view::{ExtractedWindow, SurfaceData, ViewTarget, ViewTargetAttachments};
use bevy::render::{Render, RenderApp, RenderSystems};
use bevy::window::{PrimaryWindow, RequestRedraw, WindowFocused, WindowOccluded};

use crate::model::tree::FileTree;
use crate::render::cache::{FolderImages, ThumbnailCapture};
use crate::render::status::Pulse;
use crate::view::{Animating, Busy, CurrentScene, Fade, MainCamera, Nav, WindowOccludedState};

pub struct RenderDiagPlugin;

impl Plugin for RenderDiagPlugin {
    fn build(&self, app: &mut App) {
        if !std::env::var_os("LISA_RENDER_DIAG").is_some_and(|v| !v.is_empty()) {
            return;
        }
        info!("LISA_RENDER_DIAG 有効: 1 秒ごとに描画状態をログに出します");
        app.add_systems(Last, (diag_events, diag_main_world));
        if let Some(render_app) = app.get_sub_app_mut(RenderApp) {
            render_app.add_systems(Render, diag_render_world.in_set(RenderSystems::Queue));
        }
    }
}

#[derive(Default)]
struct DiagClock {
    frames: u64,
    last_frames: u64,
    last_log: Option<Instant>,
}

impl DiagClock {
    fn tick(&mut self) -> Option<(u64, u64)> {
        self.frames += 1;
        let now = Instant::now();
        if self
            .last_log
            .is_some_and(|last| now.duration_since(last) < Duration::from_secs(1))
        {
            return None;
        }
        let delta = self.frames - self.last_frames;
        self.last_frames = self.frames;
        self.last_log = Some(now);
        Some((self.frames, delta))
    }
}

fn diag_events(
    mut occluded_events: MessageReader<WindowOccluded>,
    mut focused_events: MessageReader<WindowFocused>,
    mut redraw_events: MessageReader<RequestRedraw>,
    mut clock: Local<DiagClock>,
    mut redraw_count: Local<u64>,
    occluded: Res<WindowOccludedState>,
    pulses: Query<(), With<Pulse>>,
) {
    for ev in occluded_events.read() {
        info!(
            "[DIAG event] WindowOccluded win={:?} occluded={}",
            ev.window, ev.occluded
        );
    }
    for ev in focused_events.read() {
        info!(
            "[DIAG event] WindowFocused win={:?} focused={}",
            ev.window, ev.focused
        );
    }
    *redraw_count += redraw_events.read().count() as u64;
    if let Some((frame, df)) = clock.tick() {
        let redraws = std::mem::take(&mut *redraw_count);
        info!(
            "[DIAG activity] f={frame}(+{df}/s) occluded={} redraws={redraws} pulses={}",
            occluded.0,
            pulses.iter().count()
        );
    }
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn diag_main_world(
    mut clock: Local<DiagClock>,
    windows: Query<(Entity, &Window, Has<PrimaryWindow>)>,
    cameras: Query<(
        Entity,
        &Camera,
        &RenderTarget,
        Option<&Transform>,
        Option<&Projection>,
        Has<MainCamera>,
        Has<IsDefaultUiCamera>,
    )>,
    capture: Res<ThumbnailCapture>,
    folder_images: Res<FolderImages>,
    busy: Res<Busy>,
    animating: Option<Res<Animating>>,
    fades: Query<(), With<Fade>>,
    readbacks: Query<(), With<ReadbackOnce>>,
    screenshots: Query<(), With<Screenshot>>,
    nav: Res<Nav>,
    tree: Res<FileTree>,
    current: Option<Res<CurrentScene>>,
    all_entities: Query<()>,
    layouts: Res<crate::layout::Layouts>,
    (children, scene_roots): (
        Query<&Children>,
        Query<&bevy::camera::visibility::RenderLayers, With<crate::render::scene::SceneRoot>>,
    ),
) {
    let Some((frame, df)) = clock.tick() else {
        return;
    };
    let win_desc: Vec<String> = windows
        .iter()
        .map(|(e, w, primary)| {
            format!(
                "{e:?}(primary={primary},phys={}x{},scale={},focused={},visible={},mode={:?})",
                w.physical_width(),
                w.physical_height(),
                w.scale_factor(),
                w.focused,
                w.visible,
                w.present_mode,
            )
        })
        .collect();
    let mut win_cams = Vec::new();
    let mut bake_cam_count = 0usize;
    let mut bake_cam_issues = 0usize;
    for (e, cam, target, tf, proj, is_main, is_ui) in &cameras {
        let is_img = matches!(target, RenderTarget::Image(_));
        if is_img && !is_main && !is_ui {
            bake_cam_count += 1;
            if !cam.is_active || cam.physical_target_size().is_none() {
                bake_cam_issues += 1;
            }
            continue;
        }
        let kind = match (is_main, is_ui) {
            (true, true) => "Main+Ui",
            (true, false) => "Main",
            (false, true) => "Ui",
            (false, false) => "Other",
        };
        let pos = tf.map(|t| (t.translation.x, t.translation.y, t.translation.z));
        let scale = match proj {
            Some(Projection::Orthographic(o)) => Some(o.scale),
            _ => None,
        };
        win_cams.push(format!(
            "{kind}({e:?},ord={},act={},tgt={target:?},phys_tgt={:?},phys_vp={:?},vp={:?},clr={:?},pos={pos:?},scale={scale:?})",
            cam.order,
            cam.is_active,
            cam.physical_target_size(),
            cam.physical_viewport_size(),
            cam.viewport,
            cam.clear_color,
        ));
    }
    let bake_desc = capture.status_label();
    let (slots, baked) = folder_images.counts();
    let top_name = &tree.nodes[nav.top().node].name;
    let current_ok = current.as_ref().is_some_and(|c| all_entities.contains(c.0));
    let scene_children = current
        .as_ref()
        .and_then(|c| children.get(c.0).ok())
        .map_or(0, |c| c.len());
    let main_scenes = scene_roots
        .iter()
        .filter(|l| l.intersects(&bevy::camera::visibility::RenderLayers::default()))
        .count();
    let anchor_name = &tree.nodes[layouts.anchor].name;
    let top_in_layout = layouts.contains(nav.top().node);
    let top_fit = nav.top().frame(&layouts);
    info!(
        "[DIAG main] f={frame}(+{df}/s) win=[{}] cams=[{}] bake_cams={bake_cam_count}(issues={bake_cam_issues}) bake={bake_desc} imgs={baked}/{slots} busy={} anim={} fades={} readbacks={} shots={} nav={top_name}(depth={},in_layout={top_in_layout},fit={top_fit:?}) anchor={anchor_name} scene_ok={current_ok}(children={scene_children}) main_scenes={main_scenes}",
        win_desc.join("; "),
        win_cams.join("; "),
        busy.0,
        animating.is_some(),
        fades.iter().count(),
        readbacks.iter().count(),
        screenshots.iter().count(),
        nav.stack.len(),
    );
}

fn diag_render_world(
    mut clock: Local<DiagClock>,
    mut last_tex: Local<Option<bool>>,
    windows: Query<(Entity, MainEntity, &ExtractedWindow, Has<SurfaceData>)>,
    sorted: Res<SortedCameras>,
    cameras: Query<(&ExtractedCamera, Option<&ViewTarget>)>,
    attachments: Res<ViewTargetAttachments>,
) {
    let cur_tex = windows
        .iter()
        .next()
        .map(|(_, _, w, _)| w.swap_chain_texture.is_some());
    if cur_tex != *last_tex {
        info!(
            "[DIAG render FLIP] frame={} tex {:?} -> {:?}",
            clock.frames + 1,
            *last_tex,
            cur_tex
        );
        *last_tex = cur_tex;
    }
    let Some((frame, df)) = clock.tick() else {
        return;
    };
    let win_desc: Vec<String> = windows
        .iter()
        .map(|(re, me, w, has_surf)| {
            format!(
                "re={re:?}/me={me:?}(phys={}x{},surf={has_surf},tex={},view={},fmt={:?}/{:?},sz_chg={},init_pres={})",
                w.physical_width,
                w.physical_height,
                w.swap_chain_texture.is_some(),
                w.swap_chain_texture_view.is_some(),
                w.swap_chain_texture_format,
                w.swap_chain_texture_view_format,
                w.size_changed,
                w.needs_initial_present,
            )
        })
        .collect();
    let mut win_cam_desc = Vec::new();
    let mut img_cam_count = 0usize;
    let mut other_cam_count = 0usize;
    for sc in &sorted.0 {
        match &sc.target {
            Some(NormalizedRenderTarget::Window(wref)) => {
                let extra = match cameras.get(sc.entity) {
                    Ok((ec, vt)) => format!(
                        "phys_tgt={:?},phys_vp={:?},idx={},clr={:?},vt={},out_tex={},needs_pres={:?}",
                        ec.physical_target_size,
                        ec.physical_viewport_size,
                        ec.sorted_camera_index_for_target,
                        ec.clear_color,
                        vt.is_some(),
                        vt.is_some_and(|v| v.out_texture().is_some()),
                        vt.map(|v| v.needs_present()),
                    ),
                    Err(_) => "NO_EXTRACTED_CAM".to_string(),
                };
                win_cam_desc.push(format!(
                    "re={:?}(ord={},win={:?},out={:?},{extra})",
                    sc.entity, sc.order, wref, sc.output_mode
                ));
            }
            Some(NormalizedRenderTarget::Image(_)) => img_cam_count += 1,
            _ => other_cam_count += 1,
        }
    }
    info!(
        "[DIAG render] f={frame}(+{df}/s) ewin=[{}] sorted_total={} win_cams=[{}] img_cams={img_cam_count} other_cams={other_cam_count} vta_len={}",
        win_desc.join("; "),
        sorted.0.len(),
        win_cam_desc.join("; "),
        attachments.len(),
    );
}
