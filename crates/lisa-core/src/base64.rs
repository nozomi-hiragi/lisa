//! 標準の base64（画面の撮影と端末間の小分け転送で使う）。

const BASE64_TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// `len` バイトを base64 にしたときの文字数。
pub const fn encoded_len(len: usize) -> usize {
    len.div_ceil(3) * 4
}

/// バイト列を標準の base64 文字列にする。
pub fn encode(bytes: &[u8]) -> String {
    let mut out = Vec::with_capacity(encoded_len(bytes.len()));
    let (chunks, rem) = bytes.as_chunks::<3>();
    for c in chunks {
        let b0 = u32::from(c[0]);
        let b1 = u32::from(c[1]);
        let b2 = u32::from(c[2]);
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(BASE64_TABLE[((n >> 18) & 0x3f) as usize]);
        out.push(BASE64_TABLE[((n >> 12) & 0x3f) as usize]);
        out.push(BASE64_TABLE[((n >> 6) & 0x3f) as usize]);
        out.push(BASE64_TABLE[(n & 0x3f) as usize]);
    }
    if rem.len() == 1 {
        let n = u32::from(rem[0]) << 16;
        out.push(BASE64_TABLE[((n >> 18) & 0x3f) as usize]);
        out.push(BASE64_TABLE[((n >> 12) & 0x3f) as usize]);
        out.push(b'=');
        out.push(b'=');
    } else if rem.len() == 2 {
        let n = (u32::from(rem[0]) << 16) | (u32::from(rem[1]) << 8);
        out.push(BASE64_TABLE[((n >> 18) & 0x3f) as usize]);
        out.push(BASE64_TABLE[((n >> 12) & 0x3f) as usize]);
        out.push(BASE64_TABLE[((n >> 6) & 0x3f) as usize]);
        out.push(b'=');
    }
    // SAFETY: BASE64_TABLE と '=' はすべて ASCII。
    unsafe { String::from_utf8_unchecked(out) }
}

/// 標準の base64 文字列をバイト列に戻す。
pub fn decode(text: &str) -> Result<Vec<u8>, String> {
    let cleaned: Vec<u8> = text.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if !cleaned.len().is_multiple_of(4) {
        return Err("base64 の長さが 4 の倍数ではありません".into());
    }
    let val = |b: u8| -> Result<u32, String> {
        match b {
            b'A'..=b'Z' => Ok(u32::from(b - b'A')),
            b'a'..=b'z' => Ok(u32::from(b - b'a') + 26),
            b'0'..=b'9' => Ok(u32::from(b - b'0') + 52),
            b'+' => Ok(62),
            b'/' => Ok(63),
            _ => Err(format!("base64 に使えない文字があります: {}", b as char)),
        }
    };
    let mut out = Vec::with_capacity(cleaned.len() / 4 * 3);
    let chunks = cleaned.as_chunks::<4>().0;
    for (idx, chunk) in chunks.iter().enumerate() {
        let is_last = idx + 1 == cleaned.len() / 4;
        if (chunk[0] == b'=' || chunk[1] == b'=')
            || (!is_last && (chunk[2] == b'=' || chunk[3] == b'='))
            || (chunk[2] == b'=' && chunk[3] != b'=')
        {
            return Err("base64 のパディングが不正です".into());
        }
        let v0 = val(chunk[0])?;
        let v1 = val(chunk[1])?;
        let v2 = if chunk[2] == b'=' { 0 } else { val(chunk[2])? };
        let v3 = if chunk[3] == b'=' { 0 } else { val(chunk[3])? };
        let n = (v0 << 18) | (v1 << 12) | (v2 << 6) | v3;
        out.push(((n >> 16) & 0xff) as u8);
        if chunk[2] != b'=' {
            out.push(((n >> 8) & 0xff) as u8);
        }
        if chunk[3] != b'=' {
            out.push((n & 0xff) as u8);
        }
    }
    Ok(out)
}
