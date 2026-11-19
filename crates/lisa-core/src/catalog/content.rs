//! ファイルの中身の条件（`content`、`content_bytes`、`hash`、`size`、`head`）。

use std::collections::BTreeMap;

use regex::Regex;

/// `head` を省略したときの既定の長さ（64 KiB）。
pub const DEFAULT_HEAD_LEN: usize = 64 * 1024;
/// `head` に指定できる上限（1 MiB）。
pub const MAX_HEAD_LEN: usize = 1024 * 1024;

/// 中身の正規表現で取った名前つきの値。
pub type Captures = BTreeMap<String, String>;

/// 評価に使うファイルの情報（ファイル自体は読まない）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileFacts {
    pub size: u64,
    pub hash: Option<[u8; 32]>,
    pub cloud_only: bool,
}

/// 中身の条件を調べた結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    Hit(Captures),
    Miss,
    Unknown,
}

/// ファイルの大きさの条件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SizeCond {
    Less(u64),
    LessEq(u64),
    Greater(u64),
    GreaterEq(u64),
    /// 両端を含む範囲（`min..=max`）。
    Range(u64, u64),
}

impl SizeCond {
    fn matches(self, size: u64) -> bool {
        match self {
            SizeCond::Less(limit) => size < limit,
            SizeCond::LessEq(limit) => size <= limit,
            SizeCond::Greater(limit) => size > limit,
            SizeCond::GreaterEq(limit) => size >= limit,
            SizeCond::Range(min, max) => size >= min && size <= max,
        }
    }

    fn canonical(self) -> String {
        match self {
            SizeCond::Less(v) => format!("<{v}"),
            SizeCond::LessEq(v) => format!("<={v}"),
            SizeCond::Greater(v) => format!(">{v}"),
            SizeCond::GreaterEq(v) => format!(">={v}"),
            SizeCond::Range(a, b) => format!("{a}..{b}"),
        }
    }
}

/// 1 つの `match` または `exclude` に付ける中身の条件。
#[derive(Debug, Clone)]
pub struct Content {
    regex: Option<(String, Regex)>,
    prefix_bytes: Option<Vec<u8>>,
    hash: Option<[u8; 32]>,
    size: Option<SizeCond>,
    head_len: usize,
}

impl PartialEq for Content {
    fn eq(&self, other: &Self) -> bool {
        self.regex.as_ref().map(|(s, _)| s) == other.regex.as_ref().map(|(s, _)| s)
            && self.prefix_bytes == other.prefix_bytes
            && self.hash == other.hash
            && self.size == other.size
            && self.head_len == other.head_len
    }
}

impl Eq for Content {}

impl Content {
    /// TOML の各キーから中身の条件を組み立てる。
    pub(crate) fn parse(
        content: Option<&str>,
        content_bytes: Option<&str>,
        hash: Option<&str>,
        size: Option<&str>,
        head: Option<&str>,
    ) -> Result<Option<Self>, String> {
        if content.is_none()
            && content_bytes.is_none()
            && hash.is_none()
            && size.is_none()
            && head.is_none()
        {
            return Ok(None);
        }

        let head_len = match head {
            Some(s) => {
                let bytes = parse_byte_size(s, "head")?;
                if bytes > MAX_HEAD_LEN as u64 {
                    return Err(format!("head は 1MiB 以下にしてください: {s}"));
                }
                bytes as usize
            }
            None => DEFAULT_HEAD_LEN,
        };

        let regex = match content {
            Some(s) => {
                let Some(pattern) = s.strip_prefix("re:") else {
                    return Err(format!("content は \"re:\" で始めてください: {s}"));
                };
                let compiled = Regex::new(pattern)
                    .map_err(|e| format!("content の正規表現が不正です: {e}"))?;
                Some((pattern.to_string(), compiled))
            }
            None => None,
        };

        let prefix_bytes = match content_bytes {
            Some(s) => {
                let Some(hex_str) = s.strip_prefix("hex:") else {
                    return Err(format!("content_bytes は \"hex:\" で始めてください: {s}"));
                };
                if hex_str.is_empty() {
                    return Err("content_bytes の hex: の後ろが空です".to_string());
                }
                Some(parse_hex_bytes(hex_str, "content_bytes")?)
            }
            None => None,
        };

        let hash_bytes = match hash {
            Some(s) => {
                let Some(hex_str) = s.strip_prefix("blake3:") else {
                    return Err(format!("hash は \"blake3:\" で始めてください: {s}"));
                };
                if hex_str.len() != 64 {
                    return Err(format!("hash は 64 桁の 16 進で書いてください: {s}"));
                }
                let raw = parse_hex_bytes(hex_str, "hash")?;
                let arr: [u8; 32] = raw
                    .try_into()
                    .map_err(|_| "hash は 32 バイトで書いてください".to_string())?;
                Some(arr)
            }
            None => None,
        };

        let size_cond = match size {
            Some(s) => Some(parse_size_cond(s)?),
            None => None,
        };

        Ok(Some(Content {
            regex,
            prefix_bytes,
            hash: hash_bytes,
            size: size_cond,
            head_len,
        }))
    }

    /// `content` または `content_bytes`（先頭バイトの読み取り）が要るか。
    pub fn needs_bytes(&self) -> bool {
        self.regex.is_some() || self.prefix_bytes.is_some()
    }

    /// 先頭から読む長さ（バイト）。
    pub fn head_len(&self) -> usize {
        self.head_len
    }

    /// 条件を一意に表す文字列（DB のキー用に blake3 する元）。
    pub fn rule_key(&self) -> String {
        let mut parts = Vec::new();
        if self.needs_bytes() {
            parts.push(format!("head={}", self.head_len));
        }
        if let Some((pat, _)) = &self.regex {
            parts.push(format!("c={}:{}", pat.len(), pat));
        }
        if let Some(bytes) = &self.prefix_bytes {
            parts.push(format!("b={}", to_hex(bytes)));
        }
        if let Some(h) = &self.hash {
            parts.push(format!("h={}", to_hex(h)));
        }
        if let Some(s) = &self.size {
            parts.push(format!("s={}", s.canonical()));
        }
        parts.join("\n")
    }

    /// 中身を見て当たりか。`bytes` は先頭 `head_len` まで（それより短ければファイル全体）。
    /// `size` と `hash` は `FileFacts` から見る。
    pub fn check(&self, facts: &FileFacts, bytes: Option<&[u8]>) -> Check {
        if let Some(size_cond) = self.size
            && !size_cond.matches(facts.size)
        {
            return Check::Miss;
        }

        let mut any_unknown = false;

        if let Some(expected_hash) = &self.hash {
            match &facts.hash {
                Some(actual) if actual != expected_hash => return Check::Miss,
                Some(_) => {}
                None => any_unknown = true,
            }
        }

        let mut captures = Captures::new();
        if self.needs_bytes() {
            let Some(raw) = bytes else {
                return Check::Unknown;
            };
            let head = &raw[..raw.len().min(self.head_len)];

            if let Some(prefix) = &self.prefix_bytes
                && !head.starts_with(prefix)
            {
                return Check::Miss;
            }

            if let Some((_, re)) = &self.regex {
                let text = String::from_utf8_lossy(head);
                let Some(caps) = re.captures(&text) else {
                    return Check::Miss;
                };
                for name in re.capture_names().flatten() {
                    if let Some(m) = caps.name(name) {
                        captures.insert(name.to_string(), m.as_str().to_string());
                    }
                }
            }
        }

        if any_unknown {
            Check::Unknown
        } else {
            Check::Hit(captures)
        }
    }
}

fn parse_size_cond(text: &str) -> Result<SizeCond, String> {
    let s = text.trim();
    if let Some(rest) = s.strip_prefix("<=") {
        return Ok(SizeCond::LessEq(parse_byte_size(rest, "size")?));
    }
    if let Some(rest) = s.strip_prefix('<') {
        return Ok(SizeCond::Less(parse_byte_size(rest, "size")?));
    }
    if let Some(rest) = s.strip_prefix(">=") {
        return Ok(SizeCond::GreaterEq(parse_byte_size(rest, "size")?));
    }
    if let Some(rest) = s.strip_prefix('>') {
        return Ok(SizeCond::Greater(parse_byte_size(rest, "size")?));
    }
    if let Some((left, right)) = s.split_once("..=").or_else(|| s.split_once("..")) {
        let min = parse_byte_size(left, "size")?;
        let max = parse_byte_size(right, "size")?;
        if min > max {
            return Err(format!("size の範囲が逆です: {text}"));
        }
        return Ok(SizeCond::Range(min, max));
    }
    Err(format!(
        "size は <、>、または a..b の形で書いてください: {text}"
    ))
}

fn parse_byte_size(text: &str, key: &str) -> Result<u64, String> {
    let s = text.trim();
    let digit_end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    if digit_end == 0 {
        return Err(format!("{key} の数字が読めません: {text}"));
    }
    let num: u64 = s[..digit_end]
        .parse()
        .map_err(|_| format!("{key} の数字が大きすぎます: {text}"))?;
    let unit = s[digit_end..].trim();
    let mult: u64 = match unit {
        "" | "B" => 1,
        "KiB" => 1024,
        "MiB" => 1024 * 1024,
        "GiB" => 1024 * 1024 * 1024,
        "TiB" => 1024_u64 * 1024 * 1024 * 1024,
        "KB" => 1000,
        "MB" => 1_000_000,
        "GB" => 1_000_000_000,
        "TB" => 1_000_000_000_000,
        _ => return Err(format!("{key} の単位が不正です: {text}")),
    };
    num.checked_mul(mult)
        .ok_or_else(|| format!("{key} の値が大きすぎます: {text}"))
}

fn parse_hex_bytes(hex: &str, key: &str) -> Result<Vec<u8>, String> {
    if !hex.len().is_multiple_of(2) {
        return Err(format!("{key} の 16 進は偶数桁にしてください"));
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    for i in (0..hex.len()).step_by(2) {
        let byte = u8::from_str_radix(&hex[i..i + 2], 16)
            .map_err(|_| format!("{key} に 16 進でない文字があります"))?;
        out.push(byte);
    }
    Ok(out)
}

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}
