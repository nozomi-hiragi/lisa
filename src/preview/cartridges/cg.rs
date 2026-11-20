//! macOS の CoreGraphics まわりの共有部品（画像・動画・pdf のカートリッジが使う）。

use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGColorSpace, CGContext, CGImage, kCGColorSpaceSRGB,
};

use crate::preview::LoadedPreview;

/// sRGB・RGBA 8bit（alpha 掛け済み）の CGBitmapContext と、その裏の置き場を作る。
pub fn bitmap_context(
    width: usize,
    height: usize,
    buf: &mut [u8],
) -> Option<objc2_core_foundation::CFRetained<CGContext>> {
    assert!(buf.len() >= width * height * 4);
    // SAFETY: kCGColorSpaceSRGB は CoreGraphics が持つ有効な定数。
    let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))?;
    // SAFETY: buf は width*height*4 バイト以上で、呼び出し側が context より長く持つ。bitmap_info 1 は
    // kCGImageAlphaPremultipliedLast（RGBA の順、alpha 掛け済み）。
    unsafe {
        CGBitmapContextCreate(
            buf.as_mut_ptr().cast(),
            width,
            height,
            8,
            width * 4,
            Some(&space),
            1,
        )
    }
}

/// alpha 掛け済みの RGBA を、掛けていない値に戻す。
pub fn unpremultiply(buf: &mut [u8]) {
    for px in buf.as_chunks_mut::<4>().0 {
        let a = px[3] as u32;
        if a != 0 && a != 255 {
            for c in &mut px[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
}

/// CGImage を RGBA の絵にする。
pub fn rgba_from_cgimage(image: &CGImage) -> Option<LoadedPreview> {
    let width = CGImage::width(Some(image));
    let height = CGImage::height(Some(image));
    if width == 0 || height == 0 {
        return None;
    }
    let mut buf = vec![0u8; width * height * 4];
    let ctx = bitmap_context(width, height, &mut buf)?;
    let rect = CGRect::new(
        CGPoint::new(0.0, 0.0),
        CGSize::new(width as f64, height as f64),
    );
    CGContext::draw_image(Some(&ctx), rect, Some(image));
    drop(ctx);
    unpremultiply(&mut buf);
    Some(LoadedPreview::Image {
        width: width as u32,
        height: height as u32,
        rgba: buf,
    })
}
