//! 動画ファイルの仮のプレビュー（暗い画面に再生ボタン）。

use bevy::prelude::*;

use crate::preview::{Cartridge, DrawContext};

pub struct Video;

impl Cartridge for Video {
    fn id(&self) -> &'static str {
        "video"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["mov", "mp4"]
    }

    fn draw(&self, ctx: &mut DrawContext<'_, '_, '_, '_>) {
        let inner = ctx.inner;
        let z = ctx.z;
        ctx.painter.rect(inner, Color::srgb(0.1, 0.1, 0.12), z);
        let center = inner.center();
        let r = inner.height() * 0.22;
        ctx.painter.triangle(
            [
                center + Vec2::new(-r * 0.7, r),
                center + Vec2::new(-r * 0.7, -r),
                center + Vec2::new(r, 0.0),
            ],
            Color::srgba(1.0, 1.0, 1.0, 0.8),
            z + 0.01,
        );
    }
}
