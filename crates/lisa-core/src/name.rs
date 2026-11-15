//! 名前の扱い。ディスク上の名前はそのままのバイト列で残し、並べ替えとハッシュには NFC を使う。

use std::cmp::Ordering;
use std::ffi::OsStr;

use unicode_normalization::UnicodeNormalization;

/// ディスク上の名前のバイト列。Mac（Unix）はそのまま、Windows は UTF-16 を little endian で並べたもの。
/// 不正な UTF-8 や、対になっていない UTF-16 でも、そのまま残せる。
pub fn raw_bytes(name: &OsStr) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        name.as_bytes().to_vec()
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        name.encode_wide().flat_map(u16::to_le_bytes).collect()
    }
}

/// NFC に正規化した名前。文字にできないところは U+FFFD になる（表示と並べ替え用）。
pub fn nfc(name: &OsStr) -> String {
    name.to_string_lossy().nfc().collect()
}

/// フォルダのハッシュと同じ並び。NFC の UTF-8 のバイト順で、同じならディスク上のバイト列で決める。
/// 言語の設定によらず、どの端末でも同じ順になる。
pub fn hash_order(a_nfc: &str, a_raw: &[u8], b_nfc: &str, b_raw: &[u8]) -> Ordering {
    a_nfc
        .as_bytes()
        .cmp(b_nfc.as_bytes())
        .then_with(|| a_raw.cmp(b_raw))
}

/// OS が勝手に作るファイル。木にもハッシュにも入れない。
pub fn is_os_junk(name: &OsStr) -> bool {
    matches!(
        name.to_str(),
        Some(".DS_Store" | "Thumbs.db" | "desktop.ini" | "Desktop.ini")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nfd_and_nfc_become_the_same() {
        // 「が」を、か + 濁点（Mac の古い形, NFD）と、1 文字（NFC）で。
        let nfd = OsStr::new("\u{304B}\u{3099}");
        let nfc_name = OsStr::new("\u{304C}");
        assert_eq!(nfc(nfd), nfc(nfc_name));
        assert_ne!(raw_bytes(nfd), raw_bytes(nfc_name));
    }

    #[test]
    fn same_nfc_is_ordered_by_raw_bytes() {
        let nfd = OsStr::new("\u{304B}\u{3099}");
        let nfc_name = OsStr::new("\u{304C}");
        let (a, b) = (nfc(nfd), nfc(nfc_name));
        let order = hash_order(&a, &raw_bytes(nfd), &b, &raw_bytes(nfc_name));
        assert_ne!(order, Ordering::Equal);
        assert_eq!(
            order.reverse(),
            hash_order(&b, &raw_bytes(nfc_name), &a, &raw_bytes(nfd))
        );
    }

    #[test]
    fn order_is_by_bytes_not_locale() {
        // 大文字は小文字より先（バイト順）。
        assert_eq!(hash_order("B", b"B", "a", b"a"), Ordering::Less);
    }

    #[test]
    fn junk() {
        assert!(is_os_junk(OsStr::new(".DS_Store")));
        assert!(!is_os_junk(OsStr::new("DS_Store")));
    }
}
