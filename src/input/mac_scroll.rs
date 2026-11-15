//! macOS: NSEvent のスクロールをそのまま見る。
//!
//! winit は「指の段階」と「慣性の段階」を1つにまとめてしまうので、指がパッドに触れたのか、
//! 慣性が続いているのかが分からない。そこで、ウィンドウに届くスクロールの NSEvent を
//! ローカルモニタで覗き、段階と動き（論理ピクセル）を記録する。イベントは変えずに返すので、
//! winit にもそのまま届く。
//!
//! ピンチ（拡大縮小のジェスチャー）も覗き、指が乗っているか（段階が始まって終わるまで）を記録する。
//! winit のピンチには始まりも終わりも来ないので、これが無いと入力の途切れを待つしかない。

use bevy::math::Vec2;

use crate::input::scroll_input::RawPhase;

/// NSEvent から読んだスクロール1つ。
#[derive(Clone, Copy, Debug)]
pub struct RawScroll {
    pub phase: RawPhase,
    pub momentum_phase: RawPhase,
    /// 動き（論理ピクセル。winit の MouseWheel と同じ向き）。
    pub delta: Vec2,
}

#[cfg(target_os = "macos")]
mod imp {
    use std::ptr::NonNull;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use bevy::math::Vec2;
    use block2::RcBlock;
    use objc2_app_kit::{NSEvent, NSEventMask, NSEventPhase, NSEventType};

    use super::RawScroll;
    use crate::input::scroll_input::RawPhase;

    static QUEUE: Mutex<Vec<RawScroll>> = Mutex::new(Vec::new());
    /// ピンチの指が乗っている。
    static PINCH_TOUCHING: AtomicBool = AtomicBool::new(false);

    fn phase(p: NSEventPhase) -> RawPhase {
        if p.contains(NSEventPhase::MayBegin) {
            RawPhase::MayBegin
        } else if p.contains(NSEventPhase::Began) {
            RawPhase::Began
        } else if p.contains(NSEventPhase::Changed) {
            RawPhase::Changed
        } else if p.contains(NSEventPhase::Stationary) {
            RawPhase::Stationary
        } else if p.contains(NSEventPhase::Ended) {
            RawPhase::Ended
        } else if p.contains(NSEventPhase::Cancelled) {
            RawPhase::Cancelled
        } else {
            RawPhase::None
        }
    }

    /// モニタを付ける。メインスレッドから呼ぶ。付けられたら true。
    pub fn install() -> bool {
        let block = RcBlock::new(|event: NonNull<NSEvent>| -> *mut NSEvent {
            // SAFETY: AppKit が渡す、このハンドラの間は有効なイベント。
            let e = unsafe { event.as_ref() };
            if e.r#type() == NSEventType::Magnify {
                let p = e.phase();
                if p.contains(NSEventPhase::Ended) || p.contains(NSEventPhase::Cancelled) {
                    PINCH_TOUCHING.store(false, Ordering::Relaxed);
                } else if p.contains(NSEventPhase::Began) || p.contains(NSEventPhase::Changed) {
                    PINCH_TOUCHING.store(true, Ordering::Relaxed);
                }
                return event.as_ptr();
            }
            // トラックパッド（と Magic Mouse）のなめらかなスクロールだけ。マウスのホイールは除く。
            if e.hasPreciseScrollingDeltas() {
                let record = RawScroll {
                    phase: phase(e.phase()),
                    momentum_phase: phase(e.momentumPhase()),
                    delta: Vec2::new(e.scrollingDeltaX() as f32, e.scrollingDeltaY() as f32),
                };
                if let Ok(mut queue) = QUEUE.lock() {
                    queue.push(record);
                }
            }
            event.as_ptr()
        });
        // SAFETY: ハンドラは受け取ったイベントをそのまま返す（有効なポインタ）。
        let monitor = unsafe {
            NSEvent::addLocalMonitorForEventsMatchingMask_handler(
                NSEventMask::ScrollWheel | NSEventMask::Magnify,
                &block,
            )
        };
        match monitor {
            Some(monitor) => {
                // アプリが終わるまで付けたままにする。
                std::mem::forget(monitor);
                true
            }
            None => false,
        }
    }

    /// ピンチの指が乗っているか。
    pub fn pinch_touching() -> bool {
        PINCH_TOUCHING.load(Ordering::Relaxed)
    }

    pub fn drain() -> Vec<RawScroll> {
        QUEUE
            .lock()
            .map(|mut queue| std::mem::take(&mut *queue))
            .unwrap_or_default()
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::RawScroll;

    pub fn install() -> bool {
        false
    }

    pub fn drain() -> Vec<RawScroll> {
        Vec::new()
    }

    pub fn pinch_touching() -> bool {
        false
    }
}

pub use imp::{drain, install, pinch_touching};
