//! The service's event log: the last [`RING`] events in memory (they feed the
//! health report) and a bounded file in the protected state folder.
//!
//! Events happen on state changes (a scan finished, a journal reset, a save
//! failed, a connection ended with an error), never per request and never on a
//! timer. A repeat of the newest event only raises its count; it is not
//! written again, so a misbehaving client cannot turn into disk traffic.
//!
//! The file is `service.log`, rotated at [`ROTATE_AT`] to `service.1.log`
//! (the previous rotation is replaced, so the log is bounded at about 2 MiB).
//! It is created inside the protected state folder (so it inherits only the
//! protected grants), opened without following links, and re-validated with
//! the state-file check after every creation; a file that fails the check is
//! not written. Texts name no files beyond volume roots and the service's own
//! paths. A failure to log is remembered for a minute and never reaches the
//! caller.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ferrow_ipc::health::{EventInfo, LEVEL_ERROR, LEVEL_INFO, LEVEL_WARN, format_utc};

/// Events kept in memory.
pub const RING: usize = 200;
/// File size at which the log rotates.
pub const ROTATE_AT: u64 = 1 << 20;
pub const FILE_NAME: &str = "service.log";
pub const ROTATED_NAME: &str = "service.1.log";
/// Longest event text kept.
const MAX_TEXT: usize = 200;
/// After a failed write the file is left alone this long.
const RETRY: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Info => LEVEL_INFO,
            Level::Warn => LEVEL_WARN,
            Level::Error => LEVEL_ERROR,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub at_ms: u64,
    pub level: Level,
    pub text: String,
    /// How many times in a row this event happened.
    pub repeats: u32,
}

struct FileLog {
    path: PathBuf,
    rotate_at: u64,
    /// Validates a freshly created file (the protection check in production).
    check: fn(&Path) -> io::Result<()>,
    /// The file may be written (it exists and passed the check).
    checked: bool,
    retry_at: Option<Instant>,
}

struct Inner {
    ring: VecDeque<Event>,
    file: Option<FileLog>,
}

pub struct EventLog {
    inner: Mutex<Inner>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// One line of text: no control characters, at most [`MAX_TEXT`] characters.
fn tidy(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_TEXT)
        .collect()
}

impl FileLog {
    fn write(&mut self, event: &Event) -> io::Result<()> {
        if let Ok(meta) = std::fs::metadata(&self.path)
            && meta.len() >= self.rotate_at
        {
            std::fs::rename(&self.path, self.path.with_file_name(ROTATED_NAME))?;
            self.checked = false;
        }
        let mut options = std::fs::OpenOptions::new();
        options.append(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options.open(&self.path)?;
        if !self.checked {
            (self.check)(&self.path)?;
            self.checked = true;
        }
        let line = format!(
            "{} {} {}\n",
            format_utc(event.at_ms),
            event.level.as_str(),
            event.text
        );
        file.write_all(line.as_bytes())
    }
}

impl EventLog {
    pub const fn new() -> EventLog {
        EventLog {
            inner: Mutex::new(Inner {
                ring: VecDeque::new(),
                file: None,
            }),
        }
    }

    /// Start appending to `path` (the state folder is validated by the caller).
    pub fn attach(&self, path: PathBuf, rotate_at: u64, check: fn(&Path) -> io::Result<()>) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.file = Some(FileLog {
            path,
            rotate_at,
            check,
            checked: false,
            retry_at: None,
        });
    }

    pub fn record(&self, level: Level, text: &str) {
        let at_ms = now_ms();
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let text = tidy(text);
        if let Some(last) = inner.ring.back_mut()
            && last.level == level
            && last.text == text
        {
            last.repeats = last.repeats.saturating_add(1);
            return;
        }
        let event = Event {
            at_ms,
            level,
            text,
            repeats: 1,
        };
        if inner.ring.len() == RING {
            inner.ring.pop_front();
        }
        inner.ring.push_back(event.clone());
        let Some(file) = inner.file.as_mut() else {
            return;
        };
        if file.retry_at.is_some_and(|at| Instant::now() < at) {
            return;
        }
        file.retry_at = match file.write(&event) {
            Ok(()) => None,
            Err(_) => Some(Instant::now() + RETRY),
        };
    }

    /// The newest `n` events, oldest first.
    pub fn recent(&self, n: usize) -> Vec<Event> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let skip = inner.ring.len().saturating_sub(n);
        inner.ring.iter().skip(skip).cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .ring
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for EventLog {
    fn default() -> Self {
        EventLog::new()
    }
}

pub fn to_info(events: Vec<Event>) -> Vec<EventInfo> {
    events
        .into_iter()
        .map(|e| EventInfo {
            at_ms: e.at_ms,
            level: e.level.as_str().into(),
            text: if e.repeats > 1 {
                format!("{} (x{})", e.text, e.repeats)
            } else {
                e.text
            },
        })
        .collect()
}

/// The service's own log.
static LOG: EventLog = EventLog::new();

pub fn global() -> &'static EventLog {
    &LOG
}

/// Record an event and print it to stderr (a console run or `journalctl`).
pub fn record(level: Level, text: impl AsRef<str>) {
    let text = text.as_ref();
    match level {
        Level::Info => eprintln!("ferrowd: {text}"),
        Level::Warn => eprintln!("ferrowd: warning: {text}"),
        Level::Error => eprintln!("ferrowd: error: {text}"),
    }
    LOG.record(level, text);
}

pub fn info(text: impl AsRef<str>) {
    record(Level::Info, text);
}

pub fn warn(text: impl AsRef<str>) {
    record(Level::Warn, text);
}

pub fn error(text: impl AsRef<str>) {
    record(Level::Error, text);
}

/// Append the service's events to `service.log` next to `index` (the system
/// service only; the folder was validated by `prepare_state`).
pub fn attach_beside(index: &Path) {
    if let Some(dir) = index.parent() {
        LOG.attach(
            dir.join(FILE_NAME),
            ROTATE_AT,
            crate::protection::state_file,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("ferrow-eventlog-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn accept(_: &Path) -> io::Result<()> {
        Ok(())
    }

    #[test]
    fn the_ring_keeps_only_the_newest_events() {
        let log = EventLog::new();
        for n in 0..RING + 25 {
            log.record(Level::Info, &format!("event {n}"));
        }
        assert_eq!(log.len(), RING);
        let all = log.recent(usize::MAX);
        assert_eq!(all.first().unwrap().text, "event 25", "oldest dropped");
        assert_eq!(all.last().unwrap().text, format!("event {}", RING + 24));
        let last3 = log.recent(3);
        assert_eq!(last3.len(), 3);
        assert_eq!(
            last3[0].text,
            format!("event {}", RING + 22),
            "oldest first"
        );
    }

    #[test]
    fn texts_are_one_bounded_line() {
        let log = EventLog::new();
        log.record(Level::Warn, &format!("a\nb\u{1b}[2J{}", "x".repeat(1000)));
        let text = log.recent(1).remove(0).text;
        assert!(text.chars().count() <= MAX_TEXT);
        assert!(!text.contains('\n') && !text.contains('\u{1b}'), "{text:?}");
    }

    #[test]
    fn a_repeat_raises_a_count_and_is_not_written_again() {
        let dir = dir("repeat");
        let log = EventLog::new();
        log.attach(dir.join(FILE_NAME), ROTATE_AT, accept);
        for _ in 0..5 {
            log.record(Level::Error, "connection closed: broken pipe");
        }
        assert_eq!(log.len(), 1);
        let info = to_info(log.recent(10));
        assert!(info[0].text.ends_with("(x5)"), "{info:?}");
        let written = std::fs::read_to_string(dir.join(FILE_NAME)).unwrap();
        assert_eq!(written.lines().count(), 1, "{written}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_file_rotates_at_the_limit_and_stays_bounded() {
        let dir = dir("rotate");
        let log = EventLog::new();
        log.attach(dir.join(FILE_NAME), 300, accept);
        for n in 0..40 {
            log.record(
                Level::Info,
                &format!("event number {n:03} with some padding text"),
            );
        }
        let current = std::fs::metadata(dir.join(FILE_NAME)).unwrap().len();
        let rotated = std::fs::metadata(dir.join(ROTATED_NAME)).unwrap().len();
        // A file is rotated once it reaches the limit, so it can exceed it by one line.
        assert!(current < 300 + 100, "current log {current} bytes");
        assert!((300..400).contains(&rotated), "rotated log {rotated} bytes");
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            2,
            "only the log and one rotation exist"
        );
        let last = std::fs::read_to_string(dir.join(FILE_NAME)).unwrap();
        assert!(
            last.contains("event number 039"),
            "newest event is in the live file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_log_failure_never_reaches_the_caller_and_recovers() {
        let dir = dir("fail");
        let log = EventLog::new();
        // The parent folder does not exist: every write fails.
        log.attach(dir.join("missing").join(FILE_NAME), ROTATE_AT, accept);
        log.record(Level::Info, "first");
        log.record(Level::Info, "second");
        assert_eq!(log.len(), 2, "events still reach the ring");
        // A file that fails the protection check is never written.
        let log = EventLog::new();
        let refuse: fn(&Path) -> io::Result<()> = |_| {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unprotected",
            ))
        };
        log.attach(dir.join(FILE_NAME), ROTATE_AT, refuse);
        log.record(Level::Info, "refused");
        assert_eq!(std::fs::read(dir.join(FILE_NAME)).unwrap().len(), 0);
        assert_eq!(log.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
