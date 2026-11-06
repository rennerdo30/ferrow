//! Versioned binary persistence for [`crate::Index`].
//!
//! Layout (all integers little-endian):
//!
//! | Field | Type | Notes |
//! |---|---|---|
//! | magic | `[u8; 8]` | `FERROWID` |
//! | version | `u32` | currently 1 |
//! | entry_count | `u32` | |
//! | arena_len | `u32` | both arenas have this length |
//! | checksum | `u32` | CRC-32 (IEEE) over everything after this field |
//! | parent | `u32` × n | |
//! | name_off | `u32` × n | |
//! | name_len | `u16` × n | |
//! | flags | `u16` × n | |
//! | arena | `u8` × arena_len | |
//! | folded_arena | `u8` × arena_len | |
//! | sorted | `u32` × n | |
//!
//! Loading reads the file in one sequential read, checks magic, version
//! and checksum, validates every offset, and never panics on bad input.

use std::fmt;
use std::io::Write;
use std::path::Path;

use crate::{Index, OwnedColumns, ROOT_PARENT};

pub const MAGIC: [u8; 8] = *b"FERROWID";
pub const VERSION: u32 = 1;
const HEADER_LEN: usize = 8 + 4 + 4 + 4 + 4;

#[derive(Debug)]
pub enum SaveError {
    Io(std::io::Error),
    TooLarge(&'static str),
}

impl fmt::Display for SaveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SaveError::Io(e) => write!(f, "writing index: {e}"),
            SaveError::TooLarge(what) => write!(f, "index too large to save: {what}"),
        }
    }
}
impl std::error::Error for SaveError {}

#[derive(Debug)]
pub enum LoadError {
    Io(std::io::Error),
    WrongMagic,
    UnsupportedVersion(u32),
    Truncated,
    ChecksumMismatch,
    Corrupt(&'static str),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::Io(e) => write!(f, "reading index: {e}"),
            LoadError::WrongMagic => write!(f, "not a Ferrow index file (bad magic)"),
            LoadError::UnsupportedVersion(v) => {
                write!(
                    f,
                    "unsupported index version {v} (this build reads {VERSION})"
                )
            }
            LoadError::Truncated => write!(f, "index file is truncated"),
            LoadError::ChecksumMismatch => write!(f, "index file checksum mismatch (corrupt)"),
            LoadError::Corrupt(what) => write!(f, "index file is corrupt: {what}"),
        }
    }
}
impl std::error::Error for LoadError {}

// CRC-32 (IEEE 802.3, reflected) with a const-generated table.
const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB88320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c = CRC_TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    !c
}

pub(crate) fn save(index: &Index, path: &Path) -> Result<(), SaveError> {
    let c = index.columns();
    let n = c.parent.len();
    let n32 = u32::try_from(n).map_err(|_| SaveError::TooLarge("entry count"))?;
    let arena32 = u32::try_from(c.arena.len()).map_err(|_| SaveError::TooLarge("name arena"))?;

    let payload_len = n * (4 + 4 + 2 + 2 + 4) + c.arena.len() * 2;
    let mut buf = Vec::with_capacity(HEADER_LEN + payload_len);
    buf.extend_from_slice(&MAGIC);
    buf.extend_from_slice(&VERSION.to_le_bytes());
    buf.extend_from_slice(&n32.to_le_bytes());
    buf.extend_from_slice(&arena32.to_le_bytes());
    let checksum_at = buf.len();
    buf.extend_from_slice(&0u32.to_le_bytes()); // patched below

    for v in c.parent {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    for v in c.name_off {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    for v in c.name_len {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    for v in c.flags {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    buf.extend_from_slice(c.arena);
    buf.extend_from_slice(c.folded_arena);
    for v in c.sorted {
        buf.extend_from_slice(&v.to_le_bytes());
    }

    let sum = crc32(&buf[HEADER_LEN..]);
    buf[checksum_at..checksum_at + 4].copy_from_slice(&sum.to_le_bytes());

    // Write via a temp file in the same directory, then rename, so a crash
    // never leaves a half-written index at the target path.
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::File::create(&tmp).map_err(SaveError::Io)?;
        f.write_all(&buf).map_err(SaveError::Io)?;
        f.sync_all().map_err(SaveError::Io)?;
    }
    std::fs::rename(&tmp, path).map_err(SaveError::Io)?;
    Ok(())
}

fn take<'a>(data: &'a [u8], at: &mut usize, len: usize) -> Result<&'a [u8], LoadError> {
    let end = at
        .checked_add(len)
        .ok_or(LoadError::Corrupt("size overflow"))?;
    if end > data.len() {
        return Err(LoadError::Truncated);
    }
    let s = &data[*at..end];
    *at = end;
    Ok(s)
}

fn read_u32s(data: &[u8], at: &mut usize, n: usize) -> Result<Vec<u32>, LoadError> {
    let bytes = take(data, at, n.saturating_mul(4))?;
    Ok(bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| u32::from_le_bytes(*c))
        .collect())
}

fn read_u16s(data: &[u8], at: &mut usize, n: usize) -> Result<Vec<u16>, LoadError> {
    let bytes = take(data, at, n.saturating_mul(2))?;
    Ok(bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect())
}

pub(crate) fn load(path: &Path) -> Result<Index, LoadError> {
    let data = std::fs::read(path).map_err(LoadError::Io)?;
    from_bytes(&data)
}

/// Parse an index from bytes (also used by tests and fuzz-style checks).
pub fn from_bytes(data: &[u8]) -> Result<Index, LoadError> {
    if data.len() < HEADER_LEN {
        // Too short for a header: bad magic if the prefix disagrees, else truncated.
        if data.len() >= MAGIC.len() && data[..MAGIC.len()] != MAGIC {
            return Err(LoadError::WrongMagic);
        }
        return Err(LoadError::Truncated);
    }
    if data[..8] != MAGIC {
        return Err(LoadError::WrongMagic);
    }
    let u32at =
        |at: usize| u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]);
    let version = u32at(8);
    if version != VERSION {
        return Err(LoadError::UnsupportedVersion(version));
    }
    let n = u32at(12) as usize;
    let arena_len = u32at(16) as usize;
    let stored_sum = u32at(20);

    let expected = HEADER_LEN
        .checked_add(n.saturating_mul(16))
        .and_then(|v| v.checked_add(arena_len.saturating_mul(2)))
        .ok_or(LoadError::Corrupt("size overflow"))?;
    if data.len() < expected {
        return Err(LoadError::Truncated);
    }
    if crc32(&data[HEADER_LEN..expected]) != stored_sum {
        return Err(LoadError::ChecksumMismatch);
    }

    let mut at = HEADER_LEN;
    let parent = read_u32s(data, &mut at, n)?;
    let name_off = read_u32s(data, &mut at, n)?;
    let name_len = read_u16s(data, &mut at, n)?;
    let ent_flags = read_u16s(data, &mut at, n)?;
    let arena = take(data, &mut at, arena_len)?.to_vec();
    let folded_arena = take(data, &mut at, arena_len)?.to_vec();
    let sorted = read_u32s(data, &mut at, n)?;

    // Validate so later lookups can never panic on a crafted file.
    for i in 0..n {
        if parent[i] != ROOT_PARENT && parent[i] as usize >= n {
            return Err(LoadError::Corrupt("parent id out of range"));
        }
        let end = name_off[i] as usize + name_len[i] as usize;
        if end > arena_len {
            return Err(LoadError::Corrupt("name offset out of range"));
        }
        if sorted[i] as usize >= n {
            return Err(LoadError::Corrupt("sort order id out of range"));
        }
    }
    // Reject parent cycles, which would loop forever in path rebuild:
    // every chain must reach a root within n steps. O(n * depth) worst
    // case is too slow for big files, so walk with a visiting mark.
    let mut state = vec![0u8; n]; // 0 = unseen, 1 = on current walk, 2 = done
    for start in 0..n {
        if state[start] != 0 {
            continue;
        }
        let mut cur = start;
        let mut walk = Vec::new();
        loop {
            if state[cur] == 1 {
                return Err(LoadError::Corrupt("parent cycle"));
            }
            if state[cur] == 2 || parent[cur] == ROOT_PARENT {
                if state[cur] == 0 {
                    state[cur] = 2;
                }
                break;
            }
            state[cur] = 1;
            walk.push(cur);
            cur = parent[cur] as usize;
        }
        for id in walk {
            state[id] = 2;
        }
    }

    Ok(Index::from_columns(OwnedColumns {
        parent,
        name_off,
        name_len,
        flags: ent_flags,
        arena,
        folded_arena,
        sorted,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{IndexBuilder, flags};

    fn sample_bytes() -> Vec<u8> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let b = IndexBuilder::new();
        let root = b.add_root(b"root", flags::DIR);
        b.add(root, b"hello.txt", 0);
        b.add(root, "Ärger.log".as_bytes(), 0);
        let dir = std::env::temp_dir().join(format!(
            "ferrow-persist-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let idx = b.freeze();
        idx.save(&dir).unwrap();
        let bytes = std::fs::read(&dir).unwrap();
        let _ = std::fs::remove_file(&dir);
        bytes
    }

    #[test]
    fn round_trip() {
        let bytes = sample_bytes();
        let idx = from_bytes(&bytes).unwrap();
        assert_eq!(idx.len(), 3);
        assert_eq!(idx.name(1), b"hello.txt");
        assert_eq!(idx.parent(1), 0);
        let mut buf = Vec::new();
        idx.path_bytes(2, &mut buf);
        assert!(buf.ends_with("Ärger.log".as_bytes()));
    }

    #[test]
    fn rejects_wrong_magic() {
        let mut bytes = sample_bytes();
        bytes[0] = b'X';
        assert!(matches!(from_bytes(&bytes), Err(LoadError::WrongMagic)));
    }

    #[test]
    fn rejects_wrong_version() {
        let mut bytes = sample_bytes();
        bytes[8..12].copy_from_slice(&999u32.to_le_bytes());
        assert!(matches!(
            from_bytes(&bytes),
            Err(LoadError::UnsupportedVersion(999))
        ));
    }

    #[test]
    fn rejects_truncated() {
        let bytes = sample_bytes();
        assert!(matches!(
            from_bytes(&bytes[..bytes.len() - 1]),
            Err(LoadError::Truncated)
        ));
        assert!(matches!(from_bytes(&[]), Err(LoadError::Truncated)));
        assert!(matches!(from_bytes(b"FER"), Err(LoadError::Truncated)));
    }

    #[test]
    fn rejects_corrupt_payload() {
        let mut bytes = sample_bytes();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        assert!(matches!(
            from_bytes(&bytes),
            Err(LoadError::ChecksumMismatch)
        ));
    }

    #[test]
    fn rejects_parent_cycle_even_with_valid_checksum() {
        let dir = std::env::temp_dir().join(format!("ferrow-cycle-test-{}", std::process::id()));
        let b = IndexBuilder::new();
        let root = b.add_root(b"root", flags::DIR);
        b.add(root, b"a", 0);
        let idx = b.freeze();
        idx.save(&dir).unwrap();
        let mut bytes = std::fs::read(&dir).unwrap();
        let _ = std::fs::remove_file(&dir);
        // Point entry 1's parent at itself (parent column starts at HEADER_LEN).
        bytes[HEADER_LEN + 4..HEADER_LEN + 8].copy_from_slice(&1u32.to_le_bytes());
        let sum = crc32(&bytes[HEADER_LEN..]);
        bytes[20..24].copy_from_slice(&sum.to_le_bytes());
        assert!(matches!(from_bytes(&bytes), Err(LoadError::Corrupt(_))));
    }

    #[test]
    fn crc32_known_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF43926);
    }
}
