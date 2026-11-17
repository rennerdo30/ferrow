//! The service's event log: the last [`RING`] events in memory (they feed the
//! health report) and a bounded file in the protected state folder.
//!
//! Events happen on state changes (a scan finished, a journal reset, a save
//! failed), never per request and never on a timer. Three limits keep the log
//! from becoming a load or a place to hide things:
//!
//! - a repeat of the newest event only raises its count;
//! - the file takes at most [`MAX_LINES_PER_MINUTE`] lines a minute; the rest
//!   are counted and one "N log lines were not written" line follows;
//! - file I/O never happens under the ring's lock. The ring is a short critical
//!   section (health and every other event only ever wait for that), and the
//!   file has a lock of its own that is only *tried*: when another thread is
//!   writing, the line is counted as not written instead of waiting.
//!
//! The file is `service.log`, rotated at [`ROTATE_AT`] to `service.1.log`
//! (the previous rotation is replaced, so the log is bounded at about 2 MiB).
//! It is created inside the protected state folder (so it inherits only the
//! protected grants), opened without following links (and, on Windows, as the
//! reparse point itself), and re-validated with the state-file check after every
//! creation; a file that fails the check is not written. Texts are one line of
//! visible characters and name no files beyond volume roots and the service's
//! own paths. A failure to log is remembered for a minute and never reaches the
//! caller.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ferrow_ipc::health::{EventInfo, LEVEL_ERROR, LEVEL_INFO, LEVEL_WARN, format_utc};

/// Events kept in memory.
pub const RING: usize = 200;
/// File size at which the log rotates.
pub const ROTATE_AT: u64 = 1 << 20;
/// Most lines the file takes in one minute.
pub const MAX_LINES_PER_MINUTE: u32 = 30;
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
    /// The current minute's window and the lines written in it.
    window: Option<(Instant, u32)>,
    /// Lines left out since the last marker (limit, pause or a busy file).
    skipped: u32,
}

pub struct EventLog {
    ring: Mutex<VecDeque<Event>>,
    file: Mutex<Option<FileLog>>,
    /// Lines dropped because the file was being written by another thread.
    busy: AtomicU64,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Characters that change how text is laid out or hide it: line and
/// paragraph separators and the bidirectional controls.
fn is_hidden(c: char) -> bool {
    matches!(
        c,
        '\u{2028}'
            | '\u{2029}'
            | '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}

/// One line of text: control characters become spaces, layout and direction
/// characters are dropped, at most [`MAX_TEXT`] characters.
fn tidy(text: &str) -> String {
    text.chars()
        .filter(|&c| !is_hidden(c))
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_TEXT)
        .collect()
}

impl FileLog {
    /// Append one event, preceded by a marker when lines were left out.
    fn write(&mut self, event: &Event, now: Instant) {
        if self.retry_at.is_some_and(|at| now < at) {
            self.skipped = self.skipped.saturating_add(1);
            return;
        }
        let (start, lines) = match self.window {
            Some((start, lines)) if now.duration_since(start) < Duration::from_secs(60) => {
                (start, lines)
            }
            _ => (now, 0),
        };
        let marker = self.skipped > 0;
        if lines + 1 + u32::from(marker) > MAX_LINES_PER_MINUTE {
            self.window = Some((start, lines));
            self.skipped = self.skipped.saturating_add(1);
            return;
        }
        let mut text = String::new();
        if marker {
            text = format!(
                "{} warn {} log lines were not written (rate limit, busy or unavailable file)\n",
                format_utc(event.at_ms),
                self.skipped
            );
        }
        text.push_str(&format!(
            "{} {} {}\n",
            format_utc(event.at_ms),
            event.level.as_str(),
            event.text
        ));
        match self.append(&text) {
            Ok(()) => {
                self.skipped = 0;
                self.retry_at = None;
                self.window = Some((start, lines + 1 + u32::from(marker)));
            }
            Err(_) => {
                self.retry_at = Some(now + RETRY);
                self.skipped = self.skipped.saturating_add(1);
            }
        }
    }

    fn append(&mut self, text: &str) -> io::Result<()> {
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
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // FILE_FLAG_OPEN_REPARSE_POINT: a link is opened as itself, never followed.
            options.custom_flags(0x0020_0000);
        }
        let mut file = options.open(&self.path)?;
        if !self.checked {
            (self.check)(&self.path)?;
            self.checked = true;
        }
        file.write_all(text.as_bytes())
    }
}

impl EventLog {
    pub const fn new() -> EventLog {
        EventLog {
            ring: Mutex::new(VecDeque::new()),
            file: Mutex::new(None),
            busy: AtomicU64::new(0),
        }
    }

    /// Start appending to `path` (the state folder is validated by the caller).
    pub fn attach(&self, path: PathBuf, rotate_at: u64, check: fn(&Path) -> io::Result<()>) {
        *self.file.lock().unwrap_or_else(|e| e.into_inner()) = Some(FileLog {
            path,
            rotate_at,
            check,
            checked: false,
            retry_at: None,
            window: None,
            skipped: 0,
        });
    }

    pub fn record(&self, level: Level, text: &str) {
        let event = Event {
            at_ms: now_ms(),
            level,
            text: tidy(text),
            repeats: 1,
        };
        {
            // The ring's critical section is a few pointer moves.
            let mut ring = self.ring.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(last) = ring.back_mut()
                && last.level == event.level
                && last.text == event.text
            {
                last.repeats = last.repeats.saturating_add(1);
                return;
            }
            if ring.len() == RING {
                ring.pop_front();
            }
            ring.push_back(event.clone());
        }
        // File I/O happens outside the ring lock, and only when the file is
        // free: a busy file counts the line as not written instead of waiting.
        let write = |file: &mut Option<FileLog>| {
            if let Some(file) = file.as_mut() {
                let busy = self.busy.swap(0, Ordering::Relaxed);
                file.skipped = file
                    .skipped
                    .saturating_add(u32::try_from(busy).unwrap_or(u32::MAX));
                file.write(&event, Instant::now());
            }
        };
        match self.file.try_lock() {
            Ok(mut guard) => write(&mut guard),
            Err(std::sync::TryLockError::Poisoned(p)) => write(&mut p.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => {
                self.busy.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// The newest `n` events, oldest first.
    pub fn recent(&self, n: usize) -> Vec<Event> {
        let ring = self.ring.lock().unwrap_or_else(|e| e.into_inner());
        let skip = ring.len().saturating_sub(n);
        ring.iter().skip(skip).cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.ring.lock().unwrap_or_else(|e| e.into_inner()).len()
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
    fn texts_are_one_bounded_line_of_visible_characters() {
        let log = EventLog::new();
        log.record(
            Level::Warn,
            &format!(
                "a\nb\u{1b}[2J\u{2028}c\u{2029}d\u{202e}e\u{2066}f\u{200f}{}",
                "x".repeat(1000)
            ),
        );
        let text = log.recent(1).remove(0).text;
        assert!(text.chars().count() <= MAX_TEXT);
        assert!(text.starts_with("a b [2Jcdef"), "{text:?}");
        assert!(
            !text.chars().any(|c| c.is_control() || is_hidden(c)),
            "{text:?}"
        );
    }

    #[test]
    fn a_repeat_raises_a_count_and_is_not_written_again() {
        let dir = dir("repeat");
        let log = EventLog::new();
        log.attach(dir.join(FILE_NAME), ROTATE_AT, accept);
        for _ in 0..5 {
            log.record(Level::Error, "same text every time");
        }
        assert_eq!(log.len(), 1);
        let info = to_info(log.recent(10));
        assert!(info[0].text.ends_with("(x5)"), "{info:?}");
        let written = std::fs::read_to_string(dir.join(FILE_NAME)).unwrap();
        assert_eq!(written.lines().count(), 1, "{written}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn alternating_texts_cannot_turn_into_unbounded_file_writes() {
        let dir = dir("flood");
        let log = EventLog::new();
        log.attach(dir.join(FILE_NAME), ROTATE_AT, accept);
        // Alternating texts defeat the repeat count; the per-minute cap holds.
        for n in 0..1000 {
            log.record(Level::Warn, if n % 2 == 0 { "kind A" } else { "kind B" });
        }
        let written = std::fs::read_to_string(dir.join(FILE_NAME)).unwrap();
        let lines = written.lines().count();
        assert!(lines <= MAX_LINES_PER_MINUTE as usize, "{lines} lines");
        assert!(lines >= 2, "the first lines are written");
        assert_eq!(log.len(), RING, "the ring still sees every change");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dropped_lines_are_followed_by_one_gap_marker() {
        let dir = dir("gap");
        let log = EventLog::new();
        log.attach(dir.join(FILE_NAME), ROTATE_AT, accept);
        for n in 0..(MAX_LINES_PER_MINUTE + 10) {
            log.record(Level::Info, &format!("event {n}"));
        }
        // Pretend the minute passed: the next event writes the marker first.
        log.file.lock().unwrap().as_mut().unwrap().window = None;
        log.record(Level::Info, "after the minute");
        let written = std::fs::read_to_string(dir.join(FILE_NAME)).unwrap();
        let tail: Vec<&str> = written.lines().rev().take(2).collect();
        assert!(tail[0].ends_with("info after the minute"), "{tail:?}");
        assert!(
            tail[1].contains("10 log lines were not written"),
            "{tail:?}\n{written}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_file_rotates_at_the_limit_and_stays_bounded() {
        let dir = dir("rotate");
        let log = EventLog::new();
        log.attach(dir.join(FILE_NAME), 300, accept);
        // Fewer lines than the per-minute cap.
        for n in 0..24 {
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
            last.contains("event number 023"),
            "newest event is in the live file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_log_failure_never_reaches_the_caller_and_leaves_a_gap_marker() {
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
        // After the pause the file works again and says what was missed.
        let log = EventLog::new();
        log.attach(dir.join("later.log"), ROTATE_AT, accept);
        log.file.lock().unwrap().as_mut().unwrap().retry_at =
            Some(Instant::now() + Duration::from_secs(3600));
        log.record(Level::Info, "during the pause");
        log.file.lock().unwrap().as_mut().unwrap().retry_at = None;
        log.record(Level::Info, "after the pause");
        let written = std::fs::read_to_string(dir.join("later.log")).unwrap();
        assert!(
            written.contains("1 log lines were not written"),
            "{written}"
        );
        assert!(!written.contains("during the pause"), "{written}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_slow_file_writer_never_blocks_the_ring_or_other_events() {
        use std::sync::Arc;
        use std::sync::mpsc::{Receiver, Sender, channel};
        static ENTERED: Mutex<Option<Sender<()>>> = Mutex::new(None);
        static RELEASE: Mutex<Option<Receiver<()>>> = Mutex::new(None);
        let (entered_tx, entered) = channel();
        let (release_tx, release_rx) = channel();
        *ENTERED.lock().unwrap() = Some(entered_tx);
        *RELEASE.lock().unwrap() = Some(release_rx);
        fn slow(_: &Path) -> io::Result<()> {
            ENTERED.lock().unwrap().as_ref().unwrap().send(()).unwrap();
            let rx = RELEASE.lock().unwrap().take().unwrap();
            let _ = rx.recv_timeout(Duration::from_secs(10));
            Ok(())
        }
        let dir = dir("slow");
        let log = Arc::new(EventLog::new());
        log.attach(dir.join(FILE_NAME), ROTATE_AT, slow);
        let writer = {
            let log = log.clone();
            std::thread::spawn(move || log.record(Level::Info, "stuck in the file check"))
        };
        entered.recv_timeout(Duration::from_secs(10)).unwrap();
        // The file is busy: neither reading the ring nor recording waits for it.
        let started = Instant::now();
        assert_eq!(log.recent(10).len(), 1, "the ring already has the event");
        log.record(Level::Info, "second event");
        assert_eq!(log.recent(10).len(), 2);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "blocked {:?} behind a slow file write",
            started.elapsed()
        );
        release_tx.send(()).unwrap();
        writer.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
