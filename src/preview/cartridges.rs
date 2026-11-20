//! 組み込みのプレビュー用カートリッジの一覧。
//! ここに 1 行足す・消すだけで、カートリッジを有効・無効にできる。

#[cfg(target_os = "macos")]
mod cg;
mod image;
mod other;
mod pdf;
mod text;
mod video;

use crate::preview::PreviewRegistry;

/// 使うカートリッジを登録する。
pub fn register(r: &mut PreviewRegistry) {
    r.add(image::Image);
    r.add(pdf::Pdf);
    r.add(text::Text);
    r.add(video::Video);
    r.add(other::Other);
}
