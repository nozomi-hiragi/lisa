//! 画像ファイルのプレビュー。読めるまでは仮の絵（空、山ふたつ、太陽）、読めたら縮小版を出す。
//! 写真の向き（EXIF）は直してから出す。macOS は ImageIO（heic / heif も読める）、ほかは image クレート。

use std::path::Path;

use bevy::prelude::*;

use crate::preview::{Cartridge, DrawContext, LoadedPreview, Preview, extension};

pub struct Image;

/// これを超えるファイルは読まない。
const MAX_BYTES: u64 = 200 * 1024 * 1024;
/// 縮めたあとの長辺。
const THUMB_SIDE: u32 = 256;
/// 裏で読む拡張子。heic / heif は macOS の ImageIO でだけ読める。
#[cfg(target_os = "macos")]
const LOADABLE: &[&str] = &["jpg", "jpeg", "png", "gif", "webp", "heic", "heif"];
#[cfg(not(target_os = "macos"))]
const LOADABLE: &[&str] = &["jpg", "jpeg", "png", "gif", "webp"];

fn within_limit(len: u64) -> bool {
    len <= MAX_BYTES
}

impl Cartridge for Image {
    fn id(&self) -> &'static str {
        "image"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["jpg", "jpeg", "png", "heic", "heif", "gif", "webp"]
    }

    fn can_load(&self, name: &str) -> bool {
        LOADABLE.contains(&extension(name).to_ascii_lowercase().as_str())
    }

    fn load(&self, path: &Path) -> Option<LoadedPreview> {
        if !within_limit(std::fs::metadata(path).ok()?.len()) {
            return None;
        }
        #[cfg(target_os = "macos")]
        if let Some(loaded) = load_imageio(path) {
            return Some(loaded);
        }
        load_image_crate(path)
    }

    fn draw(&self, ctx: &mut DrawContext<'_, '_, '_, '_>) {
        let inner = ctx.inner;
        let z = ctx.z;
        if let Some(Preview::Image(handle, size)) = ctx.loaded.clone()
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

/// image クレートで読む。EXIF の向きを直してから縮める。
fn load_image_crate(path: &Path) -> Option<LoadedPreview> {
    use ::image::ImageDecoder;
    let mut decoder = ::image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .into_decoder()
        .ok()?;
    let orientation = decoder.orientation().ok()?;
    let mut img = ::image::DynamicImage::from_decoder(decoder).ok()?;
    img.apply_orientation(orientation);
    let rgba = img.thumbnail(THUMB_SIDE, THUMB_SIDE).to_rgba8();
    Some(LoadedPreview::Image {
        width: rgba.width(),
        height: rgba.height(),
        rgba: rgba.into_raw(),
    })
}

/// macOS の ImageIO で読む。向きは `CreateThumbnailWithTransform` が直す。読めなければ None。
#[cfg(target_os = "macos")]
fn load_imageio(path: &Path) -> Option<LoadedPreview> {
    use objc2_core_foundation::{
        CFBoolean, CFMutableDictionary, CFNumber, CFString, CFType, CFURL,
    };
    use objc2_image_io::{
        CGImageSource, kCGImageSourceCreateThumbnailFromImageAlways,
        kCGImageSourceCreateThumbnailWithTransform, kCGImageSourceShouldCacheImmediately,
        kCGImageSourceThumbnailMaxPixelSize,
    };

    let url = CFURL::from_file_path(path)?;
    // SAFETY: url は有効な CFURL で、options は無し。
    let source = unsafe { CGImageSource::with_url(&url, None) }?;

    let options = CFMutableDictionary::<CFString, CFType>::empty();
    let max_side = CFNumber::new_i32(THUMB_SIDE as i32);
    // SAFETY: 定数のキーは ImageIO が持つ有効な CFString。
    unsafe {
        options.set(
            kCGImageSourceCreateThumbnailFromImageAlways,
            CFBoolean::new(true).as_ref(),
        );
        options.set(
            kCGImageSourceCreateThumbnailWithTransform,
            CFBoolean::new(true).as_ref(),
        );
        options.set(
            kCGImageSourceShouldCacheImmediately,
            CFBoolean::new(true).as_ref(),
        );
        options.set(kCGImageSourceThumbnailMaxPixelSize, max_side.as_ref());
    }
    // SAFETY: options のキーと値は CFString と CFNumber / CFBoolean で、ImageIO が求める型どおり。
    let image = unsafe { source.thumbnail_at_index(0, Some(options.as_opaque())) }?;

    super::cg::rgba_from_cgimage(&image)
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
            assert_eq!(l.size().unwrap(), (256, 128), "{name}");
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
        assert!(Image.can_load("a.PNG"));
        assert_eq!(Image.can_load("a.heic"), cfg!(target_os = "macos"));
        assert_eq!(Image.can_load("a.heif"), cfg!(target_os = "macos"));
    }

    /// 横長の jpg に、EXIF の Orientation=6（右に 90 度）を差し込んだファイルを作る。
    fn write_rotated_jpg(name: &str) -> std::path::PathBuf {
        let src = write(name, ImageFormat::Jpeg);
        let jpg = std::fs::read(&src).unwrap();
        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"MM\0\x2a\0\0\0\x08"); // ヘッダ、最初の IFD は 8 バイト目
        tiff.extend_from_slice(&[0, 1]); // エントリ 1 つ
        tiff.extend_from_slice(&[0x01, 0x12, 0, 3, 0, 0, 0, 1, 0, 6, 0, 0]); // Orientation, SHORT, 1 個, 6
        tiff.extend_from_slice(&[0, 0, 0, 0]); // 次の IFD なし
        let mut app1 = b"Exif\0\0".to_vec();
        app1.extend_from_slice(&tiff);
        let mut out = jpg[..2].to_vec(); // SOI
        out.extend_from_slice(&[0xFF, 0xE1]);
        out.extend_from_slice(&((app1.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(&app1);
        out.extend_from_slice(&jpg[2..]);
        let path = src.with_file_name(format!("rot-{name}"));
        std::fs::write(&path, out).unwrap();
        path
    }

    #[test]
    fn image_crate_applies_orientation() {
        let path = write_rotated_jpg("o1.jpg");
        let l = load_image_crate(&path).unwrap();
        assert_eq!(l.size().unwrap(), (128, 256));
    }

    #[test]
    fn load_applies_orientation() {
        let path = write_rotated_jpg("o2.jpg");
        let l = Image.load(&path).unwrap();
        let (w, h) = l.size().unwrap();
        assert!(h > w, "{w}x{h}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn imageio_applies_orientation_and_reads_heic() {
        let path = write_rotated_jpg("o3.jpg");
        let l = load_imageio(&path).unwrap();
        assert_eq!(l.size().unwrap(), (128, 256));

        let png = write("h.png", ImageFormat::Png);
        let heic = png.with_file_name("h.heic");
        let ok = std::process::Command::new("sips")
            .args(["-s", "format", "heic"])
            .arg(&png)
            .arg("--out")
            .arg(&heic)
            .output()
            .map(|o| o.status.success() && heic.exists())
            .unwrap_or(false);
        if !ok {
            eprintln!("sips が使えないので heic のテストは飛ばす");
            return;
        }
        let l = Image.load(&heic).expect("heic");
        assert_eq!(l.size().unwrap(), (256, 128));
    }

    #[test]
    fn broken_file_is_none_on_both_paths() {
        let dir = std::env::temp_dir().join("lisa-preview-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("broken2.jpg");
        std::fs::write(&path, b"not an image").unwrap();
        assert!(load_image_crate(&path).is_none());
        #[cfg(target_os = "macos")]
        assert!(load_imageio(&path).is_none());
    }
}
