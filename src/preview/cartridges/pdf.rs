//! pdf のプレビュー。macOS は 1 ページ目を白地に描いて出す。読めるまで・ほかの OS は灰色の行。

use std::path::Path;

use bevy::prelude::*;

use crate::preview::{Cartridge, DrawContext, LoadedPreview, Preview};

pub struct Pdf;

/// 描いたあとの長辺。
#[cfg(target_os = "macos")]
const THUMB_SIDE: f64 = 256.0;

impl Cartridge for Pdf {
    fn id(&self) -> &'static str {
        "pdf"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["pdf"]
    }

    fn can_load(&self, name: &str) -> bool {
        cfg!(target_os = "macos") && crate::preview::extension(name).eq_ignore_ascii_case("pdf")
    }

    #[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
    fn load(&self, path: &Path) -> Option<LoadedPreview> {
        #[cfg(target_os = "macos")]
        return load_first_page(path);
        #[cfg(not(target_os = "macos"))]
        None
    }

    fn draw(&self, ctx: &mut DrawContext<'_, '_, '_, '_>) {
        if let Some(Preview::Image(handle, size)) = ctx.loaded.clone()
            && size.x > 0
            && size.y > 0
        {
            let inner = ctx.inner;
            let k = (inner.width() / size.x as f32).min(inner.height() / size.y as f32);
            let fitted = Rect::from_center_size(inner.center(), size.as_vec2() * k);
            ctx.painter.image(fitted, handle, ctx.z);
            return;
        }
        super::text::grey_lines(ctx);
    }
}

/// CGPDFDocument の 1 ページ目を、白い背景の上に長辺 256 px に収めて描く。
#[cfg(target_os = "macos")]
fn load_first_page(path: &Path) -> Option<LoadedPreview> {
    use objc2_core_foundation::{CFURL, CGPoint, CGRect, CGSize};
    use objc2_core_graphics::{CGContext, CGPDFBox, CGPDFDocument, CGPDFPage};

    let url = CFURL::from_file_path(path)?;
    let doc = CGPDFDocument::with_url(Some(&url))?;
    if CGPDFDocument::number_of_pages(Some(&doc)) == 0 {
        return None;
    }
    let page = CGPDFDocument::page(Some(&doc), 1)?;
    let media = CGPDFPage::box_rect(Some(&page), CGPDFBox::CropBox);
    // 回転を入れた見た目の大きさ。
    let (pw, ph) = {
        let rot = CGPDFPage::rotation_angle(Some(&page)).rem_euclid(360);
        if rot == 90 || rot == 270 {
            (media.size.height, media.size.width)
        } else {
            (media.size.width, media.size.height)
        }
    };
    if !(pw > 0.0 && ph > 0.0) {
        return None;
    }
    let k = THUMB_SIDE / pw.max(ph);
    let width = ((pw * k).round() as usize).max(1);
    let height = ((ph * k).round() as usize).max(1);

    let mut buf = vec![0u8; width * height * 4];
    let ctx = super::cg::bitmap_context(width, height, &mut buf)?;
    let area = CGRect::new(
        CGPoint::new(0.0, 0.0),
        CGSize::new(width as f64, height as f64),
    );
    CGContext::set_rgb_fill_color(Some(&ctx), 1.0, 1.0, 1.0, 1.0);
    CGContext::fill_rect(Some(&ctx), area);
    let transform = CGPDFPage::drawing_transform(Some(&page), CGPDFBox::CropBox, area, 0, true);
    CGContext::save_g_state(Some(&ctx));
    CGContext::concat_ctm(Some(&ctx), transform);
    CGContext::draw_pdf_page(Some(&ctx), Some(&page));
    CGContext::restore_g_state(Some(&ctx));
    drop(ctx);
    // 背景が不透明な白なので、alpha 掛けの戻しは要らない。
    Some(LoadedPreview::Image {
        width: width as u32,
        height: height as u32,
        rgba: buf,
    })
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn renders_first_page_on_white() {
        let dir = std::env::temp_dir().join("lisa-preview-test");
        std::fs::create_dir_all(&dir).unwrap();
        let txt = dir.join("p.txt");
        std::fs::write(&txt, "hello pdf\n").unwrap();
        let out = std::process::Command::new("cupsfilter")
            .arg(&txt)
            .output()
            .ok()
            .filter(|o| o.status.success() && o.stdout.starts_with(b"%PDF"));
        let Some(out) = out else {
            eprintln!("cupsfilter が使えないので pdf のテストは飛ばす");
            return;
        };
        let path = dir.join("p.pdf");
        std::fs::write(&path, out.stdout).unwrap();
        assert!(Pdf.can_load("A.PDF"));
        let Some(LoadedPreview::Image {
            width,
            height,
            rgba,
        }) = Pdf.load(&path)
        else {
            panic!("pdf を読めなかった");
        };
        assert_eq!(width.max(height), 256);
        assert!(height > width, "縦長のはず: {width}x{height}");
        assert_eq!(rgba.len(), (width * height * 4) as usize);
        assert_eq!(&rgba[..4], &[255, 255, 255, 255]);
    }

    #[test]
    fn broken_file_is_none() {
        let dir = std::env::temp_dir().join("lisa-preview-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("broken.pdf");
        std::fs::write(&path, b"not a pdf").unwrap();
        assert!(Pdf.load(&path).is_none());
        assert!(Pdf.load(&dir.join("missing.pdf")).is_none());
    }
}
