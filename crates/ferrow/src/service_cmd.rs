//! `ferrow service health [--json] | start | stop | restart | rescan | repair`.
//!
//! The app runs these verbs through the installed `ferrow.exe` (elevated for
//! the control verbs), so the verbs and exit codes are a contract:
//!
//! | code | meaning                                                          |
//! |------|------------------------------------------------------------------|
//! | 0    | done                                                             |
//! | 1    | failed (message on stderr)                                       |
//! | 2    | needs administrator / root; nothing was changed                  |
//! | 3    | no service reachable, or it did not answer within 2 s            |
//! | 4    | the service refused this account (`Unauthorized`)                |
//! | 5    | `rescan` only: nothing started; the reason is on stdout          |
//!
//! `health --json` prints the service's report, `ferrow_ipc::health::Health`,
//! on one line. `start`, `stop`, `restart` and `repair` print one line per
//! step. Ferrow never elevates itself.

use std::ffi::OsString;
use std::io;
use std::time::{Duration, Instant};

use ferrow_ipc::health::{Health, format_ago, format_duration, format_utc};
use ferrow_ipc::{Client, Endpoint, ErrorCode, Stream};
use ferrowd::control::{self, Action, IndexCheck};

pub const USAGE: &str = "ferrow service health [--json] | start | stop | restart | rescan | repair";

pub const EXIT_ELEVATION: u8 = 2;
pub const EXIT_UNREACHABLE: u8 = 3;
pub const EXIT_REFUSED: u8 = 4;
pub const EXIT_NOT_STARTED: u8 = 5;
/// How long the service gets to answer `health` and `rescan`.
const ANSWER: Duration = Duration::from_secs(2);

/// An error with its exit code; an empty message prints nothing.
#[derive(Debug)]
pub struct Failure {
    pub code: u8,
    pub msg: String,
}

impl From<String> for Failure {
    fn from(msg: String) -> Failure {
        Failure { code: 1, msg }
    }
}

fn fail(code: u8, msg: impl Into<String>) -> Failure {
    Failure {
        code,
        msg: msg.into(),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Cmd {
    Health { json: bool },
    Start,
    Stop,
    Restart,
    Rescan,
    Repair,
}

/// `None` when the first word is not one of these verbs.
pub fn parse(args: &[OsString]) -> Option<Result<Cmd, String>> {
    let words: Vec<&str> = args
        .iter()
        .map(|a| a.to_str().unwrap_or("\u{fffd}"))
        .collect();
    let usage = || Err(format!("usage: {USAGE}"));
    Some(match words.as_slice() {
        ["health"] => Ok(Cmd::Health { json: false }),
        ["health", "--json"] => Ok(Cmd::Health { json: true }),
        ["start"] => Ok(Cmd::Start),
        ["stop"] => Ok(Cmd::Stop),
        ["restart"] => Ok(Cmd::Restart),
        ["rescan"] => Ok(Cmd::Rescan),
        ["repair"] => Ok(Cmd::Repair),
        [
            "health" | "start" | "stop" | "restart" | "rescan" | "repair",
            ..,
        ] => usage(),
        _ => return None,
    })
}

fn verb(cmd: &Cmd) -> &'static str {
    match cmd {
        Cmd::Health { .. } => "health",
        Cmd::Start => "start",
        Cmd::Stop => "stop",
        Cmd::Restart => "restart",
        Cmd::Rescan => "rescan",
        Cmd::Repair => "repair",
    }
}

/// Control verbs need administrator rights; `health` and `rescan` ask the
/// service, which decides for the caller.
pub fn check_elevated(cmd: &Cmd, elevated: bool) -> Result<(), Failure> {
    if elevated || matches!(cmd, Cmd::Health { .. } | Cmd::Rescan) {
        return Ok(());
    }
    let verb = verb(cmd);
    Err(fail(
        EXIT_ELEVATION,
        if cfg!(windows) {
            format!(
                "`ferrow service {verb}` needs administrator rights.\n\
                 Re-run it from an elevated terminal. Ferrow never elevates itself."
            )
        } else {
            format!(
                "`ferrow service {verb}` needs root.\n\
                 Re-run with sudo: sudo ferrow service {verb}. Ferrow never elevates itself."
            )
        },
    ))
}

/// Run a service verb; `None` when `args` (after `service`) is not one.
pub fn try_run(args: &[OsString]) -> Option<Result<(), Failure>> {
    Some(parse(args)?.map_err(Failure::from).and_then(|cmd| {
        check_elevated(&cmd, ferrowd::accounts::is_elevated())?;
        match cmd {
            Cmd::Health { json } => health(json),
            Cmd::Rescan => rescan(),
            Cmd::Start => control_step(Action::Start),
            Cmd::Stop => control_step(Action::Stop),
            Cmd::Restart => control_step(Action::Restart),
            Cmd::Repair => repair(),
        }
    }))
}

fn control_step(action: Action) -> Result<(), Failure> {
    println!("{}", control::control(action)?);
    Ok(())
}

// ---- asking the service ------------------------------------------------------

/// Ask the first reachable service endpoint; the exit code says why not.
fn ask<T>(call: impl Fn(&mut Client<Stream>) -> io::Result<T>) -> Result<T, Failure> {
    let (mut refused, mut silent, mut gone) = (None, None, Vec::new());
    for endpoint in crate::service_candidates() {
        let deadline = Instant::now() + ANSWER;
        let mut client = match Client::connect_deadline(&endpoint, deadline) {
            Ok(client) => client,
            Err(e) => {
                gone.push(format!("{endpoint}: {e}"));
                continue;
            }
        };
        match call(&mut client) {
            Ok(value) => return Ok(value),
            Err(e) if ferrow_ipc::error_code(&e) == Some(ErrorCode::Unauthorized) => {
                refused.get_or_insert(endpoint);
            }
            Err(e) => {
                silent.get_or_insert((endpoint, e));
            }
        }
    }
    Err(unanswered(refused, silent, gone))
}

fn unanswered(
    refused: Option<Endpoint>,
    silent: Option<(Endpoint, io::Error)>,
    gone: Vec<String>,
) -> Failure {
    if let Some(endpoint) = refused {
        return fail(
            EXIT_REFUSED,
            format!(
                "the Ferrow service at {endpoint} does not answer this account; \
                 ask an administrator (see `ferrow service access show`)"
            ),
        );
    }
    if let Some((endpoint, e)) = silent {
        return fail(
            EXIT_UNREACHABLE,
            format!(
                "the Ferrow service at {endpoint} did not answer within {} s ({e}); it is \
                 probably older than this ferrow (update it with Ferrow's setup) or busy",
                ANSWER.as_secs()
            ),
        );
    }
    fail(
        EXIT_UNREACHABLE,
        format!("no Ferrow service is reachable ({})", gone.join("; ")),
    )
}

fn health(json: bool) -> Result<(), Failure> {
    let report = ask(|c| c.health())?;
    print!("{}", render(&report, json));
    Ok(())
}

fn rescan() -> Result<(), Failure> {
    let (started, msg) = ask(|c| c.rescan()).map_err(|f| match f.code {
        EXIT_REFUSED => fail(
            EXIT_REFUSED,
            "only administrators and SYSTEM or root can start a rescan",
        ),
        _ => f,
    })?;
    println!("{}", p(&msg));
    if started {
        Ok(())
    } else {
        Err(fail(EXIT_NOT_STARTED, ""))
    }
}

// ---- output ------------------------------------------------------------------

/// Text that came from the service, safe to print: control characters are
/// escaped so it cannot move the cursor or recolour the terminal.
fn p(text: &str) -> String {
    ferrowd::access::printable(text)
}

/// The report as printed: one JSON line, or the human summary.
pub fn render(h: &Health, json: bool) -> String {
    if json {
        return h.to_json() + "\n";
    }
    let mb = |bytes: u64| format!("{} MiB", bytes >> 20);
    let ago = |at_ms| format_ago(h.now_ms, at_ms);
    let problems = h.problems();
    let mut out = if problems.is_empty() {
        String::from("OK\n")
    } else {
        let mut text = format!("ATTENTION: {} problem(s)\n", problems.len());
        for problem in &problems {
            text.push_str(&format!("  ! {}\n", p(problem)));
        }
        text
    };
    out.push_str(&format!(
        "Ferrow service {} (pid {}), up {}, started {}\n",
        p(&h.version),
        h.pid,
        format_duration(h.uptime_s * 1000),
        format_utc(h.started_ms)
    ));
    out.push_str(&format!(
        "index: {} entries, {}{}; access: {}\n",
        h.entries,
        p(&h.mode),
        if h.scanning { ", scanning now" } else { "" },
        p(&h.access)
    ));
    out.push_str(&format!(
        "memory: {}{} working set{}\n",
        h.private_working_set.map_or(String::new(), |bytes| format!(
            "{} in use (private working set), ",
            mb(bytes)
        )),
        mb(h.working_set),
        h.private_bytes
            .map_or(String::new(), |bytes| format!(", {} private", mb(bytes)))
    ));
    if h.index_mapped_bytes > 0 {
        out.push_str(&format!(
            "index file: {} read through a memory map (file cache, not private memory)\n",
            mb(h.index_mapped_bytes)
        ));
    }
    out.push_str(&format!(
        "live changes applied: {}; connection errors: {}\n",
        h.live_changes, h.connection_errors
    ));
    // Not a scan, and not per volume: the live changes were merged into a new
    // index in memory (no disk read).
    if let Some(c) = &h.live_stats.last_compaction {
        out.push_str(&format!(
            "live changes merged in memory: {} ({}, {})\n",
            ago(c.at_ms),
            p(&c.reason),
            format_duration(c.took_ms)
        ));
    }
    match &h.last_save {
        Some(s) if s.ok => out.push_str(&format!(
            "last save: ok, {} ({}, {})\n",
            ago(s.at_ms),
            format_utc(s.at_ms),
            format_duration(s.took_ms)
        )),
        Some(s) => out.push_str(&format!(
            "last save: FAILED {}: {}\n",
            ago(s.at_ms),
            p(&s.error)
        )),
        None => out.push_str("last save: none yet\n"),
    }
    for v in &h.volumes {
        out.push_str(&format!("volume {}: {}", p(&v.root), p(&v.mode)));
        if let Some(n) = v.entries {
            out.push_str(&format!(", {n} entries"));
        }
        out.push_str(&format!(", journal {}\n", p(&v.journal)));
        // A live volume may carry a warning (for example a journal that
        // could not be re-checked after the MFT read); it is not a problem,
        // but the terminal should show it as the app does.
        if !v.journal_detail.is_empty() {
            out.push_str(&format!("  note: {}\n", p(&v.journal_detail)));
        }
        if let Some(scan) = &v.last_scan {
            out.push_str(&format!(
                "  last scan {} ({}, {})\n",
                ago(scan.at_ms),
                p(&scan.reason),
                format_duration(scan.took_ms)
            ));
        }
        if let Some(pending) = &v.rescan_pending {
            out.push_str(&format!(
                "  rescan pending ({}), not before {}\n",
                p(&pending.reason),
                ago(pending.earliest_ms)
            ));
        }
    }
    if h.more_volumes > 0 {
        out.push_str(&format!("  +{} more volumes not shown\n", h.more_volumes));
    }
    let shown = h.events.len().min(10);
    out.push_str(&format!("recent events ({shown} of {}):\n", h.events.len()));
    for e in &h.events[h.events.len() - shown..] {
        out.push_str(&format!(
            "  {} {} {}\n",
            format_utc(e.at_ms),
            p(&e.level),
            p(&e.text)
        ));
    }
    out
}

// ---- repair --------------------------------------------------------------------

/// The steps of `repair`, so their order and failure handling can be tested
/// without a real service.
pub trait RepairSteps {
    /// Read-only checks: `Some(summary)` when there is nothing to repair
    /// (registration, state folder and saved index are fine and the service
    /// answers), `None` otherwise.
    fn nothing_to_repair(&mut self) -> Option<String>;
    fn registration(&mut self) -> Result<String, String>;
    fn stop(&mut self) -> Result<String, String>;
    fn state_folder(&mut self) -> Result<String, String>;
    fn index(&mut self) -> Result<String, String>;
    fn start(&mut self) -> Result<String, String>;
    fn answers(&mut self) -> Result<String, String>;
}

const STOPPED_HINT: &str =
    "     the service is stopped; fix the problem above, then run `ferrow service start`";

/// Run the steps in order, one line each; the first failure ends the repair.
/// A failure after the service was stopped says that it is stopped.
pub fn run_repair(steps: &mut dyn RepairSteps, out: &mut dyn io::Write) -> Result<(), Failure> {
    if let Some(summary) = steps.nothing_to_repair() {
        let _ = writeln!(out, "Nothing to repair: {}", p(&summary));
        return Ok(());
    }
    let mut report = |name: &str, result: Result<String, String>, stopped: bool| match result {
        Ok(detail) => {
            let _ = writeln!(out, "[ok] {name}: {}", p(&detail));
            Ok(())
        }
        Err(detail) => {
            let _ = writeln!(out, "[FAILED] {name}: {}", p(&detail));
            if stopped {
                let _ = writeln!(out, "{STOPPED_HINT}");
            }
            Err(fail(1, format!("repair stopped at: {name}")))
        }
    };
    report("binary and registration", steps.registration(), false)?;
    report("stop", steps.stop(), false)?;
    report("state folder", steps.state_folder(), true)?;
    report("saved index", steps.index(), true)?;
    report("start", steps.start(), true)?;
    report("service answers", steps.answers(), false)?;
    let _ = writeln!(out, "repair finished");
    Ok(())
}

/// The "nothing to repair" decision once the registration and the state folder
/// are fine. A service that reports no problems and a successful save needs no
/// index check at all; otherwise the index is verified by streaming (never
/// loaded). One that does not answer `Health` must at least answer `Status`.
fn quiet(
    folder: &control::StateCheck,
    health: Option<Health>,
    index_ok: impl FnOnce() -> bool,
    answers: impl FnOnce() -> bool,
) -> Option<String> {
    let skip_index = match &health {
        Some(h) if !h.problems().is_empty() => return None,
        Some(h) => h.last_save.as_ref().is_some_and(|save| save.ok),
        None if answers() => false,
        None => return None,
    };
    if !skip_index && !index_ok() {
        return None;
    }
    let mut text =
        String::from("registration, state folder and saved index are fine and the service answers");
    for note in &folder.notes {
        text.push_str("; ");
        text.push_str(note);
    }
    Some(text)
}

/// The real steps.
struct Real {
    now: u64,
}

impl RepairSteps for Real {
    fn nothing_to_repair(&mut self) -> Option<String> {
        use control::{IndexProblem, Registration};
        if !matches!(control::check_registration(), Ok(Registration::Fine(_))) {
            return None;
        }
        let folder = control::check_state_folder(self.now, false).ok()?;
        if !folder.clean {
            return None;
        }
        let endpoint = Endpoint::default_service();
        let now = self.now;
        // Read-only: whatever the problem is, nothing is moved here.
        let index_ok = || {
            let Ok(index) = ferrowd::default_service_index_path() else {
                return false;
            };
            control::check_index(&index, now, |path| {
                control::load_index(path).map_err(|e| match e {
                    IndexProblem::Damaged(why) | IndexProblem::Unusable(why) => {
                        IndexProblem::Unusable(why)
                    }
                })
            })
            .is_ok()
        };
        quiet(&folder, control::health_of(&endpoint), index_ok, || {
            control::answers(&endpoint)
        })
    }
    fn registration(&mut self) -> Result<String, String> {
        use control::Registration::{Fine, Fixed, Needed};
        control::repair_registration().map(|r| match r {
            Fine(text) | Needed(text) => text,
            Fixed(text) => format!("repaired: {text}"),
        })
    }
    fn stop(&mut self) -> Result<String, String> {
        control::control(Action::Stop)
    }
    fn state_folder(&mut self) -> Result<String, String> {
        control::check_state_folder(self.now, true).map(|c| c.text)
    }
    fn index(&mut self) -> Result<String, String> {
        let index = ferrowd::default_service_index_path().map_err(|e| e.to_string())?;
        control::check_index(&index, self.now, control::load_index).map(|c| match c {
            IndexCheck::Missing => "none saved; the service builds one when it starts".into(),
            IndexCheck::Loads(what) => format!("loads ({what})"),
            IndexCheck::MovedAside { to, why } => format!(
                "damaged ({why}); moved to {} (kept, never deleted); the service rebuilds it",
                to.display()
            ),
        })
    }
    fn start(&mut self) -> Result<String, String> {
        control::control(Action::Start)
    }
    fn answers(&mut self) -> Result<String, String> {
        let endpoint = Endpoint::default_service();
        if control::wait_until(control::WAIT, Duration::from_millis(500), || {
            control::answers(&endpoint)
        }) {
            Ok("the service answers".into())
        } else {
            Err(format!(
                "no answer within {} s; see `ferrow service health` and the service log",
                control::WAIT.as_secs()
            ))
        }
    }
}

fn repair() -> Result<(), Failure> {
    let mut real = Real {
        now: ferrowd::health::now_ms(),
    };
    run_repair(&mut real, &mut io::stdout())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrow_ipc::health::{EventInfo, RescanInfo, SaveInfo, ScanInfo, VolumeHealth};

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn parses_every_verb_and_rejects_the_rest() {
        let ok = |args: &[&str]| parse(&os(args)).unwrap().unwrap();
        assert_eq!(ok(&["health"]), Cmd::Health { json: false });
        assert_eq!(ok(&["health", "--json"]), Cmd::Health { json: true });
        assert_eq!(ok(&["start"]), Cmd::Start);
        assert_eq!(ok(&["stop"]), Cmd::Stop);
        assert_eq!(ok(&["restart"]), Cmd::Restart);
        assert_eq!(ok(&["rescan"]), Cmd::Rescan);
        assert_eq!(ok(&["repair"]), Cmd::Repair);
        for bad in [
            &["health", "--yaml"][..],
            &["health", "--json", "x"],
            &["start", "now"],
            &["repair", "/some/path"],
            &["rescan", "--force"],
        ] {
            let error = parse(&os(bad)).unwrap().unwrap_err();
            assert!(error.contains("usage"), "{bad:?}: {error}");
        }
        // Verbs that belong to other commands are not ours.
        assert!(parse(&os(&["install"])).is_none());
        assert!(parse(&os(&["access", "show"])).is_none());
        assert!(parse(&os(&[])).is_none());
    }

    #[test]
    fn control_verbs_refuse_without_elevation_with_their_own_exit_code() {
        for (cmd, name) in [
            (Cmd::Start, "start"),
            (Cmd::Stop, "stop"),
            (Cmd::Restart, "restart"),
            (Cmd::Repair, "repair"),
        ] {
            let failure = check_elevated(&cmd, false).unwrap_err();
            assert_eq!(failure.code, EXIT_ELEVATION);
            assert!(
                failure.msg.contains("never elevates itself"),
                "{}",
                failure.msg
            );
            assert!(failure.msg.contains(&format!("ferrow service {name}")));
            assert!(check_elevated(&cmd, true).is_ok());
        }
        // Health and rescan ask the service, which decides for the caller.
        assert!(check_elevated(&Cmd::Health { json: true }, false).is_ok());
        assert!(check_elevated(&Cmd::Rescan, false).is_ok());
    }

    #[test]
    fn a_failed_attempt_maps_to_the_documented_exit_code() {
        let gone = vec!["pipe: not found".to_string()];
        let e = unanswered(None, None, gone.clone());
        assert_eq!(e.code, EXIT_UNREACHABLE);
        assert!(e.msg.contains("no Ferrow service is reachable"));
        let endpoint = Endpoint::named("x");
        let silent = io::Error::from(io::ErrorKind::TimedOut);
        let e = unanswered(None, Some((endpoint.clone(), silent)), gone.clone());
        assert_eq!(e.code, EXIT_UNREACHABLE);
        assert!(
            e.msg.contains("older") && e.msg.contains("busy"),
            "{}",
            e.msg
        );
        let e = unanswered(Some(endpoint), None, gone);
        assert_eq!(e.code, EXIT_REFUSED);
    }

    fn sample() -> Health {
        Health {
            version: "0.1.0-beta.18".into(),
            pid: 4242,
            started_ms: 1_709_210_096_000,
            now_ms: 1_709_220_896_000,
            uptime_s: 10_800,
            working_set: 301 << 20,
            private_bytes: Some(281 << 20),
            entries: 12_345_678,
            mode: "live".into(),
            access: "admins".into(),
            live_changes: 7,
            last_save: Some(SaveInfo {
                at_ms: 1_709_219_000_000,
                ok: true,
                error: String::new(),
                took_ms: 900,
            }),
            volumes: vec![VolumeHealth {
                root: r"C:\".into(),
                mode: "mft+usn live".into(),
                entries: Some(10_000_000),
                journal: "ok".into(),
                last_scan: Some(ScanInfo {
                    at_ms: 1_709_210_100_000,
                    reason: "startup".into(),
                    took_ms: 4_200,
                }),
                rescan_pending: Some(RescanInfo {
                    reason: "50001 live changes".into(),
                    earliest_ms: 1_709_221_000_000,
                }),
                ..VolumeHealth::default()
            }],
            events: vec![EventInfo {
                at_ms: 1_709_210_100_000,
                level: "info".into(),
                text: "walk: mft, 8 threads".into(),
            }],
            ..Health::default()
        }
    }

    #[test]
    fn json_output_is_the_report_on_one_line_and_reads_back() {
        let text = render(&sample(), true);
        assert_eq!(text.matches('\n').count(), 1, "one line");
        assert_eq!(Health::from_json(&text).unwrap(), sample());
        println!("health --json example:\n{text}");
    }

    #[test]
    fn the_summary_leads_with_the_verdict_then_the_facts() {
        let ok = render(&sample(), false);
        assert!(ok.starts_with("OK\n"), "{ok}");
        for fact in [
            "pid 4242",
            "12345678 entries",
            "301 MiB working set",
            r"volume C:\: mft+usn live, 10000000 entries",
            "rescan pending (50001 live changes)",
            "walk: mft, 8 threads",
        ] {
            assert!(ok.contains(fact), "{fact} missing from\n{ok}");
        }
        let mut bad = sample();
        bad.volumes[0].journal = "reader error".into();
        bad.volumes[0].journal_detail = "change journal keeps failing".into();
        bad.last_save = Some(SaveInfo {
            ok: false,
            error: "disk full".into(),
            ..SaveInfo::default()
        });
        let text = render(&bad, false);
        assert!(text.starts_with("ATTENTION: 2 problem(s)\n"), "{text}");
        assert!(text.contains("disk full") && text.contains("live updates stopped"));

        // A merge of the live changes in memory is shown once, among the
        // service's own lines, not under a volume.
        let mut merged = sample();
        merged.live_stats.last_compaction = Some(ScanInfo {
            at_ms: 1_709_219_000_000,
            reason: "50001 live changes".into(),
            took_ms: 12_400,
        });
        let text = render(&merged, false);
        assert!(
            text.contains("\nlive changes merged in memory: ")
                && text.contains("(50001 live changes, "),
            "{text}"
        );
        assert_eq!(text.matches("merged in memory").count(), 1, "{text}");
        assert!(!render(&sample(), false).contains("merged in memory"));

        // A live volume's warning is shown under it, but is not a problem.
        let mut warned = sample();
        warned.volumes[0].journal_detail = "could not re-check the change journal".into();
        let text = render(&warned, false);
        assert!(text.starts_with("OK\n"), "{text}");
        assert!(
            text.contains("  note: could not re-check the change journal\n"),
            "{text}"
        );
    }

    #[test]
    fn the_summary_shows_every_counter_and_never_prints_raw_control_characters() {
        let mut h = sample();
        h.connection_errors = 12;
        h.more_volumes = 3;
        h.events[0].text = "evil\u{1b}[2Jtext".into();
        h.volumes[0].root = "C:\\\u{7}".into();
        let text = render(&h, false);
        assert!(text.contains("connection errors: 12"), "{text}");
        assert!(text.contains("+3 more volumes"), "{text}");
        assert!(
            !text.chars().any(|c| c.is_control() && c != '\n'),
            "{text:?}"
        );
        assert!(text.contains("evil\\u{1b}[2Jtext"), "{text}");
        // The JSON line is the data as is; the terminal form is what is escaped.
        assert!(render(&h, true).contains("\\u001b"));
    }

    /// Records the calls and fails the named step.
    struct Script {
        calls: Vec<&'static str>,
        fail_at: Option<&'static str>,
        nothing: bool,
    }
    impl Script {
        fn new(fail_at: Option<&'static str>) -> Script {
            Script {
                calls: Vec::new(),
                fail_at,
                nothing: false,
            }
        }
        fn step(&mut self, name: &'static str) -> Result<String, String> {
            self.calls.push(name);
            if self.fail_at == Some(name) {
                Err(format!("{name} broke"))
            } else {
                Ok(format!("{name} fine"))
            }
        }
    }
    impl RepairSteps for Script {
        fn nothing_to_repair(&mut self) -> Option<String> {
            self.calls.push("check");
            self.nothing.then(|| "all fine".into())
        }
        fn registration(&mut self) -> Result<String, String> {
            self.step("registration")
        }
        fn stop(&mut self) -> Result<String, String> {
            self.step("stop")
        }
        fn state_folder(&mut self) -> Result<String, String> {
            self.step("state_folder")
        }
        fn index(&mut self) -> Result<String, String> {
            self.step("index")
        }
        fn start(&mut self) -> Result<String, String> {
            self.step("start")
        }
        fn answers(&mut self) -> Result<String, String> {
            self.step("answers")
        }
    }

    fn repair_with(script: &mut Script) -> (Result<(), Failure>, String) {
        let mut out = Vec::new();
        let result = run_repair(script, &mut out);
        (result, String::from_utf8(out).unwrap())
    }

    #[test]
    fn repair_runs_its_steps_in_order() {
        let mut script = Script::new(None);
        let (result, out) = repair_with(&mut script);
        assert!(result.is_ok());
        assert_eq!(
            script.calls,
            [
                "check",
                "registration",
                "stop",
                "state_folder",
                "index",
                "start",
                "answers"
            ]
        );
        assert!(out.contains("[ok] saved index: index fine"), "{out}");
        assert!(out.ends_with("repair finished\n"), "{out}");
        assert!(!out.contains("stopped"), "{out}");
    }

    #[test]
    fn a_failure_after_the_stop_says_the_service_is_stopped_and_ends_the_repair() {
        for failing in ["state_folder", "index", "start"] {
            let mut script = Script::new(Some(failing));
            let (result, out) = repair_with(&mut script);
            let failure = result.unwrap_err();
            assert_eq!(failure.code, 1);
            assert!(
                failure.msg.starts_with("repair stopped at:"),
                "{}",
                failure.msg
            );
            assert_eq!(
                script.calls.last(),
                Some(&failing),
                "nothing runs after the failure"
            );
            assert!(
                out.contains("[FAILED]") && out.contains(&format!("{failing} broke")),
                "{out}"
            );
            assert!(
                out.contains(
                    "the service is stopped; fix the problem above, then run `ferrow service start`"
                ),
                "{failing}: {out}"
            );
        }
        // Before the stop nothing was stopped, so no such hint.
        for failing in ["registration", "stop"] {
            let mut script = Script::new(Some(failing));
            let (result, out) = repair_with(&mut script);
            assert!(result.is_err());
            assert!(!out.contains("the service is stopped"), "{failing}: {out}");
            assert!(!script.calls.contains(&"state_folder"));
        }
    }

    #[test]
    fn nothing_to_repair_touches_nothing() {
        let mut script = Script::new(None);
        script.nothing = true;
        let (result, out) = repair_with(&mut script);
        assert!(result.is_ok());
        assert_eq!(script.calls, ["check"], "no stop, no restart");
        assert_eq!(out, "Nothing to repair: all fine\n");
    }

    #[test]
    fn a_healthy_service_needs_no_index_check_and_anything_else_streams_it() {
        let folder = control::StateCheck {
            clean: true,
            text: "x is protected".into(),
            notes: vec!["access.json fails its check (unprotected); the service ignores it".into()],
        };
        let mut healthy = sample();
        healthy.volumes[0].rescan_pending = None;
        // No problems and a successful save: the index is not even read.
        let said = quiet(
            &folder,
            Some(healthy.clone()),
            || panic!("index read"),
            || panic!("status asked"),
        )
        .unwrap();
        assert!(said.contains("service answers"), "{said}");
        assert!(
            said.contains("access.json fails its check"),
            "the note is shown: {said}"
        );
        // Never saved yet: the index is checked (and its verdict decides).
        let mut fresh = healthy.clone();
        fresh.last_save = None;
        assert!(quiet(&folder, Some(fresh.clone()), || true, || unreachable!()).is_some());
        assert!(quiet(&folder, Some(fresh), || false, || unreachable!()).is_none());
        // A problem, or an unanswering service, always means repair.
        let mut sick = healthy.clone();
        sick.volumes[0].journal = "reader error".into();
        assert!(quiet(&folder, Some(sick), || true, || true).is_none());
        assert!(quiet(&folder, None, || true, || false).is_none());
        // An older service answers Status only: its index is checked.
        assert!(quiet(&folder, None, || true, || true).is_some());
        assert!(quiet(&folder, None, || false, || true).is_none());
    }
}
