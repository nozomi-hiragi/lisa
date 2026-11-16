//! Mac の FSEvents の履歴。前回終わってから変わったフォルダを、起動時にもらう（`docs/scan.md` の「監視」）。
//!
//! FSEvents のイベント番号はシステム全体で増えていく。前回の番号（DB の `meta` に覚える）から今までの
//! 履歴を、変わったフォルダのパスとして受け取る。動いているあいだの変化は `notify` が知らせる。

use std::ffi::{c_char, c_void};
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

type CFAllocatorRef = *const c_void;
type CFArrayRef = *const c_void;
type CFStringRef = *const c_void;
type FSEventStreamRef = *mut c_void;
type DispatchQueue = *mut c_void;

const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
/// `kFSEventStreamEventFlag…`。
const USER_DROPPED: u32 = 0x02;
const KERNEL_DROPPED: u32 = 0x04;
const HISTORY_DONE: u32 = 0x10;

#[repr(C)]
struct Context {
    version: isize,
    info: *mut c_void,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
}

type Callback = extern "C" fn(
    stream: FSEventStreamRef,
    info: *mut c_void,
    count: usize,
    paths: *mut c_void,
    flags: *const u32,
    ids: *const u64,
);

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeArrayCallBacks: c_void;
    fn CFStringCreateWithBytes(
        alloc: CFAllocatorRef,
        bytes: *const u8,
        len: isize,
        encoding: u32,
        external: bool,
    ) -> CFStringRef;
    fn CFArrayCreate(
        alloc: CFAllocatorRef,
        values: *const *const c_void,
        count: isize,
        callbacks: *const c_void,
    ) -> CFArrayRef;
    fn CFRelease(cf: *const c_void);
}

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn FSEventsGetCurrentEventId() -> u64;
    fn FSEventStreamCreate(
        alloc: CFAllocatorRef,
        callback: Callback,
        context: *const Context,
        paths: CFArrayRef,
        since: u64,
        latency: f64,
        flags: u32,
    ) -> FSEventStreamRef;
    fn FSEventStreamSetDispatchQueue(stream: FSEventStreamRef, queue: DispatchQueue);
    fn FSEventStreamStart(stream: FSEventStreamRef) -> bool;
    fn FSEventStreamStop(stream: FSEventStreamRef);
    fn FSEventStreamInvalidate(stream: FSEventStreamRef);
    fn FSEventStreamRelease(stream: FSEventStreamRef);
}

unsafe extern "C" {
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> DispatchQueue;
    fn dispatch_release(object: *mut c_void);
}

/// 今のイベント番号（次に起動したときの「前回」）。
pub fn current_event_id() -> u64 {
    // SAFETY: 引数の無い問い合わせ。
    unsafe { FSEventsGetCurrentEventId() }
}

/// 履歴から集めたもの。
#[derive(Default, Debug)]
pub struct History {
    /// 変わったフォルダ（同じものも入る）。
    pub folders: Vec<PathBuf>,
    /// イベントが捨てられていた（全部はそろっていない）。
    pub dropped: bool,
}

#[derive(Default)]
struct State {
    history: History,
    done: bool,
}

struct Shared {
    state: Mutex<State>,
    ready: Condvar,
}

extern "C" fn on_events(
    _stream: FSEventStreamRef,
    info: *mut c_void,
    count: usize,
    paths: *mut c_void,
    flags: *const u32,
    _ids: *const u64,
) {
    // SAFETY: info は `since` が渡した `Shared`、paths は count 個の C 文字列、flags は count 個。
    let shared = unsafe { &*(info as *const Shared) };
    let paths = unsafe { std::slice::from_raw_parts(paths as *const *const c_char, count) };
    let flags = unsafe { std::slice::from_raw_parts(flags, count) };
    let mut state = shared.state.lock().expect("履歴");
    for (&path, &flag) in paths.iter().zip(flags) {
        if flag & HISTORY_DONE != 0 {
            state.done = true;
            continue;
        }
        if flag & (USER_DROPPED | KERNEL_DROPPED) != 0 {
            state.history.dropped = true;
        }
        // SAFETY: NUL 終端の文字列。
        let bytes = unsafe { std::ffi::CStr::from_ptr(path) }.to_bytes();
        let mut path = PathBuf::from(crate::name::from_raw_bytes(bytes));
        // フォルダ単位の知らせは、末尾に / が付くことがある。
        if path.as_os_str().len() > 1 && bytes.ends_with(b"/") {
            path = path.components().collect();
        }
        state.history.folders.push(path);
    }
    if state.done {
        shared.ready.notify_all();
    }
}

/// `roots` の下で、イベント番号 `since` より後に変わったフォルダ。`wait` までに履歴が終わらなければ、
/// そこまでの分を返す。始められなければ None。
pub fn since(roots: &[&Path], since: u64, wait: Duration) -> Option<History> {
    if roots.is_empty() {
        return Some(History::default());
    }
    let shared = Box::new(Shared {
        state: Mutex::new(State::default()),
        ready: Condvar::new(),
    });
    // SAFETY: CoreFoundation と FSEvents の決まりどおりに作って、止めてから放す。コールバックは
    // `Shared` を指すポインタを受け取り、ストリームを放すまで `shared` は生きている。
    unsafe {
        let strings: Vec<CFStringRef> = roots
            .iter()
            .map(|p| {
                let bytes = crate::name::raw_bytes(p.as_os_str());
                CFStringCreateWithBytes(
                    std::ptr::null(),
                    bytes.as_ptr(),
                    bytes.len() as isize,
                    K_CF_STRING_ENCODING_UTF8,
                    false,
                )
            })
            .collect();
        let array = CFArrayCreate(
            std::ptr::null(),
            strings.as_ptr(),
            strings.len() as isize,
            &kCFTypeArrayCallBacks,
        );
        for s in &strings {
            CFRelease(*s);
        }
        let context = Context {
            version: 0,
            info: &*shared as *const Shared as *mut c_void,
            retain: std::ptr::null(),
            release: std::ptr::null(),
            copy_description: std::ptr::null(),
        };
        let stream =
            FSEventStreamCreate(std::ptr::null(), on_events, &context, array, since, 0.0, 0);
        CFRelease(array);
        if stream.is_null() {
            return None;
        }
        let queue = dispatch_queue_create(c"lisa.fsevents.history".as_ptr(), std::ptr::null());
        FSEventStreamSetDispatchQueue(stream, queue);
        if !FSEventStreamStart(stream) {
            FSEventStreamInvalidate(stream);
            FSEventStreamRelease(stream);
            dispatch_release(queue);
            return None;
        }
        {
            let state = shared.state.lock().expect("履歴");
            let _ = shared
                .ready
                .wait_timeout_while(state, wait, |s| !s.done)
                .expect("履歴");
        }
        FSEventStreamStop(stream);
        FSEventStreamInvalidate(stream);
        FSEventStreamRelease(stream);
        dispatch_release(queue);
    }
    let state = shared.state.into_inner().expect("履歴");
    Some(state.history)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_since_a_write_includes_the_folder() {
        let dir = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("lisa-fsevents-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let before = current_event_id();
        std::fs::write(dir.join("a.txt"), b"x").unwrap();
        // FSEvents が書き込みを記録するまで、少し待つ。
        let mut found = false;
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(250));
            let history = since(&[dir.as_path()], before, Duration::from_secs(5)).unwrap();
            if history.folders.iter().any(|f| f == &dir) {
                found = true;
                break;
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(found, "書いたフォルダが履歴に無い");
    }
}
