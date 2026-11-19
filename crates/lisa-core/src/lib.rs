//! Lisa のスキャンの中身。Bevy に依存しない。
//!
//! フォルダを 1 段読む（`listing`）、ドライブの列挙（`volumes`）、読んだ木を残す（`db`）。ハッシュはこのあと足す
//! （`docs/scan.md` の「実装の順番」）。

pub mod catalog;
pub mod cloud;
pub mod db;
pub mod dups;
#[cfg(target_os = "macos")]
pub mod fsevents;
pub mod hash;
pub mod listing;
pub mod name;
pub mod organize;
pub mod pace;
pub mod query;
pub mod remote;
pub mod volumes;
