//! Where the service's memory goes (FR-37).
//!
//! The service holds more than a plain loaded index: the live layer's record
//! maps, its delta and view, structures kept from the scan, sort orders and
//! whatever the allocator keeps without handing out. This module adds the
//! numbers up: every named structure by **capacity** (what the allocator was
//! asked for, not what is filled), the process's own counters, and an explicit
//! unattributed remainder. Nothing here changes behaviour; it is computed on
//! demand (once after a scan, once before a rescan, and by the memory
//! benchmark), never per request.
//!
//! Per-allocation allocator overhead (headers, size-class rounding) and free
//! memory the allocator still holds are not in any component: they are the
//! remainder, and [`HeapStats`] splits that remainder further where the
//! platform allows.

use std::fmt::Write as _;

/// How long a component's memory stays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Held for as long as the snapshot or live session lives.
    Steady,
    /// Built on first use and kept after (for example the frozen children
    /// index, built by the first folder rename or delete).
    Lazy,
    /// Held only while something runs (a scan, a save, a rescan).
    Transient,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Steady => "steady",
            Kind::Lazy => "lazy",
            Kind::Transient => "transient",
        }
    }
}

/// One named structure and its capacity in bytes.
#[derive(Debug, Clone)]
pub struct Part {
    pub name: String,
    pub bytes: u64,
    pub kind: Kind,
}

impl Part {
    pub fn new(name: impl Into<String>, bytes: usize, kind: Kind) -> Part {
        Part {
            name: name.into(),
            bytes: bytes as u64,
            kind,
        }
    }
}

/// Allocation table of a `Vec` by capacity.
pub fn vec_bytes<T>(v: &Vec<T>) -> usize {
    v.capacity() * size_of::<T>()
}

/// Table allocation of a standard `HashMap`/`HashSet` slot of `slot` bytes
/// with room for `capacity` entries: the buckets (a power of two holding 7/8
/// of them as usable room) plus one control byte each and one 16-byte group.
/// An estimate of the hashbrown layout; values owned through pointers (a
/// `Vec` per key, say) are counted by the caller.
pub fn table_bytes(capacity: usize, slot: usize) -> usize {
    if capacity == 0 {
        return 0;
    }
    let mut buckets = 4usize;
    loop {
        let usable = if buckets < 8 {
            buckets - 1
        } else {
            buckets / 8 * 7
        };
        if usable >= capacity {
            break;
        }
        buckets *= 2;
    }
    buckets * slot + buckets + 16
}

/// Counters of the allocator itself.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HeapStats {
    /// Bytes handed out and not freed.
    pub in_use: u64,
    /// Bytes the allocator holds from the system (in use plus free).
    pub committed: u64,
}

/// What the operating system says about this process.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProcessMemory {
    /// Private (committed, unshared) bytes where the platform reports it:
    /// `PrivateUsage` on Windows, anonymous resident bytes on Linux.
    pub private: Option<u64>,
    /// Resident bytes (working set).
    pub resident: u64,
    /// Highest private (Windows: peak commit) or resident (Linux: high-water
    /// mark since the last [`reset_peak`]) value.
    pub peak: Option<u64>,
    pub heap: Option<HeapStats>,
}

impl ProcessMemory {
    /// The best private-bytes figure: private when known, else resident.
    pub fn bytes(&self) -> u64 {
        self.private.unwrap_or(self.resident)
    }
}

/// Read this process's memory counters: one `GetProcessMemoryInfo` call on
/// Windows, one `/proc/self/status` read on Linux. No allocator counters:
/// those walk or lock the heap, so only measurement code asks for them
/// ([`process_with_heap`]); the service never does.
pub fn process() -> ProcessMemory {
    read(false)
}

/// [`process`] plus the allocator's own counters (`HeapSummary` on Windows,
/// `mallinfo2` on glibc). Measurement only: `HeapSummary` walks the heap under
/// its lock, which must never happen on a service thread.
pub fn process_with_heap() -> ProcessMemory {
    read(true)
}

fn read(with_heap: bool) -> ProcessMemory {
    #[cfg(windows)]
    {
        windows::process(with_heap)
    }
    #[cfg(target_os = "linux")]
    {
        linux::process(with_heap)
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = with_heap;
        let (resident, private) = crate::health::memory();
        ProcessMemory {
            private,
            resident,
            peak: None,
            heap: None,
        }
    }
}

/// Start a new peak window (measurement only). Linux resets the resident
/// high-water mark. Windows has no reset, so the first call starts a thread
/// that samples the private bytes every millisecond into the window's maximum;
/// [`process`] then reports that maximum as `peak`. The service never calls
/// this: its `peak` stays the whole-run peak commit. Windows peaks are lower
/// bounds: a spike shorter than the sampling interval is missed, and the real
/// scan's 16 MiB read buffers (a few in flight) are not modelled.
pub fn reset_peak() {
    #[cfg(target_os = "linux")]
    linux::reset_peak();
    #[cfg(windows)]
    windows::reset_peak();
}

/// Ask the allocator to give free memory back to the system (glibc's
/// `malloc_trim`, Windows `HeapCompact` on the process heap); false where
/// there is no such call or it freed nothing. Measurement only: it shows how
/// much of the remainder is free memory the allocator still holds.
pub fn release_free_heap() -> bool {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        // SAFETY: no preconditions.
        unsafe { libc::malloc_trim(0) != 0 }
    }
    #[cfg(windows)]
    {
        windows::compact_heap()
    }
    #[cfg(not(any(all(target_os = "linux", target_env = "gnu"), windows)))]
    {
        false
    }
}

#[cfg(windows)]
mod windows {
    use super::{HeapStats, ProcessMemory};
    use std::sync::Once;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Highest private bytes seen since the last [`reset_peak`].
    static WINDOW_MAX: AtomicU64 = AtomicU64::new(0);
    static SAMPLER: Once = Once::new();

    pub fn reset_peak() {
        SAMPLER.call_once(|| {
            let _ = std::thread::Builder::new()
                .name("memory-peak-sampler".into())
                .spawn(|| {
                    loop {
                        if let Some(c) = counters() {
                            WINDOW_MAX.fetch_max(c.PrivateUsage as u64, Ordering::Relaxed);
                        }
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                });
        });
        WINDOW_MAX.store(
            counters().map_or(0, |c| c.PrivateUsage as u64),
            Ordering::Relaxed,
        );
    }

    /// Defragment the default process heap and decommit what it can
    /// (`HeapCompact`); true when it returned a non-zero largest free block.
    pub fn compact_heap() -> bool {
        use windows_sys::Win32::System::Memory::{GetProcessHeap, HeapCompact};
        // SAFETY: the process heap handle is valid for the process's life.
        unsafe { HeapCompact(GetProcessHeap(), 0) != 0 }
    }

    fn counters() -> Option<windows_sys::Win32::System::ProcessStatus::PROCESS_MEMORY_COUNTERS_EX> {
        use windows_sys::Win32::System::ProcessStatus::{
            K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        let mut c = PROCESS_MEMORY_COUNTERS_EX {
            cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
            PageFaultCount: 0,
            PeakWorkingSetSize: 0,
            WorkingSetSize: 0,
            QuotaPeakPagedPoolUsage: 0,
            QuotaPagedPoolUsage: 0,
            QuotaPeakNonPagedPoolUsage: 0,
            QuotaNonPagedPoolUsage: 0,
            PagefileUsage: 0,
            PeakPagefileUsage: 0,
            PrivateUsage: 0,
        };
        // SAFETY: the pseudo handle of this process and a correctly sized,
        // writable PROCESS_MEMORY_COUNTERS_EX (its layout begins with the
        // PROCESS_MEMORY_COUNTERS fields, as the API documents).
        let ok = unsafe {
            K32GetProcessMemoryInfo(
                GetCurrentProcess(),
                (&raw mut c).cast::<PROCESS_MEMORY_COUNTERS>(),
                c.cb,
            )
        };
        (ok != 0).then_some(c)
    }

    pub fn process(with_heap: bool) -> ProcessMemory {
        let Some(c) = counters() else {
            return ProcessMemory::default();
        };
        let private = c.PrivateUsage as u64;
        // With a sampler running (measurement code called `reset_peak`) the
        // window's maximum; otherwise the whole-run peak commit.
        let peak = if SAMPLER.is_completed() {
            WINDOW_MAX
                .fetch_max(private, Ordering::Relaxed)
                .max(private)
        } else {
            c.PeakPagefileUsage as u64
        };
        ProcessMemory {
            private: Some(private),
            resident: c.WorkingSetSize as u64,
            peak: Some(peak),
            heap: if with_heap { heap() } else { None },
        }
    }

    /// The default process heap (what Rust's allocator uses on Windows):
    /// bytes allocated by the program against bytes committed to the heap.
    fn heap() -> Option<HeapStats> {
        use windows_sys::Win32::System::Memory::{GetProcessHeap, HEAP_SUMMARY, HeapSummary};
        // SAFETY: the process heap handle is valid for the process's life;
        // `summary` is a writable HEAP_SUMMARY whose `cb` is set.
        unsafe {
            let mut summary: HEAP_SUMMARY = std::mem::zeroed();
            summary.cb = size_of::<HEAP_SUMMARY>() as u32;
            if HeapSummary(GetProcessHeap(), 0, &mut summary) == 0 {
                return None;
            }
            Some(HeapStats {
                in_use: summary.cbAllocated as u64,
                committed: summary.cbCommitted as u64,
            })
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{HeapStats, ProcessMemory};

    /// `Name:   123 kB` lines of `/proc/self/status`, in bytes.
    fn status_kb(text: &str, key: &str) -> Option<u64> {
        let line = text.lines().find(|l| l.starts_with(key))?;
        let kb: u64 = line[key.len()..].split_whitespace().next()?.parse().ok()?;
        Some(kb * 1024)
    }

    pub fn process(with_heap: bool) -> ProcessMemory {
        let text = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        // `VmHWM` counts file-backed pages too, `private` (RssAnon) does not:
        // take the file-backed part (the binary, shared libraries) off the
        // high-water mark so a peak and a private figure compare. Approximate:
        // it uses the file-backed size now, not at the peak.
        let file = status_kb(&text, "RssFile:").unwrap_or(0);
        ProcessMemory {
            private: status_kb(&text, "RssAnon:"),
            resident: status_kb(&text, "VmRSS:").unwrap_or(0),
            peak: status_kb(&text, "VmHWM:").map(|hwm| hwm.saturating_sub(file)),
            heap: if with_heap { heap() } else { None },
        }
    }

    pub fn reset_peak() {
        // Writing 5 resets the resident high-water mark of this process.
        let _ = std::fs::write("/proc/self/clear_refs", "5");
    }

    #[cfg(target_env = "gnu")]
    fn heap() -> Option<HeapStats> {
        // SAFETY: no preconditions; returns a plain struct by value.
        let m = unsafe { libc::mallinfo2() };
        Some(HeapStats {
            in_use: (m.uordblks + m.hblkhd) as u64,
            committed: (m.arena + m.hblkhd) as u64,
        })
    }

    #[cfg(not(target_env = "gnu"))]
    fn heap() -> Option<HeapStats> {
        None
    }
}

/// Everything counted at one moment: the named parts, the process counters
/// and the entry count the per-entry figures divide by.
#[derive(Debug, Clone)]
pub struct Breakdown {
    pub entries: u64,
    pub parts: Vec<Part>,
    pub process: ProcessMemory,
    /// Private bytes before any index was built (the runtime and binary),
    /// subtracted from the remainder so it names only what the parts miss.
    pub baseline: u64,
}

impl Breakdown {
    /// Parts plus the cheap process counters: what the service uses.
    pub fn new(entries: usize, parts: Vec<Part>, baseline: u64) -> Breakdown {
        Breakdown {
            entries: entries as u64,
            parts,
            process: process(),
            baseline,
        }
    }

    /// [`Breakdown::new`] with the allocator's counters too; measurement code
    /// only (see [`process_with_heap`]).
    pub fn new_with_heap(entries: usize, parts: Vec<Part>, baseline: u64) -> Breakdown {
        Breakdown {
            process: process_with_heap(),
            ..Breakdown::new(entries, parts, baseline)
        }
    }

    /// Bytes of every part present at the measurement: what the named
    /// structures add up to, compared with the process's private bytes. A
    /// part's [`Kind`] says how long it stays, not whether it is counted; a
    /// breakdown lists only what exists at that moment.
    pub fn accounted(&self) -> u64 {
        self.parts.iter().map(|p| p.bytes).sum()
    }

    /// Private bytes minus the baseline and every part: allocator overhead
    /// and free memory it still holds, plus anything not counted. Negative
    /// when the parts add up to more than the process holds.
    pub fn unattributed(&self) -> i64 {
        self.process.bytes() as i64 - self.baseline as i64 - self.accounted() as i64
    }

    pub fn per_entry(&self, bytes: i64) -> f64 {
        if self.entries == 0 {
            0.0
        } else {
            bytes as f64 / self.entries as f64
        }
    }

    /// `FERROW-MEMORY` lines: one per component, then the process counters
    /// and the total with its remainder.
    pub fn lines(&self, phase: &str) -> Vec<String> {
        let mut out = Vec::new();
        for p in &self.parts {
            out.push(format!(
                "FERROW-MEMORY phase={phase} component={} bytes={} per_entry={:.3} kind={}",
                p.name,
                p.bytes,
                self.per_entry(p.bytes as i64),
                p.kind.as_str()
            ));
        }
        let pm = &self.process;
        let mut line = format!(
            "FERROW-MEMORY-PROCESS phase={phase} private_bytes={} resident_bytes={} baseline_bytes={}",
            pm.private.map_or("na".into(), |v| v.to_string()),
            pm.resident,
            self.baseline
        );
        if let Some(peak) = pm.peak {
            let _ = write!(line, " peak_bytes={peak}");
        }
        if let Some(h) = pm.heap {
            let _ = write!(
                line,
                " heap_in_use_bytes={} heap_committed_bytes={} heap_free_held_bytes={}",
                h.in_use,
                h.committed,
                h.committed.saturating_sub(h.in_use)
            );
        }
        out.push(line);
        let rest = self.unattributed();
        out.push(format!(
            "FERROW-MEMORY-TOTAL phase={phase} entries={} accounted_bytes={} accounted_per_entry={:.3} process_bytes={} process_per_entry={:.3} unattributed_bytes={rest} unattributed_per_entry={:.3}",
            self.entries,
            self.accounted(),
            self.per_entry(self.accounted() as i64),
            pm.bytes(),
            self.per_entry(pm.bytes() as i64),
            self.per_entry(rest),
        ));
        out
    }

    /// Group name of a component: the part before the first dot.
    fn group(name: &str) -> &str {
        name.split('.').next().unwrap_or(name)
    }

    /// The service's event-log lines for this breakdown (each within the log's
    /// 200-character limit): process and groups with the remainder, then the
    /// largest components in bytes per entry.
    pub fn event_lines(&self, when: &str) -> Vec<String> {
        let mib = |b: u64| format!("{} MiB", b / (1 << 20));
        let pm = &self.process;
        let mut groups: Vec<(&str, u64)> = Vec::new();
        for p in self.parts.iter().filter(|p| p.kind != Kind::Transient) {
            let g = Self::group(&p.name);
            match groups.iter_mut().find(|(n, _)| *n == g) {
                Some((_, b)) => *b += p.bytes,
                None => groups.push((g, p.bytes)),
            }
        }
        let mut summary = format!(
            "memory {when}: {} entries, {} {}",
            self.entries,
            mib(pm.bytes()),
            if pm.private.is_some() {
                "private"
            } else {
                "resident"
            },
        );
        for (name, bytes) in &groups {
            let _ = write!(
                summary,
                ", {name} {:.1} B/entry",
                self.per_entry(*bytes as i64)
            );
        }
        let _ = write!(
            summary,
            ", unattributed {:.1} B/entry (incl. runtime)",
            self.per_entry(self.unattributed())
        );
        let mut ranked: Vec<&Part> = self
            .parts
            .iter()
            .filter(|p| p.kind != Kind::Transient)
            .collect();
        ranked.sort_by_key(|p| std::cmp::Reverse(p.bytes));
        let mut detail = format!("memory {when} B/entry:");
        for p in ranked {
            let piece = format!(" {} {:.1}", p.name, self.per_entry(p.bytes as i64));
            if detail.len() + piece.len() > 190 {
                break;
            }
            detail.push_str(&piece);
        }
        vec![summary, detail]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_bytes_follows_the_bucket_growth() {
        assert_eq!(table_bytes(0, 16), 0);
        // 1..=3 entries fit 4 buckets, 4..=7 fit 8, 8..=14 fit 16, 15..=28 fit 32.
        assert_eq!(table_bytes(3, 16), 4 * 16 + 4 + 16);
        assert_eq!(table_bytes(7, 16), 8 * 16 + 8 + 16);
        assert_eq!(table_bytes(14, 16), 16 * 16 + 16 + 16);
        assert_eq!(table_bytes(15, 16), 32 * 16 + 32 + 16);
        // The real thing agrees: a map's reported capacity maps to the same
        // bucket count, so its table is what `table_bytes` computes.
        for n in [1usize, 5, 100, 1000, 70_000] {
            let mut map = std::collections::HashMap::<u64, u64>::new();
            for i in 0..n as u64 {
                map.insert(i, i);
            }
            let cap = map.capacity();
            let buckets = (cap * 8 / 7).next_power_of_two().max(4);
            assert_eq!(
                table_bytes(cap, 16),
                buckets * 16 + buckets + 16,
                "capacity {cap}"
            );
        }
    }

    #[test]
    fn the_remainder_is_private_minus_baseline_and_every_present_part() {
        let parts = vec![
            Part::new("index.names", 400, Kind::Steady),
            Part::new("live.children", 100, Kind::Lazy),
            Part::new("save.buffer", 1000, Kind::Transient),
        ];
        let b = Breakdown {
            entries: 10,
            parts,
            process: ProcessMemory {
                private: Some(800),
                resident: 900,
                peak: None,
                heap: None,
            },
            baseline: 50,
        };
        assert_eq!(b.accounted(), 1500, "every present part is counted");
        assert_eq!(b.unattributed(), -750);
        let lines = b.lines("test");
        assert!(lines[0].contains("component=index.names bytes=400 per_entry=40.000"));
        let total = lines.last().unwrap();
        assert!(total.contains("unattributed_bytes=-750 unattributed_per_entry=-75.000"));
        for line in b.event_lines("test") {
            assert!(line.chars().count() <= 200, "{line}");
        }
    }

    #[test]
    fn this_process_reports_its_own_memory() {
        let before = process();
        assert!(before.resident > 0);
        assert!(
            before.heap.is_none(),
            "the service's counters never walk or lock the heap"
        );
        #[cfg(any(windows, all(target_os = "linux", target_env = "gnu")))]
        assert!(process_with_heap().heap.is_some_and(|h| h.in_use > 0));
        // The figure is the whole process's, and tests running in parallel
        // allocate and free at the same time (one freed 32 MB right then on
        // Ubuntu CI, FR-51). A working meter shows the 64 MB in at least one
        // of a few tries; a broken one never does.
        let mut seen = Vec::new();
        for _ in 0..5 {
            let before = process();
            let big = vec![1u8; 64 << 20];
            let after = process();
            std::hint::black_box(&big);
            if after.bytes() >= before.bytes() + (32 << 20) {
                return;
            }
            seen.push((before, after));
        }
        panic!("64 MB never showed: {seen:?}");
    }
}
