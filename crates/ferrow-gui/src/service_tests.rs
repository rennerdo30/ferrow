use super::*;
use ferrow_ipc::health::{
    EventInfo, JOURNAL_ERROR, JOURNAL_OK, JOURNAL_RESET, LEVEL_ERROR, LEVEL_INFO, MODE_LIVE,
    MODE_UNAVAILABLE, MODE_WALK, RescanInfo, SaveInfo, ScanInfo, VolumeHealth,
};
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const APP: &str = "0.1.0-beta.17";

fn live(root: &str) -> VolumeHealth {
    VolumeHealth {
        root: root.into(),
        mode: MODE_LIVE.into(),
        entries: Some(9_000_000),
        journal: JOURNAL_OK.into(),
        last_scan: Some(ScanInfo {
            at_ms: 1_000,
            reason: "startup".into(),
            took_ms: 4_200,
        }),
        ..VolumeHealth::default()
    }
}

fn report(volumes: Vec<VolumeHealth>) -> Health {
    Health {
        version: APP.into(),
        pid: 4120,
        uptime_s: 3 * 86_400 + 5_000,
        working_set: 300 << 20,
        private_bytes: Some(250 << 20),
        entries: 18_400_000,
        mode: "live".into(),
        access: "admins".into(),
        last_save: Some(SaveInfo {
            at_ms: 5,
            ok: true,
            error: String::new(),
            took_ms: 340,
        }),
        volumes,
        ..Health::default()
    }
}

fn snap(state: ScmState, health: HealthView) -> Snapshot {
    Snapshot {
        scm: Scm {
            state,
            pid: (state == ScmState::Running).then_some(4120),
            start_type: Some(2),
        },
        health,
    }
}

fn healthy() -> Snapshot {
    snap(
        ScmState::Running,
        HealthView::Report(Box::new(report(vec![live("C:\\"), live("D:\\")]))),
    )
}

fn with(f: impl FnOnce(&mut Health)) -> Snapshot {
    let mut h = report(vec![live("C:\\"), live("D:\\")]);
    f(&mut h);
    snap(ScmState::Running, HealthView::Report(Box::new(h)))
}

fn line(s: &Snapshot) -> (Level, bool, String) {
    let m = summary(Some(s), APP);
    (m.level, m.filled, format!("{} · {}", m.head, m.rest))
}

#[test]
fn test_builds_never_ask_the_real_service() {
    let s = query_snapshot();
    assert_eq!(s.scm.state, ScmState::NotInstalled);
    assert_eq!(s.health, HealthView::NoAnswer);
}

#[test]
fn the_service_manager_codes_map_to_states() {
    for (code, state) in [
        (1, ScmState::Stopped),
        (2, ScmState::Starting),
        (3, ScmState::Stopping),
        (4, ScmState::Running),
        (5, ScmState::Starting),
        (6, ScmState::Stopping),
        (7, ScmState::Other),
        (0, ScmState::Other),
    ] {
        assert_eq!(ScmState::from_code(code), state, "{code}");
    }
    assert!(snap(ScmState::Running, HealthView::NoAnswer).scm.up());
    assert!(!snap(ScmState::Stopped, HealthView::NoAnswer).scm.up());
    assert!(!snap(ScmState::NotInstalled, HealthView::NoAnswer).scm.up());
}

#[test]
fn health_is_classified_by_what_each_question_got() {
    let timeout = || std::io::Error::from(std::io::ErrorKind::TimedOut);
    let denied = || ferrow_ipc::server_error(ferrow_ipc::ErrorCode::Unauthorized, "no");
    let never = || -> std::io::Result<()> { panic!("status must not be asked") };
    assert!(matches!(
        health_view(Ok(report(vec![])), never),
        HealthView::Report(_)
    ));
    assert_eq!(health_view(Err(denied()), never), HealthView::Refused);
    // Silent on Health but answering Status: an older service.
    assert_eq!(health_view(Err(timeout()), || Ok(())), HealthView::Older);
    // A deadline on both: something is there and does not answer (busy).
    assert_eq!(
        health_view(Err(timeout()), || Err(timeout())),
        HealthView::Silent
    );
    // Nothing listens: gone, not busy.
    let gone = || std::io::Error::from(std::io::ErrorKind::NotFound);
    assert_eq!(
        health_view(Err(gone()), || Err(gone())),
        HealthView::NoAnswer
    );
    // Refused on the second question: still a refusal, not "older".
    assert_eq!(
        health_view(Err(timeout()), || Err(denied())),
        HealthView::Refused
    );
}

#[test]
fn no_service_is_reported_at_once() {
    let started = Instant::now();
    let endpoint = Endpoint::named(&format!("ferrow-health-none-{}", std::process::id()));
    assert_eq!(ask_health(&endpoint), HealthView::NoAnswer);
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn the_top_line_says_the_state_in_every_case() {
    let (good, caution, neutral) = (Level::Good, Level::Caution, Level::Neutral);
    let l = line(&healthy());
    assert_eq!(
        l,
        (
            good,
            true,
            "Healthy · running 3 d · 18.4 M items · live on C: D:".into()
        )
    );
    // Rescan pending names the volume and why.
    let pending = with(|h| {
        h.volumes[1].rescan_pending = Some(RescanInfo {
            reason: "journal reset".into(),
            earliest_ms: 9,
        });
        h.volumes[1].journal = JOURNAL_RESET.into();
    });
    assert_eq!(
        line(&pending),
        (
            caution,
            true,
            "Running · rescan pending on D: (journal reset)".into()
        )
    );
    let lost = with(|h| {
        h.volumes[1].mode = MODE_UNAVAILABLE.into();
        h.volumes[1].reason = "access denied".into();
    });
    assert_eq!(
        line(&lost),
        (
            caution,
            true,
            "Running · live updates unavailable on D:".into()
        )
    );
    let reader = with(|h| h.volumes[0].journal = JOURNAL_ERROR.into());
    assert_eq!(line(&reader).0, caution);
    let save = with(|h| h.last_save.as_mut().unwrap().ok = false);
    assert_eq!(line(&save).2, "Running · the last save of the index failed");
    let skew = with(|h| h.version = "0.1.0-beta.16".into());
    assert_eq!(
        line(&skew),
        (
            caution,
            true,
            "Running · service 0.1.0-beta.16, app 0.1.0-beta.17".into()
        )
    );
    assert_eq!(line(&with(|h| h.scanning = true)).0, neutral);
    // A walked volume is healthy but not live.
    let walked = with(|h| {
        h.volumes.truncate(1);
        h.volumes[0].mode = MODE_WALK.into();
    });
    assert_eq!(line(&walked).0, good);
    assert!(line(&walked).2.contains("not live"));

    let down = |state, health| line(&snap(state, health));
    assert_eq!(
        down(ScmState::NotInstalled, HealthView::NoAnswer),
        (caution, false, "Not installed · ".into())
    );
    assert_eq!(
        down(ScmState::Stopped, HealthView::NoAnswer),
        (caution, false, "Stopped · ".into())
    );
    assert_eq!(down(ScmState::Starting, HealthView::NoAnswer).0, neutral);
    assert_eq!(down(ScmState::Stopping, HealthView::NoAnswer).0, neutral);
    assert_eq!(
        down(ScmState::Running, HealthView::Refused).2,
        "Running, but this account isn't allowed to see its health · "
    );
    assert_eq!(
        down(ScmState::Running, HealthView::Older),
        (
            caution,
            true,
            "Older service, or busy · update with Setup if this stays".into()
        )
    );
    assert_eq!(
        down(ScmState::Running, HealthView::NoAnswer),
        (caution, true, "Not answering · ".into())
    );
    // A running process with a problem is a dot, never a ring.
    for health in [HealthView::Refused, HealthView::Older, HealthView::Silent] {
        assert!(down(ScmState::Running, health).1);
    }
    // The manager's word wins over a report that is about to be out of date.
    let stopping = Snapshot {
        scm: Scm::of(ScmState::Stopping),
        ..healthy()
    };
    assert_eq!(line(&stopping), (neutral, false, "Stopping… · ".into()));
    let starting = Snapshot {
        scm: Scm::of(ScmState::Starting),
        ..healthy()
    };
    assert_eq!(line(&starting).2, "Starting… · ");
    // A busy service shows as not answering, with what it said last.
    let stale = snap(
        ScmState::Running,
        HealthView::Stale(Box::new(report(vec![live("C:\\")]))),
    );
    assert_eq!(
        line(&stale),
        (
            caution,
            true,
            "Not answering right now · it may be busy; showing its last report".into()
        )
    );
    assert_eq!(summary(None, APP).head, "Checking the service…");
    // A running answer wins over a stale "stopped" from the manager.
    let both = Snapshot {
        scm: Scm::of(ScmState::Stopped),
        ..healthy()
    };
    assert_eq!(line(&both).0, good);
}

#[test]
fn the_fast_index_card_points_here_only_when_something_needs_a_look() {
    assert!(!healthy().needs_attention(APP));
    assert!(snap(ScmState::Stopped, HealthView::NoAnswer).needs_attention(APP));
    assert!(snap(ScmState::NotInstalled, HealthView::NoAnswer).needs_attention(APP));
    assert!(snap(ScmState::Running, HealthView::Older).needs_attention(APP));
    assert!(with(|h| h.version = "x".into()).needs_attention(APP));
    // A refused account is answered by the access card next to it.
    assert!(!snap(ScmState::Running, HealthView::Refused).needs_attention(APP));
    assert!(!snap(ScmState::Starting, HealthView::NoAnswer).needs_attention(APP));
}

#[test]
fn numbers_read_at_a_glance() {
    assert_eq!(uptime_text(30), "just started");
    assert_eq!(uptime_text(600), "10 min");
    assert_eq!(uptime_text(7_300), "2 h");
    assert_eq!(uptime_text(3 * 86_400 + 9), "3 d");
    assert_eq!(items_text(842), "842");
    assert_eq!(items_text(412_345), "412 k");
    assert_eq!(items_text(18_422_907), "18.4 M");
    assert_eq!(memory_text(300 << 20), "300 MB");
    assert_eq!(short_root("C:\\"), "C:");
    assert_eq!(short_root("D:\\Projects"), "D:\\Projects");
    assert_eq!(short_root("/home"), "/home");
}

#[test]
fn action_buttons_follow_the_state() {
    use Can::{Hidden, No, Yes};
    let a = |s: &Snapshot| actions(Some(s), true, false);
    let h = a(&healthy());
    assert_eq!(
        (h.start, h.restart, h.rescan, h.repair),
        (Hidden, Yes, Yes, Yes)
    );
    let stopped = a(&snap(ScmState::Stopped, HealthView::NoAnswer));
    assert_eq!(stopped.start, Yes);
    assert!(matches!(stopped.restart, No(why) if why.contains("Start")));
    assert!(matches!(stopped.rescan, No(why) if why.contains("isn't running")));
    assert_eq!(stopped.repair, Yes);
    let missing = a(&snap(ScmState::NotInstalled, HealthView::NoAnswer));
    assert_eq!(missing.start, Hidden);
    assert!(matches!(missing.restart, No(why) if why.contains("isn't installed")));
    assert_eq!(missing.repair, Yes);
    let older = a(&snap(ScmState::Running, HealthView::Older));
    assert!(matches!(older.rescan, No(why) if why.contains("Older")));
    assert_eq!(older.restart, Yes);
    let starting = a(&snap(ScmState::Starting, HealthView::NoAnswer));
    assert!(matches!(starting.restart, No(_)) && matches!(starting.repair, No(_)));
    // Refused here: only the elevated rescan can work.
    assert_eq!(a(&snap(ScmState::Running, HealthView::Refused)).rescan, Yes);
    assert!(matches!(
        a(&with(|h| h.scanning = true)).rescan,
        No(why) if why.contains("running already")
    ));
    // This copy cannot elevate: the elevated ones are off with the reason,
    // the unelevated rescan stays.
    let portable = actions(Some(&healthy()), false, false);
    assert_eq!(portable.restart, No(NEEDS_INSTALLED));
    assert_eq!(portable.repair, No(NEEDS_INSTALLED));
    assert_eq!(portable.rescan, Yes);
    // While one runs, none starts another; nothing before the first answer.
    let busy = actions(Some(&healthy()), true, true);
    assert!(matches!(
        (busy.restart, busy.rescan, busy.repair),
        (No(_), No(_), No(_))
    ));
    assert!(matches!(actions(None, true, false).restart, No(_)));
    // Linux and macOS have no service manager to ask: no answer means "start it".
    let unix = snap(ScmState::Unsupported, HealthView::NoAnswer);
    assert_eq!(a(&unix).start, Yes);
}

#[test]
fn elevated_verbs_are_fixed_words() {
    assert_eq!(Verb::Start.args(), "service start");
    assert_eq!(Verb::Restart.args(), "service restart");
    assert_eq!(Verb::Rescan.args(), "service rescan");
    assert_eq!(Verb::Repair.args(), "service repair");
    for v in [Verb::Start, Verb::Restart, Verb::Rescan, Verb::Repair] {
        assert!(!v.terminal_command().is_empty());
    }
    if cfg!(windows) {
        assert_eq!(Verb::Repair.terminal_command(), "ferrow service repair");
    }
}

#[test]
fn every_way_an_action_can_end_has_its_own_outcome() {
    let o = |ran, seen| outcome(Verb::Restart, ran, Some(seen));
    assert_eq!(o(Ok(0), true).outcome, Outcome::Done);
    assert!(o(Ok(0), true).text.starts_with("Restarted"));
    assert_eq!(o(Ok(0), false).outcome, Outcome::Problem);
    assert!(o(Ok(0), false).text.contains("isn't answering"));
    let declined = o(Err(RunError::Declined), false);
    assert_eq!(declined.outcome, Outcome::Declined);
    assert_eq!(declined.text, access::DECLINED_TEXT);
    // A declined prompt is never "done", even when the service looks fine.
    assert_eq!(o(Err(RunError::Declined), true).outcome, Outcome::Declined);
    assert_eq!(o(Err(RunError::Failed("boom".into())), false).text, "boom");
    for (code, word) in [(2, "administrator"), (3, "did not answer"), (4, "refused")] {
        let n = o(Ok(code), false);
        assert_eq!(n.outcome, Outcome::Problem, "{code}");
        assert!(n.text.contains(word), "{code}: {}", n.text);
    }
    let nothing = outcome(Verb::Rescan, Ok(5), Some(false));
    assert_eq!(nothing.outcome, Outcome::NoChange);
    assert!(nothing.text.contains("ended less than a minute ago"));
    // The repair text claims only what the answer shows: it may have found
    // nothing to repair.
    let repaired = outcome(Verb::Repair, Ok(0), Some(true)).text;
    assert_eq!(repaired, "Repair finished. The service is running.");
    for claim in ["registered", "re-registered", "restarted", "moved"] {
        assert!(!repaired.to_lowercase().contains(claim), "{claim}");
    }
    let failed = o(Ok(1), false);
    assert_eq!(failed.outcome, Outcome::Problem);
    // It names the command for this OS (systemctl on Linux).
    assert!(failed.text.contains(&Verb::Restart.terminal_command()));
    assert!(failed.text.contains("administrator terminal"));
    // Even a zero exit is not "done" when the answer does not show it.
    assert_ne!(o(Ok(0), false).outcome, Outcome::Done);
}

#[test]
fn after_an_action_it_looks_again_until_the_answer_shows_it_and_gives_up_in_time() {
    let stopped = snap(ScmState::Stopped, HealthView::NoAnswer);
    let calls = RefCell::new(0u32);
    let paused = RefCell::new(Duration::ZERO);
    let (shown, seen) = settle(
        Verb::Start,
        &stopped,
        || {
            *calls.borrow_mut() += 1;
            if *calls.borrow() < 4 {
                stopped.clone()
            } else {
                healthy()
            }
        },
        || true,
        |d| *paused.borrow_mut() += d,
    );
    assert!(seen == Some(true) && shown == Some(healthy()));
    assert_eq!(*calls.borrow(), 4);
    // Looks come at 0, 1.2 and 2.4 s, then at the normal cadence, and it
    // never waits longer than the bound; then it reports "not seen".
    let pauses = RefCell::new(Vec::new());
    let (_, seen) = settle(
        Verb::Start,
        &stopped,
        || stopped.clone(),
        || true,
        |d| pauses.borrow_mut().push(d),
    );
    assert_eq!(seen, Some(false));
    let pauses = pauses.into_inner();
    assert_eq!(
        pauses[..3],
        [SETTLE_FIRST, SETTLE_FIRST, REFRESH],
        "{pauses:?}"
    );
    assert!(pauses[2..].iter().all(|p| *p == REFRESH));
    let total: Duration = pauses.iter().sum();
    assert!(
        total >= SETTLE_MAX && total < SETTLE_MAX + REFRESH,
        "{total:?}"
    );
    // A restart is done only when another process answers.
    let before = healthy();
    let mut same = healthy();
    same.scm.pid = Some(4120);
    assert!(!expected(Verb::Restart, &before, &same));
    let mut new = with(|h| h.pid = 7000);
    new.scm.pid = Some(7000);
    assert!(expected(Verb::Restart, &before, &new));
    // ...and a start or repair is done when it runs and answers, however it answers.
    assert!(expected(
        Verb::Start,
        &stopped,
        &snap(ScmState::Running, HealthView::Refused)
    ));
    assert!(!expected(
        Verb::Repair,
        &stopped,
        &snap(ScmState::Running, HealthView::NoAnswer)
    ));
}

fn perform_with(
    verb: Verb,
    rescan: IpcRescan,
    ran: Result<u32, RunError>,
) -> (Done, Vec<String>, usize) {
    let elevated = RefCell::new(Vec::new());
    let asks = RefCell::new(0usize);
    let prompt = std::sync::atomic::AtomicBool::new(false);
    let mut ask = || {
        *asks.borrow_mut() += 1;
        healthy()
    };
    let done = perform(
        verb,
        &snap(ScmState::Stopped, HealthView::NoAnswer),
        &prompt,
        || rescan,
        |args| {
            elevated.borrow_mut().push(args.to_owned());
            ran
        },
        &mut ask,
        &|| true,
        |_| {},
    );
    assert!(
        !prompt.load(Ordering::Relaxed),
        "the prompt flag is cleared"
    );
    let asked = *asks.borrow();
    (done, elevated.into_inner(), asked)
}

#[test]
fn a_rescan_the_service_allows_never_asks_for_the_administrator() {
    let (done, elevated, _) = perform_with(
        Verb::Rescan,
        IpcRescan::Started("rescan requested".into()),
        Ok(0),
    );
    assert!(elevated.is_empty());
    assert_eq!(
        (done.notice.outcome, done.notice.text.as_str()),
        (Outcome::Done, "rescan requested")
    );
    // The service's refusal sentence (the 60 s gap) is shown as it came.
    let (done, elevated, _) = perform_with(
        Verb::Rescan,
        IpcRescan::NothingStarted("a rescan started 20 s ago".into()),
        Ok(0),
    );
    assert!(elevated.is_empty());
    assert_eq!(
        (done.notice.outcome, done.notice.text.as_str()),
        (Outcome::NoChange, "a rescan started 20 s ago")
    );
    let (done, _, _) = perform_with(Verb::Rescan, IpcRescan::Failed("no answer".into()), Ok(0));
    assert_eq!(done.notice.outcome, Outcome::Problem);
}

#[test]
fn an_unauthorized_rescan_falls_back_to_the_elevated_verb_and_other_actions_go_straight_to_it() {
    let (done, elevated, _) = perform_with(Verb::Rescan, IpcRescan::NotAllowed, Ok(0));
    assert_eq!(elevated, ["service rescan"]);
    assert_eq!(done.notice.outcome, Outcome::Done);
    let (_, elevated, _) = perform_with(Verb::Repair, IpcRescan::Failed("unused".into()), Ok(0));
    assert_eq!(elevated, ["service repair"]);
    // Exit 5 from the elevated rescan: nothing was started.
    let (done, _, _) = perform_with(Verb::Rescan, IpcRescan::NotAllowed, Ok(5));
    assert_eq!(done.notice.outcome, Outcome::NoChange);
}

#[test]
fn a_declined_prompt_is_reported_declined_and_does_not_wait_for_the_service() {
    let (done, elevated, asked) = perform_with(
        Verb::Restart,
        IpcRescan::NotAllowed,
        Err(RunError::Declined),
    );
    assert_eq!(elevated, ["service restart"]);
    assert_eq!(done.notice.outcome, Outcome::Declined);
    assert_eq!(asked, 1, "one look at the state, no 30 s wait");
}

#[test]
fn an_action_runs_off_the_calling_thread_and_its_result_arrives_through_poll() {
    let mut state = ServiceHealth::new();
    let (release, gate) = channel::<()>();
    let woken = Arc::new(AtomicUsize::new(0));
    let w = woken.clone();
    let caller = std::thread::current().id();
    let ran_on = Arc::new(Mutex::new(None));
    let r = ran_on.clone();
    let asked_on = Arc::new(Mutex::new(None));
    let a = asked_on.clone();
    let started = Instant::now();
    state.run_with(
        Verb::Repair,
        move || {
            w.fetch_add(1, Ordering::SeqCst);
        },
        || IpcRescan::NotAllowed,
        move |_| {
            *r.lock().unwrap() = Some(std::thread::current().id());
            gate.recv().ok(); // the administrator prompt stays open
            Ok(0)
        },
        move || {
            *a.lock().unwrap() = Some(std::thread::current().id());
            healthy()
        },
        || true,
        |_| {},
    );
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "run() must not wait"
    );
    assert!(state.busy.is_some());
    assert!(!state.poll(), "nothing yet while the prompt is open");
    // A second click while one runs is ignored.
    state.run_with(
        Verb::Start,
        || {},
        || IpcRescan::NotAllowed,
        |_| Ok(0),
        healthy,
        || true,
        |_| {},
    );
    release.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !state.poll() {
        assert!(Instant::now() < deadline, "the result never arrived");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(state.busy.is_none());
    assert_eq!(state.notice.as_ref().unwrap().outcome, Outcome::Done);
    assert_eq!(state.snap, Some(healthy()));
    assert_ne!(*ran_on.lock().unwrap(), Some(caller));
    // With no answer shown yet, the first look is on the worker too.
    assert!(asked_on.lock().unwrap().is_some_and(|t| t != caller));
    assert_eq!(woken.load(Ordering::SeqCst), 1);
}

/// A service state that counts the asks and wakes.
fn counted(interval: Duration) -> (ServiceHealth, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let mut state = ServiceHealth::new();
    state.interval = interval;
    (
        state,
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
    )
}

fn open(
    state: &mut ServiceHealth,
    asks: &Arc<AtomicUsize>,
    wakes: &Arc<AtomicUsize>,
    answer: impl Fn(usize) -> Snapshot + Send + 'static,
) {
    let (a, w) = (asks.clone(), wakes.clone());
    state.set_visible_with(
        true,
        move || {
            w.fetch_add(1, Ordering::SeqCst);
        },
        move || answer(a.fetch_add(1, Ordering::SeqCst)),
        || false,
    );
}

#[test]
fn it_asks_at_most_once_per_interval_and_only_while_shown() {
    let interval = Duration::from_millis(200);
    let (mut state, asks, wakes) = counted(interval);
    open(&mut state, &asks, &wakes, |_| healthy());
    assert!(state.watching());
    std::thread::sleep(Duration::from_millis(500));
    let while_open = asks.load(Ordering::SeqCst);
    // Asks at 0, 200 and 400 ms at the most; never a burst.
    assert!((1..=3).contains(&while_open), "{while_open}");
    // Closed (or minimized, or in the tray): the thread is gone and asks stop.
    state.set_visible_with(false, || {}, healthy, || false);
    assert!(!state.watching());
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(asks.load(Ordering::SeqCst), while_open);
}

#[test]
fn closing_and_reopening_quickly_does_not_ask_twice_inside_the_interval() {
    let interval = Duration::from_millis(600);
    let (mut state, asks, wakes) = counted(interval);
    open(&mut state, &asks, &wakes, |_| healthy());
    let deadline = Instant::now() + Duration::from_secs(2);
    while asks.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    state.set_visible_with(false, || {}, healthy, || false);
    open(&mut state, &asks, &wakes, |_| healthy());
    std::thread::sleep(Duration::from_millis(250));
    assert_eq!(
        asks.load(Ordering::SeqCst),
        1,
        "the throttle outlives the thread"
    );
}

#[test]
fn the_window_wakes_only_when_what_is_shown_changed() {
    let (mut state, asks, wakes) = counted(Duration::from_millis(30));
    // The same service every time apart from the clock: one wake. Then a
    // volume goes down: a second.
    open(&mut state, &asks, &wakes, |n| {
        if n < 5 {
            with(|h| {
                h.now_ms = n as u64 * 5_000;
                h.uptime_s += n as u64;
                h.working_set += (n as u64) << 10;
                h.live_changes = n as u64;
            })
        } else {
            with(|h| h.volumes[1].mode = MODE_UNAVAILABLE.into())
        }
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while wakes.load(Ordering::SeqCst) < 2 {
        assert!(Instant::now() < deadline, "no wake for the change");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(asks.load(Ordering::SeqCst) >= 6);
    assert_eq!(wakes.load(Ordering::SeqCst), 2, "no wake for clock jitter");
    state.poll();
    assert!(state.snap.as_ref().unwrap().report().unwrap().volumes[1].mode == MODE_UNAVAILABLE);
}

#[test]
fn what_is_shown_ignores_only_what_the_text_does_not_say() {
    let base = healthy();
    let jitter = with(|h| {
        h.now_ms += 90_000;
        h.uptime_s += 20;
        h.working_set += 4_000;
        h.entries += 5_000;
        h.live_changes += 7;
        h.volumes[0].entries = Some(9_000_100);
    });
    assert_eq!(base.display_key(), jitter.display_key());
    for changed in [
        with(|h| h.volumes[0].mode = MODE_WALK.into()),
        with(|h| {
            h.events.push(EventInfo {
                at_ms: 1,
                level: LEVEL_INFO.into(),
                text: "x".into(),
            })
        }),
        with(|h| h.uptime_s += 86_400),
        with(|h| h.scanning = true),
        with(|h| h.version = "other".into()),
        snap(ScmState::Stopped, HealthView::NoAnswer),
    ] {
        assert_ne!(base.display_key(), changed.display_key());
    }
}

#[test]
fn events_list_newest_first_five_or_all() {
    let events: Vec<EventInfo> = (0..12)
        .map(|n| EventInfo {
            at_ms: n,
            level: LEVEL_INFO.into(),
            text: format!("e{n}"),
        })
        .collect();
    let few = events_shown(&events, false);
    assert_eq!(
        few.iter().map(|e| e.at_ms).collect::<Vec<_>>(),
        [11, 10, 9, 8, 7]
    );
    let all = events_shown(&events, true);
    assert_eq!((all.len(), all[0].at_ms, all[11].at_ms), (12, 11, 0));
    assert_eq!(events_shown(&events[..3], false).len(), 3);
    assert!(events_shown(&[], false).is_empty());
}

#[test]
fn diagnostics_name_no_file_beyond_the_volume_roots() {
    let mut h = report(vec![live("C:\\"), live("D:\\Projects")]);
    h.volumes[1].mode = MODE_UNAVAILABLE.into();
    h.volumes[1].reason = "cannot open D:\\$Extend\\$UsnJrnl: access denied".into();
    h.last_save = Some(SaveInfo {
        at_ms: 5,
        ok: false,
        error: "could not write C:\\ProgramData\\Ferrow\\index.bin: disk full".into(),
        took_ms: 1,
    });
    h.events = vec![
        EventInfo { at_ms: 1_000, level: LEVEL_INFO.into(), text: "scanning D:\\Projects".into() },
        EventInfo {
            at_ms: 2_000,
            level: LEVEL_ERROR.into(),
            text: "moved C:\\Program Files\\Ferrow\\service\\index.bad and /var/lib/ferrow/index.bin aside".into(),
        },
    ];
    let text = diagnostics(
        &snap(ScmState::Running, HealthView::Report(Box::new(h))),
        APP,
        "windows x86_64",
    );
    assert!(text.contains("App: 0.1.0-beta.17") && text.contains("OS: windows x86_64"));
    assert!(text.contains("Service manager: running, process 4120"));
    assert!(
        text.contains("D:\\\\Projects"),
        "a volume root stays (JSON-escaped)"
    );
    assert!(
        text.contains("scanning D:\\Projects"),
        "a root stays in the events"
    );
    for leaked in [
        "ProgramData",
        "$Extend",
        "UsnJrnl",
        "Program Files",
        "index.bin",
        "index.bad",
        "/var/lib",
        "Ferrow\\service",
    ] {
        assert!(!text.contains(leaked), "{leaked} leaked:\n{text}");
    }
    assert!(text.contains("<path omitted>"));
    // Newest event first.
    let list = text.split("Recent events").nth(1).expect("an events list");
    assert!(list.find("moved <path omitted>").unwrap() < list.find("scanning D:").unwrap());
    // Without a report there is still a useful block.
    let none = diagnostics(
        &snap(ScmState::Stopped, HealthView::NoAnswer),
        APP,
        "linux x86_64",
    );
    assert!(none.contains("stopped") && none.contains("not available"));
    assert!(diagnostics(&snap(ScmState::Running, HealthView::Older), APP, "x").contains("older"));
}

#[test]
fn scrub_keeps_roots_and_plain_words_and_cuts_at_the_first_other_path() {
    let roots = ["C:\\", "D:\\Projects", "/home", "E:\\My Archive"];
    for same in [
        "rescan of C:\\ and /home done",
        "(\"D:\\Projects\"), live/usn",
        "Scan of E:\\My Archive finished in 4 s",
        "journal reset on D:\\Projects: rescan queued",
        "nothing to report",
        "",
    ] {
        assert_eq!(scrub(same, &roots), same);
    }
    let cut = |text: &str| scrub(text, &roots);
    assert_eq!(cut("D:\\Projects\\a.txt"), "<path omitted>");
    assert_eq!(cut("see \\\\nas\\share\\x now"), "see <path omitted>");
    assert_eq!(cut("open C:/Users/a/b failed"), "open <path omitted>");
    assert_eq!(cut("/home/alex/file"), "<path omitted>");
    // A path with a space in it must not leak its second half.
    assert_eq!(
        cut("could not read C:\\Users\\Jane Doe\\Documents: denied"),
        "could not read <path omitted>"
    );
    assert_eq!(
        cut("could not read /home/jane doe/notes failed"),
        "could not read <path omitted>"
    );
    assert_eq!(cut("opened name=/home/x now"), "opened <path omitted>");
    assert_eq!(cut("opened path:C:\\Temp\\a b"), "opened <path omitted>");
    // The cut also drops what a root-looking prefix would hide.
    assert_eq!(
        cut("scan of C:\\Users\\Jane Doe done"),
        "scan of <path omitted>"
    );
    // A root first, a path later: the root stays.
    assert_eq!(
        cut("D:\\Projects: could not read D:\\Projects\\x y z"),
        "D:\\Projects: could not read <path omitted>"
    );
}

#[test]
fn diagnostics_never_leak_the_half_of_a_path_after_a_space() {
    let mut h = report(vec![live("C:\\")]);
    h.events = vec![EventInfo {
        at_ms: 1_000,
        level: LEVEL_ERROR.into(),
        text: "could not save to C:\\Users\\Jane Doe\\AppData\\Ferrow\\index.bin".into(),
    }];
    h.volumes[0].reason = "no access to /home/jane doe/x".into();
    let text = diagnostics(
        &snap(ScmState::Running, HealthView::Report(Box::new(h))),
        APP,
        "windows x86_64",
    );
    for leaked in ["Jane", "Doe", "AppData", "index.bin", "jane doe"] {
        assert!(!text.contains(leaked), "{leaked} leaked:\n{text}");
    }
}

/// Every state the service can be in that the UI can show.
fn every_state() -> Vec<Snapshot> {
    let states = [
        ScmState::NotInstalled,
        ScmState::Stopped,
        ScmState::Starting,
        ScmState::Running,
        ScmState::Stopping,
        ScmState::Other,
        ScmState::Unknown,
        ScmState::Unsupported,
    ];
    let busy = Box::new(report(vec![live("C:\\")]));
    let healths = [
        HealthView::NoAnswer,
        HealthView::Silent,
        HealthView::Refused,
        HealthView::Older,
        HealthView::Stale(busy.clone()),
        HealthView::Report(busy),
    ];
    states
        .into_iter()
        .flat_map(|s| healths.iter().cloned().map(move |h| snap(s, h)))
        .collect()
}

#[test]
fn a_hint_never_points_at_a_button_that_is_off() {
    for s in every_state() {
        let m = summary(Some(&s), APP);
        if let Some(verb) = m.hint_verb {
            let acts = actions(Some(&s), true, false);
            let can = match verb {
                Verb::Start => acts.start,
                Verb::Restart => acts.restart,
                Verb::Rescan => acts.rescan,
                Verb::Repair => acts.repair,
            };
            assert_eq!(
                can,
                Can::Yes,
                "{:?} / {:?}: {}",
                s.scm.state,
                s.health,
                m.hint
            );
            let word = if verb == Verb::Start {
                "Start"
            } else {
                "Restart"
            };
            assert!(m.hint.starts_with(word), "{}", m.hint);
        } else if m.hint_access {
            assert_eq!(s.health, HealthView::Refused);
        }
    }
    // The cases the review named: no service manager to ask, and a busy one.
    let linux = summary(
        Some(&snap(ScmState::Unsupported, HealthView::NoAnswer)),
        APP,
    );
    assert_eq!(linux.hint_verb, Some(Verb::Start));
    let busy = summary(
        Some(&snap(
            ScmState::Running,
            HealthView::Stale(Box::new(report(vec![]))),
        )),
        APP,
    );
    assert_eq!(busy.hint_verb, Some(Verb::Restart));
    // Something is wrong in every state that has a hint, and a healthy one has none.
    assert!(summary(Some(&healthy()), APP).hint.is_empty());
}

#[test]
fn a_queued_rescan_and_a_busy_service_turn_rescan_off_with_a_reason() {
    let pending = with(|h| {
        h.volumes[0].rescan_pending = Some(RescanInfo {
            reason: "journal reset".into(),
            earliest_ms: 9,
        });
    });
    assert!(matches!(
        actions(Some(&pending), true, false).rescan,
        Can::No(why) if why.contains("queued")
    ));
    let stale = snap(
        ScmState::Running,
        HealthView::Stale(Box::new(report(vec![]))),
    );
    assert!(matches!(
        actions(Some(&stale), true, false).rescan,
        Can::No(why) if why.contains("right now")
    ));
    assert_eq!(actions(Some(&stale), true, false).restart, Can::Yes);
    let silent = snap(ScmState::Running, HealthView::Silent);
    assert!(matches!(
        actions(Some(&silent), true, false).rescan,
        Can::No(_)
    ));
}

#[test]
fn a_silent_service_is_busy_only_after_it_answered_in_this_session() {
    let (tx, rx) = channel();
    let feed = Feed::new(tx, || {});
    let running = |health| snap(ScmState::Running, health);
    // Never seen: stays "older, or busy".
    let (older, _) = feed.adopt(running(HealthView::Older));
    assert_eq!(older.health, HealthView::Older);
    // A report is seen...
    let (seen, changed) = feed.adopt(healthy());
    assert!(changed && matches!(seen.health, HealthView::Report(_)));
    // ...then it goes silent while the process runs: busy, with that report.
    for silent in [HealthView::Silent, HealthView::Older] {
        let (stale, changed) = feed.adopt(running(silent));
        let HealthView::Stale(last) = &stale.health else {
            panic!("expected Stale, got {:?}", stale.health)
        };
        assert_eq!(**last, report(vec![live("C:\\"), live("D:\\")]));
        assert!(changed || stale.display_key() == feed.adopt(stale.clone()).0.display_key());
    }
    // Stopped or gone is not busy.
    let (stopped, _) = feed.adopt(snap(ScmState::Stopped, HealthView::NoAnswer));
    assert_eq!(stopped.health, HealthView::NoAnswer);
    let (gone, _) = feed.adopt(running(HealthView::NoAnswer));
    assert_eq!(
        gone.health,
        HealthView::NoAnswer,
        "nothing listens: gone, not busy"
    );
    drop(rx);
}

#[test]
fn whether_the_section_is_on_screen_depends_on_all_three() {
    for settings in [false, true] {
        for minimized in [false, true] {
            for hidden in [false, true] {
                assert_eq!(
                    shown(settings, minimized, hidden),
                    settings && !minimized && !hidden,
                    "{settings} {minimized} {hidden}"
                );
            }
        }
    }
    assert!(shown(true, false, false));
    assert!(!shown(true, false, true), "hidden in the tray");
}

#[test]
fn hiding_the_window_stops_the_asks_without_any_frame() {
    // The window is hidden to the tray: no frame follows, so `set_visible`
    // is never called again. The watcher must notice by itself.
    let (mut state, asks, wakes) = counted(Duration::from_millis(40));
    let hidden = Arc::new(AtomicBool::new(false));
    let h = hidden.clone();
    let (a, w) = (asks.clone(), wakes.clone());
    state.set_visible_with(
        true,
        move || {
            w.fetch_add(1, Ordering::SeqCst);
        },
        move || {
            a.fetch_add(1, Ordering::SeqCst);
            healthy()
        },
        move || h.load(Ordering::SeqCst),
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    while asks.load(Ordering::SeqCst) < 2 {
        assert!(Instant::now() < deadline, "it never asked");
        std::thread::sleep(Duration::from_millis(5));
    }
    hidden.store(true, Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(120)); // an ask in flight may finish
    let at_hide = asks.load(Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(asks.load(Ordering::SeqCst), at_hide, "asked while hidden");
    assert!(!state.watching(), "the thread ends when hidden");
    // Shown again: the next frame starts it again.
    hidden.store(false, Ordering::SeqCst);
    let (a, w) = (asks.clone(), wakes.clone());
    let h = hidden.clone();
    state.set_visible_with(
        true,
        move || {
            w.fetch_add(1, Ordering::SeqCst);
        },
        move || {
            a.fetch_add(1, Ordering::SeqCst);
            healthy()
        },
        move || h.load(Ordering::SeqCst),
    );
    assert!(state.watching());
}

#[test]
fn nothing_asks_after_set_visible_false_even_when_no_frame_follows() {
    let (mut state, asks, wakes) = counted(Duration::from_millis(40));
    open(&mut state, &asks, &wakes, |_| healthy());
    std::thread::sleep(Duration::from_millis(150));
    state.set_visible_with(false, || {}, healthy, || false);
    std::thread::sleep(Duration::from_millis(100)); // an ask in flight may finish
    let at_close = asks.load(Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(asks.load(Ordering::SeqCst), at_close);
    assert!(!state.watching());
}

#[test]
fn an_actions_follow_up_asks_push_the_periodic_ask_back() {
    let interval = Duration::from_millis(300);
    let (tx, _rx) = channel();
    let feed = Arc::new(Feed::new(tx, || {}));
    let asks = Arc::new(AtomicUsize::new(0));
    let a = asks.clone();
    let _watcher = spawn_watcher(
        feed.clone(),
        interval,
        move || {
            a.fetch_add(1, Ordering::SeqCst);
            healthy()
        },
        || true,
    );
    // The first ask is at once.
    let deadline = Instant::now() + Duration::from_secs(2);
    while asks.load(Ordering::SeqCst) < 1 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    // An action keeps asking (its asks mark the throttle): the periodic one
    // does not fire while they come.
    for _ in 0..6 {
        std::thread::sleep(Duration::from_millis(100));
        feed.mark();
    }
    assert_eq!(
        asks.load(Ordering::SeqCst),
        1,
        "asked inside the gap of an action ask"
    );
    // When they stop, it resumes within one interval.
    std::thread::sleep(Duration::from_millis(500));
    assert!(asks.load(Ordering::SeqCst) >= 2);
}

#[test]
fn every_look_of_an_action_goes_through_the_throttle() {
    let mut state = ServiceHealth::new();
    let (tx, _rx) = channel();
    let feed = Arc::new(Feed::new(tx, || {}));
    state.feed = Some(feed.clone());
    let started = feed.wait(REFRESH);
    assert_eq!(started, Duration::ZERO, "nothing asked yet");
    state.snap = Some(snap(ScmState::Stopped, HealthView::NoAnswer));
    state.run_with(
        Verb::Start,
        || {},
        || IpcRescan::NotAllowed,
        |_| Ok(0),
        healthy,
        || true,
        |_| {},
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !state.poll() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    // The action's look marked the throttle: the periodic asker now waits.
    assert!(feed.wait(REFRESH) > Duration::from_secs(4));
}

#[test]
fn a_worker_that_vanishes_does_not_leave_the_buttons_waiting() {
    let mut state = ServiceHealth::new();
    let (tx, rx) = channel::<Done>();
    drop(tx);
    state.done = Some(rx);
    state.busy = Some((Verb::Restart, Arc::new(AtomicBool::new(false))));
    assert!(state.poll());
    assert!(state.busy.is_none() && state.done.is_none());
    let notice = state.notice.clone().expect("an outcome");
    assert_eq!(notice.outcome, Outcome::Problem);
    // And it can act again.
    state.run_with(
        Verb::Start,
        || {},
        || IpcRescan::NotAllowed,
        |_| Ok(0),
        healthy,
        || true,
        |_| {},
    );
    assert!(state.busy.is_some());
}

#[test]
fn what_is_shown_includes_what_the_details_show() {
    let base = healthy();
    for changed in [
        with(|h| h.connection_errors += 1),
        with(|h| h.more_volumes = 3),
    ] {
        assert_ne!(base.display_key(), changed.display_key());
    }
    // Shown with the precision it has: one more item changes nothing,
    // another hundred thousand does.
    assert_eq!(base.display_key(), with(|h| h.entries += 3).display_key());
    assert_ne!(
        base.display_key(),
        with(|h| h.entries += 600_000).display_key()
    );
    // A stale report does not look like a current one.
    let stale = snap(
        ScmState::Running,
        HealthView::Stale(Box::new(report(vec![live("C:\\"), live("D:\\")]))),
    );
    assert_ne!(base.display_key(), stale.display_key());
}

#[test]
fn while_the_service_starts_or_stops_nothing_can_be_done_whatever_the_report_says() {
    for state in [ScmState::Starting, ScmState::Stopping] {
        // Also with a (soon stale) report and with none.
        for health in [
            HealthView::NoAnswer,
            HealthView::Refused,
            HealthView::Report(Box::new(report(vec![live("C:\\")]))),
        ] {
            let acts = actions(Some(&snap(state, health.clone())), true, false);
            let what = format!("{state:?} / {health:?}");
            assert_eq!(acts.start, Can::Hidden, "{what}");
            for can in [acts.restart, acts.rescan, acts.repair] {
                assert_eq!(can, Can::No(CHANGING), "{what}");
            }
        }
    }
    assert_eq!(CHANGING, "It is changing state. Wait a moment.");
}

/// Run `verb` on a state that shows "stopped", with `allowed` the window test
/// and `elevate` the command; return the finished state and the asks made.
fn run_hidden_case(
    allowed: Arc<AtomicBool>,
    elevate: impl FnOnce(&str) -> Result<u32, RunError> + Send + 'static,
    ask_hides: bool,
) -> (ServiceHealth, usize) {
    let mut state = ServiceHealth::new();
    state.snap = Some(snap(ScmState::Stopped, HealthView::NoAnswer));
    let asks = Arc::new(AtomicUsize::new(0));
    let (a, window) = (asks.clone(), allowed.clone());
    state.run_with(
        Verb::Start,
        || {},
        || IpcRescan::NotAllowed,
        elevate,
        move || {
            a.fetch_add(1, Ordering::SeqCst);
            if ask_hides {
                window.store(false, Ordering::SeqCst); // hidden during the wait
            }
            snap(ScmState::Stopped, HealthView::NoAnswer) // never "started"
        },
        move || allowed.load(Ordering::SeqCst),
        |_| {},
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !state.poll() {
        assert!(Instant::now() < deadline, "the result never arrived");
        std::thread::sleep(Duration::from_millis(5));
    }
    let asked = asks.load(Ordering::SeqCst);
    (state, asked)
}

#[test]
fn hiding_the_window_during_the_wait_stops_the_follow_up_asks() {
    // Without the check this would ask about eight times (30 s of fake pauses).
    let window = Arc::new(AtomicBool::new(true));
    let (state, asked) = run_hidden_case(window, |_| Ok(0), true);
    assert_eq!(asked, 1, "one look, then the window was gone");
    // The outcome still comes from the exit code, and says what was not checked.
    let notice = state.notice.expect("an outcome");
    assert_eq!(notice.outcome, Outcome::Unconfirmed);
    assert!(notice.text.contains("hidden"));
    assert!(state.busy.is_none());
}

#[test]
fn hiding_while_the_prompt_is_open_asks_nothing_at_all() {
    let window = Arc::new(AtomicBool::new(true));
    let w = window.clone();
    let (state, asked) = run_hidden_case(
        window,
        move |_| {
            w.store(false, Ordering::SeqCst); // the window went to the tray
            Ok(0)
        },
        false,
    );
    assert_eq!(asked, 0);
    assert_eq!(state.notice.as_ref().unwrap().outcome, Outcome::Unconfirmed);
    // The state shown stays what it was: nothing newer is known.
    assert_eq!(
        state.snap,
        Some(snap(ScmState::Stopped, HealthView::NoAnswer))
    );
    // A declined prompt is still declined, and a failure still a failure.
    for (ran, outcome) in [
        (Err(RunError::Declined), Outcome::Declined),
        (Ok(1), Outcome::Problem),
        (Err(RunError::Failed("x".into())), Outcome::Problem),
    ] {
        let window = Arc::new(AtomicBool::new(true));
        let w = window.clone();
        let (state, asked) = run_hidden_case(
            window,
            move |_| {
                w.store(false, Ordering::SeqCst);
                ran
            },
            false,
        );
        assert_eq!(asked, 0);
        assert_eq!(state.notice.unwrap().outcome, outcome);
    }
}

#[test]
fn a_wait_that_may_not_ask_reports_nothing_checked() {
    let stopped = snap(ScmState::Stopped, HealthView::NoAnswer);
    let (shown, seen) = settle(
        Verb::Start,
        &stopped,
        || panic!("asked while not allowed"),
        || false,
        |_| {},
    );
    assert_eq!((shown, seen), (None, None));
    assert_eq!(
        outcome(Verb::Start, Ok(0), None).outcome,
        Outcome::Unconfirmed
    );
    // The exit codes keep their own words whether or not it was checked.
    assert_eq!(
        outcome(Verb::Start, Ok(4), None).text,
        outcome(Verb::Start, Ok(4), Some(false)).text
    );
}

#[test]
fn a_silent_service_nobody_heard_before_reads_like_an_older_one() {
    let older = line(&snap(ScmState::Running, HealthView::Older));
    let silent = line(&snap(ScmState::Running, HealthView::Silent));
    assert_eq!(silent, older);
    assert_eq!(
        silent.2,
        "Older service, or busy · update with Setup if this stays"
    );
    // Still no button is promised: nothing to press for "older, or busy".
    assert_eq!(
        summary(Some(&snap(ScmState::Running, HealthView::Silent)), APP).hint_verb,
        None
    );
}

#[test]
fn a_rescan_the_service_answered_does_not_look_again_when_the_window_is_hidden() {
    let mut state = ServiceHealth::new();
    state.snap = Some(healthy());
    let window = Arc::new(AtomicBool::new(true));
    let w = window.clone();
    let asks = Arc::new(AtomicUsize::new(0));
    let a = asks.clone();
    let gate = window.clone();
    state.run_with(
        Verb::Rescan,
        || {},
        move || {
            w.store(false, Ordering::SeqCst); // hidden while the request ran
            IpcRescan::Started("rescan requested".into())
        },
        |_| Ok(0),
        move || {
            a.fetch_add(1, Ordering::SeqCst);
            healthy()
        },
        move || gate.load(Ordering::SeqCst),
        |_| {},
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !state.poll() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(asks.load(Ordering::SeqCst), 0);
    // The service's own sentence is still shown.
    let notice = state.notice.unwrap();
    assert_eq!(
        (notice.outcome, notice.text.as_str()),
        (Outcome::Done, "rescan requested")
    );
}
