//! Settings → Service health: what state the Ferrow system service is in, and
//! the actions that repair it.
//!
//! Nothing here runs on the UI thread. The service control manager (Windows)
//! and the service's `Health` report are read on threads of their own with
//! short deadlines; the UI only draws the last answer. While Settings is shown
//! one watcher thread makes the periodic asks, at least [`REFRESH`] apart. It
//! exists only while Settings is visible: closed or minimized (the UI says so
//! every frame), hidden in the tray (the watcher checks the window itself
//! before every ask, because hiding requests no frame), and it ends when one
//! of these holds. It wakes the window only when what is shown has changed.
//! Actions that need administrator rights run the fixed `ferrow.exe service
//! <verb>` through the access card's `runas` path; the service's own answer,
//! not the exit code, decides whether they worked. An action's follow-up asks
//! (at 0, 1.2 and 2.4 s, then every [`REFRESH`]) are the one exception to the
//! gap, and they push the next periodic ask back.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ferrow_ipc::health::{self, Health};
use ferrow_ipc::{Client, Endpoint};

use crate::access::{self, Notice, Outcome, RunError};

pub const SERVICE_NAME: &str = "ferrowd";
const HEALTH_DEADLINE: Duration = Duration::from_millis(1000);
const STATUS_DEADLINE: Duration = Duration::from_millis(800);
// Test builds never connect, so the real asks below are not compiled in.
#[cfg_attr(test, allow(dead_code))]
const RESCAN_DEADLINE: Duration = Duration::from_secs(2);
/// The shortest gap between two periodic asks while Settings is shown.
pub const REFRESH: Duration = Duration::from_secs(5);
/// After an action: look again after 1.2 s and 2.4 s (the service applies a
/// change within about a second), then at the normal cadence, for at most
/// this long.
const SETTLE_FIRST: Duration = Duration::from_millis(1200);
const SETTLE_MAX: Duration = Duration::from_secs(30);
/// How long the elevated command may run (a restart loads the saved index).
const COMMAND_WAIT_MS: u32 = 90_000;

// -------------------------------------------------------------------
// What is known

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScmState {
    NotInstalled,
    Stopped,
    Starting,
    Running,
    Stopping,
    /// Paused, or a state this app does not know.
    Other,
    /// The service manager could not be asked.
    #[cfg_attr(any(test, not(windows)), allow(dead_code))]
    Unknown,
    /// This OS has no service manager the app can ask (Linux, macOS).
    #[cfg_attr(windows, allow(dead_code))]
    Unsupported,
}

impl ScmState {
    /// The service manager's `dwCurrentState`.
    pub fn from_code(code: u32) -> ScmState {
        match code {
            1 => ScmState::Stopped,
            2 | 5 => ScmState::Starting, // start / continue pending
            3 | 6 => ScmState::Stopping, // stop / pause pending
            4 => ScmState::Running,
            _ => ScmState::Other,
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            ScmState::NotInstalled => "not installed",
            ScmState::Stopped => "stopped",
            ScmState::Starting => "starting",
            ScmState::Running => "running",
            ScmState::Stopping => "stopping",
            ScmState::Other => "paused or in an unknown state",
            ScmState::Unknown => "not readable",
            ScmState::Unsupported => "no service manager to ask",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scm {
    pub state: ScmState,
    /// The service process id while it runs.
    pub pid: Option<u32>,
    /// `SERVICE_AUTO_START`, `DEMAND_START` (3) or `DISABLED` (4), when readable.
    pub start_type: Option<u32>,
}

impl Scm {
    const fn of(state: ScmState) -> Scm {
        Scm {
            state,
            pid: None,
            start_type: None,
        }
    }

    /// The service manager does not say it is down: running, or it could not
    /// be asked (then the service's own answer decides).
    pub fn up(&self) -> bool {
        matches!(
            self.state,
            ScmState::Running | ScmState::Unknown | ScmState::Unsupported
        )
    }

    pub fn start_text(&self) -> Option<&'static str> {
        Some(match self.start_type? {
            2 => "automatically at boot",
            3 => "on demand",
            4 => "disabled",
            _ => return None,
        })
    }
}

/// What the app got from the service when it asked for its health.
#[derive(Clone, Debug, PartialEq)]
pub enum HealthView {
    /// Nobody connected, or no answer in time.
    NoAnswer,
    /// It answered, and refuses this account.
    Refused,
    /// Connected or timed out, but neither `Health` nor `Status` was answered
    /// in time: busy (when a report was seen before, see `Stale`).
    Silent,
    /// It answers searches but not `Health`, and no report was ever seen in
    /// this session: older than this app, or busy.
    Older,
    Report(Box<Health>),
    /// A report was seen in this session, but the service does not answer
    /// `Health` right now (busy): the last report, no longer current.
    Stale(Box<Health>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    pub scm: Scm,
    pub health: HealthView,
}

impl Snapshot {
    pub fn report(&self) -> Option<&Health> {
        match &self.health {
            HealthView::Report(h) => Some(h),
            _ => None,
        }
    }

    /// The last report, current or not (the details of a busy service).
    pub fn last_report(&self) -> Option<&Health> {
        match &self.health {
            HealthView::Report(h) | HealthView::Stale(h) => Some(h),
            _ => None,
        }
    }

    /// The service process answered something (even a refusal).
    pub fn answers(&self) -> bool {
        !matches!(
            self.health,
            HealthView::NoAnswer | HealthView::Silent | HealthView::Stale(_)
        )
    }

    pub fn pid(&self) -> Option<u32> {
        self.report().map(|h| h.pid).or(self.scm.pid)
    }

    /// Everything the section shows, as text: two snapshots with the same key
    /// look the same, so the window is not repainted for the difference
    /// (uptime seconds, working-set jitter, a few more items). Numbers are
    /// cut to the precision they are shown with.
    pub fn display_key(&self) -> String {
        let Some(h) = self.last_report() else {
            return format!("{:?}|{:?}", self.scm, self.health);
        };
        let volumes: Vec<_> = h
            .volumes
            .iter()
            .map(|v| {
                (
                    (&v.root, &v.mode, &v.reason, &v.journal, &v.journal_detail),
                    v.entries.map(items_text),
                    (&v.last_scan, &v.rescan_pending),
                )
            })
            .collect();
        let stale = matches!(self.health, HealthView::Stale(_));
        let shown = (
            (stale, h.connection_errors, h.more_volumes, h.started_ms),
            (self.scm, &h.version, &h.mode, &h.access, h.scanning),
            (
                uptime_text(h.uptime_s),
                items_text(h.entries),
                memory_text(h.working_set),
                h.private_bytes.map(memory_text),
            ),
            (&h.last_save, volumes, &h.events),
        );
        format!("{shown:?}")
    }
}

/// The service's state now. Test builds never touch the real service manager
/// or endpoint (the screenshot harness injects its own snapshots).
pub fn query_snapshot() -> Snapshot {
    #[cfg(test)]
    return Snapshot {
        scm: Scm::of(ScmState::NotInstalled),
        health: HealthView::NoAnswer,
    };
    #[cfg(not(test))]
    Snapshot {
        scm: scm::query(),
        health: ask_health(&Endpoint::default_service()),
    }
}

fn is_silence(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    )
}

fn refused(error: &std::io::Error) -> bool {
    crate::engine::service_gap(error) == crate::engine::ServiceGap::Refused
}

/// Classify what the two questions (`Health`, then `Status` on a connection of
/// its own) got back. An older service never answers `Health`, so silence
/// alone is not enough: it must answer `Status` to count as older.
fn health_view(
    health: std::io::Result<Health>,
    status: impl FnOnce() -> std::io::Result<()>,
) -> HealthView {
    match health {
        Ok(report) => HealthView::Report(Box::new(report)),
        Err(e) if refused(&e) => HealthView::Refused,
        Err(first) => match status() {
            Ok(()) => HealthView::Older,
            Err(e) if refused(&e) => HealthView::Refused,
            // A deadline means something is there and does not answer; any
            // other failure (nothing listens) means it is gone.
            Err(_) if is_silence(&first) => HealthView::Silent,
            Err(_) => HealthView::NoAnswer,
        },
    }
}

fn ask_health(endpoint: &Endpoint) -> HealthView {
    let report = Client::connect_deadline(endpoint, Instant::now() + HEALTH_DEADLINE)
        .and_then(|mut client| client.health());
    health_view(report, || {
        Client::connect_deadline(endpoint, Instant::now() + STATUS_DEADLINE)
            .and_then(|mut client| client.status().map(drop))
    })
}

#[cfg(windows)]
#[cfg_attr(test, allow(dead_code))]
mod scm {
    use super::{SERVICE_NAME, Scm, ScmState};
    use windows_sys::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_SERVICE_DOES_NOT_EXIST, GetLastError,
    };
    use windows_sys::Win32::System::Services::{
        CloseServiceHandle, OpenSCManagerW, OpenServiceW, QUERY_SERVICE_CONFIGW,
        QueryServiceConfigW, QueryServiceStatusEx, SC_HANDLE, SC_MANAGER_CONNECT,
        SC_STATUS_PROCESS_INFO, SERVICE_QUERY_CONFIG, SERVICE_QUERY_STATUS, SERVICE_STATUS_PROCESS,
    };

    struct Handle(SC_HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: a handle this struct opened, closed once.
            unsafe { CloseServiceHandle(self.0) };
        }
    }

    /// Ask the service manager about the service without any extra rights:
    /// connect, read status (and, when allowed, the start type).
    pub fn query() -> Scm {
        let name: Vec<u16> = SERVICE_NAME.encode_utf16().chain(Some(0)).collect();
        // SAFETY: null machine and database mean the local active database.
        let manager =
            unsafe { OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT) };
        if manager.is_null() {
            return Scm::of(ScmState::Unknown);
        }
        let manager = Handle(manager);
        let open = |access| {
            // SAFETY: a NUL-terminated name and a live manager handle.
            unsafe { OpenServiceW(manager.0, name.as_ptr(), access) }
        };
        let mut service = open(SERVICE_QUERY_STATUS | SERVICE_QUERY_CONFIG);
        let mut error = if service.is_null() {
            // SAFETY: reads this thread's last error, right after the call.
            unsafe { GetLastError() }
        } else {
            0
        };
        if service.is_null() && error == ERROR_ACCESS_DENIED {
            // Status alone needs the fewest rights.
            service = open(SERVICE_QUERY_STATUS);
            // SAFETY: as above.
            error = if service.is_null() {
                unsafe { GetLastError() }
            } else {
                0
            };
        }
        if service.is_null() {
            return Scm::of(if error == ERROR_SERVICE_DOES_NOT_EXIST {
                ScmState::NotInstalled
            } else {
                ScmState::Unknown
            });
        }
        let service = Handle(service);
        // SAFETY: zeroed POD; the call fills it, with its size passed in.
        let mut status: SERVICE_STATUS_PROCESS = unsafe { std::mem::zeroed() };
        let mut needed = 0;
        // SAFETY: the buffer is exactly one SERVICE_STATUS_PROCESS.
        let ok = unsafe {
            QueryServiceStatusEx(
                service.0,
                SC_STATUS_PROCESS_INFO,
                (&raw mut status).cast(),
                size_of::<SERVICE_STATUS_PROCESS>() as u32,
                &raw mut needed,
            )
        };
        if ok == 0 {
            return Scm::of(ScmState::Unknown);
        }
        // The documented 8 KiB maximum, aligned for the struct at its start.
        let mut buffer = [0u64; 1024];
        // SAFETY: the buffer is 8 KiB and its size is passed in.
        let start_type = (unsafe {
            QueryServiceConfigW(
                service.0,
                buffer.as_mut_ptr().cast::<QUERY_SERVICE_CONFIGW>(),
                size_of_val(&buffer) as u32,
                &raw mut needed,
            )
        } != 0)
            // SAFETY: the call succeeded, so the buffer starts with the struct.
            .then(|| unsafe { (*buffer.as_ptr().cast::<QUERY_SERVICE_CONFIGW>()).dwStartType });
        Scm {
            state: ScmState::from_code(status.dwCurrentState),
            pid: (status.dwProcessId != 0).then_some(status.dwProcessId),
            start_type,
        }
    }
}

#[cfg(not(windows))]
#[cfg_attr(test, allow(dead_code))]
mod scm {
    use super::{Scm, ScmState};
    pub fn query() -> Scm {
        Scm::of(ScmState::Unsupported)
    }
}

// -------------------------------------------------------------------
// One line, in one second

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Good,
    Caution,
    Neutral,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    pub level: Level,
    /// A filled dot: the service process runs (even when something is wrong
    /// with it); a ring: it does not, or nothing is known yet.
    pub filled: bool,
    /// The state word, in the state's colour.
    pub head: String,
    /// What follows it on the same line.
    pub rest: String,
    /// What to do about it, when something is wrong.
    pub hint: String,
    /// The button the hint points at: always an enabled action.
    pub hint_verb: Option<Verb>,
    /// The hint points to Fast index (this account is refused).
    pub hint_access: bool,
}

/// `C:\` as `C:`; a folder root stays as it is.
pub fn short_root(root: &str) -> &str {
    let bytes = root.as_bytes();
    if bytes.len() == 3 && bytes[1] == b':' && matches!(bytes[2], b'\\' | b'/') {
        &root[..2]
    } else {
        root
    }
}

/// One unit, the coarsest that is not zero: `3 d`, `5 h`, `12 min`.
pub fn uptime_text(secs: u64) -> String {
    match secs {
        0..60 => "just started".to_owned(),
        60..3_600 => format!("{} min", secs / 60),
        3_600..86_400 => format!("{} h", secs / 3_600),
        _ => format!("{} d", secs / 86_400),
    }
}

/// `842`, `412 k`, `18.4 M`.
pub fn items_text(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{} k", n / 1_000),
        _ => format!("{:.1} M", n as f64 / 1_000_000.0),
    }
}

pub fn memory_text(bytes: u64) -> String {
    format!("{} MB", bytes >> 20)
}

fn volume_roots<'a>(h: &'a Health, mode: &str) -> Vec<&'a str> {
    h.volumes
        .iter()
        .filter(|v| v.mode == mode)
        .map(|v| short_root(&v.root))
        .collect()
}

fn make(level: Level, filled: bool, head: &str, rest: String) -> Summary {
    Summary {
        level,
        filled,
        head: head.to_owned(),
        rest,
        hint: String::new(),
        hint_verb: None,
        hint_access: false,
    }
}

impl Summary {
    fn hint(mut self, text: &str) -> Summary {
        self.hint = text.to_owned();
        self
    }

    /// The hint names a button: "Start it below."
    fn hint_do(mut self, verb: Verb) -> Summary {
        self.hint = match verb {
            Verb::Start => "Start it below.",
            _ => "Restart it below.",
        }
        .to_owned();
        self.hint_verb = Some(verb);
        self
    }
}

/// The one line for a service whose report is current.
fn report_summary(h: &Health, app_version: &str) -> Summary {
    let items = format!("{} items", items_text(h.entries));
    let live = volume_roots(h, health::MODE_LIVE);
    let lost = volume_roots(h, health::MODE_UNAVAILABLE);
    let journal_bad = h.volumes.iter().any(|v| v.journal == health::JOURNAL_ERROR);
    let running = |rest: String| make(Level::Caution, true, "Running", rest);
    if let Some(v) = h.volumes.iter().find(|v| v.rescan_pending.is_some()) {
        let pending: Vec<&str> = h
            .volumes
            .iter()
            .filter(|v| v.rescan_pending.is_some())
            .map(|v| short_root(&v.root))
            .collect();
        let why = v.rescan_pending.as_ref().map_or("", |r| r.reason.as_str());
        let why = if why.is_empty() {
            String::new()
        } else {
            format!(" ({why})")
        };
        running(format!("rescan pending on {}{why}", pending.join(" ")))
    } else if !lost.is_empty() || journal_bad {
        let list = if lost.is_empty() {
            h.volumes
                .iter()
                .filter(|v| v.journal == health::JOURNAL_ERROR)
                .map(|v| short_root(&v.root))
                .collect()
        } else {
            lost
        };
        running(format!("live updates unavailable on {}", list.join(" ")))
    } else if h.last_save.as_ref().is_some_and(|s| !s.ok) {
        running("the last save of the index failed".to_owned())
    } else if h.version != app_version {
        running(format!("service {}, app {app_version}", h.version))
            .hint("Update Ferrow with Setup so both match.")
    } else if h.scanning {
        make(Level::Neutral, true, "Scanning", format!("{items} so far"))
    } else {
        let up = format!("running {}", uptime_text(h.uptime_s));
        let how = if live.is_empty() {
            "not live (directory walk)".to_owned()
        } else {
            format!("live on {}", live.join(" "))
        };
        make(
            Level::Good,
            true,
            "Healthy",
            format!("{up} · {items} · {how}"),
        )
    }
}

pub fn summary(snap: Option<&Snapshot>, app_version: &str) -> Summary {
    let none = String::new;
    let Some(snap) = snap else {
        return make(Level::Neutral, false, "Checking the service…", none());
    };
    // The manager's word wins over a report that is about to be out of date.
    match snap.scm.state {
        ScmState::Starting => return make(Level::Neutral, false, "Starting…", none()),
        ScmState::Stopping => return make(Level::Neutral, false, "Stopping…", none()),
        _ => {}
    }
    if let Some(h) = snap.report() {
        return report_summary(h, app_version);
    }
    // The service process runs, whatever else is wrong: a dot, not a ring.
    let running = snap.scm.state == ScmState::Running || snap.answers();
    let caution = |head: &str, rest: String| make(Level::Caution, running, head, rest);
    match (snap.scm.state, &snap.health) {
        (ScmState::NotInstalled, _) => {
            caution("Not installed", none()).hint("Install it with Setup.")
        }
        (ScmState::Stopped, _) => caution("Stopped", none()).hint_do(Verb::Start),
        (_, HealthView::Refused) => {
            let mut s = caution(
                "Running, but this account isn't allowed to see its health",
                none(),
            )
            .hint("Allow this account under Fast index.");
            s.hint_access = true;
            s
        }
        (_, HealthView::Older) => caution(
            "Older service, or busy",
            "update with Setup if this stays".to_owned(),
        ),
        (_, HealthView::Stale(_)) => caution(
            "Not answering right now",
            "it may be busy; showing its last report".to_owned(),
        )
        .hint_do(Verb::Restart),
        (ScmState::Other, _) => caution("Paused", none()),
        // No service manager to ask (Linux, macOS): silence means it is not running.
        (ScmState::Unsupported, HealthView::NoAnswer) => {
            caution("Not answering", none()).hint_do(Verb::Start)
        }
        _ => caution("Not answering", none()).hint_do(Verb::Restart),
    }
}

impl Snapshot {
    /// Something the user should look at: the Fast index card then points to
    /// this section. A refused account is answered there already.
    pub fn needs_attention(&self, app_version: &str) -> bool {
        summary(Some(self), app_version).level == Level::Caution
            && !matches!(self.health, HealthView::Refused)
    }
}

// -------------------------------------------------------------------
// Actions

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verb {
    Start,
    Restart,
    Rescan,
    Repair,
}

impl Verb {
    /// The fixed `ferrow` arguments: never any user text.
    pub fn args(self) -> &'static str {
        match self {
            Verb::Start => "service start",
            Verb::Restart => "service restart",
            Verb::Rescan => "service rescan",
            Verb::Repair => "service repair",
        }
    }

    /// The same action for a terminal (Windows: elevated; Linux, macOS: sudo).
    pub fn terminal_command(self) -> String {
        let linux = cfg!(all(unix, not(target_os = "macos")));
        let mac = cfg!(target_os = "macos");
        match (self, linux, mac) {
            (Verb::Start, true, _) => format!("sudo systemctl start {SERVICE_NAME}"),
            (Verb::Restart, true, _) => format!("sudo systemctl restart {SERVICE_NAME}"),
            (Verb::Start, _, true) => {
                "sudo launchctl kickstart system/com.github.rennerdo30.ferrowd".to_owned()
            }
            (Verb::Restart, _, true) => {
                "sudo launchctl kickstart -k system/com.github.rennerdo30.ferrowd".to_owned()
            }
            _ if cfg!(windows) => format!("ferrow {}", self.args()),
            _ => format!("sudo ferrow {}", self.args()),
        }
    }

    fn done_text(self) -> &'static str {
        match self {
            Verb::Start => "Started. The service is running.",
            Verb::Restart => "Restarted. The service is running again.",
            Verb::Rescan => "Rescan started. The volumes are read again in the background.",
            // The CLI says what it did on its own output, which an elevated
            // launch cannot capture: claim only what the answer shows.
            Verb::Repair => "Repair finished. The service is running.",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Can {
    /// Not offered in this state.
    Hidden,
    /// Shown disabled, with the reason.
    No(&'static str),
    Yes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Actions {
    pub start: Can,
    pub restart: Can,
    pub rescan: Can,
    pub repair: Can,
}

pub const NEEDS_INSTALLED: &str = "Needs the Setup-installed Ferrow. Use the command below.";

pub fn actions(snap: Option<&Snapshot>, can_elevate: bool, busy: bool) -> Actions {
    let all = |can| Actions {
        start: Can::Hidden,
        restart: can,
        rescan: can,
        repair: can,
    };
    let Some(snap) = snap else {
        return all(Can::No("Checking the service…"));
    };
    if busy {
        return all(Can::No("Another action is still running."));
    }
    let admin = |ok: bool, reason: &'static str| {
        if !ok {
            Can::No(reason)
        } else if can_elevate {
            Can::Yes
        } else {
            Can::No(NEEDS_INSTALLED)
        }
    };
    let state = snap.scm.state;
    let stopped = state == ScmState::Stopped
        || (state == ScmState::Unsupported && matches!(snap.health, HealthView::NoAnswer));
    let transitional = matches!(state, ScmState::Starting | ScmState::Stopping);
    Actions {
        start: if stopped {
            admin(true, "")
        } else {
            Can::Hidden
        },
        restart: match state {
            ScmState::NotInstalled => Can::No("The service isn't installed."),
            _ if stopped => Can::No("It is stopped. Use Start."),
            _ if transitional => Can::No("It is changing state. Wait a moment."),
            _ => admin(true, ""),
        },
        rescan: match &snap.health {
            HealthView::Report(h) if h.scanning => Can::No("A scan is running already."),
            HealthView::Report(h) if h.volumes.iter().any(|v| v.rescan_pending.is_some()) => {
                Can::No("A rescan is queued already (see the volumes).")
            }
            HealthView::Report(_) => Can::Yes,
            HealthView::Older => Can::No("Older service, or busy. Update with Setup."),
            HealthView::Stale(_) | HealthView::Silent => {
                Can::No("The service isn't answering right now.")
            }
            HealthView::Refused => admin(true, ""),
            HealthView::NoAnswer => Can::No("The service isn't running."),
        },
        repair: if transitional {
            Can::No("It is changing state. Wait a moment.")
        } else {
            admin(true, "")
        },
    }
}

/// The service's answer shows what the action was for.
fn expected(verb: Verb, before: &Snapshot, now: &Snapshot) -> bool {
    let up = now.scm.up() && now.answers();
    match verb {
        Verb::Start | Verb::Repair => up,
        // A restart is only done when another process answers.
        Verb::Restart => up && (before.pid().is_none() || before.pid() != now.pid()),
        Verb::Rescan => true,
    }
}

/// The pause before look number `n` (0 is the first pause): 1.2 s, 1.2 s,
/// then the normal cadence.
fn settle_pause(n: usize) -> Duration {
    if n < 2 { SETTLE_FIRST } else { REFRESH }
}

/// Look again until the answer shows it, or `SETTLE_MAX` of pauses is used up.
/// The asks are at 0, 1.2 and 2.4 s, then every `REFRESH`.
fn settle(
    verb: Verb,
    before: &Snapshot,
    mut ask: impl FnMut() -> Snapshot,
    mut pause: impl FnMut(Duration),
) -> (Snapshot, bool) {
    let mut now = ask();
    let mut waited = Duration::ZERO;
    let mut n = 0;
    while !expected(verb, before, &now) {
        if waited >= SETTLE_MAX {
            return (now, false);
        }
        let gap = settle_pause(n);
        pause(gap);
        waited += gap;
        n += 1;
        now = ask();
    }
    (now, true)
}

/// What to tell the user: the exit code (the contract of `ferrow service`),
/// and whether the service's answer shows the result.
fn outcome(verb: Verb, ran: Result<u32, RunError>, seen: bool) -> Notice {
    let problem = |text: &str| Notice::new(Outcome::Problem, text);
    match ran {
        Err(RunError::Declined) => Notice::new(Outcome::Declined, access::DECLINED_TEXT),
        Err(RunError::Failed(why)) => problem(&why),
        Ok(0) if seen => Notice::new(Outcome::Done, verb.done_text()),
        Ok(0) => problem(
            "The command ran, but the service isn't answering yet. Check again in a moment.",
        ),
        Ok(2) => problem("This needs administrator rights, and the approved account has none."),
        Ok(3) => problem("The service did not answer within 2 seconds. Is it running?"),
        Ok(4) => problem("The service refused this account."),
        Ok(5) => Notice::new(
            Outcome::NoChange,
            "Nothing was started. A scan is running, already requested, or ended less than a minute ago.",
        ),
        Ok(_) => problem(&format!(
            "It did not work. Run `{}` in an administrator terminal for the reason.",
            verb.terminal_command()
        )),
    }
}

/// The unelevated rescan request, as the service answered it.
pub enum IpcRescan {
    Started(String),
    NothingStarted(String),
    /// Only administrators may rescan: use the elevated command.
    NotAllowed,
    Failed(String),
}

fn ask_rescan() -> IpcRescan {
    #[cfg(test)]
    return IpcRescan::Failed("test builds do not ask the service".into());
    #[cfg(not(test))]
    match Client::connect_deadline(
        &Endpoint::default_service(),
        Instant::now() + RESCAN_DEADLINE,
    )
    .and_then(|mut client| client.rescan())
    {
        Ok((true, msg)) => IpcRescan::Started(msg),
        Ok((false, msg)) => IpcRescan::NothingStarted(msg),
        Err(e) if refused(&e) => IpcRescan::NotAllowed,
        Err(_) => IpcRescan::Failed("The service did not answer the rescan request.".into()),
    }
}

// -------------------------------------------------------------------
// Keeping the answer fresh

struct FeedState {
    /// When the last ask started, whoever made it.
    last_ask: Option<Instant>,
    /// What the UI shows now ([`Snapshot::display_key`]).
    shown: Option<String>,
    /// The last current report of this session.
    report: Option<Box<Health>>,
}

/// Where every ask publishes. It remembers when the last ask started (a
/// periodic ask waits `interval` after any ask, an action's included), what the
/// UI shows, and the last report: a service that answered `Health` before and
/// goes silent while its process runs is shown as busy, with that report.
pub struct Feed {
    state: Mutex<FeedState>,
    tx: Sender<Snapshot>,
    wake: Box<dyn Fn() + Send + Sync>,
}

impl Feed {
    fn new(tx: Sender<Snapshot>, wake: impl Fn() + Send + Sync + 'static) -> Feed {
        Feed {
            state: Mutex::new(FeedState {
                last_ask: None,
                shown: None,
                report: None,
            }),
            tx,
            wake: Box::new(wake),
        }
    }

    /// The time left until the next periodic ask may start.
    fn wait(&self, interval: Duration) -> Duration {
        let last = self.state.lock().map_or(None, |s| s.last_ask);
        last.map_or(Duration::ZERO, |t| interval.saturating_sub(t.elapsed()))
    }

    fn mark(&self) {
        if let Ok(mut s) = self.state.lock() {
            s.last_ask = Some(Instant::now());
        }
    }

    /// Take an answer as what the UI will show: a silent service that was
    /// answering before becomes `Stale` (its last report), and the answer is
    /// noted as shown. True when that differs from what was shown before.
    fn adopt(&self, snap: Snapshot) -> (Snapshot, bool) {
        let Ok(mut s) = self.state.lock() else {
            return (snap, true);
        };
        let snap = match (&snap.health, &s.report) {
            (HealthView::Report(h), _) => {
                s.report = Some(h.clone());
                snap
            }
            (HealthView::Older | HealthView::Silent, Some(last)) if snap.scm.up() => Snapshot {
                health: HealthView::Stale(last.clone()),
                ..snap
            },
            _ => snap,
        };
        let key = snap.display_key();
        let changed = s.shown.as_ref() != Some(&key);
        s.shown = Some(key);
        (snap, changed)
    }

    /// Hand the answer to the UI; the window wakes only if what it shows
    /// changed.
    fn publish(&self, snap: Snapshot) {
        let (snap, changed) = self.adopt(snap);
        if changed && self.tx.send(snap).is_ok() {
            (self.wake)();
        }
    }
}

/// The periodic asker thread: it ends when its handle is dropped, or when it
/// finds the window may not be asking (`allowed` is false).
struct Watcher {
    _stop: Sender<()>,
    ended: Arc<AtomicBool>,
}

/// First ask at once (or when the throttle allows), then every `interval`.
/// `allowed` is asked before every ask: hiding the window to the tray
/// requests no frame, so the UI cannot tell the thread itself.
fn spawn_watcher(
    feed: Arc<Feed>,
    interval: Duration,
    ask: impl Fn() -> Snapshot + Send + 'static,
    allowed: impl Fn() -> bool + Send + 'static,
) -> Option<Watcher> {
    let (stop, stopped) = channel::<()>();
    let ended = Arc::new(AtomicBool::new(false));
    let flag = ended.clone();
    std::thread::Builder::new()
        .name("ferrow-health-watch".into())
        .spawn(move || {
            loop {
                if !matches!(
                    stopped.recv_timeout(feed.wait(interval)),
                    Err(RecvTimeoutError::Timeout)
                ) {
                    break;
                }
                // An action asked meanwhile: its asks count.
                if !feed.wait(interval).is_zero() {
                    continue;
                }
                if !allowed() {
                    break;
                }
                feed.mark();
                feed.publish(ask());
            }
            flag.store(true, Ordering::Release);
        })
        .ok()
        .map(|_| Watcher { _stop: stop, ended })
}

/// Whether the section is on screen: Settings is the page, and the window is
/// neither minimized nor hidden in the tray.
pub fn shown(settings_open: bool, minimized: bool, hidden: bool) -> bool {
    settings_open && !minimized && !hidden
}

// -------------------------------------------------------------------
// State of the section

struct Done {
    snap: Snapshot,
    notice: Notice,
}

pub struct ServiceHealth {
    pub snap: Option<Snapshot>,
    /// The running action, and whether its administrator prompt is still open.
    pub busy: Option<(Verb, Arc<AtomicBool>)>,
    pub notice: Option<Notice>,
    /// "Repair…" was clicked and waits for its confirmation.
    pub confirm_repair: bool,
    /// "Show all" events.
    pub events_open: bool,
    updates: Receiver<Snapshot>,
    tx: Sender<Snapshot>,
    feed: Option<Arc<Feed>>,
    watcher: Option<Watcher>,
    /// What the UI last said about [`shown`]; the watcher reads it too.
    visible: Arc<AtomicBool>,
    /// The gap between periodic asks ([`REFRESH`]; shorter in tests).
    interval: Duration,
    done: Option<Receiver<Done>>,
}

impl ServiceHealth {
    pub fn new() -> ServiceHealth {
        let (tx, updates) = channel();
        ServiceHealth {
            snap: None,
            busy: None,
            notice: None,
            confirm_repair: false,
            events_open: false,
            updates,
            tx,
            feed: None,
            watcher: None,
            visible: Arc::new(AtomicBool::new(false)),
            interval: REFRESH,
            done: None,
        }
    }

    /// The section is on screen ([`shown`]): keep the answer fresh. Otherwise
    /// nothing runs.
    pub fn set_visible(&mut self, visible: bool, wake: impl Fn() + Send + Sync + 'static) {
        // Test builds never ask the real service (the screenshot harness
        // injects its answers); unit tests start the watcher themselves.
        #[cfg(not(test))]
        self.set_visible_with(
            visible,
            wake,
            query_snapshot,
            crate::platform::is_hidden_in_tray,
        );
        #[cfg(test)]
        let _ = (visible, wake);
    }

    fn set_visible_with(
        &mut self,
        visible: bool,
        wake: impl Fn() + Send + Sync + 'static,
        ask: impl Fn() -> Snapshot + Send + 'static,
        hidden: impl Fn() -> bool + Send + 'static,
    ) {
        self.visible.store(visible, Ordering::Relaxed);
        if !visible {
            self.watcher = None;
        } else if !self.watching() {
            let feed = self
                .feed
                .get_or_insert_with(|| Arc::new(Feed::new(self.tx.clone(), wake)));
            let on_screen = self.visible.clone();
            self.watcher = spawn_watcher(feed.clone(), self.interval, ask, move || {
                on_screen.load(Ordering::Relaxed) && !hidden()
            });
        }
    }

    /// The periodic asker runs.
    pub fn watching(&self) -> bool {
        self.watcher
            .as_ref()
            .is_some_and(|w| !w.ended.load(Ordering::Acquire))
    }

    /// Take finished answers and results (every frame, also while Settings is
    /// closed). True when the engine should look for the service again: an
    /// action ended, or the service started or stopped answering.
    pub fn poll(&mut self) -> bool {
        let before = self.snap.as_ref().map(Snapshot::answers);
        while let Ok(snap) = self.updates.try_recv() {
            self.snap = Some(snap);
        }
        let flipped = before.is_some_and(|b| Some(b) != self.snap.as_ref().map(Snapshot::answers));
        let Some(rx) = &self.done else {
            return flipped;
        };
        match rx.try_recv() {
            Ok(done) => {
                self.snap = Some(done.snap);
                self.notice = Some(done.notice);
            }
            Err(TryRecvError::Empty) => return flipped,
            // The worker ended without a result: never leave the buttons
            // waiting for it.
            Err(TryRecvError::Disconnected) => {
                self.notice = Some(Notice::new(
                    Outcome::Problem,
                    "The action stopped unexpectedly. Check the service and try again.",
                ));
            }
        }
        self.done = None;
        self.busy = None;
        true
    }

    /// Screenshot tests: show a given answer without asking.
    #[cfg(all(test, feature = "screenshots"))]
    pub fn show_for_test(
        &mut self,
        snap: Option<Snapshot>,
        busy: Option<(Verb, bool)>,
        notice: Option<Notice>,
    ) {
        self.snap = snap;
        self.busy = busy.map(|(v, prompt)| (v, Arc::new(AtomicBool::new(prompt))));
        self.notice = notice;
    }

    /// Run an action off the UI thread; the result arrives through `poll`.
    pub fn run(&mut self, verb: Verb, wake: impl Fn() + Send + 'static) {
        self.run_with(
            verb,
            wake,
            ask_rescan,
            |args| access::run_elevated_code(args, COMMAND_WAIT_MS),
            query_snapshot,
            std::thread::sleep,
        );
    }

    fn run_with(
        &mut self,
        verb: Verb,
        wake: impl Fn() + Send + 'static,
        rescan: impl FnOnce() -> IpcRescan + Send + 'static,
        elevate: impl FnOnce(&str) -> Result<u32, RunError> + Send + 'static,
        mut ask: impl FnMut() -> Snapshot + Send + 'static,
        pause: impl FnMut(Duration) + Send + 'static,
    ) {
        if self.busy.is_some() || self.done.is_some() {
            return;
        }
        let prompt = Arc::new(AtomicBool::new(false));
        let shown = prompt.clone();
        let known = self.snap.clone();
        let (tx, rx) = channel();
        let feed = self.feed.clone();
        let started = std::thread::Builder::new()
            .name("ferrow-health-action".into())
            .spawn(move || {
                // Every look counts as an ask (the periodic asker waits after
                // it) and is judged like the periodic ones (busy, not gone).
                let mut look = || {
                    let snap = ask();
                    match &feed {
                        Some(f) => {
                            f.mark();
                            f.adopt(snap).0
                        }
                        None => snap,
                    }
                };
                // No answer shown yet: look first, here, not on the UI thread.
                let before = known.unwrap_or_else(&mut look);
                let done = perform(verb, &before, &prompt, rescan, elevate, &mut look, pause);
                let _ = tx.send(done);
                wake();
            });
        if started.is_ok() {
            self.busy = Some((verb, shown));
            self.done = Some(rx);
            self.notice = None;
            self.confirm_repair = false;
        }
    }
}

fn perform(
    verb: Verb,
    before: &Snapshot,
    prompt: &std::sync::atomic::AtomicBool,
    rescan: impl FnOnce() -> IpcRescan,
    elevate: impl FnOnce(&str) -> Result<u32, RunError>,
    ask: &mut impl FnMut() -> Snapshot,
    pause: impl FnMut(Duration),
) -> Done {
    use std::sync::atomic::Ordering::Relaxed;
    if verb == Verb::Rescan {
        // The service decides who may rescan: try it unelevated first.
        let (outcome, text) = match rescan() {
            IpcRescan::Started(msg) => (Some(Outcome::Done), msg),
            IpcRescan::NothingStarted(msg) => (Some(Outcome::NoChange), msg),
            IpcRescan::Failed(why) => (Some(Outcome::Problem), why),
            IpcRescan::NotAllowed => (None, String::new()),
        };
        if let Some(outcome) = outcome {
            return Done {
                snap: ask(),
                notice: Notice { outcome, text },
            };
        }
    }
    prompt.store(true, Relaxed);
    let ran = elevate(verb.args());
    prompt.store(false, Relaxed);
    let (snap, seen) = if matches!(ran, Ok(0)) {
        settle(verb, before, &mut *ask, pause)
    } else {
        (ask(), false)
    };
    Done {
        snap,
        notice: outcome(verb, ran, seen),
    }
}

// -------------------------------------------------------------------
// Events, volumes, diagnostics

/// The events to list, newest first: the last five, or all.
pub fn events_shown(events: &[health::EventInfo], all: bool) -> Vec<&health::EventInfo> {
    let newest = events.iter().rev();
    if all {
        newest.collect()
    } else {
        newest.take(5).collect()
    }
}

/// The length of a volume root that `text` starts with, when it is a whole
/// word (the root may hold spaces; `C:\Users` is not the root `C:\`).
fn root_len(text: &str, roots: &[&str]) -> Option<usize> {
    roots
        .iter()
        .filter(|r| !r.is_empty())
        .filter_map(|root| {
            let head = text.get(..root.len())?;
            let ends = text[root.len()..]
                .chars()
                .next()
                .is_none_or(|c| c.is_whitespace() || ",;:)\"'.".contains(c));
            (head.eq_ignore_ascii_case(root) && ends).then_some(root.len())
        })
        .max()
}

/// A word that names a file or folder: it holds a backslash, starts like a
/// drive, UNC or absolute path, or has one after `=` or `:`.
fn path_word(word: &str) -> bool {
    let word = word.trim_matches(['"', '\'', '(', ')', ',', ';', '.']);
    let starts = |w: &str| {
        let b = w.as_bytes();
        let drive = b.len() > 2 && b[0].is_ascii_alphabetic() && b[1] == b':';
        (drive && matches!(b[2], b'\\' | b'/')) || (w.starts_with('/') && w.len() > 1)
    };
    word.contains('\\') || starts(word) || word.split(['=', ':']).skip(1).any(starts)
}

/// `text` up to the first file or folder that is not a volume root, then
/// `<path omitted>`. A path can hold spaces (`C:\Users\Jane Doe`), so nothing
/// after its first word is kept: that is the one cut that cannot leak part of it.
pub fn scrub(text: &str, roots: &[&str]) -> String {
    let mut at = 0;
    while at < text.len() {
        let rest = &text[at..];
        let lead = rest.len() - rest.trim_start_matches(['(', '"', '\'']).len();
        if let Some(n) = root_len(&rest[lead..], roots) {
            at += lead + n;
            continue;
        }
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        if path_word(&rest[..end]) {
            return format!("{}<path omitted>", &text[..at]);
        }
        at += end + rest[end..].chars().next().map_or(0, char::len_utf8);
    }
    text.to_owned()
}

// The text "Copy diagnostics" puts on the clipboard: versions, the service
/// manager's state, the health report and the newest events. No file names
/// beyond the volume roots.
pub fn diagnostics(snap: &Snapshot, app_version: &str, os: &str) -> String {
    let mut out = format!(
        "Ferrow diagnostics\nApp: {app_version}\nOS: {os}\nService manager: {}",
        snap.scm.state.word()
    );
    if let Some(pid) = snap.scm.pid {
        out += &format!(", process {pid}");
    }
    if let Some(start) = snap.scm.start_text() {
        out += &format!(", starts {start}");
    }
    let Some(h) = snap.report() else {
        let why = match snap.health {
            HealthView::Refused => "the service refused this account",
            HealthView::Older => "the service is older and has no health report",
            _ => "the service did not answer",
        };
        return out + &format!("\nHealth: not available ({why})\n");
    };
    let roots: Vec<&str> = h.volumes.iter().map(|v| v.root.as_str()).collect();
    let clean = |s: &str| scrub(s, &roots);
    let mut shown = h.clone();
    shown
        .events
        .iter_mut()
        .for_each(|e| e.text = clean(&e.text));
    for v in &mut shown.volumes {
        v.reason = clean(&v.reason);
        v.journal_detail = clean(&v.journal_detail);
        if let Some(r) = &mut v.rescan_pending {
            r.reason = clean(&r.reason);
        }
        if let Some(s) = &mut v.last_scan {
            s.reason = clean(&s.reason);
        }
    }
    if let Some(save) = &mut shown.last_save {
        save.error = clean(&save.error);
    }
    out += "\nHealth:\n";
    out += &serde_json::to_string_pretty(&shown).unwrap_or_default();
    out += "\n\nRecent events (newest first):\n";
    for e in events_shown(&shown.events, true) {
        out += &format!("{} [{}] {}\n", health::format_utc(e.at_ms), e.level, e.text);
    }
    out
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
