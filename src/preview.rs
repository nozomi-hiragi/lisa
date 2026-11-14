//! ファイルのプレビュー。今はダミーなので、拡張子ごとの仮の絵を描く。

use bevy::math::Rect;
use bevy::prelude::*;

use crate::shapes::Painter;

pub enum PreviewKind {
    Image,
    Text,
    Video,
    Other,
}

impl PreviewKind {
    pub fn from_name(name: &str) -> Self {
        let ext = extension(name).to_ascii_lowercase();
        match ext.as_str() {
            "jpg" | "jpeg" | "png" | "heic" | "gif" => Self::Image,
            "txt" | "md" | "pdf" | "docx" | "xlsx" => Self::Text,
            "mov" | "mp4" => Self::Video,
            _ => Self::Other,
        }
    }
}

/// 拡張子（無ければ空）。
pub fn extension(name: &str) -> &str {
    name.rsplit_once('.').map_or("", |(_, ext)| ext)
}

/// 紙の形の背景（右上が折れている）と、種類ごとの仮の絵を描く。
/// 「その他」の場合は拡張子を大きく書くため、文字を置く場所を返す。
pub fn draw(painter: &mut Painter, frame: Rect, kind: &PreviewKind, z: f32) -> Option<Rect> {
    let page = Color::srgba(0.85, 0.87, 0.95, 0.10);
    let fold = Color::srgba(0.85, 0.87, 0.95, 0.22);
    let c = frame.height() * 0.15;
    painter.rect(
        Rect::new(frame.min.x, frame.min.y, frame.max.x - c, frame.max.y),
        page,
        z,
    );
    painter.rect(
        Rect::new(frame.max.x - c, frame.min.y, frame.max.x, frame.max.y - c),
        page,
        z,
    );
    painter.triangle(
        [
            Vec2::new(frame.max.x - c, frame.max.y),
            Vec2::new(frame.max.x - c, frame.max.y - c),
            Vec2::new(frame.max.x, frame.max.y - c),
        ],
        fold,
        z,
    );

    let pad = frame.height() * 0.14;
    let inner = Rect {
        min: frame.min + pad,
        max: frame.max - pad,
    };
    let z = z + 0.05;
    match kind {
        PreviewKind::Image => {
            // 空、山ふたつ、太陽。
            painter.rect(inner, Color::srgb(0.35, 0.5, 0.65), z);
            let (w, h) = (inner.width(), inner.height());
            let base = inner.min.y;
            painter.triangle(
                [
                    Vec2::new(inner.min.x, base),
                    Vec2::new(inner.min.x + w * 0.35, base + h * 0.65),
                    Vec2::new(inner.min.x + w * 0.7, base),
                ],
                Color::srgb(0.3, 0.45, 0.35),
                z + 0.01,
            );
            painter.triangle(
                [
                    Vec2::new(inner.min.x + w * 0.4, base),
                    Vec2::new(inner.min.x + w * 0.72, base + h * 0.45),
                    Vec2::new(inner.max.x, base),
                ],
                Color::srgb(0.38, 0.55, 0.4),
                z + 0.02,
            );
            painter.circle(
                Vec2::new(inner.max.x - w * 0.15, inner.max.y - h * 0.22),
                h * 0.1,
                Color::srgb(0.95, 0.85, 0.5),
                z + 0.01,
            );
            None
        }
        PreviewKind::Text => {
            // 灰色の行を何本か。
            let lines = 6;
            let line_h = inner.height() / (lines as f32 * 1.8);
            for i in 0..lines {
                let top = inner.max.y - i as f32 * line_h * 1.8;
                let width = inner.width() * [1.0, 0.92, 0.97, 0.85, 0.95, 0.6][i];
                painter.rect(
                    Rect::new(inner.min.x, top - line_h, inner.min.x + width, top),
                    Color::srgba(0.8, 0.8, 0.85, 0.35),
                    z,
                );
            }
            None
        }
        PreviewKind::Video => {
            // 暗い画面に再生ボタン。
            painter.rect(inner, Color::srgb(0.1, 0.1, 0.12), z);
            let center = inner.center();
            let r = inner.height() * 0.22;
            painter.triangle(
                [
                    center + Vec2::new(-r * 0.7, r),
                    center + Vec2::new(-r * 0.7, -r),
                    center + Vec2::new(r, 0.0),
                ],
                Color::srgba(1.0, 1.0, 1.0, 0.8),
                z + 0.01,
            );
            None
        }
        PreviewKind::Other => Some(inner),
    }
}
