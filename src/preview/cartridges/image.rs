//! 画像ファイルのプレビュー。読めるまでは仮の絵（空、山ふたつ、太陽）、読めたら縮小版を出す。
//! jpg の EXIF の回転は今のところ無視している（後回し）。

use std::path::Path;

use bevy::prelude::*;

use crate::preview::{Cartridge, DrawContext, LoadedPreview, extension};

pub struct Image;

/// これを超えるファイルは読まない。
const MAX_BYTES: u64 = 200 * 1024 * 1024;
/// 縮めたあとの長辺。
const THUMB_SIDE: u32 = 256;
/// 裏で読む拡張子（heic は今のところ読まない）。
const LOADABLE: &[&str] = &["jpg", "jpeg", "png", "gif", "webp"];

fn within_limit(len: u64) -> bool {
    len <= MAX_BYTES
}

impl Cartridge for Image {
    fn id(&self) -> &'static str {
        "image"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["jpg", "jpeg", "png", "heic", "gif", "webp"]
    }

    fn can_load(&self, name: &str) -> bool {
        LOADABLE.contains(&extension(name).to_ascii_lowercase().as_str())
    }

    fn load(&self, path: &Path) -> Option<LoadedPreview> {
        if !within_limit(std::fs::metadata(path).ok()?.len()) {
            return None;
        }
        let img = ::image::open(path).ok()?.thumbnail(THUMB_SIDE, THUMB_SIDE);
        let rgba = img.to_rgba8();
        Some(LoadedPreview {
            width: rgba.width(),
            height: rgba.height(),
            rgba: rgba.into_raw(),
        })
    }

    fn draw(&self, ctx: &mut DrawContext<'_, '_, '_, '_>) {
        let inner = ctx.inner;
        let z = ctx.z;
        if let Some((handle, size)) = ctx.loaded.clone()
            && size.x > 0
            && size.y > 0
        {
            // 縦横比を保って紙の内側に収め、中央に置く。
            let k = (inner.width() / size.x as f32).min(inner.height() / size.y as f32);
            let fitted = Rect::from_center_size(inner.center(), size.as_vec2() * k);
            ctx.painter.image(fitted, handle, z);
            return;
        }
        // 空、山ふたつ、太陽。
        ctx.painter.rect(inner, Color::srgb(0.35, 0.5, 0.65), z);
        let (w, h) = (inner.width(), inner.height());
        let base = inner.min.y;
        ctx.painter.triangle(
            [
                Vec2::new(inner.min.x, base),
                Vec2::new(inner.min.x + w * 0.35, base + h * 0.65),
                Vec2::new(inner.min.x + w * 0.7, base),
            ],
            Color::srgb(0.3, 0.45, 0.35),
            z + 0.01,
        );
        ctx.painter.triangle(
            [
                Vec2::new(inner.min.x + w * 0.4, base),
                Vec2::new(inner.min.x + w * 0.72, base + h * 0.45),
                Vec2::new(inner.max.x, base),
            ],
            Color::srgb(0.38, 0.55, 0.4),
            z + 0.02,
        );
        ctx.painter.circle(
            Vec2::new(inner.max.x - w * 0.15, inner.max.y - h * 0.22),
            h * 0.1,
            Color::srgb(0.95, 0.85, 0.5),
            z + 0.01,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::image::{ImageFormat, RgbImage};

    fn write(name: &str, fmt: ImageFormat) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("lisa-preview-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        RgbImage::from_pixel(600, 300, ::image::Rgb([200, 30, 30]))
            .save_with_format(&path, fmt)
            .unwrap();
        path
    }

    #[test]
    fn loads_small_images() {
        for (name, fmt) in [
            ("a.jpg", ImageFormat::Jpeg),
            ("a.png", ImageFormat::Png),
            ("a.webp", ImageFormat::WebP),
        ] {
            let path = write(name, fmt);
            let l = Image.load(&path).unwrap_or_else(|| panic!("{name}"));
            assert_eq!((l.width, l.height), (256, 128), "{name}");
            assert_eq!(l.rgba.len(), 256 * 128 * 4);
        }
    }

    #[test]
    fn broken_file_is_none() {
        let dir = std::env::temp_dir().join("lisa-preview-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("broken.png");
        std::fs::write(&path, b"not an image").unwrap();
        assert!(Image.load(&path).is_none());
        assert!(Image.load(&dir.join("missing.png")).is_none());
    }

    #[test]
    fn size_limit() {
        assert!(within_limit(MAX_BYTES));
        assert!(!within_limit(MAX_BYTES + 1));
    }

    #[test]
    fn can_load_by_extension() {
        assert!(Image.can_load("A.JPG"));
        assert!(!Image.can_load("a.heic"));
    }
}
