//! 文書ファイルのプレビュー。素のテキストは先頭の数行を文字で、読めない・docx・xlsx は灰色の行を出す。

use std::io::Read;
use std::path::Path;

use bevy::math::Rect;
use bevy::prelude::*;
use bevy::sprite::Anchor;

use crate::preview::{Cartridge, DrawContext, LoadedPreview, Preview, extension};
use crate::render::shapes::{em_width, over};

pub struct Text;

/// 先頭から読むバイト数。
const HEAD_BYTES: u64 = 4096;
/// 持つ行の数。
const MAX_LINES: usize = 8;
/// 1 行あたりの文字数。
const MAX_CHARS: usize = 60;
/// 素のテキストとして読む拡張子。
const PLAIN: &[&str] = &[
    "txt", "md", "log", "csv", "json", "yaml", "yml", "toml", "rs", "py", "js", "ts", "sh",
];

impl Cartridge for Text {
    fn id(&self) -> &'static str {
        "text"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &[
            "txt", "md", "log", "csv", "json", "yaml", "yml", "toml", "rs", "py", "js", "ts", "sh",
            "docx", "xlsx",
        ]
    }

    fn can_load(&self, name: &str) -> bool {
        PLAIN.contains(&extension(name).to_ascii_lowercase().as_str())
    }

    fn load(&self, path: &Path) -> Option<LoadedPreview> {
        let mut head = Vec::new();
        std::fs::File::open(path)
            .ok()?
            .take(HEAD_BYTES)
            .read_to_end(&mut head)
            .ok()?;
        head_lines(&head).map(|lines| LoadedPreview::Text { lines })
    }

    fn draw(&self, ctx: &mut DrawContext<'_, '_, '_, '_>) {
        if let Some(Preview::Text(lines)) = ctx.loaded.clone() {
            let inner = ctx.inner;
            let pitch = inner.height() / MAX_LINES as f32;
            let widest = lines.iter().map(|l| em_width(l)).fold(1.0, f32::max);
            // 全部の行を同じ大きさにして、いちばん長い行が紙に収まるようにする。
            let height = (pitch * 0.8).min(inner.width() * 0.95 / widest);
            let color = over(ctx.page, Color::srgb(0.8, 0.8, 0.85), 0.8);
            for (i, line) in lines.iter().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                ctx.painter.text(
                    line.clone(),
                    Vec2::new(inner.min.x, inner.max.y - i as f32 * pitch),
                    Anchor::TOP_LEFT,
                    height,
                    f32::MAX / 4.0,
                    color,
                    ctx.z,
                );
            }
            return;
        }
        grey_lines(ctx);
    }
}

/// 灰色の行を何本か描く（中身が読めないときの絵）。
pub fn grey_lines(ctx: &mut DrawContext<'_, '_, '_, '_>) {
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

/// 先頭のバイト列を UTF-8 として読み、最初の 8 行（各 60 文字まで、タブは空白 4 つ）にする。
/// 壊れた末尾は捨てる。UTF-8 でない・0x00 を含む・文字が無いなら None。
fn head_lines(head: &[u8]) -> Option<Vec<String>> {
    let text = match std::str::from_utf8(head) {
        Ok(t) => t,
        // 4 KB で切れた末尾の文字だけを捨てる。途中の不正なバイトはバイナリとみなす。
        Err(e) if e.error_len().is_none() => std::str::from_utf8(&head[..e.valid_up_to()]).ok()?,
        Err(_) => return None,
    };
    if text.contains('\0') || text.trim().is_empty() {
        return None;
    }
    Some(
        text.lines()
            .take(MAX_LINES)
            .map(|l| l.replace('\t', "    ").chars().take(MAX_CHARS).collect())
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(bytes: &[u8]) -> Option<Vec<String>> {
        head_lines(bytes)
    }

    #[test]
    fn caps_lines_and_width() {
        let text: String = (0..20).map(|i| format!("line {i}\n")).collect();
        assert_eq!(lines(text.as_bytes()).unwrap().len(), 8);
        let long = "x".repeat(200);
        assert_eq!(lines(long.as_bytes()).unwrap()[0].len(), 60);
    }

    #[test]
    fn tabs_become_four_spaces() {
        assert_eq!(lines(b"\ta\tb").unwrap()[0], "    a    b");
    }

    #[test]
    fn japanese_and_cut_tail() {
        assert_eq!(
            lines("こんにちは\n世界".as_bytes()).unwrap(),
            ["こんにちは", "世界"]
        );
        // 4 KB の境目で日本語の文字が途中で切れても、その文字だけ捨てて読める。
        let mut b = "あ".repeat(10).into_bytes();
        b.truncate(b.len() - 1);
        assert_eq!(lines(&b).unwrap()[0], "あ".repeat(9));
    }

    #[test]
    fn binary_and_empty_are_none() {
        assert!(lines(b"abc\0def").is_none());
        assert!(lines(&[0xff, 0xfe, b'a']).is_none());
        assert!(lines(b"").is_none());
        assert!(lines(b" \n\t\n").is_none());
    }

    #[test]
    fn load_reads_file_and_rejects_bad() {
        let dir = std::env::temp_dir().join("lisa-preview-test");
        std::fs::create_dir_all(&dir).unwrap();
        let ok = dir.join("t.txt");
        std::fs::write(&ok, "hello\nworld\n").unwrap();
        assert!(matches!(
            Text.load(&ok),
            Some(LoadedPreview::Text { lines }) if lines == ["hello", "world"]
        ));
        let bin = dir.join("b.txt");
        std::fs::write(&bin, [0u8, 1, 2, 3]).unwrap();
        assert!(Text.load(&bin).is_none());
        let empty = dir.join("e.txt");
        std::fs::write(&empty, b"").unwrap();
        assert!(Text.load(&empty).is_none());
        assert!(Text.load(&dir.join("missing.txt")).is_none());
        assert!(Text.can_load("a.JSON") && !Text.can_load("a.docx") && !Text.can_load("a.pdf"));
    }
}
