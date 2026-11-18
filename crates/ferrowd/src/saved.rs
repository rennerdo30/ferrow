//! The live layer's frozen tables, saved beside the index (ADR 0007): what a
//! restart needs to go live without reading the drives again, and what the
//! live layer reads through a memory map instead of private memory.
//!
//! File `live.bin` next to `index.bin` (all integers little-endian):
//!
//! | Field | Type | Notes |
//! |---|---|---|
//! | magic | `[u8; 8]` | `FERROWLV` |
//! | version | `u32` | 1 |
//! | checksum | `u32` | CRC-32 (IEEE) of everything after this field |
//! | index_checksum | `u32` | the checksum field of the `index.bin` it belongs to |
//! | entries | `u32` | that index's entry count |
//! | fingerprint | `u64` | roots, excludes and options it was scanned with |
//! | volume count | `u32`, pad `u32` | |
//! | per volume | | device, serial, journal id and next USN, record size, entries, excluded records, roots, table lengths |
//! | pad to 8 | | |
//! | base_record | `u64` × entries | `(volume << 48) \| record`, `u64::MAX`: none |
//! | children_first | `u32` × (entries + 1), pad to 8 | frozen children by parent |
//! | children_ids | `u32` × children, pad to 8 | |
//! | per volume | | `record_base` `u32` × records, pad; `link_records` `u64` × links; `link_ids` `u32` × links, pad |
//!
//! It is written after its index, through a temporary file renamed over the
//! old one. A crash between the two leaves a table whose `index_checksum`
//! does not match: the next start then scans as before. Every value is
//! checked when it is read, so a damaged file is refused, never trusted.

use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;

use ferrow_index::store::{Col, Plain, map_file};
use memmap2::Mmap;

const MAGIC: [u8; 8] = *b"FERROWLV";
const VERSION: u32 = 1;
const HEADER_LEN: usize = 40;
const CHECKSUM_START: usize = 16;
/// A record with several frozen entries (hard links) in `record_base`: its
/// entries are in the volume's link table.
pub const MULTI: u32 = u32::MAX - 1;
/// Ids at or above this in `record_base` are markers (several, none).
const FIRST_MARKER: u32 = MULTI;

/// One volume's frozen tables and where its change journal was read up to.
pub struct SavedVolume {
    /// `\\?\Volume{…}` device path.
    pub device: String,
    pub serial: u64,
    /// Change journal id and the position the index holds every change up to.
    pub journal: (u64, i64),
    pub record_size: u32,
    /// Frozen entries from this volume (for health).
    pub entries: u64,
    pub excluded: Vec<u64>,
    /// Records of the configured roots on this volume.
    pub roots: Vec<u64>,
    /// Frozen entry of each record (`u32::MAX`: none, `u32::MAX - 1`: several,
    /// in the link table).
    pub record_base: Col<u32>,
    /// `(record, entry)` of records with several names, sorted by record.
    pub link_records: Col<u64>,
    pub link_ids: Col<u32>,
}

/// The tables of one live layer over one frozen index.
pub struct Saved {
    pub index_checksum: u32,
    pub fingerprint: u64,
    /// Frozen id → `(volume << 48) | record`.
    pub base_record: Col<u64>,
    /// Frozen children by parent: `children_ids[first[p]..first[p + 1]]`.
    pub children_first: Col<u32>,
    pub children_ids: Col<u32>,
    pub volumes: Vec<SavedVolume>,
}

/// What a scan or the journal reader knows about one volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeFacts {
    pub device: String,
    pub serial: u64,
    /// Change journal id and the position every saved change is up to;
    /// `None`: the volume has no change journal (it is not followed, and
    /// tables are not saved).
    pub journal: Option<(u64, i64)>,
    pub record_size: u32,
}

/// What [`write`] saves: borrowed from the live layer.
pub struct SavedRef<'a> {
    pub index_checksum: u32,
    pub fingerprint: u64,
    pub base_record: &'a [u64],
    pub children_first: &'a [u32],
    pub children_ids: &'a [u32],
    pub volumes: Vec<SavedVolumeRef<'a>>,
}

pub struct SavedVolumeRef<'a> {
    pub device: &'a str,
    pub serial: u64,
    pub journal: (u64, i64),
    pub record_size: u32,
    pub entries: u64,
    pub excluded: Vec<u64>,
    pub roots: Vec<u64>,
    pub record_base: &'a [u32],
    pub link_records: &'a [u64],
    pub link_ids: &'a [u32],
}

/// Where the tables of the index at `index` are saved.
pub fn path_beside(index: &Path) -> std::path::PathBuf {
    index.with_file_name("live.bin")
}

/// A stable 64-bit FNV-1a hash of what a scan was made with: a different
/// root, exclude or option makes the saved tables useless.
pub fn fingerprint(parts: &[&[u8]]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for &byte in (part.len() as u64).to_le_bytes().iter().chain(part.iter()) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    hash
}

struct Out<W> {
    inner: W,
    crc: crc32fast::Hasher,
    at: u64,
}

impl<W: Write> Out<W> {
    fn bytes(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.inner.write_all(bytes)?;
        self.crc.update(bytes);
        self.at += bytes.len() as u64;
        Ok(())
    }
    fn u32(&mut self, v: u32) -> io::Result<()> {
        self.bytes(&v.to_le_bytes())
    }
    fn u64(&mut self, v: u64) -> io::Result<()> {
        self.bytes(&v.to_le_bytes())
    }
    fn pad(&mut self) -> io::Result<()> {
        let pad = (8 - (self.at % 8) as usize) % 8;
        self.bytes(&[0u8; 8][..pad])
    }
    fn u32s(&mut self, values: &[u32]) -> io::Result<()> {
        for chunk in values.chunks(16384) {
            let mut buf = Vec::with_capacity(chunk.len() * 4);
            for v in chunk {
                buf.extend_from_slice(&v.to_le_bytes());
            }
            self.bytes(&buf)?;
        }
        Ok(())
    }
    fn u64s(&mut self, values: &[u64]) -> io::Result<()> {
        for chunk in values.chunks(8192) {
            let mut buf = Vec::with_capacity(chunk.len() * 8);
            for v in chunk {
                buf.extend_from_slice(&v.to_le_bytes());
            }
            self.bytes(&buf)?;
        }
        Ok(())
    }
}

/// Save `tables` to `path` atomically, streamed through a small buffer.
pub fn write(path: &Path, tables: &SavedRef<'_>) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let tmp = parent.join(format!(
        ".ferrow-live-{}-{:016x}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64)
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&tmp)?;
    let written = (|| -> io::Result<()> {
        let mut head = io::BufWriter::with_capacity(1 << 20, file);
        head.write_all(&MAGIC)?;
        head.write_all(&VERSION.to_le_bytes())?;
        head.write_all(&0u32.to_le_bytes())?; // checksum, patched below
        let mut out = Out {
            inner: head,
            crc: crc32fast::Hasher::new(),
            at: CHECKSUM_START as u64,
        };
        let entries = u32::try_from(tables.base_record.len())
            .map_err(|_| io::Error::other("too many entries"))?;
        out.u32(tables.index_checksum)?;
        out.u32(entries)?;
        out.u64(tables.fingerprint)?;
        out.u32(tables.volumes.len() as u32)?;
        out.u32(0)?;
        for v in &tables.volumes {
            out.u32(v.device.len() as u32)?;
            out.bytes(v.device.as_bytes())?;
            out.u64(v.serial)?;
            out.u64(v.journal.0)?;
            out.u64(v.journal.1 as u64)?;
            out.u32(v.record_size)?;
            out.u64(v.entries)?;
            out.u32(v.excluded.len() as u32)?;
            out.u64s(&v.excluded)?;
            out.u32(v.roots.len() as u32)?;
            out.u64s(&v.roots)?;
            out.u64(v.record_base.len() as u64)?;
            out.u64(v.link_records.len() as u64)?;
        }
        out.pad()?;
        out.u64s(tables.base_record)?;
        out.u32s(tables.children_first)?;
        out.pad()?;
        out.u64(tables.children_ids.len() as u64)?;
        out.u32s(tables.children_ids)?;
        out.pad()?;
        for v in &tables.volumes {
            out.u32s(v.record_base)?;
            out.pad()?;
            out.u64s(v.link_records)?;
            out.u32s(v.link_ids)?;
            out.pad()?;
        }
        let sum = out.crc.finalize();
        let mut file = out.inner.into_inner().map_err(|e| e.into_error())?;
        use std::io::Seek;
        file.seek(io::SeekFrom::Start(12))?;
        file.write_all(&sum.to_le_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// Why saved tables cannot be used (the service scans instead).
#[derive(Debug)]
pub enum Refused {
    Missing,
    Io(io::Error),
    Corrupt(&'static str),
    /// For another index, other roots or options.
    Stale(&'static str),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::Missing => write!(f, "no saved live tables"),
            Refused::Io(e) => write!(f, "reading the saved live tables: {e}"),
            Refused::Corrupt(what) => write!(f, "the saved live tables are damaged: {what}"),
            Refused::Stale(what) => write!(f, "the saved live tables are out of date: {what}"),
        }
    }
}

struct Reader<'a> {
    map: &'a Arc<Mmap>,
    at: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], Refused> {
        let end = self
            .at
            .checked_add(n)
            .filter(|&end| end <= self.map.len())
            .ok_or(Refused::Corrupt("truncated"))?;
        let bytes = &self.map[self.at..end];
        self.at = end;
        Ok(bytes)
    }
    fn u32(&mut self) -> Result<u32, Refused> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("4 bytes"),
        ))
    }
    fn u64(&mut self) -> Result<u64, Refused> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("8 bytes"),
        ))
    }
    fn count(&mut self, limit: u64) -> Result<usize, Refused> {
        let n = self.u64()?;
        if n > limit {
            return Err(Refused::Corrupt("count out of range"));
        }
        usize::try_from(n).map_err(|_| Refused::Corrupt("count out of range"))
    }
    fn pad(&mut self) -> Result<(), Refused> {
        let pad = (8 - self.at % 8) % 8;
        if self.take(pad)?.iter().any(|&b| b != 0) {
            return Err(Refused::Corrupt("nonzero padding"));
        }
        Ok(())
    }
    fn col<T: Plain>(&mut self, n: usize) -> Result<Col<T>, Refused> {
        let start = self.at;
        self.take(
            n.checked_mul(size_of::<T>())
                .ok_or(Refused::Corrupt("size"))?,
        )?;
        Col::mapped(self.map, start, n).ok_or(Refused::Corrupt("misaligned section"))
    }
}

/// Map and check the tables at `path` for the index whose checksum field is
/// `index_checksum` with `entries` entries, scanned with `fingerprint`.
pub fn read(
    path: &Path,
    index_checksum: u32,
    entries: usize,
    fingerprint: u64,
) -> Result<Saved, Refused> {
    let map = match map_file(path) {
        Ok(map) => map,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(Refused::Missing),
        Err(e) => return Err(Refused::Io(e)),
    };
    if map.len() < HEADER_LEN || map[..8] != MAGIC {
        return Err(Refused::Corrupt("not a live table file"));
    }
    let mut r = Reader { map: &map, at: 8 };
    if r.u32()? != VERSION {
        return Err(Refused::Stale("another version"));
    }
    let checksum = r.u32()?;
    if crc32fast::hash(&map[CHECKSUM_START..]) != checksum {
        return Err(Refused::Corrupt("checksum mismatch"));
    }
    if r.u32()? != index_checksum {
        return Err(Refused::Stale("saved for another index"));
    }
    let n = r.u32()? as usize;
    if n != entries {
        return Err(Refused::Stale("saved for another index"));
    }
    if r.u64()? != fingerprint {
        return Err(Refused::Stale("other roots, excludes or options"));
    }
    let count = r.u32()? as usize;
    r.u32()?;
    if count == 0 || count > 1 << 12 {
        return Err(Refused::Corrupt("volume count"));
    }
    struct Head {
        device: String,
        serial: u64,
        journal: (u64, i64),
        record_size: u32,
        entries: u64,
        excluded: Vec<u64>,
        roots: Vec<u64>,
        records: usize,
        links: usize,
    }
    let mut heads = Vec::with_capacity(count);
    for _ in 0..count {
        let len = r.u32()? as usize;
        if len > 4096 {
            return Err(Refused::Corrupt("device name"));
        }
        let device = String::from_utf8(r.take(len)?.to_vec())
            .map_err(|_| Refused::Corrupt("device name"))?;
        let serial = r.u64()?;
        let journal = (r.u64()?, r.u64()? as i64);
        let record_size = r.u32()?;
        let volume_entries = r.u64()?;
        let excluded_len = r.u32()? as usize;
        let mut excluded = Vec::with_capacity(excluded_len.min(1 << 16));
        for _ in 0..excluded_len {
            excluded.push(r.u64()?);
        }
        let roots_len = r.u32()? as usize;
        let mut roots = Vec::with_capacity(roots_len.min(1 << 10));
        for _ in 0..roots_len {
            roots.push(r.u64()?);
        }
        let records = r.count(1 << 48)?;
        let links = r.count(u64::from(u32::MAX))?;
        heads.push(Head {
            device,
            serial,
            journal,
            record_size,
            entries: volume_entries,
            excluded,
            roots,
            records,
            links,
        });
    }
    r.pad()?;
    let base_record: Col<u64> = r.col(n)?;
    let children_first: Col<u32> = r.col(n + 1)?;
    r.pad()?;
    let children = r.count(n as u64)?;
    let children_ids: Col<u32> = r.col(children)?;
    r.pad()?;
    let mut volumes = Vec::with_capacity(count);
    for head in heads {
        let record_base: Col<u32> = r.col(head.records)?;
        r.pad()?;
        let link_records: Col<u64> = r.col(head.links)?;
        let link_ids: Col<u32> = r.col(head.links)?;
        r.pad()?;
        volumes.push(SavedVolume {
            device: head.device,
            serial: head.serial,
            journal: head.journal,
            record_size: head.record_size,
            entries: head.entries,
            excluded: head.excluded,
            roots: head.roots,
            record_base,
            link_records,
            link_ids,
        });
    }
    if r.at != map.len() {
        return Err(Refused::Corrupt("unexpected trailing bytes"));
    }
    let saved = Saved {
        index_checksum,
        fingerprint,
        base_record,
        children_first,
        children_ids,
        volumes,
    };
    validate(&saved, n)?;
    Ok(saved)
}

/// Every id, record and offset in range, so later lookups cannot panic.
fn validate(saved: &Saved, n: usize) -> Result<(), Refused> {
    let volumes = saved.volumes.len() as u64;
    for &packed in saved.base_record.iter() {
        if packed != u64::MAX {
            let volume = packed >> 48;
            let record = packed & 0x0000_FFFF_FFFF_FFFF;
            if volume >= volumes
                || record >= saved.volumes[volume as usize].record_base.len() as u64
            {
                return Err(Refused::Corrupt("entry record out of range"));
            }
        }
    }
    let first = &saved.children_first;
    if first.first() != Some(&0)
        || first.windows(2).any(|w| w[0] > w[1])
        || *first.last().expect("n + 1 values") as usize != saved.children_ids.len()
        || saved.children_ids.iter().any(|&c| c as usize >= n)
    {
        return Err(Refused::Corrupt("children table"));
    }
    for v in &saved.volumes {
        if v.record_base
            .iter()
            .any(|&id| id < FIRST_MARKER && id as usize >= n)
            || v.link_ids.iter().any(|&id| id as usize >= n)
            || v.link_records.windows(2).any(|w| w[0] > w[1])
            || v.link_records
                .iter()
                .any(|&r| r as usize >= v.record_base.len() || v.record_base[r as usize] != MULTI)
        {
            return Err(Refused::Corrupt("record table"));
        }
    }
    // The tables agree: every entry with a record is found from it (alone
    // in its slot, or among its record's links), and every record marked
    // as linked has links. A writer bug would otherwise lose entries.
    for (id, &packed) in saved.base_record.iter().enumerate() {
        if packed == u64::MAX {
            continue;
        }
        let v = &saved.volumes[(packed >> 48) as usize];
        let record = packed & 0x0000_FFFF_FFFF_FFFF;
        let found = match v.record_base[record as usize] {
            MULTI => {
                let from = v.link_records.partition_point(|&r| r < record);
                let to = v.link_records.partition_point(|&r| r <= record);
                v.link_ids[from..to].contains(&(id as u32))
            }
            slot => slot as usize == id,
        };
        if !found {
            return Err(Refused::Corrupt("record tables disagree"));
        }
    }
    for v in &saved.volumes {
        let linked = v.record_base.iter().filter(|&&slot| slot == MULTI).count();
        let distinct = v.link_records.windows(2).filter(|w| w[0] != w[1]).count()
            + usize::from(!v.link_records.is_empty());
        if distinct != linked {
            return Err(Refused::Corrupt("record tables disagree"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::type_complexity)]
    fn sample() -> (Vec<u64>, Vec<u32>, Vec<u32>, Vec<u32>, Vec<u64>, Vec<u32>) {
        // root(0) / a(1), b(2) and b's second name (3); records 5, 20, 21
        // on volume 0.
        let base_record = vec![5, 20, 21, 21];
        let first = vec![0, 3, 3, 3, 3];
        let ids = vec![1, 2, 3];
        let mut record_base = vec![u32::MAX; 22];
        record_base[5] = 0;
        record_base[20] = 1;
        record_base[21] = u32::MAX - 1;
        (
            base_record,
            first,
            ids,
            record_base,
            vec![21, 21],
            vec![2, 3],
        )
    }

    #[test]
    fn tables_round_trip_through_a_mapped_file_and_are_refused_when_stale_or_damaged() {
        let dir = std::env::temp_dir().join(format!("ferrowd-live-tables-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = path_beside(&dir.join("index.bin"));
        let (base_record, first, ids, record_base, link_records, link_ids) = sample();
        let tables = SavedRef {
            index_checksum: 77,
            fingerprint: 99,
            base_record: &base_record,
            children_first: &first,
            children_ids: &ids,
            volumes: vec![SavedVolumeRef {
                device: r"\\?\Volume{1}",
                serial: 3,
                journal: (11, 12345),
                record_size: 1024,
                entries: 4,
                excluded: vec![30],
                roots: vec![5],
                record_base: &record_base,
                link_records: &link_records,
                link_ids: &link_ids,
            }],
        };
        write(&path, &tables).unwrap();
        let saved = read(&path, 77, 4, 99).unwrap();
        assert_eq!(&*saved.base_record, &base_record[..]);
        assert_eq!(&*saved.children_ids, &ids[..]);
        let v = &saved.volumes[0];
        assert_eq!(
            (v.device.as_str(), v.journal),
            (r"\\?\Volume{1}", (11, 12345))
        );
        assert_eq!(
            (v.excluded.as_slice(), v.roots.as_slice()),
            (&[30][..], &[5][..])
        );
        assert_eq!(&*v.record_base, &record_base[..]);
        assert_eq!(&*v.link_ids, &[2, 3][..]);
        assert!(v.record_base.is_mapped());
        // For another index, other options: refused.
        assert!(matches!(read(&path, 78, 4, 99), Err(Refused::Stale(_))));
        assert!(matches!(read(&path, 77, 5, 99), Err(Refused::Stale(_))));
        assert!(matches!(read(&path, 77, 4, 98), Err(Refused::Stale(_))));
        drop(saved);
        // A flipped byte anywhere: refused.
        let bytes = std::fs::read(&path).unwrap();
        for at in [20, bytes.len() / 2, bytes.len() - 1] {
            let mut bad = bytes.clone();
            bad[at] ^= 1;
            std::fs::write(&path, &bad).unwrap();
            assert!(read(&path, 77, 4, 99).is_err(), "flip at {at}");
        }
        // Missing: said so.
        std::fs::remove_file(&path).unwrap();
        assert!(matches!(read(&path, 77, 4, 99), Err(Refused::Missing)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_fingerprint_follows_every_part() {
        let a = fingerprint(&[b"C:\\", b"x"]);
        assert_eq!(a, fingerprint(&[b"C:\\", b"x"]));
        assert_ne!(a, fingerprint(&[b"C:\\x", b""]), "parts are delimited");
        assert_ne!(a, fingerprint(&[b"D:\\", b"x"]));
    }

    #[test]
    fn tables_that_disagree_are_refused() {
        let dir =
            std::env::temp_dir().join(format!("ferrowd-live-disagree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = path_beside(&dir.join("index.bin"));
        let (base_record, first, ids, mut record_base, link_records, link_ids) = sample();
        // Record 20 claims entry 2, but entry 1 is on record 20: entry 1
        // could not be found from its record.
        record_base[20] = 2;
        let tables = SavedRef {
            index_checksum: 1,
            fingerprint: 2,
            base_record: &base_record,
            children_first: &first,
            children_ids: &ids,
            volumes: vec![SavedVolumeRef {
                device: r"\\?\Volume{1}",
                serial: 3,
                journal: (11, 12345),
                record_size: 1024,
                entries: 4,
                excluded: vec![],
                roots: vec![5],
                record_base: &record_base,
                link_records: &link_records,
                link_ids: &link_ids,
            }],
        };
        write(&path, &tables).unwrap();
        assert!(matches!(
            read(&path, 1, 4, 2),
            Err(Refused::Corrupt("record tables disagree"))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
