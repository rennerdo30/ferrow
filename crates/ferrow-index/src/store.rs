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

/// What keeps a column's elements alive.
enum Backing<T> {
    Owned(Vec<T>),
    Mapped(#[allow(dead_code)] Arc<Mmap>),
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
            backing: Backing::Mapped(map.clone()),
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
        // is read-only and kept alive by the `Arc`; its file is opened
        // without write sharing and never written in place (a save replaces
        // it by renaming a new file over it).
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
