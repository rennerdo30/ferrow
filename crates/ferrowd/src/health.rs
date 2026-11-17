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

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ferrow_ipc::health::{
    Health, JOURNAL_ERROR, JOURNAL_NONE, JOURNAL_RESET, MAX_EVENTS, MODE_UNAVAILABLE, MODE_WALK,
    RescanInfo, SaveInfo, ScanInfo, VolumeHealth,
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
}

pub struct State {
    inner: Mutex<Inner>,
    started: Mutex<Option<(Instant, u64)>>,
    live_changes: AtomicU64,
    scanning: AtomicBool,
    /// A requested rescan was sent and has not started yet.
    forced: AtomicBool,
}

impl State {
    pub const fn new() -> State {
        State {
            inner: Mutex::new(Inner {
                last_save: None,
                scan: None,
                pending: None,
                volumes: Vec::new(),
            }),
            started: Mutex::new(None),
            live_changes: AtomicU64::new(0),
            scanning: AtomicBool::new(false),
            forced: AtomicBool::new(false),
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

    pub fn scan_failed(&self) {
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

    pub fn add_live_changes(&self, n: usize) {
        self.live_changes.fetch_add(n as u64, Ordering::Relaxed);
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

    /// Claim the one requested rescan: false when a scan runs or one is
    /// already on its way.
    pub fn claim_rescan(&self) -> Result<(), &'static str> {
        if self.is_scanning() {
            return Err("a scan is already running and covers every volume");
        }
        self.forced
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| "a rescan was already requested and is about to start")
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
            entries: snapshot.entries() as u64,
            mode: snapshot.mode.clone(),
            access: policy.mode.as_str().into(),
            live_changes: self.live_changes.load(Ordering::Relaxed),
            scanning: self.is_scanning(),
            last_save: inner.last_save.clone(),
            volumes,
            events: eventlog::to_info(eventlog::global().recent(MAX_EVENTS)),
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
            vec![Response::Rescan { started, msg }]
        }
        _ => vec![Response::Error {
            code: ErrorCode::BadRequest,
            msg: "not a health request".into(),
        }],
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
            Some("1000".into())
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
            &Fake { admin: false },
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
            let answer = ask(&state, request, &Fake { admin: true }, false, || {
                panic!("a refused caller must not reach the rescan")
            });
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
        let user = Fake { admin: false };
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
        let admin = ask(&state, Request::Rescan, &Fake { admin: true }, true, || {
            (true, "ok".into())
        });
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
        assert!(state.claim_rescan().is_ok());
        let again = state.claim_rescan().unwrap_err();
        assert!(again.contains("already requested"), "{again}");
        // Once the scan starts, the claim is spent and the running scan answers.
        state.scan_started();
        assert!(
            state
                .claim_rescan()
                .unwrap_err()
                .contains("already running")
        );
        state.scan_finished("rescan", Duration::ZERO, vec![]);
        assert!(
            state.claim_rescan().is_ok(),
            "a finished scan frees the next request"
        );
        state.release_rescan();
        assert!(
            state.claim_rescan().is_ok(),
            "an undelivered request is given back"
        );
    }
}
