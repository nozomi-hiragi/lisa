//! パスのパターン（`glob` と `re:`、`**`、NFC 正規化、大文字小文字の扱い）。

use std::borrow::Cow;

use regex::Regex;
use unicode_normalization::{IsNormalized, UnicodeNormalization, is_nfc_quick};

/// パスの 1 段または `**` を表す。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    /// `**`（0 段以上のフォルダ）。
    Any,
    /// 1 段分の名前の一致判定。
    Name(NameMatcher),
}

/// パスの 1 段（フォルダ名またはファイル名）を比べるもの。
#[derive(Debug, Clone)]
pub struct NameMatcher {
    regex: Regex,
    regex_str: String,
    literal: Option<String>,
}

impl PartialEq for NameMatcher {
    fn eq(&self, other: &Self) -> bool {
        self.regex_str == other.regex_str && self.literal == other.literal
    }
}

impl Eq for NameMatcher {}

impl NameMatcher {
    /// 1 段分の文字列（`re:…` または glob）から作る。
    fn parse(seg: &str, case_sensitive: bool) -> Result<Self, String> {
        let (inner_regex, literal) = if let Some(rest) = seg.strip_prefix("re:") {
            let re_nfc: String = rest.nfc().collect();
            (re_nfc, None)
        } else {
            glob_to_regex(seg)?
        };

        let regex_str = if case_sensitive {
            format!("(?-i:{inner_regex})")
        } else {
            format!("(?i:{inner_regex})")
        };

        let regex = Regex::new(&regex_str)
            .map_err(|e| format!("パターンの正規表現が不正です（{seg}）: {e}"))?;

        Ok(Self {
            regex,
            regex_str,
            literal,
        })
    }

    /// glob が文字そのもの（`*`, `?`, `{}` を含まない）なら小文字にした文字列を返す。
    pub fn literal(&self) -> Option<&str> {
        self.literal.as_deref()
    }

    /// 1 段の名前（NFC を想定、NFD が来ても NFC にそろえて比べる）が当たるか。
    pub fn is_match(&self, name_nfc: &str) -> bool {
        let normalized = to_nfc(name_nfc);
        self.regex.is_match(&normalized)
    }

    /// `RegexSet` などにそのまま渡せる正規表現の文字列（大文字小文字の印つき）。
    pub(crate) fn regex_str(&self) -> &str {
        &self.regex_str
    }
}

/// カタログの `path` パターン。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pattern {
    segments: Vec<Segment>,
    is_dir: bool,
}

impl Pattern {
    /// `path` 文字列を解析する。`/` で段に分け、末尾の `/` はフォルダの印とする。
    pub fn parse(text: &str, case_sensitive: bool) -> Result<Pattern, String> {
        if text.is_empty() {
            return Err("path が空です".to_string());
        }
        if text.starts_with('/') {
            return Err(format!("path の先頭に / は使えません: {text}"));
        }

        let is_dir = text.ends_with('/');
        let body = text.strip_suffix('/').unwrap_or(text);
        if body.is_empty() {
            return Err(format!("path が空です: {text}"));
        }

        let raw_segments: Vec<&str> = body.split('/').collect();
        let last_idx = raw_segments.len() - 1;
        let mut segments = Vec::with_capacity(raw_segments.len());

        for (idx, seg) in raw_segments.iter().enumerate() {
            if seg.is_empty() {
                return Err(format!("path に空の段（//）は使えません: {text}"));
            }
            if *seg == "." || *seg == ".." {
                return Err(format!("path の段に . や .. は使えません: {text}"));
            }
            if *seg == "**" {
                if idx == last_idx {
                    return Err(format!("path の最後の段に ** は使えません: {text}"));
                }
                segments.push(Segment::Any);
            } else {
                segments.push(Segment::Name(NameMatcher::parse(seg, case_sensitive)?));
            }
        }

        Ok(Pattern { segments, is_dir })
    }

    /// 末尾が `/`（フォルダを指すパターン）なら true。
    pub fn is_dir(&self) -> bool {
        self.is_dir
    }

    /// 1 段目を返す。
    pub fn first(&self) -> &Segment {
        &self.segments[0]
    }

    /// すべての段を返す。
    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    /// 1 段目の正規表現（`**` なら None。大文字小文字の印を含む）。
    pub fn first_segment_regex(&self) -> Option<String> {
        match self.first() {
            Segment::Any => None,
            Segment::Name(matcher) => Some(matcher.regex_str().to_string()),
        }
    }
}

/// 必要なら NFC にそろえる（すでに NFC ならコピーしない）。
pub(crate) fn to_nfc(s: &str) -> Cow<'_, str> {
    if is_nfc_quick(s.chars()) == IsNormalized::Yes {
        Cow::Borrowed(s)
    } else {
        Cow::Owned(s.nfc().collect())
    }
}

/// glob 1 段を `^...$` 形式の正規表現と、文字そのものなら小文字文字列に変換する。
fn glob_to_regex(seg: &str) -> Result<(String, Option<String>), String> {
    let seg_nfc: String = seg.nfc().collect();
    let mut regex = String::from("^");
    let mut literal = String::new();
    let mut has_wildcard = false;
    let mut chars = seg_nfc.chars();
    let mut utf8_buf = [0u8; 4];

    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                let next = chars
                    .next()
                    .ok_or_else(|| format!("path の末尾の \\ の後ろに文字がありません: {seg}"))?;
                let s = next.encode_utf8(&mut utf8_buf);
                regex.push_str(&regex::escape(s));
                literal.push(next);
            }
            '*' => {
                has_wildcard = true;
                regex.push_str(".*");
            }
            '?' => {
                has_wildcard = true;
                regex.push('.');
            }
            '{' => {
                has_wildcard = true;
                regex.push_str("(?:");
                let mut closed = false;
                while let Some(inner) = chars.next() {
                    match inner {
                        '\\' => {
                            let next = chars.next().ok_or_else(|| {
                                format!("path の末尾の \\ の後ろに文字がありません: {seg}")
                            })?;
                            let s = next.encode_utf8(&mut utf8_buf);
                            regex.push_str(&regex::escape(s));
                        }
                        '{' => {
                            return Err(format!("path の {{...}} は入れ子にできません: {seg}"));
                        }
                        '}' => {
                            regex.push(')');
                            closed = true;
                            break;
                        }
                        ',' => regex.push('|'),
                        '*' => regex.push_str(".*"),
                        '?' => regex.push('.'),
                        other => {
                            let s = other.encode_utf8(&mut utf8_buf);
                            regex.push_str(&regex::escape(s));
                        }
                    }
                }
                if !closed {
                    return Err(format!("path に閉じられていない {{ があります: {seg}"));
                }
            }
            '}' => {
                return Err(format!("path に対応する {{ が無い }} があります: {seg}"));
            }
            other => {
                let s = other.encode_utf8(&mut utf8_buf);
                regex.push_str(&regex::escape(s));
                literal.push(other);
            }
        }
    }

    regex.push('$');
    let literal_opt = if has_wildcard {
        None
    } else {
        Some(literal.to_lowercase())
    };
    Ok((regex, literal_opt))
}
