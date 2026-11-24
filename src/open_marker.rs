//! Finder・エクスプローラーから開かれたマージ済みの目印（`.lmf`／`.lmd`）を受け取って表示する。
//!
//! - 受け取り口は 3 つ: 起動時の引数（Windows の関連付けと手で渡したパス）、起動中のアプリへの
//!   制御口の `open_marker`（二重起動したプロセスが渡す）、macOS の文書オープンイベント。
//! - 目印は通常のリンクと同じ検証を通す。`.lmf` は元の階層を開いてリンクの詳細を出し、`.lmd` は
//!   その階層でリンク先の中身を展開する（`crate::merged` のその場展開）。閲覧だけで、何も動かさない。

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use bevy::prelude::*;
use bevy::window::{PrimaryWindow, RequestRedraw};
use bevy::winit::{EventLoopProxy, EventLoopProxyWrapper, WinitUserEvent};
use lisa_core::listing::Kind;

use crate::model::tree::FileTree;
use crate::organize::OrganizeState;
use crate::scan::{ScanControl, Store};
use crate::ui::context_menu::{ContextAction, ContextCommand};
use crate::view::{Nav, SyncRequest};

/// 開かれた順に処理する目印のパス。
static QUEUE: Mutex<VecDeque<PathBuf>> = Mutex::new(VecDeque::new());
/// 止まっている間に届いたとき、アプリを起こす口。
static WAKE: Mutex<Option<EventLoopProxy<WinitUserEvent>>> = Mutex::new(None);

/// 目印の場所が DB に載るまで待つ長さ（新しく置かれた目印は読み終わるまで道のりが無い）。
const WAIT_FOR_PLACE: Duration = Duration::from_secs(20);

pub(crate) struct OpenMarkerPlugin;

impl Plugin for OpenMarkerPlugin {
    fn build(&self, app: &mut App) {
        for path in marker_args() {
            enqueue(path);
        }
        #[cfg(target_os = "macos")]
        mac::install_open_handler();
        #[cfg(windows)]
        std::thread::Builder::new()
            .name("lisa-file-association".into())
            .spawn(windows::register_current_user)
            .ok();
        app.add_systems(Startup, remember_proxy)
            .add_systems(Update, open_markers.after(crate::view::ViewSystems));
    }
}

/// 目印を処理の列に足し、止まっているアプリを起こす。
pub(crate) fn enqueue(path: PathBuf) {
    if let Ok(mut queue) = QUEUE.lock() {
        queue.push_back(path);
    }
    if let Ok(proxy) = WAKE.lock()
        && let Some(proxy) = proxy.as_ref()
    {
        let _ = proxy.send_event(WinitUserEvent::WakeUp);
    }
}

fn is_marker(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("lmf") || ext.eq_ignore_ascii_case("lmd"))
}

/// 起動時の引数のうち、目印のパス（相対パスは今のフォルダから）。
pub(crate) fn marker_args() -> Vec<PathBuf> {
    std::env::args_os()
        .skip(1)
        .map(PathBuf::from)
        .filter(|path| is_marker(path))
        .map(|path| std::path::absolute(&path).unwrap_or(path))
        .collect()
}

/// すでに動いている Lisa（同じデータのフォルダ）に目印を渡す。渡せたら true（このプロセスは終わってよい）。
pub(crate) fn forward_to_running(paths: &[PathBuf]) -> bool {
    use lisa_core::remote::{self, Command};

    if paths.is_empty() {
        return false;
    }
    let Some(dir) = crate::scan::db_path().and_then(|path| path.parent().map(Path::to_path_buf))
    else {
        return false;
    };
    let Some(endpoint) = remote::read_endpoint(&dir) else {
        return false;
    };
    if endpoint.pid == std::process::id() {
        return false;
    }
    // 前面に出す権利は、利用者が今操作したこのプロセスにある。起動中のアプリへ譲る。
    #[cfg(windows)]
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(endpoint.pid);
    }
    let mut delivered = false;
    for path in paths {
        match remote::send(&dir, Command::OpenMarker { path: path.clone() }) {
            // 目印の不備などはアプリの側で表示する。
            Ok(_) => delivered = true,
            Err(error) if !delivered => {
                eprintln!("起動中の Lisa に渡せませんでした: {error}");
                return false;
            }
            Err(error) => eprintln!("{} を渡せませんでした: {error}", path.display()),
        }
    }
    delivered
}

fn remember_proxy(proxy: Option<Res<EventLoopProxyWrapper>>) {
    if let Some(proxy) = proxy
        && let Ok(mut slot) = WAKE.lock()
    {
        *slot = Some((**proxy).clone());
    }
    // 起動前に届いていた分を処理させる。
    enqueue_wake();
}

fn enqueue_wake() {
    if let Ok(proxy) = WAKE.lock()
        && let Some(proxy) = proxy.as_ref()
    {
        let _ = proxy.send_event(WinitUserEvent::WakeUp);
    }
}

/// 元の階層が開くのを待っている目印。
struct Waiting {
    path: PathBuf,
    link_id: String,
    kind: Kind,
    since: Instant,
}

#[allow(clippy::too_many_arguments)]
fn open_markers(
    mut waiting: Local<Option<Waiting>>,
    mut location: ResMut<crate::view::Location>,
    mut sync: ResMut<SyncRequest>,
    nav: Res<Nav>,
    tree: Res<FileTree>,
    store: Option<Res<Store>>,
    control: Option<Res<ScanControl>>,
    mut organize: Option<ResMut<OrganizeState>>,
    dialog: Res<crate::ui::dialog::DialogState>,
    window: Option<Single<Entity, With<PrimaryWindow>>>,
    mut commands: MessageWriter<ContextCommand>,
    mut redraw: MessageWriter<RequestRedraw>,
    _main_thread: bevy::ecs::system::NonSendMarker,
) {
    if waiting.is_none() {
        let next = QUEUE.lock().ok().and_then(|mut queue| queue.pop_front());
        let Some(path) = next else {
            return;
        };
        if let Some(window) = window.as_deref() {
            bring_to_front(*window);
        }
        redraw.write(RequestRedraw);
        let marker = match lisa_core::merged::inspect(&path) {
            Ok(marker) => marker,
            Err(reason) => {
                notify(organize.as_deref_mut(), reason);
                return;
            }
        };
        // 目印そのもの（.lmd）か、目印を置いた元の階層（.lmf）を開く。新しく置かれた目印でも
        // 読み終えた所から順に開けるよう、パスで追う。
        let parent = path.parent().map(Path::to_path_buf);
        let open = if marker.kind == Kind::Dir {
            Some(path.clone())
        } else {
            parent.clone()
        };
        let Some(open) = open else {
            notify(organize.as_deref_mut(), "目印の場所が分かりません".into());
            return;
        };
        if let (Some(control), Some(parent)) = (control.as_deref(), parent.as_deref()) {
            control.visible(parent.to_path_buf());
        }
        *location = crate::view::Location::Renamed(open);
        sync.request_reset();
        *waiting = Some(Waiting {
            path,
            link_id: marker.link_id,
            kind: marker.kind,
            since: Instant::now(),
        });
        return;
    }
    let Some(current) = waiting.as_ref() else {
        return;
    };
    let top = nav.top().node;
    let is_link = |id: crate::model::tree::NodeId| {
        tree.nodes
            .get(id)
            .and_then(|node| node.merged.as_ref())
            .is_some_and(|marker| marker.link_id == current.link_id)
    };
    let done = match current.kind {
        // 開いた階層でリンク先の展開が始まっている。
        Kind::Dir => is_link(top),
        _ => {
            let child = tree
                .nodes
                .get(top)
                .into_iter()
                .flat_map(|node| node.children.iter().copied())
                .find(|&child| is_link(child));
            match child {
                Some(node) if !dialog.busy() => {
                    let target = crate::ui::context_menu::target_of(
                        node,
                        &tree,
                        store.as_deref(),
                        &nav,
                        organize.as_deref(),
                    );
                    commands.write(ContextCommand {
                        action: ContextAction::Details,
                        target,
                        world: Vec2::ZERO,
                    });
                    true
                }
                _ => false,
            }
        }
    };
    if done {
        *waiting = None;
        redraw.write(RequestRedraw);
    } else if current.since.elapsed() >= WAIT_FOR_PLACE {
        let reason = format!(
            "{} の場所を開けませんでした（Lisa が読んだ場所の外にあるか、まだ読み終わっていません）",
            current.path.display()
        );
        *waiting = None;
        notify(organize.as_deref_mut(), reason);
        redraw.write(RequestRedraw);
    } else {
        // 読み終わるのを待つ間も確かめ直す。
        redraw.write(RequestRedraw);
    }
}

fn notify(organize: Option<&mut OrganizeState>, message: String) {
    warn!("{message}");
    if let Some(organize) = organize {
        organize.set_notice(message);
    }
}

/// 窓を最小化から戻して前面に出す（メインスレッドから呼ぶ）。
fn bring_to_front(window: Entity) {
    bevy::winit::WINIT_WINDOWS.with_borrow(|windows| {
        if let Some(window) = windows.get_window(window) {
            window.set_minimized(false);
            window.set_visible(true);
            window.focus_window();
        }
    });
}

#[cfg(target_os = "macos")]
mod mac {
    //! Finder の文書オープンイベントを winit のアプリデリゲートで受ける。

    use std::ffi::CStr;
    use std::path::PathBuf;

    use objc2::ffi::class_addMethod;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
    use objc2::{msg_send, sel};
    use objc2_foundation::{NSArray, NSString, NSURL};

    /// `-application:openURLs:` を winit のデリゲートに足す。AppKit は起動直後と起動中の両方で
    /// 開かれた文書をこれで渡す。winit が実装していない場合だけ足す。
    pub(super) fn install_open_handler() {
        let Some(class) = AnyClass::get(c"WinitApplicationDelegate") else {
            bevy::log::warn!("文書を開くイベントを受けられません（デリゲートがありません）");
            return;
        };
        let selector = sel!(application:openURLs:);
        if class.instance_method(selector).is_some() {
            return;
        }
        let types: &CStr = c"v@:@@";
        // SAFETY: 引数と戻り値の型は `types` と一致する。クラスは登録済みで、足すのは一度だけ。
        let added = unsafe {
            let imp: Imp = std::mem::transmute::<
                extern "C-unwind" fn(&AnyObject, Sel, &AnyObject, &NSArray<NSURL>),
                Imp,
            >(open_urls);
            class_addMethod(
                class as *const AnyClass as *mut AnyClass,
                selector,
                imp,
                types.as_ptr(),
            )
        };
        if !added.as_bool() {
            bevy::log::warn!("文書を開くイベントを受けられません");
        }
    }

    extern "C-unwind" fn open_urls(
        _this: &AnyObject,
        _cmd: Sel,
        _app: &AnyObject,
        urls: &NSArray<NSURL>,
    ) {
        let paths: Vec<PathBuf> = urls
            .iter()
            .filter_map(|url| {
                let path: Option<Retained<NSString>> = unsafe { msg_send![&*url, path] };
                path.map(|path| PathBuf::from(path.to_string()))
            })
            .filter(|path| super::is_marker(path))
            .collect();
        // 起動の途中なら、同じデータで動いている別の Lisa（.app でない起動など）に渡して終わる。
        if !super::started() && super::forward_to_running(&paths) {
            std::process::exit(0);
        }
        for path in paths {
            super::enqueue(path);
        }
    }
}

/// 制御口を開いて画面の準備ができたか（それまでに届いた文書は、別の Lisa へ渡せるか確かめる）。
#[cfg(target_os = "macos")]
fn started() -> bool {
    WAKE.lock().is_ok_and(|proxy| proxy.is_some())
}

#[cfg(windows)]
mod windows {
    //! 今のユーザーの関連付け（HKCU\\Software\\Classes）に、Lisa を .lmf・.lmd の既定のアプリとして載せる。
    //! 利用者が別のアプリを選んでいれば（UserChoice）そちらが優先される。管理者権限は要らない。

    use std::os::windows::ffi::OsStrExt;
    use std::ptr::null_mut;

    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
        RegCreateKeyExW, RegQueryValueExW, RegSetValueExW,
    };
    use windows_sys::Win32::UI::Shell::{SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify};

    const PROG_ID: &str = "Lisa.MergedLink";

    fn wide(text: &str) -> Vec<u16> {
        std::ffi::OsStr::new(text)
            .encode_wide()
            .chain([0])
            .collect()
    }

    struct Key(HKEY);

    impl Drop for Key {
        fn drop(&mut self) {
            unsafe { RegCloseKey(self.0) };
        }
    }

    fn open(path: &str) -> Option<Key> {
        let path = wide(path);
        let mut key: HKEY = null_mut();
        // SAFETY: パスは NUL 終端で、出力先は有効。
        let status = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                path.as_ptr(),
                0,
                std::ptr::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_READ | KEY_WRITE,
                std::ptr::null(),
                &mut key,
                null_mut(),
            )
        };
        (status == ERROR_SUCCESS).then_some(Key(key))
    }

    fn read(key: &Key, name: Option<&str>) -> Option<String> {
        let name = name.map(wide);
        let name_ptr = name.as_ref().map_or(std::ptr::null(), |name| name.as_ptr());
        let mut buffer = vec![0u16; 1024];
        let mut bytes = (buffer.len() * 2) as u32;
        // SAFETY: 書き込み先とその大きさは一致する。
        let status = unsafe {
            RegQueryValueExW(
                key.0,
                name_ptr,
                null_mut(),
                null_mut(),
                buffer.as_mut_ptr().cast(),
                &mut bytes,
            )
        };
        if status != ERROR_SUCCESS {
            return None;
        }
        let len = (bytes as usize / 2).min(buffer.len());
        let text = String::from_utf16_lossy(&buffer[..len]);
        Some(text.trim_end_matches('\0').to_owned())
    }

    /// 値が違うときだけ書く。書いたら true。
    fn write(key: &Key, name: Option<&str>, value: &str) -> bool {
        if read(key, name).as_deref() == Some(value) {
            return false;
        }
        let name = name.map(wide);
        let name_ptr = name.as_ref().map_or(std::ptr::null(), |name| name.as_ptr());
        let value = wide(value);
        // SAFETY: 値は NUL 終端の UTF-16 で、大きさは終端を含むバイト数。
        unsafe {
            RegSetValueExW(
                key.0,
                name_ptr,
                0,
                REG_SZ,
                value.as_ptr().cast(),
                (value.len() * 2) as u32,
            ) == ERROR_SUCCESS
        }
    }

    pub(super) fn register_current_user() {
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        let exe = exe.display().to_string();
        let mut changed = false;
        let class = format!("Software\\Classes\\{PROG_ID}");
        if let Some(key) = open(&class) {
            changed |= write(&key, None, "Lisa のマージ済みリンク");
        }
        // 別の場所の Lisa が登録済みで、その exe がまだあるなら奪わない（一時フォルダや検証用の
        // コピーを一度起動しただけで関連付けが移り、消えたあとに開けなくなるのを防ぐ）。
        let command_key = open(&format!("{class}\\shell\\open\\command"));
        let registered = command_key
            .as_ref()
            .and_then(|key| read(key, None))
            .and_then(|command| {
                command
                    .strip_prefix('"')?
                    .split_once('"')
                    .map(|(path, _)| path.to_owned())
            });
        let keep_other = registered.as_deref().is_some_and(|path| {
            !path.eq_ignore_ascii_case(&exe) && std::path::Path::new(path).is_file()
        });
        if !keep_other {
            if let Some(key) = open(&format!("{class}\\DefaultIcon")) {
                changed |= write(&key, None, &format!("\"{exe}\",0"));
            }
            if let Some(key) = &command_key {
                changed |= write(key, None, &format!("\"{exe}\" \"%1\""));
            }
        }
        for extension in [".lmf", ".lmd"] {
            let path = format!("Software\\Classes\\{extension}");
            if let Some(key) = open(&format!("{path}\\OpenWithProgids")) {
                changed |= write(&key, Some(PROG_ID), "");
            }
            // Lisa の拡張子なので Lisa を既定にする。利用者が「プログラムから開く」で選んだ既定
            // （Explorer の UserChoice）はこれより優先され、ここでは触らない。
            if let Some(key) = open(&path) {
                changed |= write(&key, None, PROG_ID);
            }
        }
        if changed {
            // SAFETY: 引数を使わない通知。
            unsafe {
                SHChangeNotify(
                    SHCNE_ASSOCCHANGED as i32,
                    SHCNF_IDLIST,
                    std::ptr::null(),
                    std::ptr::null(),
                )
            };
            bevy::log::info!("今のユーザーの関連付けに .lmf・.lmd を登録しました（{exe}）");
        }
    }
}
