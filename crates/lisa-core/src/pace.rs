//! 裏で読むときの決まりに要る、OS への問い合わせ（`docs/scan.md` の「裏で読む分」）。
//!
//! - このスレッドの I/O を後回しにする（Mac は `setiopolicy_np` の THROTTLE、Windows は
//!   `THREAD_MODE_BACKGROUND_BEGIN`）。
//! - 電池で動いているか（Mac は IOKit の `IOPSGetTimeRemainingEstimate`、Windows は
//!   `GetSystemPowerStatus`）。
//! - CPU の混み具合（Unix は 1 分の load average をコアの数で割ったもの）。

/// このスレッドの I/O を、裏で読む分として後回しにする（`false` で元に戻す）。
pub fn set_thread_background(on: bool) {
    #[cfg(target_os = "macos")]
    {
        use std::ffi::c_int;
        // <sys/resource.h>
        const IOPOL_TYPE_DISK: c_int = 0;
        const IOPOL_SCOPE_THREAD: c_int = 1;
        const IOPOL_DEFAULT: c_int = 0;
        const IOPOL_THROTTLE: c_int = 3;
        unsafe extern "C" {
            fn setiopolicy_np(iotype: c_int, scope: c_int, policy: c_int) -> c_int;
        }
        let policy = if on { IOPOL_THROTTLE } else { IOPOL_DEFAULT };
        // SAFETY: 引数は整数だけで、ポインタを渡さない。
        unsafe {
            setiopolicy_np(IOPOL_TYPE_DISK, IOPOL_SCOPE_THREAD, policy);
        }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN,
            THREAD_MODE_BACKGROUND_END,
        };
        let mode = if on {
            THREAD_MODE_BACKGROUND_BEGIN
        } else {
            THREAD_MODE_BACKGROUND_END
        };
        // SAFETY: 今のスレッドの擬似ハンドルを渡すだけ。
        unsafe {
            SetThreadPriority(GetCurrentThread(), mode);
        }
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    let _ = on;
}

/// 電池で動いているか。分からなければ false。
pub fn on_battery() -> bool {
    #[cfg(target_os = "macos")]
    {
        #[link(name = "IOKit", kind = "framework")]
        unsafe extern "C" {
            fn IOPSGetTimeRemainingEstimate() -> f64;
        }
        // kIOPSTimeRemainingUnlimited（-2）なら電源につながっている。
        // SAFETY: 引数の無い問い合わせ。
        let remaining = unsafe { IOPSGetTimeRemainingEstimate() };
        remaining != -2.0
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
        let mut status: SYSTEM_POWER_STATUS = unsafe { std::mem::zeroed() };
        // SAFETY: 書き込み先は手元の構造体。
        let ok = unsafe { GetSystemPowerStatus(&mut status) } != 0;
        // ACLineStatus: 0 = 電池、1 = 電源、255 = 分からない。
        ok && status.ACLineStatus == 0
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    false
}

/// CPU の混み具合（%、コア全部を使い切って 100）。分からなければ None。
pub fn cpu_load_percent() -> Option<f32> {
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn getloadavg(loadavg: *mut f64, nelem: std::ffi::c_int) -> std::ffi::c_int;
        }
        let mut load = [0.0f64; 1];
        // SAFETY: 1 個分の場所を渡して、1 個だけ書いてもらう。
        let n = unsafe { getloadavg(load.as_mut_ptr(), 1) };
        if n < 1 {
            return None;
        }
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        Some((load[0] / cores as f64 * 100.0) as f32)
    }
    #[cfg(not(unix))]
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_do_not_crash() {
        set_thread_background(true);
        set_thread_background(false);
        let _ = on_battery();
        #[cfg(unix)]
        assert!(cpu_load_percent().is_some());
    }
}
