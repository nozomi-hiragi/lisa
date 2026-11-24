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
    if settings::load().lan_share >= 0.5 {
        lan::ensure_sidecar();
    }
    let (present_mode, present_label, bad_present) =
        parse_present_mode(std::env::var("LISA_PRESENT_MODE").ok().as_deref());
    let (_, update_label, _) =
        view::parse_update_mode(std::env::var("LISA_UPDATE_MODE").ok().as_deref());
    let backend_label =
        describe_wgpu_backend(std::env::var("WGPU_BACKEND").ok().as_deref(), cfg!(windows));
    let mut app = App::new();
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
        .set(LogPlugin {
            // TODO: 日本語・中国語の改行位置データ（ICU4X のセグメンテーションモデル）を同梱して
            // いないため、その警告（icu_provider::error）を止めている。ラベルを折り返すように
            // なったら、Bevy の `complex_script_segmentation` 機能でデータを入れ、この絞り込みと
            // Cargo.toml の icu_provider の logging 機能を外す（README の「既知の課題」を参照）。
            filter: format!("{DEFAULT_FILTER},icu_provider=off"),
            custom_layer: file_log_layer,
            ..default()
        })
        // 音とゲームパッドは使わない。どちらも作るときに OS の機器を開くので、Windows で
        // 起動の途中で止まることがあった（GPU の情報のあとで止まり、Lisa の処理まで来なかった）。
        .set(render_plugin())
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
        settings::SettingsPlugin,
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
    log_startup(&backend_label, present_label, update_label);
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

/// 起動ログに出す backend の表示（環境変数 `WGPU_BACKEND` の値か、既定のどちらか）。
fn describe_wgpu_backend(raw: Option<&str>, is_windows: bool) -> String {
    if let Some(s) = raw.map(str::trim).filter(|s| !s.is_empty()) {
        format!("{s} (env)")
    } else if is_windows {
        "gl (default)".into()
    } else {
        "auto (default)".into()
    }
}

/// 描き方の設定。Windows は、`WGPU_BACKEND` が無ければ（または空文字列なら）OpenGL で描く。
/// Intel Iris Graphics 550 の PC で、Vulkan だと GPU の情報を出した直後に、panic もなく
/// プロセスごと落ち（ドライバーの中で落ちていると見ている）、DirectX 12 だと画面がちらつき、
/// しばらくすると GPU を見失って落ちた（`material uniform data 0` が invalid）ため。
fn render_plugin() -> bevy::render::RenderPlugin {
    use bevy::render::settings::{Backends, RenderCreation, WgpuSettings};
    let mut settings = WgpuSettings::default();
    let env = std::env::var_os("WGPU_BACKEND");
    let default_gl = cfg!(windows) && is_unset(env.as_deref());
    if default_gl {
        settings.backends = Some(Backends::GL);
    }
    if default_gl || is_gl_backend(env.as_deref()) {
        // OpenGL では、Bevy が組み込む SSAO の compute shader（textureGatherOffset）が作れずに落ちる。
        // Lisa は SSAO を使わないので、SSAO が「使えない GPU」と判断して組み込みを飛ばす値まで下げる。
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

fn is_gl_backend(wgpu_backend: Option<&std::ffi::OsStr>) -> bool {
    wgpu_backend.and_then(|v| v.to_str()).is_some_and(|v| {
        v.split(',').any(|b| {
            matches!(
                b.trim().to_ascii_lowercase().as_str(),
                "gl" | "gles" | "opengl"
            )
        })
    })
}

fn is_unset(wgpu_backend: Option<&std::ffi::OsStr>) -> bool {
    wgpu_backend.filter(|v| !v.is_empty()).is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gl_default_handles_unset_and_empty_wgpu_backend() {
        assert!(is_gl_backend(Some(std::ffi::OsStr::new("gl"))));
        assert!(is_gl_backend(Some(std::ffi::OsStr::new("dx12,GL"))));
        assert!(!is_gl_backend(Some(std::ffi::OsStr::new("dx12"))));
        assert!(!is_gl_backend(None));

        assert!(is_unset(None));
        assert!(is_unset(Some(std::ffi::OsStr::new(""))));
        assert!(!is_unset(Some(std::ffi::OsStr::new("vulkan"))));
        assert!(!is_unset(Some(std::ffi::OsStr::new("dx12"))));

        assert_eq!(describe_wgpu_backend(None, true), "gl (default)");
        assert_eq!(describe_wgpu_backend(Some(""), false), "auto (default)");
        assert_eq!(describe_wgpu_backend(Some("vulkan"), true), "vulkan (env)");
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
