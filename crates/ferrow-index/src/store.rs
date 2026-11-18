//! Column storage: a heap vector, or a slice of a read-only memory map of a
//! saved index (ADR 0007). Both read as a plain slice, so search code does
//! not know which one it has; a mapped column costs no private memory (its
//! pages are the operating system's file cache, shared and reclaimable).

use std::fmt;
use std::fs::File;
use std::io;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use memmap2::{Mmap, MmapMut};

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
    Mapped(#[allow(dead_code)] Arc<Mmap>),
    /// A temporary file's map (see [`TempColumn`] and [`Appender`]).
    Temp(#[allow(dead_code)] Arc<TempMap>),
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

    /// `len` elements of a temporary file's map from byte `off` (see
    /// [`TempColumn::freeze`]); `None` when out of range or misaligned.
    fn temp(map: &Arc<TempMap>, off: usize, len: usize) -> Option<Col<T>> {
        let bytes = len.checked_mul(size_of::<T>())?;
        let end = off.checked_add(bytes)?;
        if end > map.map.len() {
            return None;
        }
        // SAFETY: `off <= map.len()`, so the pointer stays inside (or one
        // past the end of) the map.
        let ptr = unsafe { map.map.as_ptr().add(off) }.cast::<T>();
        ptr.is_aligned().then(|| Col {
            ptr,
            len,
            backing: Backing::Temp(map.clone()),
        })
    }

    /// Heap bytes this column holds (its capacity); 0 when mapped.
    pub fn heap_bytes(&self) -> usize {
        match &self.backing {
            Backing::Owned(v) => v.capacity() * size_of::<T>(),
            Backing::Mapped(_) | Backing::Temp(_) => 0,
        }
    }

    /// Mapped bytes this column reads; 0 when owned.
    pub fn mapped_bytes(&self) -> usize {
        match &self.backing {
            Backing::Owned(_) => 0,
            Backing::Mapped(_) | Backing::Temp(_) => self.len * size_of::<T>(),
        }
    }

    /// Whether the elements are read from a map.
    pub fn is_mapped(&self) -> bool {
        !matches!(self.backing, Backing::Owned(_))
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
            _ => unreachable!("checked in owned_mut"),
        }
    }
}

impl<T: Plain> DerefMut for OwnedMut<'_, T> {
    fn deref_mut(&mut self) -> &mut Vec<T> {
        match &mut self.col.backing {
            Backing::Owned(v) => v,
            _ => unreachable!("checked in owned_mut"),
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

/// Positional file I/O: no shared cursor, so a read never disturbs a write.
#[cfg(unix)]
pub(crate) fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buf, offset)
}
#[cfg(unix)]
pub(crate) fn write_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.write_all_at(buf, offset)
}
#[cfg(windows)]
pub(crate) fn read_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_read(buf, offset) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
#[cfg(windows)]
pub(crate) fn write_at(file: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_write(buf, offset) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => {
                buf = &buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// An anonymous temporary file in `dir`: it is gone when its handle closes,
/// whatever ends the process (deleted on close on Windows, unlinked at once
/// on Unix), so a failed or killed build leaves nothing behind. Used for
/// spilled shards, file-backed scan tables and the tables built after a scan.
pub fn create_temp_file(dir: &Path) -> io::Result<File> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    for _ in 0..100 {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!(".ferrow-spill-{pid}-{seq}.tmp"));
        let mut opts = std::fs::OpenOptions::new();
        opts.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            const GENERIC_READ: u32 = 0x8000_0000;
            const GENERIC_WRITE: u32 = 0x4000_0000;
            const DELETE: u32 = 0x0001_0000;
            const FILE_ATTRIBUTE_TEMPORARY: u32 = 0x100;
            const FILE_FLAG_DELETE_ON_CLOSE: u32 = 0x0400_0000;
            opts.access_mode(GENERIC_READ | GENERIC_WRITE | DELETE)
                .attributes(FILE_ATTRIBUTE_TEMPORARY)
                .custom_flags(FILE_FLAG_DELETE_ON_CLOSE);
        }
        match opts.open(&path) {
            Ok(file) => {
                #[cfg(unix)]
                std::fs::remove_file(&path)?;
                return Ok(file);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not create a unique temporary spill file",
    ))
}

/// A zeroed vector, taken from the allocator zeroed (no pass over the memory:
/// the pages are only committed when written).
pub fn zeroed_vec<T: Plain>(len: usize) -> Vec<T> {
    if len == 0 || size_of::<T>() == 0 {
        return Vec::new();
    }
    let layout = std::alloc::Layout::array::<T>(len).expect("table size overflows");
    // SAFETY: non-zero size; the pointer comes from the global allocator with
    // the layout of `[T; len]`, which is what `Vec::from_raw_parts` needs, and
    // all-zero bytes are a valid `T: Plain`.
    unsafe {
        let ptr = std::alloc::alloc_zeroed(layout);
        if ptr.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        Vec::from_raw_parts(ptr.cast::<T>(), len, len)
    }
}

/// A read-only map of an anonymous temporary file, with the file kept open so
/// the file lives exactly as long as the map (and goes with it).
pub struct TempMap {
    map: Mmap,
    _file: File,
}

fn other(what: impl Into<String>) -> io::Error {
    io::Error::other(what.into())
}

/// A zero-filled column in an anonymous temporary file, written through a
/// writable map, then [`TempColumn::freeze`]n into a read-only [`Col`]. Its
/// dirty pages are the operating system's file cache, not this process's
/// private memory, so a big table built this way costs the heap nothing.
/// The file is private to the process (never opened by anyone else, never
/// resized while mapped).
pub struct TempColumn<T: Plain> {
    map: Option<MmapMut>,
    file: File,
    len: usize,
    _marker: PhantomData<T>,
}

impl<T: Plain> TempColumn<T> {
    /// `len` zeroed elements in a new temporary file in `dir`. An error (no
    /// directory, a full disk) leaves nothing behind.
    pub fn zeroed(dir: &Path, len: usize) -> io::Result<TempColumn<T>> {
        let bytes = len
            .checked_mul(size_of::<T>())
            .ok_or_else(|| other("a temporary column is too large"))?;
        let file = create_temp_file(dir)?;
        let map = if bytes == 0 {
            None
        } else {
            file.set_len(bytes as u64)?;
            // SAFETY: the file is private to this column (created
            // exclusively, never opened by anyone else) and not resized while
            // mapped.
            Some(unsafe { MmapMut::map_mut(&file)? })
        };
        Ok(TempColumn {
            map,
            file,
            len,
            _marker: PhantomData,
        })
    }

    /// Bytes of the file behind this column.
    pub fn bytes(&self) -> usize {
        self.len * size_of::<T>()
    }

    /// The finished column, read-only, through a second map of the same
    /// file (the pages are shared with this column's map, so it sees every
    /// value written so far). It keeps the file alive: drop this column
    /// afterwards and the file goes with the returned column. On an error
    /// this column is intact, so the caller can still copy its values.
    pub fn freeze(&self) -> io::Result<Col<T>> {
        if self.map.is_none() {
            return Ok(Col::default());
        }
        // SAFETY: the file is private to this column and not resized while
        // mapped; the writable map is dropped by the caller before any
        // further write could race a reader (no one reads until frozen).
        let map = unsafe { Mmap::map(&self.file)? };
        let keep = Arc::new(TempMap {
            map,
            _file: self.file.try_clone()?,
        });
        Col::temp(&keep, 0, self.len).ok_or_else(|| other("a temporary column is misaligned"))
    }
}

impl<T: Plain> Deref for TempColumn<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        match &self.map {
            // SAFETY: a page-aligned map of exactly `len` elements of a
            // plain type; every bit pattern is valid.
            Some(map) => unsafe { std::slice::from_raw_parts(map.as_ptr().cast::<T>(), self.len) },
            None => &[],
        }
    }
}

impl<T: Plain> DerefMut for TempColumn<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        match &mut self.map {
            // SAFETY: as in `deref`; the map is writable and uniquely borrowed.
            Some(map) => unsafe {
                std::slice::from_raw_parts_mut(map.as_mut_ptr().cast::<T>(), self.len)
            },
            None => &mut [],
        }
    }
}

/// Bytes an [`Appender`] collects before it writes them.
const APPEND_BUFFER: usize = 256 << 10;

enum AppendSink<T> {
    File {
        file: File,
        /// Bytes durably written: the file's valid prefix.
        flushed: u64,
        /// Bytes not written yet (intact after a failed write).
        buf: Vec<u8>,
    },
    /// The file failed; what it held is in memory (correct, larger).
    Heap(Vec<T>),
    /// The file failed and could not be read back.
    Lost(String),
}

/// A column written by appending (records of a chunk at a time, never
/// resized), then finished into a read-only [`Col`] over its temporary file.
/// The heap holds one write buffer, not the column. When the file fails (a
/// full disk) the column continues in the heap with what the file held
/// ([`Appender::fallback`] says why), so the result is complete either way;
/// only a file that fails and cannot be read back loses it, and `finish` then
/// reports an error.
pub struct Appender<T: Plain> {
    sink: AppendSink<T>,
    len: usize,
    fallback: Option<String>,
}

impl<T: Plain> Appender<T> {
    pub fn new(dir: &Path) -> io::Result<Appender<T>> {
        Ok(Appender {
            sink: AppendSink::File {
                file: create_temp_file(dir)?,
                flushed: 0,
                buf: Vec::with_capacity(APPEND_BUFFER),
            },
            len: 0,
            fallback: None,
        })
    }

    /// An appender that only collects in the heap (no file).
    pub fn in_memory() -> Appender<T> {
        Appender {
            sink: AppendSink::Heap(Vec::new()),
            len: 0,
            fallback: None,
        }
    }

    /// Elements appended so far.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Why the column moved to the heap, if it did.
    pub fn fallback(&self) -> Option<&str> {
        self.fallback.as_deref()
    }

    /// Heap this appender holds (its write buffer, or the column after a
    /// fallback), by capacity.
    pub fn heap_bytes(&self) -> usize {
        match &self.sink {
            AppendSink::File { buf, .. } => buf.capacity(),
            AppendSink::Heap(v) => v.capacity() * size_of::<T>(),
            AppendSink::Lost(_) => 0,
        }
    }

    /// Bytes the file holds (written, or waiting to be); 0 after a fallback.
    pub fn file_bytes(&self) -> usize {
        match &self.sink {
            AppendSink::File { .. } => self.len * size_of::<T>(),
            _ => 0,
        }
    }

    pub fn push(&mut self, value: T) {
        self.extend_from_slice(std::slice::from_ref(&value));
    }

    pub fn extend_from_slice(&mut self, values: &[T]) {
        self.len += values.len();
        match &mut self.sink {
            AppendSink::File { buf, .. } => {
                buf.extend_from_slice(as_bytes(values));
                if buf.len() >= APPEND_BUFFER
                    && let Err(error) = self.flush()
                {
                    self.degrade(&error);
                }
            }
            AppendSink::Heap(v) => v.extend_from_slice(values),
            AppendSink::Lost(_) => {}
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        if let AppendSink::File { file, flushed, buf } = &mut self.sink
            && !buf.is_empty()
        {
            write_at(file, buf, *flushed)?;
            *flushed += buf.len() as u64;
            buf.clear();
            // A large slice may have grown the buffer: keep it near its size.
            if buf.capacity() > 2 * APPEND_BUFFER {
                buf.shrink_to(APPEND_BUFFER);
            }
        }
        Ok(())
    }

    /// The file failed: read back what it durably holds and go on in memory.
    fn degrade(&mut self, error: &io::Error) {
        let AppendSink::File { file, flushed, buf } =
            std::mem::replace(&mut self.sink, AppendSink::Lost(String::new()))
        else {
            return;
        };
        let total = flushed as usize + buf.len();
        let mut all: Vec<T> = Vec::with_capacity(total / size_of::<T>());
        // SAFETY: `all` has room for `total` bytes; `T: Plain` accepts any
        // bytes, and `total` is a whole number of elements. The length is set
        // only after the bytes are in.
        let target =
            unsafe { std::slice::from_raw_parts_mut(all.as_mut_ptr().cast::<u8>(), total) };
        let (head, tail) = target.split_at_mut(flushed as usize);
        match read_at(&file, head, 0) {
            Ok(()) => {
                tail.copy_from_slice(&buf);
                // SAFETY: every byte of `total` was just written.
                unsafe { all.set_len(total / size_of::<T>()) };
                self.fallback = Some(error.to_string());
                self.sink = AppendSink::Heap(all);
            }
            Err(read) => {
                self.sink = AppendSink::Lost(format!(
                    "{error}; the part already written could not be read back ({read})"
                ));
            }
        }
    }

    /// The finished column: a map of the file, or the heap column after a
    /// fallback. An error only when the file failed and could not be read
    /// back, or cannot be mapped.
    pub fn finish(mut self) -> io::Result<Col<T>> {
        if let Err(error) = self.flush() {
            self.degrade(&error);
        }
        match self.sink {
            AppendSink::Heap(v) => Ok(v.into()),
            AppendSink::Lost(why) => Err(other(why)),
            AppendSink::File { file, .. } => {
                if self.len == 0 {
                    return Ok(Col::default());
                }
                // SAFETY: the file is private to this appender and not
                // written again once mapped.
                let map = unsafe { Mmap::map(&file)? };
                let keep = Arc::new(TempMap { map, _file: file });
                Col::temp(&keep, 0, self.len)
                    .ok_or_else(|| other("an appended column is misaligned"))
            }
        }
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

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ferrow-store-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn leftovers(dir: &Path) -> usize {
        std::fs::read_dir(dir)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".ferrow-")
            })
            .count()
    }

    #[test]
    fn a_temp_column_is_zeroed_written_through_its_map_and_read_back_frozen() {
        let dir = scratch("column");
        let mut column = TempColumn::<u32>::zeroed(&dir, 100_000).unwrap();
        assert_eq!(column.bytes(), 400_000);
        assert!(column.iter().all(|&v| v == 0), "zero-filled");
        for (i, v) in column.iter_mut().enumerate() {
            *v = (i as u32).wrapping_mul(2_654_435_761);
        }
        assert_eq!(leftovers(&dir), 1, "one temporary file while it lives");
        let col = column.freeze().unwrap();
        drop(column);
        assert_eq!(leftovers(&dir), 1, "the frozen column keeps the file");
        assert!(col.is_mapped() && col.heap_bytes() == 0);
        assert_eq!(col.mapped_bytes(), 400_000);
        assert_eq!(col.len(), 100_000);
        assert!(
            col.iter()
                .enumerate()
                .all(|(i, &v)| v == (i as u32).wrapping_mul(2_654_435_761))
        );
        drop(col);
        assert_eq!(leftovers(&dir), 0, "gone with the column");
        // Empty columns need no file mapping.
        let empty = TempColumn::<u64>::zeroed(&dir, 0).unwrap();
        assert!(empty.is_empty());
        assert!(empty.freeze().unwrap().is_empty());
        // No directory: an error, nothing left.
        assert!(TempColumn::<u8>::zeroed(&dir.join("missing"), 10).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_appender_collects_a_column_in_a_file_and_finishes_it_mapped() {
        let dir = scratch("append");
        let mut app = Appender::<u64>::new(&dir).unwrap();
        // More than its write buffer, in pushes and slices.
        let total = (APPEND_BUFFER / 8) * 3 + 17;
        for i in 0..total / 2 {
            app.push(i as u64);
        }
        let rest: Vec<u64> = (total as u64 / 2..total as u64).collect();
        app.extend_from_slice(&rest);
        assert_eq!(app.len(), total);
        assert!(
            app.heap_bytes() <= 2 * APPEND_BUFFER,
            "{}",
            app.heap_bytes()
        );
        assert_eq!(app.file_bytes(), total * 8);
        assert_eq!(app.fallback(), None);
        let col = app.finish().unwrap();
        assert!(col.is_mapped());
        assert!(col.iter().copied().eq(0..total as u64));
        drop(col);
        assert_eq!(leftovers(&dir), 0);
        // Nothing appended: an empty column.
        assert!(
            Appender::<u32>::new(&dir)
                .unwrap()
                .finish()
                .unwrap()
                .is_empty()
        );
        assert!(Appender::<u32>::new(&dir.join("missing")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_appender_whose_file_fails_continues_in_the_heap_with_everything() {
        let dir = scratch("degrade");
        let mut app = Appender::<u32>::new(&dir).unwrap();
        // Some already written (flushed), some still buffered.
        let first: Vec<u32> = (0..(APPEND_BUFFER as u32 / 4) + 5).collect();
        app.extend_from_slice(&first);
        assert!(app.flush().is_ok());
        let second: Vec<u32> = (10_000_000..10_000_100).collect();
        app.extend_from_slice(&second);
        // The disk fails now (as a full disk would): what the file holds and
        // what was buffered move to the heap.
        app.degrade(&io::Error::other("disk full"));
        assert_eq!(app.fallback(), Some("disk full"));
        assert_eq!(app.file_bytes(), 0);
        let third: Vec<u32> = (20_000_000..20_000_010).collect();
        app.extend_from_slice(&third);
        let col = app.finish().unwrap();
        assert!(!col.is_mapped());
        assert!(
            col.iter()
                .copied()
                .eq(first.iter().chain(&second).chain(&third).copied())
        );
        assert_eq!(leftovers(&dir), 0, "the failed file is gone");
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
