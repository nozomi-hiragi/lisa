//! 画像ファイルの仮のプレビュー（空、山ふたつ、太陽）。

use bevy::prelude::*;

use crate::preview::{Cartridge, DrawContext};

pub struct Image;

impl Cartridge for Image {
    fn id(&self) -> &'static str {
        "image"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["jpg", "jpeg", "png", "heic", "gif", "webp"]
    }

    fn draw(&self, ctx: &mut DrawContext<'_, '_, '_, '_>) {
        let inner = ctx.inner;
        let z = ctx.z;
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
