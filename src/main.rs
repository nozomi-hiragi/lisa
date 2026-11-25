// Windows のリリースビルドでは、起動したときに黒いコンソールの窓を出さない。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod catalog;
mod crash;
#[cfg(debug_assertions)]
mod debug;
mod dups;
mod input;
mod lan;
mod layout;
mod merged;
mod model;
mod open_marker;
mod organize;
mod preview;
mod remote;
mod render;
mod scan;
mod settings;
mod ui;
mod view;

use bevy::log::{BoxedLayer, DEFAULT_FILTER, LogPlugin, tracing_subscriber};
use bevy::prelude::*;
use bevy::window::PresentMode;

fn main() {
    crash::install();
    // 目印をダブルクリックされたとき、すでに動いている Lisa があればそちらで開いて終わる。
    if open_marker::forward_to_running(&open_marker::marker_args()) {
        return;
    }
    let settings = settings::load();
    if settings.lan_share >= 0.5 {
        lan::ensure_sidecar();
    }
    let (present_mode, present_label, bad_present) =
        parse_present_mode(std::env::var("LISA_PRESENT_MODE").ok().as_deref());
    let (_, update_label, _) =
        view::parse_update_mode(std::env::var("LISA_UPDATE_MODE").ok().as_deref());
    let mut app = App::new();
    app.add_plugins(LogPlugin {
        // ICU4X の改行位置データを同梱していないため、その警告を止める。
        filter: format!("{DEFAULT_FILTER},icu_provider=off"),
        custom_layer: file_log_layer,
        ..default()
    });
    let backend_env = std::env::var_os("WGPU_BACKEND");
    #[cfg(windows)]
    let backend = windows_backend::choose(
        backend_env.as_deref(),
        &settings.windows_backend,
        scan::data_dir().as_deref(),
        env!("CARGO_PKG_VERSION"),
        windows_backend::has_vulkan_adapter,
    );
    let mut primary = Window {
        title: "Lisa — Root".into(),
        present_mode,
        ..default()
    };
    if let Some((w, h)) = parse_window_size(std::env::var("LISA_WINDOW_SIZE").ok().as_deref()) {
        primary.resolution = bevy::window::WindowResolution::new(w, h);
    }
    let default_plugins = DefaultPlugins
        .set(WindowPlugin {
            primary_window: Some(primary),
            ..default()
        })
        .disable::<LogPlugin>()
        // 音とゲームパッドは使わない。どちらも作るときに OS の機器を開くので、Windows で
        // 起動の途中で止まることがあった（GPU の情報のあとで止まり、Lisa の処理まで来なかった）。
        .set(render_plugin(
            backend_env.as_deref(),
            #[cfg(windows)]
            backend.backends,
        ))
        .disable::<bevy::audio::AudioPlugin>()
        .disable::<bevy::gilrs::GilrsPlugin>();
    // macOS では wgpu-hal 30.0.2（src/metal/surface.rs）が acquire_texture の中で
    // NSWindow.occlusionState を参照する。PipelinedRenderingPlugin が有効だと prepare_windows が
    // メインスレッド外の Render thread から実行され、AppKit のメインスレッド制約に反する未定義動作
    // （可視状態でも Occluded 扱いになる等）を起こし得るため、macOS だけ無効にしてメインスレッドで描く。
    #[cfg(target_os = "macos")]
    let default_plugins =
        default_plugins.disable::<bevy::render::pipelined_rendering::PipelinedRenderingPlugin>();
    app.add_plugins(default_plugins);
    info!("Bevy の準備ができました");
    app.add_plugins(preview::PreviewPlugin);
    app.add_plugins((
        settings::SettingsPlugin(
            settings,
            #[cfg(windows)]
            backend_env.as_ref().is_some_and(|value| !value.is_empty()),
        ),
        scan::ScanPlugin,
        lan::LanPlugin,
        view::TreeViewPlugin,
        ui::menu::MenuPlugin,
        ui::progress::ProgressPlugin,
        ui::dups::DupsPanelPlugin,
        ui::tooltip::TooltipPlugin,
        ui::context_menu::ContextMenuPlugin,
        dups::DupsPlugin,
        catalog::CatalogPlugin,
        ui::catalog::CatalogPanelPlugin,
        organize::OrganizePlugin,
        ui::organize::OrganizePanelPlugin,
        remote::RemotePlugin,
    ));
    app.add_plugins(open_marker::OpenMarkerPlugin);
    app.add_plugins(ui::SidePanelScrollPlugin);
    app.add_plugins(render::diag::RenderDiagPlugin);
    #[cfg(debug_assertions)]
    app.add_plugins(debug::DebugTitlePlugin);
    if let Some(bad) = bad_present {
        warn!("知らない LISA_PRESENT_MODE={bad:?}（auto_vsync を使います）");
    }
    #[cfg(windows)]
    if let Some(path) = backend.probe {
        windows_backend::clear_after_rendering(&mut app, path);
    }
    #[cfg(windows)]
    app.add_systems(
        Startup,
        move |adapter: Option<Res<bevy::render::renderer::RenderAdapterInfo>>| {
            let label =
                describe_wgpu_backend(backend_env.as_deref(), &backend.reason, adapter.as_deref());
            log_startup(&label, present_label, update_label);
        },
    );
    #[cfg(not(windows))]
    log_startup(
        &describe_wgpu_backend(backend_env.as_deref()),
        present_label,
        update_label,
    );
    app.run();
}

/// 実際に開いたログファイルの場所（起動時の 1 行に出すため）。
static LOG_FILE_PATH: std::sync::Mutex<Option<std::path::PathBuf>> = std::sync::Mutex::new(None);

/// ログを `<data_dir>/logs/lisa.log` にも出す。起動ごとに前の `lisa.log` を `lisa.1.log` に回し、
/// 2 世代だけ残す。別の Lisa が `lisa.log` を開いていて回せない・開けないときは、`lisa-<pid>.log` に書く。
/// 書けなければ黙って続ける。
fn file_log_layer(_app: &mut App) -> Option<BoxedLayer> {
    let logs_dir = scan::data_dir()?.join("logs");
    let (file, path) = open_log_file_in(&logs_dir, std::process::id())?;
    if let Ok(mut slot) = LOG_FILE_PATH.lock() {
        *slot = Some(path);
    }
    Some(Box::new(
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(file)),
    ))
}

fn open_log_file_in(
    logs_dir: &std::path::Path,
    pid: u32,
) -> Option<(std::fs::File, std::path::PathBuf)> {
    std::fs::create_dir_all(logs_dir).ok()?;
    let current = logs_dir.join("lisa.log");
    let prev = logs_dir.join("lisa.1.log");
    let rotated = if current.exists() {
        let _ = std::fs::remove_file(&prev);
        std::fs::rename(&current, &prev).is_ok()
    } else {
        true
    };
    if rotated
        && let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&current)
    {
        return Some((file, current));
    }
    let fallback = logs_dir.join(format!("lisa-{pid}.log"));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&fallback)
        .ok()?;
    Some((file, fallback))
}

/// 起動したらすぐ、版、OS、exe の場所、data_dir、ログファイル、選んだ描き方（backend、present_mode、update_mode、
/// Windows なら管理者かどうかも）を 1 行出し、起動前に試した `ensure_sidecar` の結果も残す。
fn log_startup(backend: &str, present_mode: &str, update_mode: &str) {
    let version = env!("CARGO_PKG_VERSION");
    let os = std::env::consts::OS;
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "?".into());
    let data = scan::data_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "?".into());
    let log = LOG_FILE_PATH
        .lock()
        .ok()
        .and_then(|s| s.clone())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "?".into());
    #[cfg(windows)]
    {
        let admin = unsafe { windows_sys::Win32::UI::Shell::IsUserAnAdmin() != 0 };
        info!(
            "Lisa {version} 起動: os={os}, exe={exe}, data_dir={data}, log={log}, admin={admin}, backend={backend}, present_mode={present_mode}, update_mode={update_mode}"
        );
    }
    #[cfg(not(windows))]
    {
        info!(
            "Lisa {version} 起動: os={os}, exe={exe}, data_dir={data}, log={log}, backend={backend}, present_mode={present_mode}, update_mode={update_mode}"
        );
    }
    if let Some(sidecar) = lan::take_startup_sidecar_log() {
        match sidecar {
            Ok(msg) => info!("{msg}"),
            Err(err) => warn!("{err}"),
        }
    }
}

/// 環境変数 `LISA_WINDOW_SIZE`（例: `960x600`）からウィンドウの大きさ（論理ピクセル）を読む。
fn parse_window_size(raw: Option<&str>) -> Option<(u32, u32)> {
    let s = raw?.trim();
    let (w, h) = s.split_once('x').or_else(|| s.split_once('X'))?;
    let w: u32 = w.trim().parse().ok()?;
    let h: u32 = h.trim().parse().ok()?;
    Some((w.clamp(640, 3840), h.clamp(400, 2160)))
}

/// 環境変数 `LISA_PRESENT_MODE` から `PresentMode` と起動ログ用の名前を決める。
fn parse_present_mode(raw: Option<&str>) -> (PresentMode, &'static str, Option<String>) {
    let Some(s) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return (PresentMode::AutoVsync, "auto_vsync (default)", None);
    };
    match s.to_ascii_lowercase().as_str() {
        "auto_vsync" => (PresentMode::AutoVsync, "auto_vsync", None),
        "auto_no_vsync" => (PresentMode::AutoNoVsync, "auto_no_vsync", None),
        "fifo" => (PresentMode::Fifo, "fifo", None),
        "fifo_relaxed" => (PresentMode::FifoRelaxed, "fifo_relaxed", None),
        "mailbox" => (PresentMode::Mailbox, "mailbox", None),
        "immediate" => (PresentMode::Immediate, "immediate", None),
        _ => (
            PresentMode::AutoVsync,
            "auto_vsync (default)",
            Some(s.to_string()),
        ),
    }
}

/// 起動ログに出す backend と選定理由。
fn describe_wgpu_backend(
    raw: Option<&std::ffi::OsStr>,
    #[cfg(windows)] reason: &str,
    #[cfg(windows)] adapter: Option<&bevy::render::renderer::RenderAdapterInfo>,
) -> String {
    #[cfg(windows)]
    if let Some(adapter) = adapter {
        return format!(
            "{} ({reason})",
            format!("{:?}", adapter.backend).to_ascii_lowercase()
        );
    }
    let requested = raw
        .and_then(|value| value.to_str())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    #[cfg(windows)]
    return format!("{} ({reason})", requested.unwrap_or("auto"));
    #[cfg(not(windows))]
    requested.map_or_else(|| "auto (default)".into(), |value| format!("{value} (env)"))
}

fn render_plugin(
    env: Option<&std::ffi::OsStr>,
    #[cfg(windows)] chosen: Option<wgpu::Backends>,
) -> bevy::render::RenderPlugin {
    use bevy::render::settings::{RenderCreation, WgpuSettings};
    let mut settings = WgpuSettings::default();
    #[cfg(windows)]
    if let Some(chosen) = chosen {
        settings.backends = Some(chosen);
    }
    #[cfg(windows)]
    let gl = {
        use bevy::render::renderer::initialize_renderer;
        use wgpu::{Backend, Backends};

        if let Some(candidates) = settings.backends
            && candidates.contains(Backends::GL)
            && candidates != Backends::GL
        {
            // 複数候補を指定した場合は Bevy と同じ選定を行い、GL だったときだけ制限付きで開き直す。
            // この時点では Winit のイベントループ開始前で、通常の自動初期化にもウィンドウのハンドルはない。
            let resources = bevy::tasks::block_on(initialize_renderer(candidates, None, &settings));
            if resources.2.backend != Backend::Gl {
                return bevy::render::RenderPlugin {
                    render_creation: resources.into(),
                    ..default()
                };
            }
            drop(resources);
            settings.backends = Some(Backends::GL);
        }
        settings.backends == Some(Backends::GL)
    };
    #[cfg(not(windows))]
    let gl = env.and_then(|v| v.to_str()).is_some_and(|v| {
        v.split(',').any(|b| {
            matches!(
                b.trim().to_ascii_lowercase().as_str(),
                "gl" | "gles" | "opengl"
            )
        })
    });
    #[cfg(windows)]
    let _ = env;
    if gl {
        // Lisa で使わない SSAO の compute shader は OpenGL で組み込まない。
        settings.constrained_limits = Some(bevy::render::settings::WgpuLimits {
            max_storage_textures_per_shader_stage: 4,
            ..bevy::render::settings::WgpuLimits::unlimited()
        });
    }
    bevy::render::RenderPlugin {
        render_creation: RenderCreation::Automatic(Box::new(settings)),
        ..default()
    }
}

#[cfg(any(windows, test))]
mod windows_backend {
    use bevy::prelude::*;
    use bevy::render::settings::Backends;
    use serde::{Deserialize, Serialize};
    use std::ffi::OsStr;
    use std::fs;
    use std::path::{Path, PathBuf};

    const FALLBACK: &str = "gpu-backend-fallback.json";
    const PROBE: &str = "gpu-backend-probe.json";
    const INCOMPLETE: &str = "previous_vulkan_probe_incomplete";

    #[derive(Deserialize, Serialize)]
    struct Marker {
        backend: String,
        version: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    }

    enum ReadMarker {
        Present(Marker),
        Broken,
        Unavailable,
    }

    fn read_marker(path: &Path, backend: &str) -> ReadMarker {
        let text = match fs::read(path) {
            Ok(text) => text,
            Err(error) => {
                if error.kind() != std::io::ErrorKind::NotFound {
                    warn!(
                        "描画方式の記録を読めませんでした（{}）: {error}",
                        path.display()
                    );
                }
                return ReadMarker::Unavailable;
            }
        };
        match serde_json::from_slice::<Marker>(&text) {
            Ok(marker) if marker.backend == backend => ReadMarker::Present(marker),
            _ => {
                warn!("描画方式の記録が壊れています（{}）", path.display());
                ReadMarker::Broken
            }
        }
    }

    fn write_marker(path: &Path, backend: &str, version: &str, reason: Option<&str>) -> bool {
        let marker = Marker {
            backend: backend.into(),
            version: version.into(),
            reason: reason.map(str::to_owned),
        };
        let result = path
            .parent()
            .map_or(Ok(()), fs::create_dir_all)
            .and_then(|_| serde_json::to_vec(&marker).map_err(std::io::Error::other))
            .and_then(|text| fs::write(path, text));
        if let Err(error) = result {
            warn!(
                "描画方式を記録できませんでした（{}）: {error}",
                path.display()
            );
            return false;
        }
        true
    }

    fn remove_marker(path: &Path) {
        if let Err(error) = fs::remove_file(path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            warn!(
                "描画方式の記録を消せませんでした（{}）: {error}",
                path.display()
            );
        }
    }

    pub(crate) fn clear_markers(dir: &Path) {
        remove_marker(&dir.join(FALLBACK));
        remove_marker(&dir.join(PROBE));
    }

    pub(super) struct Selection {
        pub backends: Option<Backends>,
        pub reason: String,
        pub probe: Option<PathBuf>,
    }

    pub(super) fn choose(
        env: Option<&OsStr>,
        setting: &str,
        dir: Option<&Path>,
        version: &str,
        has_vulkan_adapter: impl FnOnce() -> bool,
    ) -> Selection {
        let select = |backends, reason: &str| Selection {
            backends,
            reason: reason.into(),
            probe: None,
        };
        if env.is_some_and(|value| !value.is_empty()) {
            return select(None, "env");
        }
        match setting {
            "vulkan" => return select(Some(Backends::VULKAN), "setting"),
            "gl" => return select(Some(Backends::GL), "setting"),
            _ => {}
        }
        let mut probe = None;
        if let Some(dir) = dir {
            let fallback = dir.join(FALLBACK);
            match read_marker(&fallback, "gl") {
                ReadMarker::Present(marker) if marker.version == version => {
                    let reason = marker.reason.as_deref().unwrap_or(INCOMPLETE);
                    return select(Some(Backends::GL), &format!("fallback: {reason}"));
                }
                ReadMarker::Present(_) | ReadMarker::Broken => remove_marker(&fallback),
                ReadMarker::Unavailable => {}
            }
            let path = dir.join(PROBE);
            match read_marker(&path, "vulkan") {
                ReadMarker::Present(Marker {
                    version: previous, ..
                }) if previous != version => {
                    remove_marker(&path);
                }
                ReadMarker::Present(_) | ReadMarker::Broken => {
                    warn!("前回の Vulkan 描画が安定する前に終了したため OpenGL で起動します");
                    write_marker(&fallback, "gl", version, Some(INCOMPLETE));
                    remove_marker(&path);
                    return select(Some(Backends::GL), &format!("fallback: {INCOMPLETE}"));
                }
                ReadMarker::Unavailable => {}
            }
            if write_marker(&path, "vulkan", version, None) {
                probe = Some(path);
            }
        }

        // 列挙自体でドライバーが異常終了しても次回は OpenGL に切り替える。
        if !has_vulkan_adapter() {
            if let Some(path) = probe {
                remove_marker(&path);
            }
            return select(Some(Backends::GL), "fallback: no_vulkan_adapter");
        }

        Selection {
            probe,
            ..select(Some(Backends::VULKAN), "auto")
        }
    }

    #[cfg(windows)]
    pub(super) fn has_vulkan_adapter() -> bool {
        let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
        descriptor.backends = Backends::VULKAN;
        let instance = wgpu::Instance::new(descriptor);
        !bevy::tasks::block_on(instance.enumerate_adapters(Backends::VULKAN)).is_empty()
    }

    #[cfg(windows)]
    pub(super) fn clear_after_rendering(app: &mut App, path: PathBuf) {
        use bevy::render::{Render, RenderApp, RenderSystems, view::ExtractedWindow};
        use bevy::window::{PrimaryWindow, RequestRedraw};
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };

        let Some(render_app) = app.get_sub_app_mut(RenderApp) else {
            return;
        };
        let probing = Arc::new(AtomicBool::new(true));
        let rendering = probing.clone();
        let mut path = Some(path);
        let mut frames = 0;
        render_app.add_systems(
            Render,
            (move |window: Option<Single<&ExtractedWindow, With<PrimaryWindow>>>| {
                if path.is_none() {
                    return;
                }
                // 描画後に取得済みのテクスチャが取り外されていれば present を通った。
                if window.is_some_and(|win| {
                    win.swap_chain_texture_view.is_some() && win.swap_chain_texture.is_none()
                }) {
                    frames += 1;
                    if frames == 5
                        && let Some(path) = path.take()
                    {
                        remove_marker(&path);
                        rendering.store(false, Ordering::Relaxed);
                    }
                }
            })
            .after(RenderSystems::Render)
            .before(RenderSystems::Cleanup),
        );
        app.add_systems(Update, move |mut redraw: MessageWriter<RequestRedraw>| {
            if probing.load(Ordering::Relaxed) {
                redraw.write(RequestRedraw);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_backend_auto_probe_and_version() {
        use bevy::render::settings::Backends;
        use windows_backend::choose;
        let dir = std::env::temp_dir().join(format!("lisa-backend-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let data = Some(dir.as_path());
        let probe = dir.join("gpu-backend-probe.json");
        let fallback = dir.join("gpu-backend-fallback.json");
        std::fs::write(&probe, "{").unwrap();
        assert_eq!(
            choose(
                Some(std::ffi::OsStr::new("vulkan")),
                "gl",
                data,
                "1",
                || panic!()
            )
            .reason,
            "env"
        );
        for (setting, expected) in [("gl", Backends::GL), ("vulkan", Backends::VULKAN)] {
            assert_eq!(
                choose(None, setting, data, "1", || panic!()).backends,
                Some(expected)
            );
        }
        assert_eq!(std::fs::read_to_string(&probe).unwrap(), "{");
        assert_eq!(
            choose(None, "auto", data, "1", || panic!()).backends,
            Some(Backends::GL)
        );
        assert!(!probe.exists());
        assert!(fallback.exists());
        assert_eq!(
            choose(None, "auto", data, "1", || panic!()).backends,
            Some(Backends::GL)
        );
        let retried = choose(None, "auto", data, "2", || true);
        assert_eq!(retried.backends, Some(Backends::VULKAN));
        assert_eq!(retried.probe, Some(probe.clone()));
        assert!(!fallback.exists());
        assert_eq!(
            choose(None, "auto", data, "2", || panic!()).backends,
            Some(Backends::GL)
        );
        std::fs::write(&fallback, "{").unwrap();
        let missing = choose(None, "auto", data, "2", || false);
        assert_eq!(missing.backends, Some(Backends::GL));
        assert_eq!(missing.reason, "fallback: no_vulkan_adapter");
        assert!(!fallback.exists());
        assert!(!probe.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn parse_present_mode_values() {
        assert_eq!(
            parse_present_mode(None),
            (PresentMode::AutoVsync, "auto_vsync (default)", None)
        );
        assert_eq!(
            parse_present_mode(Some("")),
            (PresentMode::AutoVsync, "auto_vsync (default)", None)
        );
        assert_eq!(
            parse_present_mode(Some("auto_vsync")),
            (PresentMode::AutoVsync, "auto_vsync", None)
        );
        assert_eq!(
            parse_present_mode(Some("auto_no_vsync")),
            (PresentMode::AutoNoVsync, "auto_no_vsync", None)
        );
        assert_eq!(
            parse_present_mode(Some("fifo")),
            (PresentMode::Fifo, "fifo", None)
        );
        assert_eq!(
            parse_present_mode(Some("fifo_relaxed")),
            (PresentMode::FifoRelaxed, "fifo_relaxed", None)
        );
        assert_eq!(
            parse_present_mode(Some("MAILBOX")),
            (PresentMode::Mailbox, "mailbox", None)
        );
        assert_eq!(
            parse_present_mode(Some("immediate")),
            (PresentMode::Immediate, "immediate", None)
        );
        assert_eq!(
            parse_present_mode(Some("unknown_mode")),
            (
                PresentMode::AutoVsync,
                "auto_vsync (default)",
                Some("unknown_mode".into())
            )
        );
    }

    #[test]
    fn open_log_file_rotates_and_falls_back_when_locked() {
        let dir = std::env::temp_dir().join(format!("lisa-log-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let (mut f1, p1) = open_log_file_in(&dir, 1111).unwrap();
        assert_eq!(p1, dir.join("lisa.log"));
        use std::io::Write;
        writeln!(f1, "first").unwrap();
        drop(f1);

        let (_f2, p2) = open_log_file_in(&dir, 2222).unwrap();
        assert_eq!(p2, dir.join("lisa.log"));
        assert!(dir.join("lisa.1.log").exists());

        // lisa.1.log をフォルダにして rename が失敗する状態を作ると、lisa-<pid>.log にフォールバックする。
        let _ = std::fs::remove_file(dir.join("lisa.1.log"));
        std::fs::create_dir_all(dir.join("lisa.1.log").join("keep")).unwrap();
        let (_f3, p3) = open_log_file_in(&dir, 3333).unwrap();
        assert_eq!(p3, dir.join("lisa-3333.log"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
