//! カタログの印（`catalog.rs` の `CatalogMarks`）を本物の場面に描く。画像には焼かない。
//!
//! - 型か仕分けに当たったフォルダは、耳のすぐ右に札（アプリの色と `label`）。
//! - 中の `cache`・`install`・`log` は、少し暗い膜をかぶせる。
//! - パネルで選んだ当たりは、アプリの色の枠。

use bevy::prelude::*;

use crate::catalog::CatalogMarks;
use crate::model::tree::NodeId;
use crate::render::ear::{self, Slot};
use crate::render::shapes::Painter;

/// 暗く描く膜の濃さ。
const DIM_ALPHA: f32 = 0.38;

/// タイル `id` の本体に、暗い膜と選んだ枠を描く（本体の入れ物の中で呼ぶ）。
pub fn draw_body_marks(
    painter: &mut Painter,
    marks: &CatalogMarks,
    id: NodeId,
    frame: Rect,
    z: f32,
) {
    if marks.dim.contains(&id) {
        painter.rect(frame, Color::BLACK.with_alpha(DIM_ALPHA), z + 0.3);
    }
    if let Some((selected, color)) = marks.selected
        && selected == id
    {
        let w = (frame.height() * 0.04).max(painter.scale * 2.5);
        let (min, max) = (frame.min, frame.max);
        for rect in [
            Rect::new(min.x, min.y, max.x, min.y + w),
            Rect::new(min.x, max.y - w, max.x, max.y),
            Rect::new(min.x, min.y + w, min.x + w, max.y - w),
            Rect::new(max.x - w, min.y + w, max.x, max.y - w),
        ] {
            painter.rect(rect, color, z + 0.42);
        }
    }
}

/// フォルダ `id`（名前 `name`）が当たっていれば、耳の右に札を描く。
pub fn draw_tag(
    painter: &mut Painter,
    marks: &CatalogMarks,
    id: NodeId,
    slot: Slot,
    size: ear::Size,
    name: &str,
    z: f32,
) {
    if let Some((label, color)) = marks.tags.get(&id) {
        ear::draw_tag(painter, slot, size, name, label, *color, z + 0.5);
    }
}
