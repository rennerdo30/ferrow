//! Column storage: a heap vector, or a slice of a read-only memory map of a
//! saved index (ADR 0007). Both read as a plain slice, so search code does
//! not know which one it has; a mapped column costs no private memory (its
//! pages are the operating system's file cache, shared and reclaimable).

use std::fmt;
use std::fs::File;
use std::io;
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
/// Implemented for the unsigned integers below and, by the crates that keep
/// a table of records in a mapped file, for structs that satisfy all of:
/// `#[repr(C)]`; every field an integer (or an array of integers), so every
/// bit pattern is a valid value; **no padding bytes** (the field sizes add up
/// to `size_of`, checked by a `const` assertion beside each impl, because
/// padding is uninitialised memory when a table is written out and read
/// back); and an alignment no larger than 8, which a map's page alignment
/// always gives. Implement it for nothing else (no `bool`, `char`, enums,
/// references or `repr(Rust)` structs).
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
    /// A read-only map of a saved file, a filled temporary buffer or a
    /// temporary file's map: only kept alive here, never written again.
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

    /// `len` elements of a temporary file's map from byte `off` (see
    /// [`Appender::finish`]); `None` when out of range or misaligned.
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
/// on Unix), so a failed or killed build leaves nothing behind. The one
/// creator of temporary files: spilled shards, sort orders, the tables of a
/// scan and the tables built after it all come from here.
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
            const FILE_SHARE_READ: u32 = 0x1;
            const FILE_SHARE_DELETE: u32 = 0x4;
            opts.access_mode(GENERIC_READ | GENERIC_WRITE | DELETE)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
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
struct TempMap {
    map: Mmap,
    _file: File,
}

fn other(what: impl Into<String>) -> io::Error {
    io::Error::other(what.into())
}

/// How many temporary buffers that should have been files were made in the
/// heap instead (the directory could not take them: full, read-only). A
/// service reports a growing count: its memory is then not what it plans.
static SCRATCH_IN_HEAP: AtomicU64 = AtomicU64::new(0);

/// See [`SCRATCH_IN_HEAP`].
pub fn scratch_in_heap() -> u64 {
    SCRATCH_IN_HEAP.load(Ordering::Relaxed)
}

/// Count one buffer that should have been a file and went to the heap (every
/// fallback calls this: [`Scratch::zeroed`], [`Appender::or_heap`], a failed
/// [`Appender`] that continued in memory, and callers that choose the heap
/// because a file would not fit).
pub fn count_heap_fallback() {
    SCRATCH_IN_HEAP.fetch_add(1, Ordering::Relaxed);
}

/// A writable buffer of plain values: an anonymous temporary file in a
/// directory, mapped (file cache, not private memory; ADR 0007), or the
/// heap when there is no directory or the file cannot be made. Frozen into
/// a read-only [`Col`] when filled.
pub enum Scratch<T: Plain> {
    Heap(Vec<T>),
    Mapped {
        map: MmapMut,
        len: usize,
        // Kept open while mapped: on Windows the file is deleted on close.
        _file: File,
    },
}

impl<T: Plain> Scratch<T> {
    /// `len` zeroed values in a temporary file in `dir` (real disk space is
    /// taken before the first write through the map). An error (no directory,
    /// a full disk) leaves nothing behind and does not fall back to the heap:
    /// a caller that cannot afford the heap decides what failing means.
    pub fn try_zeroed(len: usize, dir: &Path) -> io::Result<Scratch<T>> {
        if len == 0 {
            return Ok(Scratch::Heap(Vec::new()));
        }
        let bytes = len
            .checked_mul(size_of::<T>())
            .ok_or_else(|| other("a temporary buffer is too large"))?;
        let file = create_temp_file(dir)?;
        reserve(&file, bytes as u64)?;
        // SAFETY: an anonymous file of this process, sized above: it has no
        // name on Unix, and on Windows it is opened allowing others only to
        // read or delete it (no writer) and deleted on close.
        let map = unsafe { MmapMut::map_mut(&file)? };
        Ok(Scratch::Mapped {
            map,
            len,
            _file: file,
        })
    }

    /// `len` zeroed values, in a temporary file in `dir` when given. When the
    /// file cannot be made they are in the heap and the fallback is counted
    /// ([`scratch_in_heap`]); use [`Scratch::try_zeroed`] where the heap is
    /// not an option.
    pub fn zeroed(len: usize, dir: Option<&Path>) -> Scratch<T> {
        if let Some(dir) = dir.filter(|_| len > 0) {
            match Scratch::try_zeroed(len, dir) {
                Ok(buffer) => return buffer,
                Err(_) => count_heap_fallback(),
            }
        }
        Scratch::Heap(zeroed_vec(len))
    }

    /// Whether the values are in a temporary file.
    pub fn is_mapped(&self) -> bool {
        matches!(self, Scratch::Mapped { .. })
    }

    /// Bytes of heap held (capacity); 0 in a file.
    pub fn heap_bytes(&self) -> usize {
        match self {
            Scratch::Heap(v) => v.capacity() * size_of::<T>(),
            Scratch::Mapped { .. } => 0,
        }
    }

    /// Bytes of the temporary file behind the buffer; 0 in the heap.
    pub fn file_bytes(&self) -> usize {
        match self {
            Scratch::Heap(_) => 0,
            Scratch::Mapped { len, .. } => len * size_of::<T>(),
        }
    }

    pub fn as_mut_slice(&mut self) -> &mut [T] {
        match self {
            Scratch::Heap(v) => v,
            // SAFETY: the map is page-aligned and `len` values long (sized
            // in `try_zeroed`); `T: Plain` accepts any bytes; the borrow is
            // exclusive through `&mut self`.
            Scratch::Mapped { map, len, .. } => unsafe {
                std::slice::from_raw_parts_mut(map.as_mut_ptr().cast::<T>(), *len)
            },
        }
    }

    /// The filled values, read-only. Consumes the buffer: the column only
    /// hands out shared slices of the map, and nothing can write through it
    /// while the column reads (the writable view is not reachable any more).
    pub fn freeze(self) -> Col<T> {
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

impl<T: Plain> Default for Scratch<T> {
    /// An empty buffer in the heap.
    fn default() -> Scratch<T> {
        Scratch::Heap(Vec::new())
    }
}

impl<T: Plain> Deref for Scratch<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        match self {
            Scratch::Heap(v) => v,
            // SAFETY: as in `as_mut_slice`, shared.
            Scratch::Mapped { map, len, .. } => unsafe {
                std::slice::from_raw_parts(map.as_ptr().cast::<T>(), *len)
            },
        }
    }
}

impl<T: Plain> DerefMut for Scratch<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        self.as_mut_slice()
    }
}

/// Bytes an [`Appender`] collects before it writes them.
const APPEND_BUFFER: usize = 256 << 10;

/// The most a failed [`Appender`] that may continue in the heap reads back
/// into one vector. A larger column whose file fails is lost (`finish`
/// errors) instead: reading gigabytes into the heap is exactly what the file
/// was there to avoid.
pub const DEGRADE_LIMIT: usize = 128 << 20;

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
    /// The file failed and the column was not kept (it may not move to the
    /// heap, is too large for it, or could not be read back).
    Lost(String),
}

/// A column written by appending (records of a chunk at a time, never
/// resized), then finished into a read-only [`Col`] over its temporary file.
/// The heap holds one write buffer, not the column.
///
/// When the file fails (a full disk) the column is lost, and `finish`
/// reports an error, unless the appender was told it may continue in the
/// heap ([`Appender::allow_heap`], for a small table) and what the file held
/// fits [`DEGRADE_LIMIT`]: then it reads the file back, goes on in memory,
/// counts the fallback ([`scratch_in_heap`]) and says why
/// ([`Appender::fallback`]). A caller polls [`Appender::failure`] to stop a
/// long read as soon as a column is lost.
pub struct Appender<T: Plain> {
    sink: AppendSink<T>,
    len: usize,
    fallback: Option<String>,
    /// How many bytes a failed file may be read back into the heap (0: none).
    heap_limit: usize,
}

impl<T: Plain> Appender<T> {
    /// An appender over a new temporary file in `dir`; an error when the file
    /// cannot be made. A failure of the file later loses the column (see the
    /// type's documentation).
    pub fn new(dir: &Path) -> io::Result<Appender<T>> {
        Ok(Appender {
            sink: AppendSink::File {
                file: create_temp_file(dir)?,
                flushed: 0,
                buf: Vec::with_capacity(APPEND_BUFFER),
            },
            len: 0,
            fallback: None,
            heap_limit: 0,
        })
    }

    /// Let a failed file continue in the heap while it holds at most `limit`
    /// bytes (never more than [`DEGRADE_LIMIT`]).
    #[must_use]
    pub fn allow_heap(mut self, limit: usize) -> Appender<T> {
        self.heap_limit = limit.min(DEGRADE_LIMIT);
        self
    }

    /// A file appender that may continue in the heap when it fails, or, when
    /// its file cannot be made, an appender that collects in the heap from
    /// the start (counted, with [`Appender::fallback`] saying why). For small
    /// tables only.
    pub fn or_heap(dir: &Path) -> Appender<T> {
        match Appender::new(dir) {
            Ok(appender) => appender.allow_heap(DEGRADE_LIMIT),
            Err(error) => {
                count_heap_fallback();
                Appender {
                    sink: AppendSink::Heap(Vec::new()),
                    len: 0,
                    fallback: Some(error.to_string()),
                    heap_limit: 0,
                }
            }
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

    /// Why the column was lost, if it was: the scan that fills it cannot
    /// succeed and should stop.
    pub fn failure(&self) -> Option<&str> {
        match &self.sink {
            AppendSink::Lost(why) => Some(why),
            _ => None,
        }
    }

    /// Treat the file as failed now, as a failed write does (a full disk
    /// cannot be forced in a test).
    #[doc(hidden)]
    pub fn fail(&mut self, error: io::Error) {
        self.degrade(&error);
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

    /// The file failed. A column that may go to the heap and fits there is
    /// read back and goes on in memory (counted); any other is lost, its file
    /// dropped (its disk space returns). A column that is not in a file any
    /// more is left as it is.
    fn degrade(&mut self, error: &io::Error) {
        let total = match &self.sink {
            AppendSink::File { flushed, buf, .. } => *flushed as usize + buf.len(),
            _ => return,
        };
        // An appender that may not use the heap is lost, whatever it held
        // (a failed flush always holds something; only a forced failure of
        // an empty one gets here with nothing).
        if self.heap_limit == 0 || total > self.heap_limit {
            let why = if self.heap_limit == 0 {
                error.to_string()
            } else {
                format!("{error}; {total} bytes are too many to keep in memory instead")
            };
            self.sink = AppendSink::Lost(why);
            return;
        }
        let AppendSink::File { file, flushed, buf } =
            std::mem::replace(&mut self.sink, AppendSink::Lost(String::new()))
        else {
            unreachable!("checked above");
        };
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
                count_heap_fallback();
            }
            Err(read) => {
                self.sink = AppendSink::Lost(format!(
                    "{error}; the part already written could not be read back ({read})"
                ));
            }
        }
    }

    /// The finished column: a map of the file, or the heap column after a
    /// fallback. An error when the column was lost (see
    /// [`Appender::failure`]) or the file cannot be mapped.
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

/// Give `file` `bytes` of real disk space. On NTFS extending a file
/// allocates it (a full disk fails here); elsewhere a file extended with
/// `set_len` is sparse (Linux allocates with `posix_fallocate`; elsewhere
/// the zeros are written), and a write through a map into a block the disk
/// cannot give stops the process (SIGBUS): the space is taken first.
pub(crate) fn reserve(file: &File, bytes: u64) -> io::Result<()> {
    #[cfg(windows)]
    {
        file.set_len(bytes)
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let len = libc::off_t::try_from(bytes).map_err(io::Error::other)?;
        // SAFETY: an open file descriptor; posix_fallocate allocates blocks
        // and extends the file, or returns an error number.
        match unsafe { libc::posix_fallocate(file.as_raw_fd(), 0, len) } {
            0 => Ok(()),
            // A file system without it: write the zeros instead.
            libc::EOPNOTSUPP => write_zeros(file, bytes),
            errno => Err(io::Error::from_raw_os_error(errno)),
        }
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        write_zeros(file, bytes)
    }
}

/// Write `bytes` zeros at the start of `file` (its real disk space).
#[cfg(not(windows))]
fn write_zeros(file: &File, bytes: u64) -> io::Result<()> {
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

    /// Tests that move the global fallback counter take turns.
    static COUNTER: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ferrow-store-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Named temporary files a live buffer shows in its directory: one on
    /// Windows (deleted on close); none on Unix (unlinked once created).
    const LIVE: usize = if cfg!(windows) { 1 } else { 0 };

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
    fn a_scratch_buffer_lives_in_a_temporary_file_and_leaves_nothing() {
        let _turn = COUNTER.lock().unwrap_or_else(|e| e.into_inner());
        let dir = scratch("column");
        let mut buf = Scratch::<u32>::zeroed(100_000, Some(&dir));
        assert!(buf.is_mapped());
        assert_eq!((buf.file_bytes(), buf.heap_bytes()), (400_000, 0));
        assert!(buf.iter().all(|&v| v == 0), "zero-filled");
        for (i, v) in buf.iter_mut().enumerate() {
            *v = (i as u32).wrapping_mul(2_654_435_761);
        }
        assert_eq!(leftovers(&dir), LIVE, "one temporary file while it lives");
        let col = buf.freeze();
        assert!(col.is_mapped() && col.heap_bytes() == 0);
        assert_eq!((col.len(), col.mapped_bytes()), (100_000, 400_000));
        assert!(
            col.iter()
                .enumerate()
                .all(|(i, &v)| v == (i as u32).wrapping_mul(2_654_435_761))
        );
        drop(col);
        assert_eq!(leftovers(&dir), 0, "gone with the column");
        // Empty buffers need no file.
        let empty = Scratch::<u64>::try_zeroed(0, &dir).unwrap();
        assert!(empty.is_empty() && !empty.is_mapped());
        // No directory: `try_zeroed` fails and leaves nothing; `zeroed` goes
        // to the heap with the same zeroes.
        assert!(Scratch::<u8>::try_zeroed(10, &dir.join("missing")).is_err());
        let heap = Scratch::<u8>::zeroed(10, Some(&dir.join("missing")));
        assert!(!heap.is_mapped() && heap.iter().all(|&v| v == 0));
        let none = Scratch::<u8>::zeroed(10, None);
        assert!(!none.is_mapped());
        assert_eq!(leftovers(&dir), 0);
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
        assert_eq!((app.fallback(), app.failure()), (None, None));
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
    fn an_appender_that_may_use_the_heap_continues_there_with_everything() {
        let _turn = COUNTER.lock().unwrap_or_else(|e| e.into_inner());
        let dir = scratch("degrade");
        let counted = scratch_in_heap();
        let mut app = Appender::<u32>::new(&dir)
            .unwrap()
            .allow_heap(DEGRADE_LIMIT);
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
        assert_eq!((app.file_bytes(), app.failure()), (0, None));
        // A second failure report (a flush that fails again) changes nothing:
        // the column is not in a file any more.
        app.degrade(&io::Error::other("again"));
        assert_eq!((app.fallback(), app.failure()), (Some("disk full"), None));
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
        assert_eq!(scratch_in_heap(), counted + 1, "one fallback, counted once");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_appender_that_may_not_use_the_heap_loses_its_column_and_says_so() {
        let dir = scratch("lost");
        // The default: no heap. The failure is reported at once, the file is
        // gone, later appends are ignored and `finish` is an error.
        let mut app = Appender::<u32>::new(&dir).unwrap();
        app.extend_from_slice(&vec![7u32; APPEND_BUFFER / 4 + 1]);
        assert_eq!(leftovers(&dir), LIVE);
        app.degrade(&io::Error::other("disk full"));
        assert_eq!(app.failure(), Some("disk full"));
        assert_eq!(leftovers(&dir), 0, "its disk space returns");
        app.push(1);
        assert!(app.finish().is_err());
        // Allowed, but the column is above the limit: lost, not read back.
        let mut app = Appender::<u32>::new(&dir).unwrap().allow_heap(1000);
        app.extend_from_slice(&vec![7u32; 5000]);
        app.degrade(&io::Error::other("disk full"));
        assert!(app.failure().is_some_and(|why| why.contains("too many")));
        assert!(app.finish().is_err());
        // The limit has a ceiling of its own.
        let capped = Appender::<u8>::new(&dir).unwrap().allow_heap(usize::MAX);
        assert_eq!(capped.heap_limit, DEGRADE_LIMIT);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_appender_without_a_directory_collects_in_the_heap() {
        let _turn = COUNTER.lock().unwrap_or_else(|e| e.into_inner());
        let dir = scratch("orheap");
        let counted = scratch_in_heap();
        let mut app = Appender::<u16>::or_heap(&dir.join("missing"));
        assert!(app.fallback().is_some());
        assert_eq!(scratch_in_heap(), counted + 1);
        app.extend_from_slice(&[1, 2, 3]);
        let col = app.finish().unwrap();
        assert!(!col.is_mapped());
        assert_eq!(&col[..], &[1, 2, 3]);
        let mut app = Appender::<u16>::or_heap(&dir);
        assert_eq!(app.fallback(), None);
        app.push(9);
        assert!(app.finish().unwrap().is_mapped());
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
