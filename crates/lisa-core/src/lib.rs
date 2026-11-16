//! Lisa のスキャンの中身。Bevy に依存しない。
//!
//! フォルダを 1 段読む（`listing`）、ドライブの列挙（`volumes`）、読んだ木を残す（`db`）。ハッシュはこのあと足す
//! （`docs/scan.md` の「実装の順番」）。

pub mod cloud;
pub mod db;
pub mod hash;
pub mod listing;
pub mod name;
pub mod volumes;
