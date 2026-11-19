//! 種類のわからないファイルの受け皿プレビュー（拡張子を大きく書く）。

use bevy::sprite::Anchor;

use crate::preview::{Cartridge, DrawContext, extension};
use crate::render::shapes::over;

pub struct Other;

impl Cartridge for Other {
    fn id(&self) -> &'static str {
        "other"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &[]
    }

    fn draw(&self, ctx: &mut DrawContext<'_, '_, '_, '_>) {
        let ext = extension(ctx.name).to_uppercase();
        let ext = if ext.is_empty() {
            "FILE".to_string()
        } else {
            ext
        };
        ctx.painter.text(
            ext,
            ctx.inner.center(),
            Anchor::CENTER,
            ctx.inner.height() * 0.4,
            ctx.inner.width(),
            over(ctx.page, ctx.paper, 0.5),
            ctx.z,
        );
    }
}
