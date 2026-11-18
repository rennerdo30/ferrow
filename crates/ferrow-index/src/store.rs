//! Column storage: a heap vector, or a slice of a read-only memory map of a
//! saved index (ADR 0007). Both read as a plain slice, so search code does
//! not know which one it has; a mapped column costs no private memory (its
//! pages are the operating system's file cache, shared and reclaimable).

use std::fmt;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use memmap2::Mmap;

#[cfg(target_endian = "big")]
compile_error!("ferrow-index maps little-endian files in place and needs a little-endian target");

/// A column element that may be read straight from file bytes: no padding,
/// every bit pattern valid, little-endian on every supported target.
///
/// # Safety
/// Implemented only for the unsigned integers below; implement it for
/// nothing else.
pub unsafe trait Plain: Copy + Send + Sync + 'static {}
// SAFETY: plain integers, no padding, any bit pattern is a valid value.
unsafe impl Plain for u8 {}
// SAFETY: as above.
unsafe impl Plain for u16 {}
// SAFETY: as above.
unsafe impl Plain for u32 {}
// SAFETY: as above.
unsafe impl Plain for u64 {}

/// The bytes of a slice of plain values (little-endian on every supported
/// target), for writing a column to a file without a per-value encode.
pub(crate) fn as_bytes<T: Plain>(values: &[T]) -> &[u8] {
    // SAFETY: `T: Plain` has no padding and no invalid bit patterns; the
    // byte view covers exactly the slice's memory.
    unsafe { std::slice::from_raw_parts(values.as_ptr().cast::<u8>(), size_of_val(values)) }
}

/// The same, writable: a column is filled straight from a file read.
pub(crate) fn as_bytes_mut<T: Plain>(values: &mut [T]) -> &mut [u8] {
    // SAFETY: as in `as_bytes`; any bytes written are valid values of `T`.
    unsafe { std::slice::from_raw_parts_mut(values.as_mut_ptr().cast::<u8>(), size_of_val(values)) }
}

/// A writable byte region as a slice of plain values.
///
/// # Panics
/// When the region is not aligned for `T` or its length is not a multiple of
/// `size_of::<T>()`: the file layout places every column on its boundary.
pub(crate) fn cast_mut<T: Plain>(bytes: &mut [u8]) -> &mut [T] {
    // SAFETY: `T: Plain` accepts any bytes; the checks below reject a region
    // that does not split exactly into aligned elements.
    let (head, mid, tail) = unsafe { bytes.align_to_mut::<T>() };
    assert!(
        head.is_empty() && tail.is_empty(),
        "a file region must be aligned for its column"
    );
    mid
}

/// What keeps a column's elements alive.
enum Backing<T> {
    Owned(Vec<T>),
    /// A read-only map of a saved file, or a filled temporary buffer: only
    /// kept alive here, never written again.
    Mapped(#[allow(dead_code)] Arc<dyn std::any::Any + Send + Sync>),
}

/// One column: owned, or elements of a shared map. Reading it is a plain
/// slice of `ptr` and `len` (no branch per access: searches read columns
/// once per entry); `backing` keeps the memory alive.
pub struct Col<T: Plain> {
    ptr: *const T,
    len: usize,
    backing: Backing<T>,
}

// SAFETY: `ptr` points into `backing`, which is `Send + Sync` (a `Vec` of
// `Send + Sync` plain values, or a read-only map behind an `Arc`); the
// column hands out only shared slices, or a `&mut Vec` through `&mut self`.
unsafe impl<T: Plain> Send for Col<T> {}
// SAFETY: as above.
unsafe impl<T: Plain> Sync for Col<T> {}

impl<T: Plain> Col<T> {
    /// `len` elements of `map` from byte `off`; `None` when that range is
    /// outside the map or not aligned for `T`.
    pub fn mapped(map: &Arc<Mmap>, off: usize, len: usize) -> Option<Col<T>> {
        let bytes = len.checked_mul(size_of::<T>())?;
        let end = off.checked_add(bytes)?;
        if end > map.len() {
            return None;
        }
        // SAFETY: `off <= map.len()`, so the pointer stays inside (or one
        // past the end of) the map.
        let ptr = unsafe { map.as_ptr().add(off) }.cast::<T>();
        ptr.is_aligned().then(|| Col {
            ptr,
            len,
            backing: Backing::Mapped(map.clone() as Arc<dyn std::any::Any + Send + Sync>),
        })
    }

    /// Heap bytes this column holds (its capacity); 0 when mapped.
    pub fn heap_bytes(&self) -> usize {
        match &self.backing {
            Backing::Owned(v) => v.capacity() * size_of::<T>(),
            Backing::Mapped(_) => 0,
        }
    }

    /// Mapped bytes this column reads; 0 when owned.
    pub fn mapped_bytes(&self) -> usize {
        match &self.backing {
            Backing::Owned(_) => 0,
            Backing::Mapped(_) => self.len * size_of::<T>(),
        }
    }

    /// Whether the elements are read from a map.
    pub fn is_mapped(&self) -> bool {
        matches!(self.backing, Backing::Mapped(_))
    }

    /// The vector of an owned column, for the builder that fills it; the
    /// column reads the vector again when the guard is dropped.
    ///
    /// # Panics
    /// On a mapped column, which is never written.
    pub(crate) fn owned_mut(&mut self) -> OwnedMut<'_, T> {
        assert!(!self.is_mapped(), "a mapped column is read-only");
        OwnedMut { col: self }
    }
}

/// Write access to an owned column's vector (see [`Col::owned_mut`]).
pub(crate) struct OwnedMut<'a, T: Plain> {
    col: &'a mut Col<T>,
}

impl<T: Plain> Deref for OwnedMut<'_, T> {
    type Target = Vec<T>;
    fn deref(&self) -> &Vec<T> {
        match &self.col.backing {
            Backing::Owned(v) => v,
            Backing::Mapped(_) => unreachable!("checked in owned_mut"),
        }
    }
}

impl<T: Plain> DerefMut for OwnedMut<'_, T> {
    fn deref_mut(&mut self) -> &mut Vec<T> {
        match &mut self.col.backing {
            Backing::Owned(v) => v,
            Backing::Mapped(_) => unreachable!("checked in owned_mut"),
        }
    }
}

impl<T: Plain> Drop for OwnedMut<'_, T> {
    fn drop(&mut self) {
        // The vector may have grown into a new allocation.
        if let Backing::Owned(v) = &self.col.backing {
            self.col.ptr = v.as_ptr();
            self.col.len = v.len();
        }
    }
}

impl<T: Plain> Deref for Col<T> {
    type Target = [T];
    #[inline]
    fn deref(&self) -> &[T] {
        // SAFETY: `ptr` and `len` describe live, initialised, aligned
        // elements of `backing`: an owned vector (refreshed after every
        // write through `OwnedMut`), or a range `mapped` checked to be
        // inside the map and aligned; `T: Plain` accepts any bytes. The map
        // is read-only and kept alive by the `Arc`; Ferrow never writes a
        // saved index in place (a save renames a new file over it). On
        // Windows the file is opened without write sharing, so no one else
        // can either; on Unix it relies on the file's 0600 owner-only mode.
        // A disk error on a mapped page stops the process, as a failed read
        // of the heap's own pages would.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl<T: Plain> From<Vec<T>> for Col<T> {
    fn from(v: Vec<T>) -> Col<T> {
        Col {
            ptr: v.as_ptr(),
            len: v.len(),
            backing: Backing::Owned(v),
        }
    }
}

impl<T: Plain> Default for Col<T> {
    fn default() -> Col<T> {
        Vec::new().into()
    }
}

impl<T: Plain + fmt::Debug> fmt::Debug for Col<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = if self.is_mapped() { "Mapped" } else { "Owned" };
        write!(f, "{kind}(len {})", self.len)
    }
}

/// A writable buffer of plain values: an anonymous temporary file in a
/// directory, mapped (file cache, not private memory; ADR 0007), or the
/// heap when there is no directory or the file cannot be made. Frozen into
/// a read-only [`Col`] when filled.
pub(crate) enum Scratch<T: Plain> {
    Heap(Vec<T>),
    Mapped {
        map: memmap2::MmapMut,
        len: usize,
        // Kept open while mapped: on Windows the file is deleted on close.
        _file: std::fs::File,
    },
}

impl<T: Plain> Scratch<T> {
    /// `len` zeroed values, in a temporary file in `dir` when given.
    pub(crate) fn zeroed(len: usize, dir: Option<&std::path::Path>) -> Scratch<T> {
        let mapped = dir.filter(|_| len > 0).and_then(|dir| {
            let bytes = len.checked_mul(size_of::<T>())?;
            let file = crate::build::create_spill_file(dir).ok()?;
            reserve(&file, bytes as u64).ok()?;
            // SAFETY: an anonymous file of this process, sized above: it has
            // no name on Unix, and on Windows it is opened allowing others
            // only to read or delete it (no writer) and deleted on close.
            let map = unsafe { memmap2::MmapMut::map_mut(&file) }.ok()?;
            Some(Scratch::Mapped {
                map,
                len,
                _file: file,
            })
        });
        mapped.unwrap_or_else(|| {
            if dir.is_some() && len > 0 {
                SCRATCH_IN_HEAP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            Scratch::Heap(vec![zero(); len])
        })
    }

    /// Whether the values are in a temporary file.
    #[cfg(test)]
    pub(crate) fn is_mapped(&self) -> bool {
        matches!(self, Scratch::Mapped { .. })
    }

    pub(crate) fn as_mut_slice(&mut self) -> &mut [T] {
        match self {
            Scratch::Heap(v) => v,
            // SAFETY: the map is page-aligned and `len` values long (sized
            // in `zeroed`); `T: Plain` accepts any bytes; the borrow is
            // exclusive through `&mut self`.
            Scratch::Mapped { map, len, .. } => unsafe {
                std::slice::from_raw_parts_mut(map.as_mut_ptr().cast::<T>(), *len)
            },
        }
    }

    /// The filled values, read-only: the column only hands out shared
    /// slices of them from now on.
    pub(crate) fn freeze(self) -> Col<T> {
        match self {
            Scratch::Heap(v) => v.into(),
            Scratch::Mapped { map, len, _file } => {
                // The map keeps its own handle to the file (and the file's
                // data) while it lives; the file goes when it drops.
                drop(_file);
                let ptr = map.as_ptr().cast::<T>();
                Col {
                    ptr,
                    len,
                    backing: Backing::Mapped(Arc::new(map)),
                }
            }
        }
    }
}

/// How many temporary buffers that should have been files were made in the
/// heap instead (the directory could not take them: full, read-only). A
/// service reports a growing count: its memory is then not what it plans.
static SCRATCH_IN_HEAP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// See [`SCRATCH_IN_HEAP`].
pub fn scratch_in_heap() -> u64 {
    SCRATCH_IN_HEAP.load(std::sync::atomic::Ordering::Relaxed)
}

/// Give `file` `bytes` of real disk space. On NTFS extending a file
/// allocates it (a full disk fails here); elsewhere a file extended with
/// `set_len` is sparse (Linux allocates with `posix_fallocate`; elsewhere
/// the zeros are written), and a write through a map into a block the disk
/// cannot give stops the process (SIGBUS): the space is taken first.
pub(crate) fn reserve(file: &std::fs::File, bytes: u64) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        file.set_len(bytes)
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let len = libc::off_t::try_from(bytes).map_err(std::io::Error::other)?;
        // SAFETY: an open file descriptor; posix_fallocate allocates blocks
        // and extends the file, or returns an error number.
        match unsafe { libc::posix_fallocate(file.as_raw_fd(), 0, len) } {
            0 => Ok(()),
            errno => Err(std::io::Error::from_raw_os_error(errno)),
        }
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        use std::io::Write;
        let zeros = vec![0u8; 1 << 20];
        let mut left = bytes;
        let mut out = file;
        while left > 0 {
            let n = left.min(zeros.len() as u64) as usize;
            out.write_all(&zeros[..n])?;
            left -= n as u64;
        }
        out.flush()
    }
}

fn zero<T: Plain>() -> T {
    // SAFETY: `T: Plain` accepts any bytes, zero included.
    unsafe { std::mem::zeroed() }
}

/// Open `path` and map it read-only. On Windows the handle allows delete
/// sharing (and nothing else beyond reading): a save may then rename a new
/// file over this one while the map keeps serving the old contents, and no
/// one may open it for writing meanwhile.
pub fn map_file(path: &std::path::Path) -> std::io::Result<Arc<Mmap>> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scratch_buffer_lives_in_a_temporary_file_and_leaves_nothing() {
        let dir = std::env::temp_dir().join(format!("ferrow-scratch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut buf = Scratch::<u32>::zeroed(100_000, Some(&dir));
        assert!(buf.is_mapped());
        for (i, v) in buf.as_mut_slice().iter_mut().enumerate() {
            *v = i as u32 * 3;
        }
        let col = buf.freeze();
        assert!(col.is_mapped());
        assert_eq!((col.len(), col[99_999]), (100_000, 299_997));
        assert_eq!(col.heap_bytes(), 0);
        drop(col);
        let left = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(left, 0, "the temporary file is gone");
        // Without a directory: the heap.
        let heap = Scratch::<u8>::zeroed(10, None);
        assert!(!heap.is_mapped());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_owned_column_reads_its_vector_after_it_grows() {
        let mut col: Col<u32> = vec![1, 2, 3].into();
        {
            let mut v = col.owned_mut();
            v.reserve(10_000);
            v.extend(4..1000);
        }
        assert_eq!(col.len(), 999);
        assert_eq!(col[998], 999);
        assert!(col.heap_bytes() >= 999 * 4);
    }
}
