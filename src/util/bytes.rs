//! Small byte-slice helpers shared across modules.

use std::fmt::Write as _;

/// Length of the longest common prefix of `a` and `b`.
///
/// Drives prefix compression inside data blocks, where each key stores only the
/// bytes that differ from its predecessor.
pub fn common_prefix_len(a: &[u8], b: &[u8]) -> usize {
    let max = a.len().min(b.len());
    let mut i = 0;
    while i < max && a[i] == b[i] {
        i += 1;
    }
    i
}

/// Renders bytes as a lowercase hex string. For diagnostics and error context;
/// not on any hot path.
pub fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        // Writing to a String is infallible.
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Renders bytes as an ASCII string with non-printable bytes shown as `\xNN`.
/// Handy for logging keys that are usually, but not always, text.
pub fn escape(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len());
    for &b in bytes {
        if (0x20..0x7f).contains(&b) && b != b'\\' {
            s.push(b as char);
        } else {
            let _ = write!(s, "\\x{b:02x}");
        }
    }
    s
}

/// Truncates a byte slice to at most `max` bytes for display, appending an
/// ellipsis marker when truncation happened.
pub fn preview(bytes: &[u8], max: usize) -> String {
    if bytes.len() <= max {
        escape(bytes)
    } else {
        format!("{}… (+{} bytes)", escape(&bytes[..max]), bytes.len() - max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_len_basics() {
        assert_eq!(common_prefix_len(b"", b"abc"), 0);
        assert_eq!(common_prefix_len(b"abc", b"abd"), 2);
        assert_eq!(common_prefix_len(b"abc", b"abc"), 3);
        assert_eq!(common_prefix_len(b"abcx", b"abc"), 3);
    }

    #[test]
    fn hex_and_escape() {
        assert_eq!(to_hex(&[0x00, 0xff, 0x10]), "00ff10");
        assert_eq!(escape(b"ab\x00c"), "ab\\x00c");
        assert_eq!(escape(b"plain"), "plain");
    }

    #[test]
    fn preview_truncates() {
        let p = preview(&[b'a'; 100], 4);
        assert!(p.contains("+96 bytes"));
        assert_eq!(preview(b"hi", 4), "hi");
    }
}
