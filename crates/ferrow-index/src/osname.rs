//! Lossless conversion between OS file names and the index's raw bytes.
//!
//! Unix: names are arbitrary bytes and stored as-is. Windows: names are
//! UTF-16 and stored as WTF-8 (UTF-8 for valid scalar values, with lone
//! surrogates encoded as their 3-byte UTF-8-style forms), so every name
//! round-trips exactly.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

/// Encode UTF-16 code units as WTF-8 bytes.
pub fn encode_wtf8(units: &[u16], out: &mut Vec<u8>) {
    let mut i = 0;
    while i < units.len() {
        let u = units[i];
        if (0xD800..0xDC00).contains(&u)
            && i + 1 < units.len()
            && (0xDC00..0xE000).contains(&units[i + 1])
        {
            // Valid surrogate pair: encode the scalar value as UTF-8.
            let c = 0x10000 + (((u as u32 - 0xD800) << 10) | (units[i + 1] as u32 - 0xDC00));
            push_wtf8_unit(c, out);
            i += 2;
        } else {
            // BMP char or lone surrogate: WTF-8 encodes both the same way.
            push_wtf8_unit(u as u32, out);
            i += 1;
        }
    }
}

fn push_wtf8_unit(c: u32, out: &mut Vec<u8>) {
    if c < 0x80 {
        out.push(c as u8);
    } else if c < 0x800 {
        out.push(0xC0 | (c >> 6) as u8);
        out.push(0x80 | (c & 0x3F) as u8);
    } else if c < 0x10000 {
        out.push(0xE0 | (c >> 12) as u8);
        out.push(0x80 | ((c >> 6) & 0x3F) as u8);
        out.push(0x80 | (c & 0x3F) as u8);
    } else {
        out.push(0xF0 | (c >> 18) as u8);
        out.push(0x80 | ((c >> 12) & 0x3F) as u8);
        out.push(0x80 | ((c >> 6) & 0x3F) as u8);
        out.push(0x80 | (c & 0x3F) as u8);
    }
}

/// Decode WTF-8 bytes back to UTF-16 code units. Malformed bytes decode to
/// U+FFFD so this never fails; our own encoder's output is always exact.
pub fn decode_wtf8(bytes: &[u8], out: &mut Vec<u16>) {
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let (mut c, len): (u32, usize) = if b < 0x80 {
            (b as u32, 1)
        } else if b >> 5 == 0b110 {
            (b as u32 & 0x1F, 2)
        } else if b >> 4 == 0b1110 {
            (b as u32 & 0x0F, 3)
        } else if b >> 3 == 0b11110 {
            (b as u32 & 0x07, 4)
        } else {
            out.push(0xFFFD);
            i += 1;
            continue;
        };
        if i + len > bytes.len() || bytes[i + 1..i + len].iter().any(|&x| x >> 6 != 0b10) {
            out.push(0xFFFD);
            i += 1;
            continue;
        }
        for &x in &bytes[i + 1..i + len] {
            c = (c << 6) | (x as u32 & 0x3F);
        }
        i += len;
        if c < 0x10000 {
            // Includes lone surrogates, which is the point of WTF-8.
            out.push(c as u16);
        } else if c <= 0x10FFFF {
            let c = c - 0x10000;
            out.push(0xD800 + (c >> 10) as u16);
            out.push(0xDC00 + (c & 0x3FF) as u16);
        } else {
            out.push(0xFFFD);
        }
    }
}

/// Raw index bytes for an OS file name.
pub fn name_to_bytes(name: &OsStr, out: &mut Vec<u8>) {
    out.clear();
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        out.extend_from_slice(name.as_bytes());
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let units: Vec<u16> = name.encode_wide().collect();
        encode_wtf8(&units, out);
    }
    #[cfg(not(any(unix, windows)))]
    {
        out.extend_from_slice(name.to_string_lossy().as_bytes());
    }
}

/// Rebuild an `OsString` from raw index bytes.
pub fn bytes_to_os(bytes: &[u8]) -> OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(bytes.to_vec())
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        let mut units = Vec::new();
        decode_wtf8(bytes, &mut units);
        OsString::from_wide(&units)
    }
    #[cfg(not(any(unix, windows)))]
    {
        OsString::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

/// Rebuild a path from raw index bytes (already separator-joined).
pub fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    PathBuf::from(bytes_to_os(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wtf8_round_trip_valid() {
        let units: Vec<u16> = "Report Ä 2024 😀.txt".encode_utf16().collect();
        let mut bytes = Vec::new();
        encode_wtf8(&units, &mut bytes);
        // Valid UTF-16 encodes as plain UTF-8.
        assert_eq!(bytes, "Report Ä 2024 😀.txt".as_bytes());
        let mut back = Vec::new();
        decode_wtf8(&bytes, &mut back);
        assert_eq!(back, units);
    }

    #[test]
    fn wtf8_round_trip_lone_surrogates() {
        // Lone high surrogate, ASCII, lone low surrogate, valid pair.
        let units: Vec<u16> = vec![0xD83D, b'a' as u16, 0xDE00, 0xD83D, 0xDE00, 0xD800];
        let mut bytes = Vec::new();
        encode_wtf8(&units, &mut bytes);
        // Lone surrogates make it invalid UTF-8 (that is expected for WTF-8).
        assert!(std::str::from_utf8(&bytes).is_err());
        let mut back = Vec::new();
        decode_wtf8(&bytes, &mut back);
        assert_eq!(back, units);
    }

    #[test]
    fn wtf8_decode_malformed_never_panics() {
        let mut out = Vec::new();
        decode_wtf8(&[0xFF, 0xC3, b'a', 0xE0, 0x80], &mut out);
        assert!(!out.is_empty());
    }

    #[test]
    fn os_name_round_trip() {
        let name = OsString::from("Ünïcode ファイル.txt");
        let mut bytes = Vec::new();
        name_to_bytes(&name, &mut bytes);
        assert_eq!(bytes_to_os(&bytes), name);
    }
}
