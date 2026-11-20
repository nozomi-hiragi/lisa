//! 動画ファイルのプレビュー。macOS は 1 秒目あたりの 1 コマを取って出し、上に再生ボタンを重ねる。
//! 読めるまで・ほかの OS は、暗い画面に再生ボタン。

use std::path::Path;

use bevy::prelude::*;

use crate::preview::{Cartridge, DrawContext, LoadedPreview, Preview};

pub struct Video;

/// 取った 1 コマの長辺。
#[cfg(target_os = "macos")]
const THUMB_SIDE: f64 = 256.0;

impl Cartridge for Video {
    fn id(&self) -> &'static str {
        "video"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["mov", "mp4", "m4v"]
    }

    fn can_load(&self, name: &str) -> bool {
        cfg!(target_os = "macos")
            && ["mov", "mp4", "m4v"].contains(
                &crate::preview::extension(name)
                    .to_ascii_lowercase()
                    .as_str(),
            )
    }

    #[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
    fn load(&self, path: &Path) -> Option<LoadedPreview> {
        #[cfg(target_os = "macos")]
        return load_frame(path);
        #[cfg(not(target_os = "macos"))]
        None
    }

    fn draw(&self, ctx: &mut DrawContext<'_, '_, '_, '_>) {
        let inner = ctx.inner;
        let z = ctx.z;
        if let Some(Preview::Image(handle, size)) = ctx.loaded.clone()
            && size.x > 0
            && size.y > 0
        {
            let k = (inner.width() / size.x as f32).min(inner.height() / size.y as f32);
            let fitted = Rect::from_center_size(inner.center(), size.as_vec2() * k);
            ctx.painter.image(fitted, handle, z);
            play_button(ctx, fitted, z + 0.01);
            return;
        }
        ctx.painter.rect(inner, Color::srgb(0.1, 0.1, 0.12), z);
        play_button(ctx, inner, z + 0.01);
    }
}

/// 四角の中央に、白い三角の再生ボタンを重ねる。
fn play_button(ctx: &mut DrawContext<'_, '_, '_, '_>, area: Rect, z: f32) {
    let center = area.center();
    let r = area.height() * 0.22;
    ctx.painter.triangle(
        [
            center + Vec2::new(-r * 0.7, r),
            center + Vec2::new(-r * 0.7, -r),
            center + Vec2::new(r, 0.0),
        ],
        Color::srgba(1.0, 1.0, 1.0, 0.8),
        z,
    );
}

/// AVAssetImageGenerator で 1 秒目（短ければ 0 秒目）の 1 コマを、向きを直して長辺 256 px で取る。
#[cfg(target_os = "macos")]
fn load_frame(path: &Path) -> Option<LoadedPreview> {
    use objc2_av_foundation::{AVAsset, AVAssetImageGenerator};
    use objc2_core_foundation::CGSize;
    use objc2_core_media::CMTime;
    use objc2_foundation::NSURL;

    let url = NSURL::from_file_path(path)?;
    // SAFETY: url は有効な NSURL。
    let asset = unsafe { AVAsset::assetWithURL(&url) };
    // SAFETY: asset は有効な AVAsset。
    let generator = unsafe { AVAssetImageGenerator::assetImageGeneratorWithAsset(&asset) };
    // SAFETY: 生成器は自分で作ったもので、設定はこのスレッドだけが触る。
    unsafe {
        generator.setAppliesPreferredTrackTransform(true);
        generator.setMaximumSize(CGSize::new(THUMB_SIDE, THUMB_SIDE));
    }
    // SAFETY: asset は有効。
    let duration = unsafe { asset.duration() };
    let seconds = if duration.timescale > 0 {
        duration.value as f64 / duration.timescale as f64
    } else {
        0.0
    };
    let frame_at = |t: f64| {
        // SAFETY: CMTime を作るだけ。
        let time = unsafe { CMTime::with_seconds(t, 600) };
        // SAFETY: actual_time は null（要らない）。同期版は裏のスレッドで呼ぶ前提。
        #[allow(deprecated)]
        unsafe { generator.copyCGImageAtTime_actualTime_error(time, std::ptr::null_mut()) }.ok()
    };
    let image = if seconds > 1.0 { frame_at(1.0) } else { None }.or_else(|| frame_at(0.0))?;
    super::cg::rgba_from_cgimage(&image)
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn extracts_frame_with_aspect() {
        let dir = std::env::temp_dir().join("lisa-preview-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("v.mp4");
        let ok = std::process::Command::new("ffmpeg")
            .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i"])
            .arg("testsrc=size=640x360:rate=10:duration=2")
            .args(["-pix_fmt", "yuv420p"])
            .arg(&path)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            eprintln!("ffmpeg が使えないので動画のテストは飛ばす");
            return;
        }
        assert!(Video.can_load("A.MP4"));
        let (w, h) = Video.load(&path).expect("frame").size().unwrap();
        assert_eq!((w, h), (256, 144));
    }

    #[test]
    fn broken_file_is_none() {
        let dir = std::env::temp_dir().join("lisa-preview-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("broken.mp4");
        std::fs::write(&path, b"not a video").unwrap();
        assert!(Video.load(&path).is_none());
        assert!(Video.load(&dir.join("missing.mp4")).is_none());
    }
}
