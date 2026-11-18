//! Column storage: a heap vector, or a slice of a read-only memory map of a
//! saved index (ADR 0007). Both read as a plain slice, so search code does
//! not know which one it has; a mapped column costs no private memory (its
//! pages are the operating system's file cache, shared and reclaimable).

use std::fmt;
use std::ops::Deref;
use std::sync::Arc;

use memmap2::Mmap;

#[cfg(target_endian = "big")]
compile_error!("ferrow-index maps little-endian files in place and needs a little-endian target");

/// A column element that may be read straight from file bytes: no padding,
/// every bit pattern valid, little-endian on every supported target.
///
/// # Safety
/// Implemented only for the unsigned integers below.
pub(crate) unsafe trait Plain: Copy + Send + Sync + 'static {}
// SAFETY: plain integers, no padding, any bit pattern is a valid value.
unsafe impl Plain for u8 {}
// SAFETY: as above.
unsafe impl Plain for u16 {}
// SAFETY: as above.
unsafe impl Plain for u32 {}

/// One column: owned, or `len` elements of a shared map from byte `off`.
pub(crate) enum Col<T: Plain> {
    Owned(Vec<T>),
    Mapped {
        map: Arc<Mmap>,
        off: usize,
        len: usize,
    },
}

impl<T: Plain> Col<T> {
    /// `len` elements of `map` from byte `off`; `None` when that range is
    /// outside the map or not aligned for `T`.
    pub(crate) fn mapped(map: &Arc<Mmap>, off: usize, len: usize) -> Option<Col<T>> {
        let bytes = len.checked_mul(size_of::<T>())?;
        let end = off.checked_add(bytes)?;
        let aligned = (map.as_ptr() as usize + off).is_multiple_of(align_of::<T>());
        (end <= map.len() && aligned).then(|| Col::Mapped {
            map: map.clone(),
            off,
            len,
        })
    }

    /// Heap bytes this column holds (its capacity); 0 when mapped.
    pub(crate) fn heap_bytes(&self) -> usize {
        match self {
            Col::Owned(v) => v.capacity() * size_of::<T>(),
            Col::Mapped { .. } => 0,
        }
    }

    /// Mapped bytes this column reads; 0 when owned.
    pub(crate) fn mapped_bytes(&self) -> usize {
        match self {
            Col::Owned(_) => 0,
            Col::Mapped { len, .. } => len * size_of::<T>(),
        }
    }

    /// The vector of an owned column, for the builder that fills it.
    ///
    /// # Panics
    /// On a mapped column, which is never written.
    pub(crate) fn owned_mut(&mut self) -> &mut Vec<T> {
        match self {
            Col::Owned(v) => v,
            Col::Mapped { .. } => panic!("a mapped column is read-only"),
        }
    }
}

impl<T: Plain> Deref for Col<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        match self {
            Col::Owned(v) => v,
            // SAFETY: `mapped` checked that the range is inside the map and
            // aligned for `T`, and `T: Plain` accepts any bytes. The map is
            // read-only and kept alive by the `Arc`; the file is opened
            // without write sharing and never written in place (a save
            // replaces it by renaming a new file over it).
            Col::Mapped { map, off, len } => unsafe {
                std::slice::from_raw_parts(map.as_ptr().add(*off).cast::<T>(), *len)
            },
        }
    }
}

impl<T: Plain> From<Vec<T>> for Col<T> {
    fn from(v: Vec<T>) -> Col<T> {
        Col::Owned(v)
    }
}

impl<T: Plain> Default for Col<T> {
    fn default() -> Col<T> {
        Col::Owned(Vec::new())
    }
}

impl<T: Plain + fmt::Debug> fmt::Debug for Col<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Col::Owned(v) => write!(f, "Owned(len {})", v.len()),
            Col::Mapped { off, len, .. } => write!(f, "Mapped(off {off}, len {len})"),
        }
    }
}

/// Open `path` and map it read-only. On Windows the handle allows delete
/// sharing (and nothing else beyond reading): a save may then rename a new
/// file over this one while the map keeps serving the old contents, and no
/// one may open it for writing meanwhile.
pub(crate) fn map_file(path: &std::path::Path) -> std::io::Result<Arc<Mmap>> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x1;
        const FILE_SHARE_DELETE: u32 = 0x4;
        options.share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE);
    }
    let file = options.open(path)?;
    // SAFETY: see `Col::deref`: the file is never modified in place while
    // mapped. An empty file cannot be mapped on every platform; the caller
    // rejects it by its header length first.
    let map = unsafe { Mmap::map(&file)? };
    Ok(Arc::new(map))
}
