//! 文書ファイルの仮のプレビュー（灰色の行を何本か）。

use bevy::math::Rect;
use bevy::prelude::*;

use crate::preview::{Cartridge, DrawContext};
use crate::render::shapes::over;

pub struct Text;

impl Cartridge for Text {
    fn id(&self) -> &'static str {
        "text"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["txt", "md", "pdf", "docx", "xlsx"]
    }

    fn draw(&self, ctx: &mut DrawContext<'_, '_, '_, '_>) {
        let inner = ctx.inner;
        let lines = 6;
        let line_h = inner.height() / (lines as f32 * 1.8);
        for i in 0..lines {
            let top = inner.max.y - i as f32 * line_h * 1.8;
            let width = inner.width() * [1.0, 0.92, 0.97, 0.85, 0.95, 0.6][i];
            ctx.painter.rect(
                Rect::new(inner.min.x, top - line_h, inner.min.x + width, top),
                over(ctx.page, Color::srgb(0.8, 0.8, 0.85), 0.35),
                ctx.z,
            );
        }
    }
}
