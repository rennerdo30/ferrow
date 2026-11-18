//! What the service knows about itself, for `Request::Health`.
//!
//! [`State`] holds the few facts that are not already in the snapshot: when
//! the last save and the last scan happened, what each volume is doing, the
//! journal's state, a pending rescan and the count of applied live changes.
//! Every one is written only when its event happens (a scan finished, a save
//! ended, a batch was applied); nothing is polled and nothing runs on a
//! request. The report is built when a caller asks, from this state, the
//! current snapshot, the access policy and the event ring.
//!
//! Authorization is the searches' own: a caller the access policy refuses
//! gets `Unauthorized`. `Rescan` additionally needs an administrator or
//! SYSTEM (root), whatever the policy allows others.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ferrow_ipc::health::{
    Health, JOURNAL_ERROR, JOURNAL_NONE, JOURNAL_RESET, LiveStats, MAX_EVENTS, MODE_UNAVAILABLE,
    MODE_WALK, RescanInfo, SaveInfo, ScanInfo, VolumeHealth, format_duration,
};
use ferrow_ipc::{ErrorCode, Request, Response};

use crate::access::{Caller, Policy};
use crate::{SnapshotStore, eventlog};

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// One root the service indexes, as the last scan left it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VolumeState {
    pub root: String,
    /// The `\\?\Volume{…}` device a journal reader watches (live volumes).
    pub device: Option<String>,
    pub mode: &'static str,
    pub reason: String,
    pub entries: Option<u64>,
    pub journal: &'static str,
    pub journal_detail: String,
}

impl VolumeState {
    pub fn walked(root: String, reason: &str) -> VolumeState {
        VolumeState {
            root,
            device: None,
            mode: if reason.is_empty() {
                MODE_WALK
            } else {
                MODE_UNAVAILABLE
            },
            reason: reason.into(),
            entries: None,
            journal: JOURNAL_NONE,
            journal_detail: String::new(),
        }
    }
}

#[derive(Default)]
struct Inner {
    last_save: Option<SaveInfo>,
    scan: Option<ScanInfo>,
    pending: Option<RescanInfo>,
    volumes: Vec<VolumeState>,
    /// When the last scan ended (the earliest a requested rescan may follow).
    last_finished: Option<Instant>,
    /// Scans started, by kind of reason.
    scans_by_reason: BTreeMap<String, u64>,
    /// The last in-memory compaction since the last scan, how many there were
    /// and their total time.
    compaction: Option<ScanInfo>,
    merge_blocked: Option<String>,
    compactions: u64,
    compaction_ms: u64,
}

/// Least time between the end of one scan and a requested rescan: an
/// administrator's own repeated requests must not keep the service scanning.
pub const MIN_FORCED_GAP: Duration = Duration::from_secs(60);

/// An index save in flight (see [`State::saving`]).
pub struct SavingGuard<'a>(&'a AtomicBool);

impl Drop for SavingGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

pub struct State {
    inner: Mutex<Inner>,
    started: Mutex<Option<(Instant, u64)>>,
    live_changes: AtomicU64,
    /// Bytes of the saved live tables read through a map.
    tables_mapped: AtomicU64,
    folder_reads: AtomicU64,
    folder_read_entries: AtomicU64,
    folder_read_ms: AtomicU64,
    folder_read_skipped: AtomicU64,
    folder_read_fallbacks: AtomicU64,
    connection_errors: AtomicU64,
    scanning: AtomicBool,
    /// A requested rescan was sent and has not started yet.
    forced: AtomicBool,
    /// The index is being written to disk (the saver holds it alive).
    saving: AtomicBool,
}

impl State {
    pub const fn new() -> State {
        State {
            inner: Mutex::new(Inner {
                last_save: None,
                scan: None,
                pending: None,
                volumes: Vec::new(),
                last_finished: None,
                scans_by_reason: BTreeMap::new(),
                compaction: None,
                merge_blocked: None,
                compactions: 0,
                compaction_ms: 0,
            }),
            started: Mutex::new(None),
            live_changes: AtomicU64::new(0),
            tables_mapped: AtomicU64::new(0),
            folder_reads: AtomicU64::new(0),
            folder_read_entries: AtomicU64::new(0),
            folder_read_ms: AtomicU64::new(0),
            folder_read_skipped: AtomicU64::new(0),
            folder_read_fallbacks: AtomicU64::new(0),
            connection_errors: AtomicU64::new(0),
            scanning: AtomicBool::new(false),
            forced: AtomicBool::new(false),
            saving: AtomicBool::new(false),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The service began serving (first call wins).
    pub fn mark_started(&self) {
        self.started
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert((Instant::now(), now_ms()));
    }

    pub fn scan_started(&self) {
        self.scanning.store(true, Ordering::Release);
        self.forced.store(false, Ordering::Release);
        self.lock().pending = None;
    }

    /// A scan failed: it still ended, so a requested rescan waits out the same
    /// gap (an instantly failing scan must not allow back-to-back rescans).
    pub fn scan_failed(&self) {
        self.lock().last_finished = Some(Instant::now());
        self.scanning.store(false, Ordering::Release);
    }

    /// A scan ended: its time and why, and the volumes as it found them. A
    /// journal reset is answered by the scan; a stopped reader stays reported.
    pub fn scan_finished(&self, reason: &str, took: Duration, mut volumes: Vec<VolumeState>) {
        let mut inner = self.lock();
        for volume in &mut volumes {
            let stopped = inner.volumes.iter().find(|old| {
                old.device.is_some() && old.device == volume.device && old.journal == JOURNAL_ERROR
            });
            if let Some(old) = stopped {
                volume.journal = JOURNAL_ERROR;
                volume.journal_detail = old.journal_detail.clone();
            }
        }
        inner.volumes = volumes;
        inner.last_finished = Some(Instant::now());
        // The index was built from the disks again: an earlier merge of live
        // changes says nothing about it.
        inner.compaction = None;
        inner.scan = Some(ScanInfo {
            at_ms: now_ms(),
            reason: reason.into(),
            took_ms: took.as_millis() as u64,
        });
        drop(inner);
        self.scanning.store(false, Ordering::Release);
    }

    /// A journal reset (rescan due) or a stopped reader (`fatal`).
    pub fn journal_problem(&self, device: &str, fatal: bool, detail: &str) {
        let mut inner = self.lock();
        for volume in inner
            .volumes
            .iter_mut()
            .filter(|v| v.device.as_deref() == Some(device))
        {
            if volume.journal != JOURNAL_ERROR {
                volume.journal = if fatal { JOURNAL_ERROR } else { JOURNAL_RESET };
                volume.journal_detail = detail.into();
            }
        }
    }

    /// The rescan waiting for its time, or none.
    pub fn set_pending(&self, pending: Option<(&str, Instant)>) {
        self.lock().pending = pending.map(|(reason, earliest)| RescanInfo {
            reason: reason.into(),
            earliest_ms: now_ms()
                + earliest
                    .saturating_duration_since(Instant::now())
                    .as_millis() as u64,
        });
    }

    /// A connection ended with an error (counted; never logged one by one).
    pub fn connection_error(&self) {
        self.connection_errors.fetch_add(1, Ordering::Relaxed);
    }

    /// The live layer now reads `bytes` of its saved tables through a map.
    pub fn set_tables_mapped(&self, bytes: usize) {
        self.tables_mapped.store(bytes as u64, Ordering::Relaxed);
    }

    pub fn add_live_changes(&self, n: usize) {
        self.live_changes.fetch_add(n as u64, Ordering::Relaxed);
    }

    /// A scan starts for `reason`: counted under its kind (see
    /// [`crate::live::reason_kind`]), so a storm of one kind is visible.
    pub fn count_scan(&self, reason: &str) {
        *self
            .lock()
            .scans_by_reason
            .entry(crate::live::reason_kind(reason).into())
            .or_default() += 1;
    }

    /// A compaction finished: the live layer grown past `reason`'s limit was
    /// folded into a new index in memory in `took`. Not a scan (no disk was
    /// read, [`State::is_scanning`] never said so): it has its own counters.
    /// Merges keep failing (`Some(why)`), or work again (`None`).
    pub fn merge_blocked(&self, why: Option<String>) {
        self.lock().merge_blocked = why;
    }

    pub fn compaction_done(&self, reason: &str, took: Duration) {
        let mut inner = self.lock();
        inner.merge_blocked = None;
        inner.compactions += 1;
        inner.compaction_ms += took.as_millis() as u64;
        inner.compaction = Some(ScanInfo {
            at_ms: now_ms(),
            reason: reason.into(),
            took_ms: took.as_millis() as u64,
        });
    }

    /// A background read of a moved-in folder finished with `entries`,
    /// `skipped` subfolders having vanished meanwhile.
    pub fn folder_read(&self, entries: usize, skipped: usize, took: Duration) {
        self.folder_read_skipped
            .fetch_add(skipped as u64, Ordering::Relaxed);
        self.folder_reads.fetch_add(1, Ordering::Relaxed);
        self.folder_read_entries
            .fetch_add(entries as u64, Ordering::Relaxed);
        self.folder_read_ms
            .fetch_add(took.as_millis() as u64, Ordering::Relaxed);
    }

    /// A background read failed or was too large: a rescan is asked for.
    pub fn folder_read_fell_back(&self) {
        self.folder_read_fallbacks.fetch_add(1, Ordering::Relaxed);
    }

    /// The index save starts; it is in flight until the guard is dropped (on
    /// success, failure or panic). The maintainer holds back an in-memory
    /// compaction meanwhile: the saver keeps the old index alive.
    pub fn saving(&self) -> SavingGuard<'_> {
        self.saving.store(true, Ordering::Release);
        SavingGuard(&self.saving)
    }

    /// Whether an index save is in flight.
    pub fn is_saving(&self) -> bool {
        self.saving.load(Ordering::Acquire)
    }

    pub fn save_done(&self, took: Duration, error: Option<String>) {
        self.lock().last_save = Some(SaveInfo {
            at_ms: now_ms(),
            ok: error.is_none(),
            error: error.unwrap_or_default(),
            took_ms: took.as_millis() as u64,
        });
    }

    pub fn is_scanning(&self) -> bool {
        self.scanning.load(Ordering::Acquire)
    }

    /// Claim the one requested rescan at `now`. Refused, with the reason and
    /// when it may run, while a scan runs, one is already on its way, or the
    /// last scan ended less than [`MIN_FORCED_GAP`] ago.
    pub fn claim_rescan(&self, now: Instant) -> Result<(), String> {
        if self.is_scanning() {
            return Err("a scan is already running and covers every volume".into());
        }
        if let Some(done) = self.lock().last_finished {
            let wait = (done + MIN_FORCED_GAP).saturating_duration_since(now);
            if !wait.is_zero() {
                return Err(format!(
                    "a scan ended {} ago; a rescan can be requested in {}",
                    format_duration(now.duration_since(done).as_millis() as u64),
                    format_duration(wait.as_millis() as u64)
                ));
            }
        }
        self.forced
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| "a rescan was already requested and is about to start".into())
    }

    /// Give the claim back (the request could not be delivered).
    pub fn release_rescan(&self) {
        self.forced.store(false, Ordering::Release);
    }

    /// The report for one request: built now from this state, the snapshot,
    /// the access policy and the event ring.
    pub fn report(&self, store: &SnapshotStore, policy: &Policy) -> Health {
        let snapshot = store.get();
        let now = now_ms();
        let (started_ms, uptime_s) = match *self.started.lock().unwrap_or_else(|e| e.into_inner()) {
            Some((at, ms)) => (ms, at.elapsed().as_secs()),
            None => (now, 0),
        };
        let (working_set, private_bytes) = memory();
        let private_working_set = private_working_set();
        // The ring has a lock of its own: read it before taking this state's.
        let events = eventlog::to_info(eventlog::global().recent(MAX_EVENTS));
        let inner = self.lock();
        let volumes = inner
            .volumes
            .iter()
            .map(|v| VolumeHealth {
                root: v.root.clone(),
                mode: v.mode.into(),
                reason: v.reason.clone(),
                entries: v.entries,
                journal: v.journal.into(),
                journal_detail: v.journal_detail.clone(),
                last_scan: inner.scan.clone(),
                rescan_pending: inner.pending.clone(),
            })
            .collect();
        let mut health = Health {
            version: env!("CARGO_PKG_VERSION").into(),
            pid: std::process::id(),
            started_ms,
            now_ms: now,
            uptime_s,
            working_set,
            private_bytes,
            private_working_set,
            index_mapped_bytes: snapshot.index.mapped_bytes() as u64
                + self.tables_mapped.load(Ordering::Relaxed),
            merge_blocked: inner.merge_blocked.clone(),
            entries: snapshot.entries() as u64,
            mode: snapshot.mode.clone(),
            access: policy.mode.as_str().into(),
            live_changes: self.live_changes.load(Ordering::Relaxed),
            scanning: self.is_scanning(),
            last_save: inner.last_save.clone(),
            volumes,
            connection_errors: self.connection_errors.load(Ordering::Relaxed),
            live_stats: LiveStats {
                folder_reads: self.folder_reads.load(Ordering::Relaxed),
                folder_read_entries: self.folder_read_entries.load(Ordering::Relaxed),
                folder_read_ms: self.folder_read_ms.load(Ordering::Relaxed),
                folder_read_skipped: self.folder_read_skipped.load(Ordering::Relaxed),
                folder_read_fallbacks: self.folder_read_fallbacks.load(Ordering::Relaxed),
                scans_by_reason: inner.scans_by_reason.clone(),
                compactions: inner.compactions,
                compaction_ms: inner.compaction_ms,
                last_compaction: inner.compaction.clone(),
            },
            events,
            ..Health::default()
        };
        drop(inner);
        health.fit();
        health
    }
}

impl Default for State {
    fn default() -> Self {
        State::new()
    }
}

/// The service's own state.
pub static STATE: State = State::new();

const UNAUTHORIZED: &str = "per-user visibility not yet available; the system service answers SYSTEM, accounts in the Administrators group (not sandboxed), root and accounts an administrator allowed with `ferrow service access`; development access requires its owner";

/// What a request is answered from.
pub struct Scope<'a> {
    /// The installed service (the development endpoint serves only its
    /// owner, who may rescan).
    pub system: bool,
    /// The caller may search (the access policy, or ownership).
    pub allowed: bool,
    pub state: &'a State,
    pub store: &'a SnapshotStore,
    pub policy: &'a Policy,
}

/// Answer `Health` or `Rescan`. `rescan` asks the live maintainer.
pub fn answer(
    request: &Request,
    caller: &impl Caller,
    scope: &Scope<'_>,
    rescan: impl FnOnce() -> (bool, String),
) -> Vec<Response> {
    let refuse = |msg: &str| {
        vec![Response::Error {
            code: ErrorCode::Unauthorized,
            msg: msg.into(),
        }]
    };
    if !scope.allowed {
        return refuse(UNAUTHORIZED);
    }
    match request {
        Request::Health => vec![Response::Health(Box::new(
            scope.state.report(scope.store, scope.policy),
        ))],
        Request::Rescan if scope.system && !caller.admin() => {
            refuse("only administrators and SYSTEM or root can start a rescan")
        }
        Request::Rescan => {
            let (started, msg) = rescan();
            if started {
                // Who asked: an audit line (an account id, never a file name).
                eventlog::info(format!(
                    "rescan requested by account {}",
                    caller.id().unwrap_or_else(|| "unknown".into())
                ));
            }
            vec![Response::Rescan { started, msg }]
        }
        _ => vec![Response::Error {
            code: ErrorCode::BadRequest,
            msg: "not a health request".into(),
        }],
    }
}

/// The private part of this process's working set (Windows 10 1809 and
/// later; what Task Manager's Memory column shows): pages of the mapped
/// index file are in the working set but not in this figure.
pub fn private_working_set() -> Option<u64> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::ProcessStatus::{
            K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX2,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        // SAFETY: all-zero is a valid PROCESS_MEMORY_COUNTERS_EX2 (integers).
        let mut counters: PROCESS_MEMORY_COUNTERS_EX2 = unsafe { std::mem::zeroed() };
        counters.cb = size_of::<PROCESS_MEMORY_COUNTERS_EX2>() as u32;
        // SAFETY: the pseudo handle of this process and a correctly sized,
        // writable PROCESS_MEMORY_COUNTERS_EX2 (its layout begins with the
        // PROCESS_MEMORY_COUNTERS fields, as the API documents). An older
        // Windows rejects the size and the call fails.
        let ok = unsafe {
            K32GetProcessMemoryInfo(
                GetCurrentProcess(),
                (&raw mut counters).cast::<PROCESS_MEMORY_COUNTERS>(),
                counters.cb,
            )
        };
        (ok != 0).then_some(counters.PrivateWorkingSetSize as u64)
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// `(working set, private bytes)` of this process; Unix reports the resident
/// set size and no private figure.
pub fn memory() -> (u64, Option<u64>) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::ProcessStatus::{
            K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        let mut counters = PROCESS_MEMORY_COUNTERS_EX {
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
                (&raw mut counters).cast::<PROCESS_MEMORY_COUNTERS>(),
                counters.cb,
            )
        };
        if ok == 0 {
            return (0, None);
        }
        (
            counters.WorkingSetSize as u64,
            Some(counters.PrivateUsage as u64),
        )
    }
    #[cfg(target_os = "linux")]
    {
        // statm: size resident shared … in pages.
        let pages = std::fs::read_to_string("/proc/self/statm")
            .ok()
            .and_then(|text| text.split_whitespace().nth(1)?.parse::<u64>().ok())
            .unwrap_or(0);
        // SAFETY: sysconf has no preconditions.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(0) as u64;
        (pages * page, None)
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: zeroed is a valid output structure; the size passed is its own.
        let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
        let size = size_of::<libc::proc_taskinfo>() as i32;
        // SAFETY: live output buffer of exactly `size` bytes for this process.
        let got = unsafe {
            libc::proc_pidinfo(
                std::process::id() as i32,
                libc::PROC_PIDTASKINFO,
                0,
                (&raw mut info).cast(),
                size,
            )
        };
        (
            if got == size {
                info.pti_resident_size
            } else {
                0
            },
            None,
        )
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        (0, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Snapshot;
    use ferrow_ipc::AccessMode;
    use ferrow_ipc::health::{JOURNAL_OK, MODE_LIVE};

    struct Fake {
        admin: bool,
        id: &'static str,
    }
    impl Caller for Fake {
        fn admin(&self) -> bool {
            self.admin
        }
        fn unsandboxed(&self) -> bool {
            true
        }
        fn person(&self) -> bool {
            true
        }
        fn id(&self) -> Option<String> {
            Some(self.id.into())
        }
    }

    fn live_volume() -> VolumeState {
        VolumeState {
            root: "C:\\".into(),
            device: Some("\\\\?\\Volume{1}".into()),
            mode: MODE_LIVE,
            reason: String::new(),
            entries: Some(1234),
            journal: JOURNAL_OK,
            journal_detail: String::new(),
        }
    }

    fn ask(
        state: &State,
        request: Request,
        caller: &Fake,
        allowed: bool,
        rescan: impl FnOnce() -> (bool, String),
    ) -> Vec<Response> {
        let store = SnapshotStore::new(Snapshot::empty());
        let policy = Policy {
            mode: AccessMode::Everyone,
            users: vec![],
        };
        let scope = Scope {
            system: true,
            allowed,
            state,
            store: &store,
            policy: &policy,
        };
        answer(&request, caller, &scope, rescan)
    }

    #[test]
    fn the_report_counts_scans_by_reason_and_folder_reads() {
        let state = State::new();
        state.count_scan("startup");
        state.count_scan("a folder moved into an indexed location (the walk failed)");
        state.count_scan("a folder moved into an indexed location");
        state.count_scan("50001 live changes");
        state.count_scan("\\\\?\\Volume{1}: change journal reset: overrun");
        state.folder_read(40, 0, Duration::from_millis(1500));
        state.folder_read(2, 3, Duration::from_millis(30));
        state.folder_read_fell_back();
        let store = SnapshotStore::new(Snapshot::empty());
        let policy = Policy {
            mode: AccessMode::Everyone,
            users: vec![],
        };
        let stats = state.report(&store, &policy).live_stats;
        assert_eq!(stats.scans_by_reason["startup"], 1);
        assert_eq!(stats.scans_by_reason["moved-in folder"], 2);
        assert_eq!(stats.scans_by_reason["too many live changes"], 1);
        assert_eq!(stats.scans_by_reason["journal"], 1);
        assert_eq!(
            (
                stats.folder_reads,
                stats.folder_read_entries,
                stats.folder_read_ms
            ),
            (2, 42, 1530)
        );
        assert_eq!(stats.folder_read_skipped, 3);
        assert_eq!(stats.folder_read_fallbacks, 1);
        // The wire form is JSON; an older reader ignores the new field and a
        // newer one reads an older report without it.
        let json = state.report(&store, &policy).to_json();
        assert!(json.contains("\"scans_by_reason\""), "{json}");
        let old = ferrow_ipc::health::Health::from_json(r#"{"version":"1"}"#).unwrap();
        assert_eq!(old.live_stats, LiveStats::default());
    }

    #[test]
    fn a_merge_in_memory_has_its_own_counters_and_is_not_a_scan() {
        let state = State::new();
        state.scan_started();
        state.scan_finished("startup", Duration::from_secs(70), vec![live_volume()]);
        state.count_scan("startup");
        state.compaction_done("50001 live changes", Duration::from_millis(12_400));
        state.compaction_done("60000 live changes", Duration::from_millis(600));
        assert!(!state.is_scanning(), "folding in memory reads no disk");
        let store = SnapshotStore::new(Snapshot::empty());
        let policy = Policy {
            mode: AccessMode::Everyone,
            users: vec![],
        };
        let report = state.report(&store, &policy);
        let stats = &report.live_stats;
        // Counted beside the scans, never among them.
        assert_eq!(stats.scans_by_reason.len(), 1);
        assert_eq!(stats.scans_by_reason["startup"], 1);
        assert_eq!((stats.compactions, stats.compaction_ms), (2, 13_000));
        // The volume still reports its last real scan; the last merge is one
        // service-level fact, not repeated on every volume.
        assert_eq!(
            report.volumes[0].last_scan.as_ref().unwrap().reason,
            "startup"
        );
        let last = stats.last_compaction.as_ref().unwrap();
        assert_eq!(
            (last.reason.as_str(), last.took_ms),
            ("60000 live changes", 600)
        );
        assert!(last.at_ms > 0);
        // Additive on the wire: a report of an older service has none.
        let json = report.to_json();
        assert!(json.contains("\"last_compaction\"") && json.contains("\"compactions\":2"));
        let old = ferrow_ipc::health::Health::from_json(
            r#"{"live_stats":{"folder_reads":1},"volumes":[{"root":"C:\\"}]}"#,
        )
        .unwrap();
        assert_eq!(old.live_stats.compactions, 0);
        assert_eq!(old.live_stats.last_compaction, None);
        // A later scan builds the index from the disks again: the earlier
        // merge no longer says anything about it, the totals stay.
        state.scan_started();
        state.scan_finished("journal", Duration::from_secs(60), vec![live_volume()]);
        let report = state.report(&store, &policy);
        assert_eq!(report.live_stats.last_compaction, None);
        assert_eq!(report.live_stats.compactions, 2);
    }

    #[test]
    fn an_index_save_in_flight_is_visible_until_its_guard_ends() {
        let state = State::new();
        assert!(!state.is_saving());
        {
            let _saving = state.saving();
            assert!(state.is_saving());
        }
        assert!(!state.is_saving());
        // Also when the save panics.
        let state = std::sync::Arc::new(State::new());
        let s = state.clone();
        let _ = std::thread::spawn(move || {
            let _saving = s.saving();
            panic!("save failed");
        })
        .join();
        assert!(!state.is_saving(), "a panic does not leave it set");
    }

    #[test]
    fn the_report_carries_what_the_events_recorded() {
        let state = State::new();
        state.mark_started();
        state.scan_started();
        assert!(state.is_scanning());
        state.scan_finished(
            "startup",
            Duration::from_millis(4200),
            vec![live_volume(), VolumeState::walked("E:\\docs".into(), "")],
        );
        state.set_pending(Some((
            "journal reset",
            Instant::now() + Duration::from_secs(60),
        )));
        state.journal_problem("\\\\?\\Volume{1}", false, "change journal reset: overrun");
        state.add_live_changes(7);
        state.save_done(Duration::from_millis(900), None);
        let report = match &ask(
            &state,
            Request::Health,
            &Fake {
                admin: false,
                id: "1000",
            },
            true,
            || unreachable!(),
        )[..]
        {
            [Response::Health(h)] => (**h).clone(),
            other => panic!("{other:?}"),
        };
        assert_eq!(report.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(report.pid, std::process::id());
        assert_eq!(report.access, "everyone");
        assert_eq!(report.live_changes, 7);
        assert!(!report.scanning);
        assert!(report.working_set > 0, "memory is read from the OS");
        let save = report.last_save.clone().unwrap();
        assert!(save.ok && save.took_ms == 900);
        let [live, walked] = &report.volumes[..] else {
            panic!("{:?}", report.volumes)
        };
        assert_eq!(
            (live.root.as_str(), live.mode.as_str()),
            ("C:\\", MODE_LIVE)
        );
        assert_eq!(
            (live.journal.as_str(), live.entries),
            (JOURNAL_RESET, Some(1234))
        );
        assert_eq!(live.journal_detail, "change journal reset: overrun");
        let scan = live.last_scan.clone().unwrap();
        assert_eq!((scan.reason.as_str(), scan.took_ms), ("startup", 4200));
        let pending = live.rescan_pending.clone().unwrap();
        assert!(pending.reason == "journal reset" && pending.earliest_ms > report.now_ms);
        assert_eq!(
            (
                walked.mode.as_str(),
                walked.journal.as_str(),
                walked.entries
            ),
            (MODE_WALK, JOURNAL_NONE, None)
        );
        // The JSON shape the app and `ferrow service health --json` read.
        let json = report.to_json();
        for key in [
            "\"version\"",
            "\"started_ms\"",
            "\"uptime_s\"",
            "\"working_set\"",
            "\"private_bytes\"",
            "\"entries\"",
            "\"last_save\"",
            "\"volumes\"",
            "\"events\"",
            "\"rescan_pending\"",
        ] {
            assert!(json.contains(key), "{key} missing from {json}");
        }
    }

    #[test]
    fn a_stopped_reader_stays_reported_after_a_rescan_and_a_reset_does_not() {
        let state = State::new();
        state.scan_finished("startup", Duration::ZERO, vec![live_volume()]);
        state.journal_problem("\\\\?\\Volume{1}", true, "change journal keeps failing");
        state.journal_problem("\\\\?\\Volume{1}", false, "a later reset must not hide it");
        state.scan_finished("rescan", Duration::ZERO, vec![live_volume()]);
        let store = SnapshotStore::new(Snapshot::empty());
        let v = &state.report(&store, &Policy::default()).volumes[0];
        assert_eq!(v.journal, JOURNAL_ERROR);
        assert_eq!(v.journal_detail, "change journal keeps failing");
        // A plain reset is answered by the scan.
        let state = State::new();
        state.scan_finished("startup", Duration::ZERO, vec![live_volume()]);
        state.journal_problem("\\\\?\\Volume{1}", false, "reset");
        state.scan_finished("rescan", Duration::ZERO, vec![live_volume()]);
        assert_eq!(
            state.report(&store, &Policy::default()).volumes[0].journal,
            JOURNAL_OK
        );
    }

    #[test]
    fn a_refused_caller_gets_unauthorized_for_both_requests() {
        let state = State::new();
        for request in [Request::Health, Request::Rescan] {
            let answer = ask(
                &state,
                request,
                &Fake {
                    admin: true,
                    id: "1000",
                },
                false,
                || panic!("a refused caller must not reach the rescan"),
            );
            assert!(
                matches!(
                    &answer[..],
                    [Response::Error {
                        code: ErrorCode::Unauthorized,
                        ..
                    }]
                ),
                "{answer:?}"
            );
        }
    }

    #[test]
    fn an_allowed_non_admin_gets_health_but_not_a_rescan() {
        let state = State::new();
        let user = Fake {
            admin: false,
            id: "1000",
        };
        let health = ask(&state, Request::Health, &user, true, || unreachable!());
        assert!(matches!(&health[..], [Response::Health(_)]));
        let rescan = ask(&state, Request::Rescan, &user, true, || {
            panic!("a non-administrator must not reach the rescan")
        });
        assert!(matches!(
            &rescan[..],
            [Response::Error {
                code: ErrorCode::Unauthorized,
                ..
            }]
        ));
        let admin = ask(
            &state,
            Request::Rescan,
            &Fake {
                admin: true,
                id: "1000",
            },
            true,
            || (true, "ok".into()),
        );
        assert_eq!(
            admin,
            vec![Response::Rescan {
                started: true,
                msg: "ok".into()
            }]
        );
    }

    #[test]
    fn a_second_rescan_request_is_a_no_op_with_a_clear_answer() {
        let state = State::new();
        let now = Instant::now();
        assert!(state.claim_rescan(now).is_ok());
        let again = state.claim_rescan(now).unwrap_err();
        assert!(again.contains("already requested"), "{again}");
        // Once the scan starts, the claim is spent and the running scan answers.
        state.scan_started();
        assert!(
            state
                .claim_rescan(now)
                .unwrap_err()
                .contains("already running")
        );
        state.scan_finished("rescan", Duration::ZERO, vec![]);
        let later = Instant::now() + MIN_FORCED_GAP;
        assert!(
            state.claim_rescan(later).is_ok(),
            "a finished scan frees the next request after the gap"
        );
        state.release_rescan();
        assert!(
            state.claim_rescan(later).is_ok(),
            "an undelivered request is given back"
        );
    }

    #[test]
    fn a_failed_scan_also_starts_the_gap() {
        let state = State::new();
        state.scan_started();
        state.scan_failed();
        assert!(!state.is_scanning());
        let why = state
            .claim_rescan(Instant::now() + Duration::from_secs(5))
            .unwrap_err();
        assert!(why.contains("a rescan can be requested in"), "{why}");
        assert!(
            state
                .claim_rescan(Instant::now() + MIN_FORCED_GAP + Duration::from_secs(1))
                .is_ok()
        );
    }

    #[test]
    fn a_rescan_right_after_a_scan_is_refused_with_the_time_it_may_run() {
        let state = State::new();
        state.scan_started();
        state.scan_finished("startup", Duration::from_secs(3), vec![]);
        let soon = Instant::now() + Duration::from_secs(10);
        let why = state.claim_rescan(soon).unwrap_err();
        assert!(why.contains("a rescan can be requested in"), "{why}");
        assert!(why.contains("50.0 s") || why.contains("49."), "{why}");
        // Refusals do not take the claim: after the gap it is granted once.
        let after = Instant::now() + MIN_FORCED_GAP + Duration::from_secs(1);
        assert!(state.claim_rescan(after).is_ok());
        assert!(state.claim_rescan(after).is_err(), "one request at a time");
    }

    #[test]
    fn an_asking_account_is_named_in_the_audit_event() {
        let state = State::new();
        let admin = Fake {
            admin: true,
            id: "S-1-5-21-7777",
        };
        ask(&state, Request::Rescan, &admin, true, || {
            (true, "ok".into())
        });
        let events = eventlog::global().recent(usize::MAX);
        assert!(
            events
                .iter()
                .any(|e| e.text == "rescan requested by account S-1-5-21-7777"),
            "{events:?}"
        );
        // A refused attempt starts nothing and is not an audit line.
        let user = Fake {
            admin: false,
            id: "S-1-5-21-8888",
        };
        ask(&state, Request::Rescan, &user, true, || unreachable!());
        assert!(
            !eventlog::global()
                .recent(usize::MAX)
                .iter()
                .any(|e| e.text.contains("8888"))
        );
    }

    #[test]
    fn the_development_endpoint_lets_its_non_admin_owner_rescan() {
        let state = State::new();
        let store = SnapshotStore::new(Snapshot::empty());
        let policy = Policy::default();
        let scope = Scope {
            system: false,
            allowed: true,
            state: &state,
            store: &store,
            policy: &policy,
        };
        let owner = Fake {
            admin: false,
            id: "1000",
        };
        let answer = answer(&Request::Rescan, &owner, &scope, || (true, "ok".into()));
        assert_eq!(
            answer,
            vec![Response::Rescan {
                started: true,
                msg: "ok".into()
            }]
        );
    }

    #[test]
    fn listed_mode_answers_a_listed_account_health_but_never_a_rescan() {
        use ferrow_ipc::AccessUser;
        let state = State::new();
        let store = SnapshotStore::new(Snapshot::empty());
        let policy = Policy {
            mode: AccessMode::Listed,
            users: vec![AccessUser {
                id: "1000".into(),
                name: "alice".into(),
            }],
        };
        let ask_as = |who: &Fake, request: Request| {
            let scope = Scope {
                system: true,
                allowed: policy.allows(who),
                state: &state,
                store: &store,
                policy: &policy,
            };
            answer(&request, who, &scope, || {
                unreachable!("not an administrator")
            })
        };
        let alice = Fake {
            admin: false,
            id: "1000",
        };
        let Some(Response::Health(report)) = ask_as(&alice, Request::Health).into_iter().next()
        else {
            panic!("a listed account must get Health")
        };
        assert_eq!(report.access, "listed");
        assert!(matches!(
            &ask_as(&alice, Request::Rescan)[..],
            [Response::Error {
                code: ErrorCode::Unauthorized,
                ..
            }]
        ));
        let bob = Fake {
            admin: false,
            id: "1001",
        };
        for request in [Request::Health, Request::Rescan] {
            assert!(matches!(
                &ask_as(&bob, request)[..],
                [Response::Error {
                    code: ErrorCode::Unauthorized,
                    ..
                }]
            ));
        }
    }

    #[test]
    fn connection_errors_are_counted_not_logged_and_reach_the_report() {
        let state = State::new();
        for _ in 0..500 {
            state.connection_error();
        }
        // Other tests share the global ring, so look at what it holds, not its size.
        assert!(
            !eventlog::global()
                .recent(usize::MAX)
                .iter()
                .any(|e| e.text.contains("connection")),
            "an error became an event"
        );
        let store = SnapshotStore::new(Snapshot::empty());
        let report = state.report(&store, &Policy::default());
        assert_eq!(report.connection_errors, 500);
    }

    #[test]
    fn many_volumes_leave_a_marker_instead_of_vanishing() {
        let state = State::new();
        let volumes = (0..40)
            .map(|n| VolumeState::walked(format!("E:\r{n}"), ""))
            .collect();
        state.scan_finished("startup", Duration::ZERO, volumes);
        let store = SnapshotStore::new(Snapshot::empty());
        let report = state.report(&store, &Policy::default());
        assert_eq!(report.volumes.len(), 32);
        assert_eq!(report.more_volumes, 8);
    }
}
