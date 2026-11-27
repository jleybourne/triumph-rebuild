//! Printable strings in an image (ASCII and UTF-16LE), and the two shapes the run-time lookup census
//! cares about: DLL names and API-like identifiers. Names only; nothing here keeps bytes.

/// One string found in a buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// Byte offset of the first character.
    pub offset: usize,
    /// UTF-16LE (true) or ASCII (false).
    pub wide: bool,
    pub text: String,
}

fn printable(c: u16) -> bool {
    (0x20..0x7f).contains(&c)
}

/// NUL-terminated runs of at least `min` printable ASCII characters.
pub fn ascii(data: &[u8], min: usize) -> Vec<Found> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, &b) in data.iter().enumerate() {
        if printable(b as u16) {
            start.get_or_insert(i);
            continue;
        }
        if let Some(s) = start.take() {
            if b == 0 && i - s >= min {
                out.push(Found { offset: s, wide: false, text: String::from_utf8_lossy(&data[s..i]).into_owned() });
            }
        }
    }
    out
}

/// NUL-terminated runs of at least `min` printable characters in UTF-16LE (ASCII range only), at both
/// byte alignments. A string at one alignment never reads as printable at the other.
pub fn utf16(data: &[u8], min: usize) -> Vec<Found> {
    let mut out = Vec::new();
    for align in 0..2 {
        let mut start = None;
        let mut text = String::new();
        let mut i = align;
        while i + 1 < data.len() {
            let c = u16::from_le_bytes([data[i], data[i + 1]]);
            if printable(c) {
                start.get_or_insert(i);
                text.push(c as u8 as char);
            } else {
                if let Some(s) = start.take() {
                    if c == 0 && text.len() >= min {
                        out.push(Found { offset: s, wide: true, text: std::mem::take(&mut text) });
                    }
                }
                text.clear();
            }
            i += 2;
        }
    }
    out.sort_by_key(|f| f.offset);
    out
}

/// The lowercase file name if `s` names a DLL (`xgameruntime.dll`, `C:\x\Foo.DLL`); `None` otherwise.
pub fn dll_name(s: &str) -> Option<String> {
    let base = s.rsplit(['\\', '/']).next()?;
    let lower = base.to_ascii_lowercase();
    let stem = lower.strip_suffix(".dll")?;
    let ok = !stem.is_empty()
        && stem.len() <= 64
        && stem.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.');
    ok.then_some(lower)
}

/// Looks like an exported C function name: an identifier of 4-80 characters that starts with a
/// letter, has both cases (or is a `snake_case` name with a lowercase prefix like `curl_`), and is not
/// all one word of lowercase letters.
pub fn api_like(s: &str) -> bool {
    let b = s.as_bytes();
    if !(4..=80).contains(&b.len()) || !b[0].is_ascii_alphabetic() {
        return false;
    }
    if !b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_') {
        return false;
    }
    let upper = b.iter().filter(|c| c.is_ascii_uppercase()).count();
    let lower = b.iter().filter(|c| c.is_ascii_lowercase()).count();
    if b[0].is_ascii_uppercase() {
        // CamelCase: at least two capitals and some lowercase (GetProcAddress, XGameGetXboxTitleId)
        upper >= 2 && lower >= 2
    } else {
        // lowercase_with_underscores (curl_easy_init) or lowerCamel (ffxCreateContext)
        lower >= 3 && (b.contains(&b'_') || upper >= 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_ascii_and_wide() {
        let mut d = b"\x01QueryApiImpl\0\x01".to_vec(); // the wide string starts at 15: odd
        for c in "xgameruntime.dll".encode_utf16() {
            d.extend_from_slice(&c.to_le_bytes());
        }
        d.extend_from_slice(&[0, 0, 0xff]); // the next starts at 50: even
        for c in "XUserAddAsync".encode_utf16() {
            d.extend_from_slice(&c.to_le_bytes());
        }
        d.extend_from_slice(&[0, 0]);
        let a: Vec<_> = ascii(&d, 4).into_iter().map(|f| f.text).collect();
        assert_eq!(a, ["QueryApiImpl"]);
        let w: Vec<_> = utf16(&d, 4).into_iter().map(|f| (f.offset, f.text)).collect();
        assert_eq!(w, [(15, "xgameruntime.dll".to_string()), (50, "XUserAddAsync".to_string())]);
    }

    #[test]
    fn unterminated_runs_are_not_strings() {
        assert!(ascii(b"GetProcAddress\x01", 4).is_empty());
        assert!(utf16(&[b'A', 0, b'B', 0, b'C', 0, b'D', 0, 1, 1], 4).is_empty());
    }

    #[test]
    fn shapes() {
        assert_eq!(dll_name("XGameRuntime.DLL").as_deref(), Some("xgameruntime.dll"));
        assert_eq!(dll_name(r"C:\Windows\System32\kernel32.dll").as_deref(), Some("kernel32.dll"));
        assert_eq!(dll_name("api-ms-win-core-synch-l1-2-0.dll").as_deref(), Some("api-ms-win-core-synch-l1-2-0.dll"));
        assert_eq!(dll_name("not a dll.dll"), None);
        assert_eq!(dll_name(".dll"), None);
        for yes in ["QueryApiImpl", "XGameRuntimeInitialize", "curl_easy_init", "ffxCreateContext", "SetThreadDescription"] {
            assert!(api_like(yes), "{yes}");
        }
        for no in ["hello", "HELLO_WORLD", "Hello", "a b c", "GetProcAddress()", "x", "%s.dll"] {
            assert!(!api_like(no), "{no}");
        }
    }
}
