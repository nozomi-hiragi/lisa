//! Lisa のスキャンの中身。Bevy に依存しない。
//!
//! 今はフォルダを 1 段読むところまで（`listing`）。ドライブの列挙、ハッシュ、DB はこのあと足す
//! （`docs/scan.md` の「実装の順番」）。

pub mod cloud;
pub mod listing;
pub mod name;
pub mod volumes;
