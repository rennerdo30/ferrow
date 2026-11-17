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
    println!("{msg}");
    if started {
        Ok(())
    } else {
        Err(fail(EXIT_NOT_STARTED, ""))
    }
}

// ---- output ------------------------------------------------------------------

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
        for p in &problems {
            text.push_str(&format!("  ! {p}\n"));
        }
        text
    };
    out.push_str(&format!(
        "Ferrow service {} (pid {}), up {}, started {}\n",
        h.version,
        h.pid,
        format_duration(h.uptime_s * 1000),
        format_utc(h.started_ms)
    ));
    out.push_str(&format!(
        "index: {} entries, {}{}; access: {}\n",
        h.entries,
        h.mode,
        if h.scanning { ", scanning now" } else { "" },
        h.access
    ));
    out.push_str(&format!(
        "memory: {} working set{}\n",
        mb(h.working_set),
        h.private_bytes
            .map_or(String::new(), |p| format!(", {} private", mb(p)))
    ));
    out.push_str(&format!("live changes applied: {}\n", h.live_changes));
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
            s.error
        )),
        None => out.push_str("last save: none yet\n"),
    }
    for v in &h.volumes {
        out.push_str(&format!("volume {}: {}", v.root, v.mode));
        if let Some(n) = v.entries {
            out.push_str(&format!(", {n} entries"));
        }
        out.push_str(&format!(", journal {}\n", v.journal));
        if let Some(scan) = &v.last_scan {
            out.push_str(&format!(
                "  last scan {} ({}, {})\n",
                ago(scan.at_ms),
                scan.reason,
                format_duration(scan.took_ms)
            ));
        }
        if let Some(p) = &v.rescan_pending {
            out.push_str(&format!(
                "  rescan pending ({}), not before {}\n",
                p.reason,
                ago(p.earliest_ms)
            ));
        }
    }
    let shown = h.events.len().min(10);
    out.push_str(&format!("recent events ({shown} of {}):\n", h.events.len()));
    for e in &h.events[h.events.len() - shown..] {
        out.push_str(&format!(
            "  {} {} {}\n",
            format_utc(e.at_ms),
            e.level,
            e.text
        ));
    }
    out
}

// ---- repair --------------------------------------------------------------------

fn repair() -> Result<(), Failure> {
    let now = ferrowd::health::now_ms();
    let stopped_hint =
        "the service is stopped; fix the problem above, then run `ferrow service start`";
    // One line per step; the first failure ends the repair.
    let run = |name: &str, result: Result<String, String>| -> Result<(), Failure> {
        match result {
            Ok(detail) => {
                println!("[ok] {name}: {detail}");
                Ok(())
            }
            Err(detail) => {
                println!("[FAILED] {name}: {detail}");
                Err(fail(1, format!("repair stopped at: {name}")))
            }
        }
    };
    run("binary and registration", control::repair_registration())?;
    run("stop", control::control(Action::Stop))?;
    let after_stop = |r: Result<String, String>| r.map_err(|e| format!("{e}\n{stopped_hint}"));
    run("state folder", after_stop(control::check_state_folder(now)))?;
    let index = ferrowd::default_service_index_path().map_err(|e| e.to_string())?;
    let checked = control::check_index(&index, now, control::load_index).map(|c| match c {
        IndexCheck::Missing => "none saved; the service builds one when it starts".into(),
        IndexCheck::Loads(what) => format!("loads ({what})"),
        IndexCheck::MovedAside { to, why } => format!(
            "did not load ({why}); moved to {} (kept, never deleted); the service rebuilds it",
            to.display()
        ),
    });
    run("saved index", after_stop(checked))?;
    run("start", control::control(Action::Start))?;
    let endpoint = Endpoint::default_service();
    let answering = control::wait_until(control::WAIT, Duration::from_millis(500), || {
        control::answers(&endpoint)
    });
    run(
        "service answers",
        if answering {
            Ok("health request answered".into())
        } else {
            Err(format!(
                "no answer within {} s; see `ferrow service health` and the service log",
                control::WAIT.as_secs()
            ))
        },
    )?;
    println!("repair finished");
    Ok(())
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
    }
}
