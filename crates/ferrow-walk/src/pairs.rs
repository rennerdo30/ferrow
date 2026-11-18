//! The record map of a scan: `(MFT record number, entry id)` for every entry
//! a volume's MFT became, 16 bytes each.
//!
//! At 27 million entries that is 435 MB, which a scan used to hold in the
//! heap until the live layer was built. [`Pairs::spilling`] appends the pairs
//! to an anonymous temporary file instead and [`Pairs::seal`] maps it, so the
//! map costs the heap one write buffer whatever its length (ADR 0007). It
//! reads as a sequence of `(u64, u32)` either way.

use std::fmt;
use std::path::Path;

use ferrow_index::store::{Appender, Col, Plain};

/// One pair as a table row: integers only, no padding.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
struct Pair {
    record: u64,
    id: u32,
    reserved: u32,
}

// SAFETY: `repr(C)`, integers only, no padding (8 + 4 + 4 bytes, checked
// below): every bit pattern is a valid value.
unsafe impl Plain for Pair {}
const _: () = assert!(size_of::<Pair>() == 16);

/// Pairs a map being written collects before it hands them to its file in one
/// piece (a typed buffer: one push costs about what a vector's does).
const STAGE: usize = 4096;

/// The file of a map being written, and the pairs waiting to go to it.
struct Writer {
    file: Appender<Pair>,
    stage: Vec<Pair>,
}

impl Writer {
    #[inline]
    fn push(&mut self, pair: Pair) {
        self.stage.push(pair);
        if self.stage.len() == STAGE {
            self.flush();
        }
    }

    fn flush(&mut self) {
        self.file.extend_from_slice(&self.stage);
        self.stage.clear();
    }

    fn len(&self) -> usize {
        self.file.len() + self.stage.len()
    }
}

enum Inner {
    Heap(Vec<(u64, u32)>),
    /// Being appended to a file; not readable until sealed.
    Writing(Writer),
    Sealed(Col<Pair>),
}

/// `(record number, entry id)` pairs, in the order the scan made the entries.
/// A record with several names (hard links) appears once per name.
pub struct Pairs {
    inner: Inner,
    note: Option<String>,
}

impl Default for Pairs {
    fn default() -> Pairs {
        Pairs::new()
    }
}

impl Pairs {
    /// A copy of the pairs as a vector.
    pub fn to_vec(&self) -> Vec<(u64, u32)> {
        self.iter().collect()
    }
}

impl From<Vec<(u64, u32)>> for Pairs {
    fn from(pairs: Vec<(u64, u32)>) -> Pairs {
        Pairs {
            inner: Inner::Heap(pairs),
            note: None,
        }
    }
}

impl Pairs {
    /// An empty map in the heap.
    pub fn new() -> Pairs {
        Pairs {
            inner: Inner::Heap(Vec::new()),
            note: None,
        }
    }

    /// An empty map appended to a temporary file in `dir`, for a volume small
    /// enough for the heap to stand in: if the file cannot be made, or fails
    /// later, the map is kept in the heap, counted
    /// ([`ferrow_index::store::scratch_in_heap`]) and [`Pairs::note`] says why.
    pub fn spilling(dir: &Path) -> Pairs {
        let file = Appender::or_heap(dir);
        let note = file.fallback().map(|error| {
            format!("the record map could not be put in a file ({error}); it is kept in memory")
        });
        Pairs {
            inner: Inner::Writing(Writer {
                file,
                stage: Vec::with_capacity(STAGE),
            }),
            note,
        }
    }

    /// [`Pairs::spilling`] for a volume too large for the heap: no fallback.
    /// An error when the file cannot be made; a file that fails later makes
    /// [`Pairs::seal`] return an error.
    pub fn spilling_strict(dir: &Path) -> std::io::Result<Pairs> {
        Ok(Pairs {
            inner: Inner::Writing(Writer {
                file: Appender::new(dir)?,
                stage: Vec::with_capacity(STAGE),
            }),
            note: None,
        })
    }

    /// Why the map is in the heap although a file was wanted, if it is.
    pub fn note(&self) -> Option<&str> {
        self.note.as_deref()
    }

    /// Add a pair (only before [`Pairs::seal`]).
    ///
    /// # Panics
    /// On a sealed map.
    pub fn push(&mut self, record: u64, id: u32) {
        match &mut self.inner {
            Inner::Heap(v) => v.push((record, id)),
            Inner::Writing(writer) => writer.push(Pair {
                record,
                id,
                reserved: 0,
            }),
            Inner::Sealed(_) => panic!("a sealed record map cannot grow"),
        }
    }

    /// Room for `more` further pairs; only a map in the heap has any to make.
    pub fn reserve_exact(&mut self, more: usize) {
        if let Inner::Heap(v) = &mut self.inner {
            v.reserve_exact(more);
        }
    }

    /// Finish writing: the map is then read through a map of its file. Without
    /// a file this does nothing. An error (the file failed and could not be
    /// read back) leaves the map unusable; the caller treats the scan as
    /// failed.
    pub fn seal(&mut self) -> Result<(), String> {
        let Inner::Writing(writer) = &mut self.inner else {
            return Ok(());
        };
        writer.flush();
        let fell_back = writer.file.fallback().map(str::to_string);
        let Inner::Writing(writer) = std::mem::replace(&mut self.inner, Inner::Heap(Vec::new()))
        else {
            unreachable!("checked above");
        };
        let col = writer
            .file
            .finish()
            .map_err(|e| format!("the record map: {e}"))?;
        if let Some(why) = fell_back {
            self.note = Some(format!(
                "the record map's file failed ({why}); it is kept in memory"
            ));
        }
        self.inner = Inner::Sealed(col);
        Ok(())
    }

    /// Pairs held.
    pub fn len(&self) -> usize {
        match &self.inner {
            Inner::Heap(v) => v.len(),
            Inner::Writing(writer) => writer.len(),
            Inner::Sealed(col) => col.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The pairs in order.
    ///
    /// # Panics
    /// On a map that is still being written (see [`Pairs::seal`]).
    pub fn iter(&self) -> PairsIter<'_> {
        PairsIter(match &self.inner {
            Inner::Heap(v) => IterInner::Heap(v.iter()),
            Inner::Sealed(col) => IterInner::File(col.iter()),
            Inner::Writing(_) => panic!("seal the record map before reading it"),
        })
    }

    /// Heap held, by capacity: the vector, or a file's write buffer.
    pub fn heap_bytes(&self) -> usize {
        match &self.inner {
            Inner::Heap(v) => v.capacity() * size_of::<(u64, u32)>(),
            Inner::Writing(writer) => {
                writer.file.heap_bytes() + writer.stage.capacity() * size_of::<Pair>()
            }
            Inner::Sealed(col) => col.heap_bytes(),
        }
    }

    /// Bytes read from (or written to) a temporary file: file cache, not
    /// private memory.
    pub fn file_bytes(&self) -> usize {
        match &self.inner {
            Inner::Heap(_) => 0,
            Inner::Writing(writer) => {
                writer.file.file_bytes() + writer.stage.len() * size_of::<Pair>()
            }
            Inner::Sealed(col) => col.mapped_bytes(),
        }
    }
}

enum IterInner<'a> {
    Heap(std::slice::Iter<'a, (u64, u32)>),
    File(std::slice::Iter<'a, Pair>),
}

/// The pairs of a [`Pairs`], by value.
pub struct PairsIter<'a>(IterInner<'a>);

impl Iterator for PairsIter<'_> {
    type Item = (u64, u32);

    #[inline]
    fn next(&mut self) -> Option<(u64, u32)> {
        match &mut self.0 {
            IterInner::Heap(it) => it.next().copied(),
            IterInner::File(it) => it.next().map(|p| (p.record, p.id)),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match &self.0 {
            IterInner::Heap(it) => it.size_hint(),
            IterInner::File(it) => it.size_hint(),
        }
    }
}

impl ExactSizeIterator for PairsIter<'_> {}

impl<'a> IntoIterator for &'a Pairs {
    type Item = (u64, u32);
    type IntoIter = PairsIter<'a>;
    fn into_iter(self) -> PairsIter<'a> {
        self.iter()
    }
}

impl Clone for Pairs {
    /// A copy in the heap.
    fn clone(&self) -> Pairs {
        Pairs {
            inner: Inner::Heap(self.iter().collect()),
            note: self.note.clone(),
        }
    }
}

impl PartialEq for Pairs {
    fn eq(&self, other: &Pairs) -> bool {
        self.len() == other.len() && self.iter().eq(other.iter())
    }
}

impl fmt::Debug for Pairs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match &self.inner {
            Inner::Heap(_) => "heap",
            Inner::Writing(_) => "writing",
            Inner::Sealed(_) => "file",
        };
        write!(f, "Pairs({kind}, {} pairs)", self.len())
    }
}
