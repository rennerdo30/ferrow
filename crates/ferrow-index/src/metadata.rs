//! Packed optional metadata, exact overflows and background sort preparation.
use crate::{LoadError, RawEntry};
use std::cmp::Ordering;
use std::sync::{
    Arc, Condvar, Mutex, OnceLock,
    atomic::{AtomicBool, AtomicU8, Ordering as AtomicOrdering},
};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColumnSet(u32);
impl ColumnSet {
    pub const NONE: Self = Self(0);
    pub const SIZE: Self = Self(1);
    pub const MODIFIED: Self = Self(2);
    pub const CREATED: Self = Self(4);
    pub const ACCESSED: Self = Self(8);
    pub const ATTRIBUTES: Self = Self(16);
    pub const ALL: Self = Self(31);
    pub const DEFAULT: Self = Self(3);
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
    pub const fn bits(self) -> u32 {
        self.0
    }
    pub const fn from_bits(bits: u32) -> Option<Self> {
        if bits & !31 == 0 {
            Some(Self(bits))
        } else {
            None
        }
    }
}
impl Default for ColumnSet {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Times use signed Unix 100ns ticks. Packed dates have whole-second precision.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EntryMetadata {
    pub size: Option<u64>,
    pub modified: Option<i64>,
    pub created: Option<i64>,
    pub accessed: Option<i64>,
    pub attributes: Option<u32>,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SortKey {
    #[default]
    Name,
    Size,
    Modified,
    Created,
    Accessed,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortStatus {
    Unprepared,
    Pending,
    Ready,
    Unavailable,
    Failed,
}

#[derive(Debug, Default)]
struct Signal {
    state: AtomicU8,
    retired: AtomicBool,
    pinned: AtomicBool,
    requests: Mutex<Vec<Arc<AtomicBool>>>,
    wait: Mutex<()>,
    ready: Condvar,
}
#[derive(Debug)]
struct PreparationCancelled;
const PREPARE_INTERVAL: usize = 1024;
impl Signal {
    fn status(&self) -> SortStatus {
        match self.state.load(AtomicOrdering::Acquire) {
            0 => SortStatus::Unprepared,
            1 => SortStatus::Pending,
            2 => SortStatus::Ready,
            _ => SortStatus::Failed,
        }
    }
    fn finish(&self, state: u8) {
        let _guard = self.wait.lock().unwrap_or_else(|e| e.into_inner());
        self.state.store(state, AtomicOrdering::Release);
        self.ready.notify_all();
    }
}
/// Wait only on a worker, then post a GUI event. Query paths never wait on this.
#[derive(Debug, Clone)]
pub struct SortTicket(Arc<Signal>);
impl SortTicket {
    pub fn status(&self) -> SortStatus {
        self.0.status()
    }
    pub fn wait_timeout(&self, timeout: Duration) -> SortStatus {
        let Some(deadline) = Instant::now().checked_add(timeout) else {
            return self.status();
        };
        let mut guard = self.0.wait.lock().unwrap_or_else(|e| e.into_inner());
        while self.status() == SortStatus::Pending {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            let (next, result) = self
                .0
                .ready
                .wait_timeout(guard, left)
                .unwrap_or_else(|e| e.into_inner());
            guard = next;
            if result.timed_out() {
                break;
            }
        }
        self.status()
    }
}
#[derive(Debug)]
struct PackedIds {
    bytes: Vec<u8>,
    width: usize,
    known: usize,
}
impl PackedIds {
    fn width(n: usize) -> usize {
        if n <= 1 << 23 {
            3
        } else if n <= 1 << 31 {
            4
        } else {
            5
        }
    }
    fn from_ids(
        ids: &[u32],
        known: usize,
        boundary: impl Fn(usize) -> bool,
        check: impl Fn() -> Result<(), PreparationCancelled>,
    ) -> Result<Self, PreparationCancelled> {
        check()?;
        let width = Self::width(ids.len());
        let flag = 1u64 << (width * 8 - 1);
        let mut bytes = Vec::with_capacity(ids.len() * width);
        for (i, &id) in ids.iter().enumerate() {
            if i.is_multiple_of(PREPARE_INTERVAL) {
                check()?;
            }
            let code = u64::from(id) | if boundary(i) { flag } else { 0 };
            bytes.extend_from_slice(&code.to_le_bytes()[..width]);
        }
        Ok(Self {
            bytes,
            width,
            known,
        })
    }
    fn len(&self) -> usize {
        self.bytes.len() / self.width
    }
    fn group_start(&self, i: usize) -> bool {
        self.bytes[(i + 1) * self.width - 1] & 128 != 0
    }
    fn get(&self, i: usize) -> u32 {
        let bytes = &self.bytes[i * self.width..(i + 1) * self.width];
        match self.width {
            3 => u32::from_le_bytes([bytes[0], bytes[1], bytes[2] & 127, 0]),
            4 => u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3] & 127]),
            _ => u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        }
    }
}

trait Value: Copy + Default + Ord + Send + Sync + 'static {
    const WIDTH: usize;
    const RAW: usize;
    const MAX: u64;
    fn pack(self) -> (u64, Option<Self>);
    fn unpack(value: u64) -> Self;
    fn write(self, out: &mut Vec<u8>);
    fn read(bytes: &[u8]) -> Self;
}
macro_rules! raw {
    ($width:expr) => {
        const RAW: usize = $width;
        fn write(self, out: &mut Vec<u8>) {
            out.extend_from_slice(&self.to_le_bytes());
        }
        fn read(bytes: &[u8]) -> Self {
            Self::from_le_bytes(bytes.try_into().expect("checked width"))
        }
    };
}
impl Value for u64 {
    const WIDTH: usize = 5;
    const MAX: u64 = (1 << 40) - 1;
    raw!(8);
    fn pack(self) -> (u64, Option<Self>) {
        if self <= <Self as Value>::MAX {
            (self, None)
        } else {
            (<Self as Value>::MAX, Some(self))
        }
    }
    fn unpack(value: u64) -> Self {
        value
    }
}
impl Value for i64 {
    const WIDTH: usize = 4;
    const MAX: u64 = u32::MAX as u64;
    raw!(8);
    fn pack(self) -> (u64, Option<Self>) {
        let seconds = self.div_euclid(10_000_000);
        if (0..=u32::MAX as i64).contains(&seconds) {
            (seconds as u64, None)
        } else {
            (<Self as Value>::MAX, Some(self))
        }
    }
    fn unpack(value: u64) -> Self {
        value as i64 * 10_000_000
    }
}
impl Value for u32 {
    const WIDTH: usize = 4;
    const MAX: u64 = u32::MAX as u64;
    raw!(4);
    fn pack(self) -> (u64, Option<Self>) {
        (u64::from(self), None)
    }
    fn unpack(value: u64) -> Self {
        value as u32
    }
}
#[derive(Debug)]
struct Column<T> {
    packed: Vec<u8>,
    valid: Vec<u8>,
    overflow_ids: Vec<u32>,
    overflow_values: Vec<T>,
    order: OnceLock<PackedIds>,
    signal: Arc<Signal>,
}
fn take<'a>(data: &'a [u8], at: &mut usize, n: usize) -> Result<&'a [u8], LoadError> {
    let end = at
        .checked_add(n)
        .ok_or(LoadError::Corrupt("column size overflow"))?;
    let result = data.get(*at..end).ok_or(LoadError::Truncated)?;
    *at = end;
    Ok(result)
}
impl<T: Value> Column<T> {
    fn check_live(&self) -> Result<(), PreparationCancelled> {
        if self.signal.retired.load(AtomicOrdering::Relaxed)
            || (!self.signal.pinned.load(AtomicOrdering::Relaxed)
                && !self
                    .signal
                    .requests
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .iter()
                    .any(|cancel| !cancel.load(AtomicOrdering::Relaxed)))
        {
            return Err(PreparationCancelled);
        }
        Ok(())
    }
    // Stable name ties, one reusable half-size scratch buffer, and bounded
    // cancellation even in panic=abort builds. Only the small leaf sorts use
    // the standard library; merges check every PREPARE_INTERVAL output ids.
    fn sort_ids(
        &self,
        ids: &mut [u32],
        scratch: &mut Vec<u32>,
        check: &mut impl FnMut() -> Result<(), PreparationCancelled>,
    ) -> Result<(), PreparationCancelled> {
        check()?;
        if ids.len() <= PREPARE_INTERVAL {
            ids.sort_by(|&a, &b| self.cmp(a, b));
            return check();
        }
        let mid = ids.len() / 2;
        let (left, right) = ids.split_at_mut(mid);
        self.sort_ids(left, scratch, check)?;
        self.sort_ids(right, scratch, check)?;
        if self.cmp(ids[mid - 1], ids[mid]) != Ordering::Greater {
            return check();
        }
        scratch.clear();
        for chunk in ids[..mid].chunks(PREPARE_INTERVAL) {
            check()?;
            scratch.extend_from_slice(chunk);
        }
        let (mut left, mut right, mut out) = (0, mid, 0usize);
        while left < mid {
            if out.is_multiple_of(PREPARE_INTERVAL) {
                check()?;
            }
            if right == ids.len() || self.cmp(scratch[left], ids[right]) != Ordering::Greater {
                ids[out] = scratch[left];
                left += 1;
            } else {
                ids[out] = ids[right];
                right += 1;
            }
            out += 1;
        }
        check()
    }
    fn build(raw: &[RawEntry], get: impl Fn(&EntryMetadata) -> Option<T>) -> Arc<Self> {
        let mut c = Self {
            packed: Vec::with_capacity(raw.len() * T::WIDTH),
            valid: vec![0; raw.len().div_ceil(8)],
            overflow_ids: Vec::new(),
            overflow_values: Vec::new(),
            order: OnceLock::new(),
            signal: Arc::default(),
        };
        for (i, e) in raw.iter().enumerate() {
            let (packed, overflow) = get(&e.metadata).map_or((0, None), |value| {
                c.valid[i / 8] |= 1 << (i % 8);
                value.pack()
            });
            c.packed
                .extend_from_slice(&packed.to_le_bytes()[..T::WIDTH]);
            if let Some(value) = overflow {
                c.overflow_ids.push(i as u32);
                c.overflow_values.push(value);
            }
        }
        c.overflow_ids.shrink_to_fit();
        c.overflow_values.shrink_to_fit();
        Arc::new(c)
    }
    fn code(&self, id: usize) -> u64 {
        let mut bytes = [0; 8];
        bytes[..T::WIDTH].copy_from_slice(&self.packed[id * T::WIDTH..(id + 1) * T::WIDTH]);
        u64::from_le_bytes(bytes)
    }
    fn get(&self, id: u32) -> Option<T> {
        let i = id as usize;
        if self.valid[i / 8] & (1 << (i % 8)) == 0 {
            return None;
        }
        let code = self.code(i);
        if code == T::MAX
            && let Ok(slot) = self.overflow_ids.binary_search(&id)
        {
            return Some(self.overflow_values[slot]);
        }
        Some(T::unpack(code))
    }
    fn cmp(&self, a: u32, b: u32) -> Ordering {
        match (self.get(a), self.get(b)) {
            (Some(a), Some(b)) => a.cmp(&b),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            _ => Ordering::Equal,
        }
    }
    #[cfg(test)]
    fn request(self: &Arc<Self>, names: Arc<Vec<u32>>) -> SortStatus {
        self.request_using(names, || {})
    }
    #[cfg(any(test, feature = "test-hooks"))]
    fn request_using(
        self: &Arc<Self>,
        names: Arc<Vec<u32>>,
        before: impl FnOnce() + Send + 'static,
    ) -> SortStatus {
        self.request_for(names, None, before)
    }
    fn request_for(
        self: &Arc<Self>,
        names: Arc<Vec<u32>>,
        cancel: Option<Arc<AtomicBool>>,
        before: impl FnOnce() + Send + 'static,
    ) -> SortStatus {
        let status = self.signal.status();
        if matches!(status, SortStatus::Ready | SortStatus::Failed) {
            return status;
        }
        // Admission and completion share this lock: a live request arriving
        // while cancellation unwinds must either keep this worker or start one.
        let mut requests = self
            .signal
            .requests
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let status = self.signal.status();
        if !matches!(status, SortStatus::Unprepared | SortStatus::Pending)
            || cancel
                .as_ref()
                .is_some_and(|c| c.load(AtomicOrdering::Relaxed))
        {
            return status;
        }
        if let Some(cancel) = cancel {
            requests.retain(|c| !c.load(AtomicOrdering::Relaxed));
            if !requests.iter().any(|c| Arc::ptr_eq(c, &cancel)) {
                requests.push(cancel);
            }
        } else {
            self.signal.pinned.store(true, AtomicOrdering::Relaxed);
        }
        if self
            .signal
            .state
            .compare_exchange(0, 1, AtomicOrdering::AcqRel, AtomicOrdering::Acquire)
            .is_ok()
        {
            let column = self.clone();
            let launched = std::thread::Builder::new()
                .name("ferrow-sort".into())
                .spawn(move || {
                    let signal = column.signal.clone();
                    let mut before = Some(before);
                    loop {
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            if let Some(before) = before.take() {
                                before();
                            }
                            column.check_live()?;
                            let mut ids = Vec::with_capacity(names.len());
                            for chunk in names.chunks(PREPARE_INTERVAL) {
                                column.check_live()?;
                                ids.extend_from_slice(chunk);
                            }
                            column.check_live()?;
                            // Stable input name order preserves ties. This dedicated
                            // thread never occupies or waits for the query Rayon pool.
                            let mut scratch = Vec::with_capacity(if ids.len() > PREPARE_INTERVAL {
                                ids.len() / 2
                            } else {
                                0
                            });
                            column.sort_ids(&mut ids, &mut scratch, &mut || column.check_live())?;
                            drop(scratch);
                            column.check_live()?;
                            let known = ids.partition_point(|&id| column.get(id).is_some());
                            let order = PackedIds::from_ids(
                                &ids,
                                known,
                                |i| i == 0 || column.cmp(ids[i - 1], ids[i]) != Ordering::Equal,
                                || column.check_live(),
                            )?;
                            column.check_live()?;
                            let _ = column.order.set(order);
                            Ok::<_, PreparationCancelled>(())
                        }));
                        let state = match result {
                            Ok(Ok(())) => 2,
                            Ok(Err(_)) => 0,
                            Err(_) => 3,
                        };
                        let mut requests =
                            signal.requests.lock().unwrap_or_else(|e| e.into_inner());
                        if state == 0
                            && !column.signal.retired.load(AtomicOrdering::Relaxed)
                            && (column.signal.pinned.load(AtomicOrdering::Relaxed)
                                || requests.iter().any(|c| !c.load(AtomicOrdering::Relaxed)))
                        {
                            // New demand arrived after the cancellation checkpoint.
                            // Old scratch has already dropped; reuse this worker.
                            continue;
                        }
                        *requests = Vec::new();
                        column.signal.pinned.store(false, AtomicOrdering::Relaxed);
                        // A completion ticket retains only the small signal, never
                        // retired columns or the name permutation.
                        drop(column);
                        drop(names);
                        // Finish under the admission lock so no request observes a
                        // pending worker that has already decided to stop.
                        signal.finish(state);
                        drop(requests);
                        break;
                    }
                });
            if launched.is_err() {
                *requests = Vec::new();
                self.signal.pinned.store(false, AtomicOrdering::Relaxed);
                self.signal.finish(3);
            }
        }
        self.signal.status()
    }
    fn heap_bytes(&self) -> usize {
        self.packed.capacity()
            + self.valid.capacity()
            + self.overflow_ids.capacity() * 4
            + self.overflow_values.capacity() * T::RAW
            + self.order.get().map_or(0, |o| o.bytes.capacity())
            + std::mem::size_of::<Self>()
            + std::mem::size_of::<Signal>()
            + self
                .signal
                .requests
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .capacity()
                * std::mem::size_of::<Arc<AtomicBool>>()
            + 4 * std::mem::size_of::<usize>()
    }
    fn serialized_len(&self) -> usize {
        4 + self.packed.len() + self.valid.len() + self.overflow_ids.len() * (4 + T::RAW)
    }
    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&(self.overflow_ids.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.packed);
        out.extend_from_slice(&self.valid);
        for id in &self.overflow_ids {
            out.extend_from_slice(&id.to_le_bytes());
        }
        for value in &self.overflow_values {
            value.write(out);
        }
    }
    fn read(data: &[u8], at: &mut usize, n: usize) -> Result<Arc<Self>, LoadError> {
        let k = u32::from_le_bytes(take(data, at, 4)?.try_into().expect("checked width")) as usize;
        if k > n {
            return Err(LoadError::Corrupt("too many metadata overflows"));
        }
        let packed = take(data, at, n * T::WIDTH)?.to_vec();
        let valid = take(data, at, n.div_ceil(8))?.to_vec();
        let overflow_ids: Vec<_> = take(data, at, k * 4)?
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| u32::from_le_bytes(*b))
            .collect();
        let overflow_values = take(data, at, k * T::RAW)?
            .chunks_exact(T::RAW)
            .map(T::read)
            .collect();
        let c = Self {
            packed,
            valid,
            overflow_ids,
            overflow_values,
            order: OnceLock::new(),
            signal: Arc::default(),
        };
        if !n.is_multiple_of(8) && c.valid.last().is_some_and(|v| *v >> (n % 8) != 0) {
            return Err(LoadError::Corrupt("noncanonical validity bitmap"));
        }
        for (byte, &mask) in c.valid.iter().enumerate() {
            if mask == 255 {
                continue;
            }
            for bit in 0..8 {
                let i = byte * 8 + bit;
                if i < n && mask & (1 << bit) == 0 && c.code(i) != 0 {
                    return Err(LoadError::Corrupt("noncanonical unknown metadata"));
                }
            }
        }
        for (slot, &id) in c.overflow_ids.iter().enumerate() {
            if id as usize >= n
                || (slot > 0 && c.overflow_ids[slot - 1] >= id)
                || c.valid[id as usize / 8] & (1 << (id % 8)) == 0
                || c.code(id as usize) != T::MAX
                || c.overflow_values[slot].pack().1.is_none()
            {
                return Err(LoadError::Corrupt("invalid metadata overflow"));
            }
        }
        Ok(Arc::new(c))
    }
}
#[derive(Debug, Default)]
pub(crate) struct MetadataColumns {
    size: Option<Arc<Column<u64>>>,
    modified: Option<Arc<Column<i64>>>,
    created: Option<Arc<Column<i64>>>,
    accessed: Option<Arc<Column<i64>>>,
    attributes: Option<Arc<Column<u32>>>,
}
impl Drop for MetadataColumns {
    fn drop(&mut self) {
        fn retire<T>(column: &Option<Arc<Column<T>>>) {
            if let Some(c) = column {
                c.signal.retired.store(true, AtomicOrdering::Relaxed);
            }
        }
        retire(&self.size);
        retire(&self.modified);
        retire(&self.created);
        retire(&self.accessed);
        retire(&self.attributes);
    }
}
impl MetadataColumns {
    #[cfg(feature = "test-hooks")]
    pub(crate) fn request_with_hook(
        &self,
        key: SortKey,
        names: Arc<Vec<u32>>,
        hook: Arc<dyn Fn() + Send + Sync>,
    ) -> SortStatus {
        match key {
            SortKey::Name => SortStatus::Ready,
            SortKey::Size => self.size.as_ref().map_or(SortStatus::Unavailable, |c| {
                c.request_using(names, move || hook())
            }),
            SortKey::Modified => self.modified.as_ref().map_or(SortStatus::Unavailable, |c| {
                c.request_using(names, move || hook())
            }),
            SortKey::Created => self.created.as_ref().map_or(SortStatus::Unavailable, |c| {
                c.request_using(names, move || hook())
            }),
            SortKey::Accessed => self.accessed.as_ref().map_or(SortStatus::Unavailable, |c| {
                c.request_using(names, move || hook())
            }),
        }
    }
    pub(crate) fn build(raw: &[RawEntry], set: ColumnSet) -> Self {
        Self {
            size: set
                .contains(ColumnSet::SIZE)
                .then(|| Column::build(raw, |m| m.size)),
            modified: set
                .contains(ColumnSet::MODIFIED)
                .then(|| Column::build(raw, |m| m.modified)),
            created: set
                .contains(ColumnSet::CREATED)
                .then(|| Column::build(raw, |m| m.created)),
            accessed: set
                .contains(ColumnSet::ACCESSED)
                .then(|| Column::build(raw, |m| m.accessed)),
            attributes: set
                .contains(ColumnSet::ATTRIBUTES)
                .then(|| Column::build(raw, |m| m.attributes)),
        }
    }
    pub(crate) fn present(&self) -> ColumnSet {
        ColumnSet(
            u32::from(self.size.is_some())
                | (u32::from(self.modified.is_some()) << 1)
                | (u32::from(self.created.is_some()) << 2)
                | (u32::from(self.accessed.is_some()) << 3)
                | (u32::from(self.attributes.is_some()) << 4),
        )
    }
    pub(crate) fn get(&self, id: u32) -> EntryMetadata {
        EntryMetadata {
            size: self.size.as_ref().and_then(|c| c.get(id)),
            modified: self.modified.as_ref().and_then(|c| c.get(id)),
            created: self.created.as_ref().and_then(|c| c.get(id)),
            accessed: self.accessed.as_ref().and_then(|c| c.get(id)),
            attributes: self.attributes.as_ref().and_then(|c| c.get(id)),
        }
    }
    pub(crate) fn request(
        &self,
        key: SortKey,
        names: Arc<Vec<u32>>,
        cancel: Option<Arc<AtomicBool>>,
    ) -> SortStatus {
        match key {
            SortKey::Name => SortStatus::Ready,
            SortKey::Size => self.size.as_ref().map_or(SortStatus::Unavailable, |c| {
                c.request_for(names, cancel, || {})
            }),
            SortKey::Modified => self.modified.as_ref().map_or(SortStatus::Unavailable, |c| {
                c.request_for(names, cancel, || {})
            }),
            SortKey::Created => self.created.as_ref().map_or(SortStatus::Unavailable, |c| {
                c.request_for(names, cancel, || {})
            }),
            SortKey::Accessed => self.accessed.as_ref().map_or(SortStatus::Unavailable, |c| {
                c.request_for(names, cancel, || {})
            }),
        }
    }
    pub(crate) fn ticket(&self, key: SortKey) -> Option<SortTicket> {
        let signal = match key {
            SortKey::Size => self.size.as_ref().map(|c| c.signal.clone()),
            SortKey::Modified => self.modified.as_ref().map(|c| c.signal.clone()),
            SortKey::Created => self.created.as_ref().map(|c| c.signal.clone()),
            SortKey::Accessed => self.accessed.as_ref().map(|c| c.signal.clone()),
            _ => None,
        };
        signal.map(SortTicket)
    }
    pub(crate) fn attributes(&self, id: u32) -> Option<u32> {
        self.attributes.as_ref().and_then(|c| c.get(id))
    }
    pub(crate) fn value(&self, key: SortKey, id: u32) -> Option<i128> {
        match key {
            SortKey::Size => self.size.as_ref().and_then(|c| c.get(id)).map(i128::from),
            SortKey::Modified => self
                .modified
                .as_ref()
                .and_then(|c| c.get(id))
                .map(i128::from),
            SortKey::Created => self
                .created
                .as_ref()
                .and_then(|c| c.get(id))
                .map(i128::from),
            SortKey::Accessed => self
                .accessed
                .as_ref()
                .and_then(|c| c.get(id))
                .map(i128::from),
            _ => None,
        }
    }
    pub(crate) fn order(&self, key: SortKey, descending: bool) -> Option<MetadataOrder<'_>> {
        let ids = match key {
            SortKey::Size => self.size.as_ref().and_then(|c| c.order.get()),
            SortKey::Modified => self.modified.as_ref().and_then(|c| c.order.get()),
            SortKey::Created => self.created.as_ref().and_then(|c| c.order.get()),
            SortKey::Accessed => self.accessed.as_ref().and_then(|c| c.order.get()),
            _ => None,
        }?;
        let result = MetadataOrder {
            columns: self,
            ids,
            key,
            descending,
            known: ids.known,
        };
        Some(result)
    }
    pub(crate) fn heap_bytes(&self) -> usize {
        self.size.as_ref().map_or(0, |c| c.heap_bytes())
            + self.attributes.as_ref().map_or(0, |c| c.heap_bytes())
            + [&self.modified, &self.created, &self.accessed]
                .into_iter()
                .flatten()
                .map(|c| c.heap_bytes())
                .sum::<usize>()
    }
    pub(crate) fn serialized_len(&self) -> usize {
        self.size.as_ref().map_or(0, |c| c.serialized_len())
            + self.attributes.as_ref().map_or(0, |c| c.serialized_len())
            + [&self.modified, &self.created, &self.accessed]
                .into_iter()
                .flatten()
                .map(|c| c.serialized_len())
                .sum::<usize>()
    }
    pub(crate) fn write(&self, out: &mut Vec<u8>) {
        if let Some(c) = &self.size {
            c.write(out);
        }
        for c in [&self.modified, &self.created, &self.accessed]
            .into_iter()
            .flatten()
        {
            c.write(out);
        }
        if let Some(c) = &self.attributes {
            c.write(out);
        }
    }
    pub(crate) fn read(
        data: &[u8],
        at: &mut usize,
        n: usize,
        set: ColumnSet,
    ) -> Result<Self, LoadError> {
        Ok(Self {
            size: if set.contains(ColumnSet::SIZE) {
                Some(Column::read(data, at, n)?)
            } else {
                None
            },
            modified: if set.contains(ColumnSet::MODIFIED) {
                Some(Column::read(data, at, n)?)
            } else {
                None
            },
            created: if set.contains(ColumnSet::CREATED) {
                Some(Column::read(data, at, n)?)
            } else {
                None
            },
            accessed: if set.contains(ColumnSet::ACCESSED) {
                Some(Column::read(data, at, n)?)
            } else {
                None
            },
            attributes: if set.contains(ColumnSet::ATTRIBUTES) {
                Some(Column::read(data, at, n)?)
            } else {
                None
            },
        })
    }
}
/// Prepared ids with a direction-aware range iterator. No allocation or waiting.
#[derive(Clone, Copy)]
pub struct MetadataOrder<'a> {
    columns: &'a MetadataColumns,
    ids: &'a PackedIds,
    key: SortKey,
    descending: bool,
    known: usize,
}
impl MetadataOrder<'_> {
    pub fn len(&self) -> usize {
        self.ids.len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    fn bound(&self, mut lo: usize, mut hi: usize, predicate: impl Fn(u32) -> bool) -> usize {
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if predicate(self.ids.get(mid)) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }
    pub fn range(&self, start: usize, end: usize) -> impl Iterator<Item = u32> + '_ {
        assert!(start <= end && end <= self.len());
        let mut virtual_pos = start;
        let mut group_end = 0;
        let mut group_pos = 0;
        std::iter::from_fn(move || {
            if virtual_pos == end {
                return None;
            }
            let id = if !self.descending || virtual_pos >= self.known {
                self.ids.get(virtual_pos)
            } else {
                if group_pos == group_end {
                    let mirrored = self.known - 1 - virtual_pos;
                    // The spare high bit marks value-group starts. Singleton
                    // and two-row groups need no random metadata reads.
                    let start = if self.ids.group_start(mirrored) {
                        mirrored
                    } else if mirrored > 0 && self.ids.group_start(mirrored - 1) {
                        mirrored - 1
                    } else {
                        let value = self.columns.value(self.key, self.ids.get(mirrored));
                        self.bound(0, mirrored, |id| self.columns.value(self.key, id) < value)
                    };
                    group_end = if mirrored + 1 == self.known || self.ids.group_start(mirrored + 1)
                    {
                        mirrored + 1
                    } else if mirrored + 2 == self.known || self.ids.group_start(mirrored + 2) {
                        mirrored + 2
                    } else {
                        let value = self.columns.value(self.key, self.ids.get(mirrored));
                        self.bound(mirrored + 1, self.known, |id| {
                            self.columns.value(self.key, id) <= value
                        })
                    };
                    group_pos = start + virtual_pos - (self.known - group_end);
                }
                let id = self.ids.get(group_pos);
                group_pos += 1;
                id
            };
            virtual_pos += 1;
            Some(id)
        })
    }
}
pub fn timestamp_ticks(time: std::time::SystemTime) -> Option<i64> {
    let (duration, negative) = match time.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => (d, false),
        Err(e) => (e.duration(), true),
    };
    let nanos = i128::try_from(duration.as_nanos()).ok()?;
    i64::try_from(if negative {
        (-nanos).div_euclid(100)
    } else {
        nanos / 100
    })
    .ok()
}

/// Whole-second precision in the packed epoch range; exact ticks outside it.
pub fn metadata_timestamp(time: std::time::SystemTime) -> Option<i64> {
    let ticks = timestamp_ticks(time)?;
    let (packed, overflow) = ticks.pack();
    Some(overflow.unwrap_or_else(|| i64::unpack(packed)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancellable_merge_matches_stable_oracle_and_returns_errors_normally() {
        let builder = crate::IndexBuilder::with_columns(ColumnSet::MODIFIED);
        for i in 0..4097 {
            builder.add_with_metadata(
                crate::ROOT_PARENT,
                format!("file-{i:04}").as_bytes(),
                0,
                EntryMetadata {
                    modified: (i % 7 != 0).then_some((i * 7919 % 101) * 10_000_000),
                    ..Default::default()
                },
            );
        }
        let index = builder.freeze();
        let column = index.metadata.modified.as_ref().unwrap();
        for len in [0, 4, 1024, 1025, 2049, 4097] {
            let mut ids = index.sorted[..len].to_vec();
            let mut expected = ids.clone();
            expected.sort_by(|&a, &b| column.cmp(a, b));
            let mut scratch = Vec::with_capacity(len / 2);
            column
                .sort_ids(&mut ids, &mut scratch, &mut || Ok(()))
                .unwrap();
            assert_eq!(ids, expected, "len={len}");
            assert_eq!(scratch.capacity(), len / 2);
        }
        let mut ids = (*index.sorted).clone();
        let mut calls = 0;
        let result = column.sort_ids(&mut ids, &mut Vec::new(), &mut || {
            calls += 1;
            if calls == 20 {
                Err(PreparationCancelled)
            } else {
                Ok(())
            }
        });
        assert!(result.is_err());
        assert_eq!(calls, 20);
        // Cancellation at the second packing block, without a timing race.
        let calls = std::cell::Cell::new(0usize);
        let result = PackedIds::from_ids(
            &index.sorted,
            0,
            |_| false,
            || {
                calls.set(calls.get() + 1);
                if calls.get() == 3 {
                    Err(PreparationCancelled)
                } else {
                    Ok(())
                }
            },
        );
        assert!(result.is_err());
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn cancelled_requests_stop_shared_preparation_and_allow_retry() {
        use std::sync::mpsc;
        for keep_second in [false, true] {
            let builder = crate::IndexBuilder::with_columns(ColumnSet::MODIFIED);
            for i in 0..4 {
                builder.add_with_metadata(
                    crate::ROOT_PARENT,
                    b"file",
                    0,
                    EntryMetadata {
                        modified: Some(i),
                        ..Default::default()
                    },
                );
            }
            let index = builder.freeze();
            let column = index.metadata.modified.as_ref().unwrap();
            let ticket = SortTicket(column.signal.clone());
            let first = Arc::new(AtomicBool::new(false));
            let second = Arc::new(AtomicBool::new(false));
            let (started_tx, started_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            column.request_for(index.sorted.clone(), Some(first.clone()), move || {
                started_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            });
            started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            assert_eq!(
                column.request_for(index.sorted.clone(), Some(second.clone()), || {
                    panic!("a second request must share the existing worker")
                }),
                SortStatus::Pending
            );
            first.store(true, AtomicOrdering::Relaxed);
            second.store(!keep_second, AtomicOrdering::Relaxed);
            release_tx.send(()).unwrap();
            assert_eq!(
                ticket.wait_timeout(Duration::from_secs(2)),
                if keep_second {
                    SortStatus::Ready
                } else {
                    SortStatus::Unprepared
                }
            );
            assert_eq!(column.order.get().is_some(), keep_second);
            assert_eq!(Arc::strong_count(&first), 1);
            assert_eq!(Arc::strong_count(&second), 1);
            if !keep_second {
                column.request_for(
                    index.sorted.clone(),
                    Some(Arc::new(AtomicBool::new(false))),
                    || {},
                );
                assert_eq!(
                    ticket.wait_timeout(Duration::from_secs(2)),
                    SortStatus::Ready
                );
            }
        }
    }

    #[test]
    fn retired_worker_releases_columns_and_names_before_signalling() {
        use std::sync::mpsc;
        let builder = crate::IndexBuilder::with_columns(ColumnSet::MODIFIED);
        for i in 0..4 {
            builder.add_with_metadata(
                crate::ROOT_PARENT,
                b"file",
                0,
                EntryMetadata {
                    modified: Some(i),
                    ..Default::default()
                },
            );
        }
        let index = builder.freeze();
        let column = index.metadata.modified.as_ref().unwrap();
        let weak_column = Arc::downgrade(column);
        let weak_names = Arc::downgrade(&index.sorted);
        let ticket = SortTicket(column.signal.clone());
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        column.request_using(index.sorted.clone(), move || {
            started_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        drop(index);
        release_tx.send(()).unwrap();
        assert_eq!(
            ticket.wait_timeout(Duration::from_secs(2)),
            SortStatus::Unprepared
        );
        assert!(weak_column.upgrade().is_none());
        assert!(weak_names.upgrade().is_none());
    }

    #[test]
    fn packed_boundaries_seconds_and_exact_overflows() {
        for value in [
            0,
            (1u64 << 32) - 1,
            1 << 32,
            (1 << 40) - 1,
            1 << 40,
            u64::MAX,
        ] {
            let (packed, overflow) = value.pack();
            assert_eq!(overflow.unwrap_or_else(|| u64::unpack(packed)), value);
            assert_eq!(overflow.is_some(), value >= 1 << 40);
        }
        for value in [
            i64::MIN,
            -1,
            0,
            10_000_001,
            u32::MAX as i64 * 10_000_000,
            (u32::MAX as i64 + 1) * 10_000_000,
            i64::MAX,
        ] {
            let (packed, overflow) = value.pack();
            let expected = if value >= 0 && value / 10_000_000 <= u32::MAX as i64 {
                value / 10_000_000 * 10_000_000
            } else {
                value
            };
            assert_eq!(overflow.unwrap_or_else(|| i64::unpack(packed)), expected);
        }
        assert_eq!(PackedIds::width(1 << 23), 3);
        assert_eq!(PackedIds::width((1 << 23) + 1), 4);
        let ids = PackedIds {
            bytes: vec![255, 255, 255, 0, 0, 0],
            width: 3,
            known: 0,
        };
        assert_eq!(ids.get(0), 0x7fffff);
        assert!(ids.group_start(0));
        assert_eq!(ids.get(1), 0);
        let ids = PackedIds {
            bytes: vec![255, 255, 255, 255, 128],
            width: 5,
            known: 0,
        };
        assert_eq!(ids.get(0), u32::MAX);
    }
    #[test]
    fn failed_preparation_is_explicit_and_not_retried() {
        let column = Column::<u64>::build(&[], |m| m.size);
        column.request_using(Arc::new(Vec::new()), || panic!("mock sort failure"));
        let ticket = SortTicket(column.signal.clone());
        assert_eq!(
            ticket.wait_timeout(Duration::from_secs(2)),
            SortStatus::Failed
        );
        assert_eq!(column.request(Arc::new(Vec::new())), SortStatus::Failed);
        assert!(column.order.get().is_none());
    }
}
