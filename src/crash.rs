//! 落ちたときに、理由を残して知らせる。
//!
//! Windows のリリースビルドはコンソールの窓を出さないので、落ちても何も見えない。そこで、panic の
//! 中身をデータのフォルダの `crash.log` に足し、Windows では小さな窓で知らせる。

use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};

/// 複数スレッドで同時に panic したときに、窓が何個も出ないように 1 回だけにする。
static PANICKING: AtomicBool = AtomicBool::new(false);

/// panic のときの処理を足す（元の処理も呼ぶ）。
pub fn install() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let first = !PANICKING.swap(true, Ordering::SeqCst);
        // hook の中で万一また panic しても、その場で abort せず元の処理まで進める。
        let _ = catch_unwind(AssertUnwindSafe(|| {
            let thread = std::thread::current();
            let message = format!(
                "Lisa が止まりました（スレッド {}）:\n{info}\n\n{}",
                thread.name().unwrap_or("?"),
                std::backtrace::Backtrace::force_capture()
            );
            let log = crate::scan::data_dir().map(|dir| {
                let _ = std::fs::create_dir_all(&dir);
                dir.join("crash.log")
            });
            if let Some(path) = &log
                && let Ok(mut file) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
            {
                let _ = writeln!(file, "---- {:?}\n{message}", std::time::SystemTime::now());
            }
            if first {
                show(&message, log.as_deref());
            }
        }));
        let _ = catch_unwind(AssertUnwindSafe(|| {
            default(info);
        }));
    }));
}

#[cfg(windows)]
fn show(message: &str, log: Option<&std::path::Path>) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    // 長い backtrace は窓に入りきらないので、最初の数行だけ出す。
    let mut text: String = message.lines().take(12).collect::<Vec<_>>().join("\n");
    if let Some(log) = log {
        text.push_str(&format!("\n\n詳しくは {} にあります。", log.display()));
    }
    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let (text, title) = (wide(&text), wide("Lisa"));
    // ウィンドウを持つスレッドでそのまま MessageBoxW を呼ぶとメッセージループが回って二重 panic する
    // おそれがあるため、別スレッドで出して待つ。
    if let Ok(handle) = std::thread::Builder::new()
        .name("lisa-crash-dialog".into())
        .spawn(move || unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                text.as_ptr(),
                title.as_ptr(),
                MB_OK | MB_ICONERROR,
            );
        })
    {
        let _ = handle.join();
    }
}

#[cfg(not(windows))]
fn show(_message: &str, _log: Option<&std::path::Path>) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reentrancy_guard_only_allows_first_dialog() {
        let flag = AtomicBool::new(false);
        assert!(!flag.swap(true, Ordering::SeqCst));
        assert!(flag.swap(true, Ordering::SeqCst));
    }
}
