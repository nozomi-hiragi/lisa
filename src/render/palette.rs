//! 描画用のカラーパレット定義。
//!
//! 描画・UI 全体で参照する色トークンと、画面比較用の色切替定数をまとめる。

use bevy::prelude::Color;

// Brand
pub const PRIMARY: Color = Color::srgb_u8(0xd1, 0xde, 0x79); // #d1de79
#[allow(dead_code)]
pub const PRIMARY_CONTENT: Color = Color::srgb_u8(0x1f, 0x24, 0x10); // #1f2410

pub const SECONDARY: Color = Color::srgb_u8(0xf2, 0xa7, 0xc3); // #f2a7c3
#[allow(dead_code)]
pub const SECONDARY_CONTENT: Color = Color::srgb_u8(0x2e, 0x12, 0x20); // #2e1220

pub const ACCENT: Color = Color::srgb_u8(0x8d, 0xb4, 0xea); // #8db4ea
pub const ACCENT_CONTENT: Color = Color::srgb_u8(0x0f, 0x1f, 0x36); // #0f1f36

pub const NEUTRAL: Color = Color::srgb_u8(0x3a, 0x3c, 0x40); // #3a3c40
#[allow(dead_code)]
pub const NEUTRAL_CONTENT: Color = Color::srgb_u8(0xe4, 0xe4, 0xde); // #e4e4de

pub const FILE_WHITE: Color = Color::srgb_u8(0xf2, 0xf2, 0xee); // #f2f2ee

// Base
pub const BASE_100: Color = Color::srgb_u8(0x1c, 0x1d, 0x20); // #1c1d20（最前面サーフェス・プレート・パネル・カード）
pub const BASE_200: Color = Color::srgb_u8(0x15, 0x16, 0x18); // #151618（キャンバス背景・HUD グリッド展開面）
pub const BASE_300: Color = Color::srgb_u8(0x0e, 0x0f, 0x11); // #0e0f11（最下層背景・メニューバー）
pub const BASE_CONTENT: Color = Color::srgb_u8(0xe8, 0xe8, 0xe0); // #e8e8e0（主要テキスト・ラベル）

// State / Muted
pub const INFO: Color = Color::srgb_u8(0x6f, 0xd0, 0xdc); // #6fd0dc
#[allow(dead_code)]
pub const INFO_CONTENT: Color = Color::srgb_u8(0x06, 0x2a, 0x2e); // #062a2e

pub const SUCCESS: Color = Color::srgb_u8(0x7f, 0xd6, 0x8c); // #7fd68c
pub const SUCCESS_CONTENT: Color = Color::srgb_u8(0x0c, 0x26, 0x11); // #0c2611

pub const WARNING: Color = Color::srgb_u8(0xf0, 0xa0, 0x4b); // #f0a04b
pub const WARNING_CONTENT: Color = Color::srgb_u8(0x2a, 0x17, 0x05); // #2a1705

pub const ERROR: Color = Color::srgb_u8(0xf0, 0x7a, 0x7a); // #f07a7a
pub const ERROR_CONTENT: Color = Color::srgb_u8(0x2e, 0x0b, 0x0b); // #2e0b0b

pub const MUTED: Color = Color::srgb_u8(0x9c, 0x9d, 0x96); // #9c9d96（補助テキスト・未読込要素）

/// `BASE_100` に不透明度を指定した色（半透明パネル・ポップアップ用）。
pub const fn base_100_alpha(alpha: f32) -> Color {
    Color::srgba(28.0 / 255.0, 29.0 / 255.0, 32.0 / 255.0, alpha)
}

/// ファイル枠・プレビュー台紙の配色候補。
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileColorChoice {
    Pink,
    Gray,
    White,
}

/// 現在のファイル配色（画面確認時の比較用スイッチ）。
pub const CURRENT_FILE_COLOR: FileColorChoice = FileColorChoice::White;

/// 指定された選択肢に対応するファイル色を返す。
pub const fn file_color_for(choice: FileColorChoice) -> Color {
    match choice {
        FileColorChoice::Pink => SECONDARY,
        FileColorChoice::Gray => NEUTRAL,
        FileColorChoice::White => FILE_WHITE,
    }
}

/// 現在のファイル色を返す。
pub const fn file_color() -> Color {
    file_color_for(CURRENT_FILE_COLOR)
}

/// 読込中スピナーの配色候補。
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoadingColorChoice {
    Blue,
    Info,
}

/// 現在の読込中スピナー配色（画面確認時の比較用スイッチ）。
pub const CURRENT_LOADING_COLOR: LoadingColorChoice = LoadingColorChoice::Blue;

/// 指定された選択肢に対応する読込中スピナー色を返す。
pub const fn loading_color_for(choice: LoadingColorChoice) -> Color {
    match choice {
        LoadingColorChoice::Blue => ACCENT,
        LoadingColorChoice::Info => INFO,
    }
}

/// 現在の読込中スピナー色を返す。
pub const fn loading_color() -> Color {
    loading_color_for(CURRENT_LOADING_COLOR)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn to_u8_rgb(color: Color) -> (u8, u8, u8) {
        let srgba = color.to_srgba();
        (
            (srgba.red * 255.0).round() as u8,
            (srgba.green * 255.0).round() as u8,
            (srgba.blue * 255.0).round() as u8,
        )
    }

    #[test]
    fn palette_tokens_match_hex_definitions() {
        assert_eq!(to_u8_rgb(PRIMARY), (0xd1, 0xde, 0x79));
        assert_eq!(to_u8_rgb(PRIMARY_CONTENT), (0x1f, 0x24, 0x10));
        assert_eq!(to_u8_rgb(SECONDARY), (0xf2, 0xa7, 0xc3));
        assert_eq!(to_u8_rgb(SECONDARY_CONTENT), (0x2e, 0x12, 0x20));
        assert_eq!(to_u8_rgb(ACCENT), (0x8d, 0xb4, 0xea));
        assert_eq!(to_u8_rgb(ACCENT_CONTENT), (0x0f, 0x1f, 0x36));
        assert_eq!(to_u8_rgb(NEUTRAL), (0x3a, 0x3c, 0x40));
        assert_eq!(to_u8_rgb(NEUTRAL_CONTENT), (0xe4, 0xe4, 0xde));

        assert_eq!(to_u8_rgb(BASE_100), (0x1c, 0x1d, 0x20));
        assert_eq!(to_u8_rgb(BASE_200), (0x15, 0x16, 0x18));
        assert_eq!(to_u8_rgb(BASE_300), (0x0e, 0x0f, 0x11));
        assert_eq!(to_u8_rgb(BASE_CONTENT), (0xe8, 0xe8, 0xe0));

        assert_eq!(to_u8_rgb(INFO), (0x6f, 0xd0, 0xdc));
        assert_eq!(to_u8_rgb(INFO_CONTENT), (0x06, 0x2a, 0x2e));
        assert_eq!(to_u8_rgb(SUCCESS), (0x7f, 0xd6, 0x8c));
        assert_eq!(to_u8_rgb(SUCCESS_CONTENT), (0x0c, 0x26, 0x11));
        assert_eq!(to_u8_rgb(WARNING), (0xf0, 0xa0, 0x4b));
        assert_eq!(to_u8_rgb(WARNING_CONTENT), (0x2a, 0x17, 0x05));
        assert_eq!(to_u8_rgb(ERROR), (0xf0, 0x7a, 0x7a));
        assert_eq!(to_u8_rgb(ERROR_CONTENT), (0x2e, 0x0b, 0x0b));
        assert_eq!(to_u8_rgb(MUTED), (0x9c, 0x9d, 0x96));
    }

    #[test]
    fn file_color_switch_returns_expected_token() {
        assert_eq!(file_color_for(FileColorChoice::Pink), SECONDARY);
        assert_eq!(file_color_for(FileColorChoice::Gray), NEUTRAL);
        assert_eq!(file_color_for(FileColorChoice::White), FILE_WHITE);
        assert_eq!(to_u8_rgb(file_color()), (0xf2, 0xf2, 0xee));
    }

    #[test]
    fn loading_color_switch_returns_expected_token() {
        assert_eq!(loading_color_for(LoadingColorChoice::Blue), ACCENT);
        assert_eq!(loading_color_for(LoadingColorChoice::Info), INFO);
        assert_eq!(loading_color(), loading_color_for(CURRENT_LOADING_COLOR));
    }
}
