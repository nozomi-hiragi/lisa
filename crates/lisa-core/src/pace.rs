//! 裏で読むときの決まりに要る、OS への問い合わせ（`docs/scan.md` の「裏で読む分」）。
//!
//! - このスレッドの I/O を後回しにする（Mac は `setiopolicy_np` の THROTTLE、Windows は
//!   `THREAD_MODE_BACKGROUND_BEGIN`）。
//! - 電池で動いているか（Mac は IOKit の `IOPSGetTimeRemainingEstimate`、Windows は
//!   `GetSystemPowerStatus`）。
//! - CPU の混み具合（Unix は 1 分の load average をコアの数で割ったもの）。
//! - ディスクの読み書きの累計（Mac は IOKit の `IOBlockStorageDriver` の統計）。

/// このスレッドの I/O を、裏で読む分として後回しにする（`false` で元に戻す）。
pub fn set_thread_background(on: bool) {
    let _ = try_set_thread_background(on, 0);
}

#[derive(Clone, Copy)]
struct PriorityFailure {
    background: bool,
    revision: u64,
    error: i32,
}

thread_local! {
    // 新しいスレッドは通常優先度から始まる。失敗も同じ変更の間は繰り返さない。
    static PRIORITY: std::cell::Cell<(bool, Option<PriorityFailure>)> =
        const { std::cell::Cell::new((false, None)) };
}

/// 優先度が変わるときだけ OS を呼び、変更できたら true。失敗時は OS のエラー番号を返す。
pub fn try_set_thread_background(on: bool, revision: u64) -> Result<bool, i32> {
    PRIORITY.with(|priority| {
        let (current, failure) = priority.get();
        if current == on {
            priority.set((current, None));
            return Ok(false);
        }
        if let Some(failure) = failure
            && (failure.background, failure.revision) == (on, revision)
        {
            return Err(failure.error);
        }
        match change_thread_background(on) {
            Ok(()) => {
                priority.set((on, None));
                Ok(true)
            }
            Err(error) => {
                priority.set((
                    current,
                    Some(PriorityFailure {
                        background: on,
                        revision,
                        error,
                    }),
                ));
                Err(error)
            }
        }
    })
}

fn change_thread_background(on: bool) -> Result<(), i32> {
    #[cfg(target_os = "macos")]
    let ok = {
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
        (unsafe { setiopolicy_np(IOPOL_TYPE_DISK, IOPOL_SCOPE_THREAD, policy) }) == 0
    };
    #[cfg(windows)]
    let ok = {
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
        (unsafe { SetThreadPriority(GetCurrentThread(), mode) }) != 0
    };
    #[cfg(not(any(target_os = "macos", windows)))]
    let ok = {
        let _ = on;
        true
    };
    if ok {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    }
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

/// これまでにディスク（全部のブロックデバイス）で読み書きしたバイト数の合計。起動してからの累計なので、
/// 2 回の差を時間で割ると速さになる。分からなければ None。
///
/// Mac: IOKit の `IOBlockStorageDriver` の `Statistics`（`Bytes (Read)` と `Bytes (Write)`）。
pub fn disk_bytes() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        mac_disk::bytes()
    }
    #[cfg(not(target_os = "macos"))]
    None
}

#[cfg(target_os = "macos")]
mod mac_disk {
    use std::ffi::{c_char, c_void};

    type CFTypeRef = *const c_void;
    type CFStringRef = *const c_void;
    type CFDictionaryRef = *const c_void;
    type IoObject = u32;

    const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
    const K_CF_NUMBER_SINT64_TYPE: isize = 4;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithCString(
            alloc: *const c_void,
            text: *const c_char,
            encoding: u32,
        ) -> CFStringRef;
        fn CFDictionaryGetValue(dict: CFDictionaryRef, key: *const c_void) -> *const c_void;
        fn CFNumberGetValue(number: *const c_void, kind: isize, out: *mut c_void) -> bool;
        fn CFGetTypeID(cf: CFTypeRef) -> usize;
        fn CFDictionaryGetTypeID() -> usize;
        fn CFRelease(cf: CFTypeRef);
    }

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOServiceMatching(name: *const c_char) -> *mut c_void;
        fn IOServiceGetMatchingServices(
            main_port: u32,
            matching: *mut c_void,
            iterator: *mut IoObject,
        ) -> i32;
        fn IOIteratorNext(iterator: IoObject) -> IoObject;
        fn IORegistryEntryCreateCFProperty(
            entry: IoObject,
            key: CFStringRef,
            allocator: *const c_void,
            options: u32,
        ) -> CFTypeRef;
        fn IOObjectRelease(object: IoObject) -> i32;
    }

    /// C の文字列から CFString を作る（呼んだ側が CFRelease する）。
    fn cf_string(text: &std::ffi::CStr) -> CFStringRef {
        // SAFETY: NUL 終端の文字列を渡す。
        unsafe {
            CFStringCreateWithCString(std::ptr::null(), text.as_ptr(), K_CF_STRING_ENCODING_UTF8)
        }
    }

    pub fn bytes() -> Option<u64> {
        let statistics = cf_string(c"Statistics");
        let read = cf_string(c"Bytes (Read)");
        let write = cf_string(c"Bytes (Write)");
        let mut total: Option<u64> = None;
        // SAFETY: IOKit と CoreFoundation の決まりどおりに作って放す。辞書と数は、型を確かめてから読む。
        unsafe {
            let matching = IOServiceMatching(c"IOBlockStorageDriver".as_ptr());
            let mut iterator: IoObject = 0;
            // IOServiceGetMatchingServices は matching を受け取って放す。
            if !matching.is_null() && IOServiceGetMatchingServices(0, matching, &mut iterator) == 0
            {
                loop {
                    let service = IOIteratorNext(iterator);
                    if service == 0 {
                        break;
                    }
                    let dict =
                        IORegistryEntryCreateCFProperty(service, statistics, std::ptr::null(), 0);
                    if !dict.is_null() {
                        if CFGetTypeID(dict) == CFDictionaryGetTypeID() {
                            for key in [read, write] {
                                let number = CFDictionaryGetValue(dict, key);
                                let mut value: i64 = 0;
                                if !number.is_null()
                                    && CFNumberGetValue(
                                        number,
                                        K_CF_NUMBER_SINT64_TYPE,
                                        (&mut value as *mut i64).cast(),
                                    )
                                {
                                    *total.get_or_insert(0) += value.max(0) as u64;
                                }
                            }
                        }
                        CFRelease(dict);
                    }
                    IOObjectRelease(service);
                }
                IOObjectRelease(iterator);
            }
            for s in [statistics, read, write] {
                if !s.is_null() {
                    CFRelease(s);
                }
            }
        }
        total
    }
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
        #[cfg(target_os = "macos")]
        {
            // 読み書きの累計は、減らない。
            let a = disk_bytes().expect("ディスクの統計");
            let b = disk_bytes().expect("ディスクの統計");
            assert!(b >= a);
        }
    }
}
