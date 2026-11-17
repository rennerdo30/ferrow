//! Settings → Service access: who may use the Ferrow system service.
//!
//! Everything slow runs on its own thread, so the window never waits: the
//! status query uses a short deadline (an older service does not answer the
//! request at all), and changes go through the elevated `ferrow` command line
//! (`ShellExecuteExW` verb `runas`, Windows). After the command exits the
//! thread asks the service again, a few times, until the answer shows the
//! change, and that answer (not the exit code) decides whether it worked. The
//! app polls the result every frame, so it applies even when Settings is not
//! open, and tells the engine to look for the service at once, so a newly
//! allowed account switches over without restarting the app.

use std::sync::OnceLock;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use ferrow_ipc::{AccessInfo, AccessMode, Client, Endpoint};

/// How long to wait for the service's answer. An older service never answers.
const QUERY_DEADLINE: Duration = Duration::from_millis(800);
/// Pauses before the second and third look at the service after a change:
/// it applies a change within about a second, so the answers come at 0, 1.2
/// and 2.4 seconds.
const CONFIRM_PAUSES: [Duration; 2] = [Duration::from_millis(1200), Duration::from_millis(1200)];

#[derive(Clone, Debug, PartialEq)]
pub enum View {
    Checking,
    NoService,
    /// The service is running but did not answer the access request: it
    /// predates it, or it is busy.
    TooOld,
    Info(AccessInfo),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Allow this account (by the SID/uid the service saw, never "whoever
    /// runs the elevated command": that would be the administrator).
    AllowMe(String),
    Everyone,
    Admins,
    Remove(String),
}

/// A canonical Windows SID (`S-1-<authority>(-<subauthority>)+`) or decimal
/// uid: the only account ids that reach a command line.
pub fn canonical_id(id: &str) -> bool {
    let number = |s: &str, max: usize| {
        !s.is_empty() && s.len() <= max && s.bytes().all(|b| b.is_ascii_digit())
    };
    if let Some(rest) = id.strip_prefix("S-1-") {
        let mut parts = rest.split('-');
        let authority = parts.next().is_some_and(|a| number(a, 15));
        let subs: Vec<&str> = parts.collect();
        return authority && (1..=15).contains(&subs.len()) && subs.iter().all(|s| number(s, 10));
    }
    id.parse::<u32>().is_ok_and(|n| n.to_string() == id)
}

impl Action {
    /// The `ferrow` arguments, or `None` when an id is not canonical.
    pub fn args(&self) -> Option<String> {
        let id = |id: &str| canonical_id(id).then(|| id.to_owned());
        Some(match self {
            Action::AllowMe(i) => format!("service access add {}", id(i)?),
            Action::Remove(i) => format!("service access remove {}", id(i)?),
            Action::Everyone => "service access everyone".into(),
            Action::Admins => "service access admins".into(),
        })
    }

    /// The same change as a terminal command (Linux, macOS, portable copies).
    pub fn terminal_command(&self) -> Option<String> {
        let args = self.args()?;
        Some(if cfg!(windows) {
            format!("ferrow {args}")
        } else {
            format!("sudo ferrow {args}")
        })
    }

    /// Whether the service's answer shows this change.
    pub fn reflected(&self, info: &AccessInfo) -> bool {
        match self {
            Action::AllowMe(_) => info.you_allowed,
            Action::Everyone => info.mode == AccessMode::Everyone,
            Action::Admins => info.mode == AccessMode::Admins,
            Action::Remove(id) => match &info.users {
                Some(users) => !users.iter().any(|u| u.id.eq_ignore_ascii_case(id)),
                // Not an administrator: the list is hidden, so only this
                // account's own removal can be seen.
                None => !info.you.id.eq_ignore_ascii_case(id) || !info.you_allowed,
            },
        }
    }
}

struct Done {
    view: View,
    message: Option<String>,
    /// A change was attempted (the engine should look at the service again).
    acted: bool,
}

pub struct Access {
    pub view: View,
    pub busy: bool,
    /// The last action's outcome.
    pub message: Option<String>,
    /// "Allow everyone" was clicked once and waits for its confirmation.
    pub confirm_everyone: bool,
    asked: bool,
    rx: Option<Receiver<Done>>,
}

/// Ask the service on a connection of its own.
fn query() -> View {
    query_at(&Endpoint::default_service())
}

fn query_at(endpoint: &Endpoint) -> View {
    let deadline = Instant::now() + QUERY_DEADLINE;
    let Ok(mut client) = Client::connect_deadline(endpoint, deadline) else {
        return View::NoService;
    };
    match client.access_info() {
        Ok(info) => View::Info(info),
        Err(_) => View::TooOld,
    }
}

fn reflects(action: &Action, view: &View) -> bool {
    matches!(view, View::Info(info) if action.reflected(info))
}

/// Ask until the answer shows the change or the pauses run out. Returns the
/// last answer and whether it shows the change.
fn confirm(
    action: &Action,
    mut ask: impl FnMut() -> View,
    mut pause: impl FnMut(Duration),
) -> (View, bool) {
    let mut view = ask();
    for delay in CONFIRM_PAUSES {
        if reflects(action, &view) {
            break;
        }
        pause(delay);
        view = ask();
    }
    let shown = reflects(action, &view);
    (view, shown)
}

/// What to tell the user; the service's answer decides, the exit code only
/// explains a change that did not show up.
fn outcome(shown: bool, view: &View, ran: Result<(), String>) -> String {
    match (shown, ran) {
        (true, _) => "Done. The service applies it within a second.".into(),
        (false, Err(why)) => why,
        (false, Ok(())) if matches!(view, View::Info(_)) => {
            "The change ran, but the service does not show it yet. Check again in a moment.".into()
        }
        (false, Ok(())) => {
            "The change ran, but the service could not be asked to confirm it.".into()
        }
    }
}

impl Access {
    pub fn new() -> Access {
        Access {
            view: View::Checking,
            busy: false,
            message: None,
            confirm_everyone: false,
            asked: false,
            rx: None,
        }
    }

    /// True once per visit to the page: the card asks when first shown.
    pub fn needs_refresh(&self) -> bool {
        !self.asked && self.rx.is_none()
    }

    /// Forget the answer so the next visit asks again (never while a request
    /// is running).
    pub fn invalidate(&mut self) {
        if self.rx.is_none() {
            self.asked = false;
        }
    }

    /// Ask again now (the Retry button).
    pub fn retry(&mut self) {
        if self.rx.is_none() {
            self.asked = false;
            self.view = View::Checking;
        }
    }

    fn spawn(
        &mut self,
        wake: impl Fn() + Send + 'static,
        work: impl FnOnce() -> Done + Send + 'static,
    ) {
        let (tx, rx) = channel();
        let started = std::thread::Builder::new()
            .name("ferrow-access".into())
            .spawn(move || {
                let _ = tx.send(work());
                wake();
            });
        if started.is_ok() {
            self.rx = Some(rx);
        } else {
            self.view = View::NoService;
        }
    }

    pub fn refresh(&mut self, wake: impl Fn() + Send + 'static) {
        self.refresh_with(wake, query);
    }

    fn refresh_with(
        &mut self,
        wake: impl Fn() + Send + 'static,
        ask: impl FnOnce() -> View + Send + 'static,
    ) {
        self.asked = true;
        self.spawn(wake, move || Done {
            view: ask(),
            message: None,
            acted: false,
        });
    }

    /// Run a change elevated (waits for the approval prompt off the UI thread).
    pub fn run(&mut self, action: Action, wake: impl Fn() + Send + 'static) {
        self.run_with(action, wake, run_elevated, query, std::thread::sleep);
    }

    fn run_with(
        &mut self,
        action: Action,
        wake: impl Fn() + Send + 'static,
        elevate: impl FnOnce(&str) -> Result<(), String> + Send + 'static,
        mut ask: impl FnMut() -> View + Send + 'static,
        pause: impl FnMut(Duration) + Send + 'static,
    ) {
        let Some(args) = action.args() else {
            self.message = Some("That account id is not valid.".into());
            return;
        };
        if self.busy || self.rx.is_some() {
            return;
        }
        self.busy = true;
        self.confirm_everyone = false;
        self.message = None;
        self.asked = true;
        self.spawn(wake, move || {
            let ran = elevate(&args);
            let (view, shown) = confirm(&action, &mut ask, pause);
            Done {
                message: Some(outcome(shown, &view, ran)),
                view,
                acted: true,
            }
        });
        if self.rx.is_none() {
            self.busy = false;
        }
    }

    /// Take a finished result (called every frame, next to the updater's).
    /// True when a change was attempted: the engine should look for the
    /// service again.
    pub fn poll(&mut self) -> bool {
        let Some(rx) = &self.rx else { return false };
        let Ok(done) = rx.try_recv() else {
            return false;
        };
        self.rx = None;
        self.busy = false;
        self.view = done.view;
        if done.message.is_some() {
            self.message = done.message;
        }
        done.acted
    }

    /// Whether the actions can run from this app (installed Windows copy).
    pub fn can_elevate() -> bool {
        static CAN: OnceLock<bool> = OnceLock::new();
        *CAN.get_or_init(|| {
            cfg!(windows) && crate::update::installed_sibling("ferrow.exe").is_some()
        })
    }

    /// This process is sandboxed (restricted, AppContainer, low integrity):
    /// the service refuses it whatever the policy says, so "Allow me" would
    /// not help.
    pub fn sandboxed() -> bool {
        static SANDBOXED: OnceLock<bool> = OnceLock::new();
        *SANDBOXED.get_or_init(|| {
            ferrow_ipc::CallerIdentity::current().is_ok_and(|me| !me.is_unsandboxed())
        })
    }
}

/// The card's first line and detail, in one second: is this account served,
/// and under which rule.
pub fn summary(view: &View) -> (String, String) {
    let text = |a: &str, b: &str| (a.to_owned(), b.to_owned());
    match view {
        View::Checking => text("Checking service access…", ""),
        View::NoService => text(
            "The Ferrow service isn't running",
            "The app uses its own, slower index. Install or start the service with Setup.",
        ),
        View::TooOld => text(
            "The service did not answer the access request",
            "It is older than this app, or busy. Retry, or update Ferrow with Setup.",
        ),
        View::Info(info) => {
            let rule = match (info.mode, info.users.as_ref()) {
                (AccessMode::Admins, _) => "Only administrators can use it.".to_owned(),
                (AccessMode::Everyone, _) => {
                    "Every person who signs in to this PC can use it and sees every file name on it."
                        .to_owned()
                }
                (AccessMode::Listed, Some(users)) => {
                    format!(
                        "Administrators and {} listed account(s) can use it.",
                        users.len()
                    )
                }
                (AccessMode::Listed, None) => {
                    "Administrators and the listed accounts can use it.".to_owned()
                }
            };
            if info.you_allowed {
                ("This account uses the Ferrow service".to_owned(), rule)
            } else {
                (
                    "This account can't use the Ferrow service".to_owned(),
                    format!("Searches use this app's own, slower index. {rule}"),
                )
            }
        }
    }
}

#[cfg(windows)]
fn run_elevated(args: &str) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_CANCELLED, GetLastError, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::System::Com::{
        COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize,
    };
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
    use windows_sys::Win32::UI::Shell::{
        SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
    };
    let Some(exe) = crate::update::installed_sibling("ferrow.exe") else {
        return Err("ferrow.exe is not installed next to this app. Run the command in an elevated terminal.".into());
    };
    let wide = |s: &std::ffi::OsStr| -> Vec<u16> { s.encode_wide().chain(Some(0)).collect() };
    let verb = wide("runas".as_ref());
    let file = wide(exe.as_os_str());
    let params = wide(args.as_ref());
    let folder = wide(exe.parent().unwrap_or(&exe).as_os_str()); // the install folder
    // ShellExecuteEx wants COM initialized on the calling thread.
    // SAFETY: balanced with CoUninitialize below when it succeeded.
    let com = unsafe {
        CoInitializeEx(
            std::ptr::null(),
            (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
        )
    } >= 0;
    // SAFETY: zeroed POD struct with its size set; every string outlives the call.
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = params.as_ptr();
    info.lpDirectory = folder.as_ptr();
    info.nShow = 0; // SW_HIDE: the command line has no window to show
    // SAFETY: initialized struct; the shell fills hProcess on success.
    let started = unsafe { ShellExecuteExW(&raw mut info) } != 0;
    // SAFETY: reads this thread's last error, right after the call.
    let code = if started {
        0
    } else {
        unsafe { GetLastError() }
    };
    let result = if !started {
        Err(if code == ERROR_CANCELLED {
            "Administrator approval was declined; nothing was changed.".to_owned()
        } else {
            format!("The change could not be started (Windows error {code}).")
        })
    } else if info.hProcess.is_null() {
        Err("The change could not be started.".to_owned())
    } else {
        let mut exit = 1u32;
        // SAFETY: the process handle is ours (NOCLOSEPROCESS) and closed once.
        // The command is short; a bounded wait keeps this thread from hanging on it.
        let timed_out = unsafe {
            let waited = WaitForSingleObject(info.hProcess, 30_000);
            GetExitCodeProcess(info.hProcess, &raw mut exit);
            CloseHandle(info.hProcess);
            waited == WAIT_TIMEOUT
        };
        if timed_out {
            Err("The change is taking longer than expected. Check again in a moment.".to_owned())
        } else if exit == 0 {
            Ok(())
        } else {
            Err("The change was not applied. Run `ferrow service access show` in a terminal for details.".to_owned())
        }
    };
    if com {
        // SAFETY: matches the successful CoInitializeEx above.
        unsafe { CoUninitialize() };
    }
    result
}

#[cfg(not(windows))]
fn run_elevated(_args: &str) -> Result<(), String> {
    Err("Run the command in a terminal with sudo.".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrow_ipc::AccessUser;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    const SID: &str = "S-1-5-21-1-2-3-1001";

    fn user(id: &str) -> AccessUser {
        AccessUser {
            id: id.into(),
            name: String::new(),
        }
    }

    fn info_of(mode: AccessMode, allowed: bool, users: Option<Vec<AccessUser>>) -> AccessInfo {
        AccessInfo {
            mode,
            you_allowed: allowed,
            you: user(SID),
            users,
        }
    }

    fn info(mode: AccessMode, allowed: bool, users: Option<usize>) -> View {
        View::Info(info_of(
            mode,
            allowed,
            users.map(|n| vec![AccessUser::default(); n]),
        ))
    }

    #[test]
    fn allow_me_passes_the_standard_users_sid_never_the_current_user() {
        assert_eq!(
            Action::AllowMe(SID.into()).args().as_deref(),
            Some("service access add S-1-5-21-1-2-3-1001")
        );
        assert_eq!(
            Action::AllowMe("1000".into())
                .terminal_command()
                .map(|c| c.ends_with("add 1000")),
            Some(true)
        );
    }

    #[test]
    fn only_canonical_ids_reach_the_elevated_command_line() {
        for bad in [
            "",
            "S-1-",
            "S-1-5",
            "S-1--5",
            "S-1-5-",
            "alice",
            "1000 & calc",
            "S-1-5-21-1\" --x",
            "-1",
            "01000",
            "S-2-5-18",
        ] {
            assert_eq!(Action::AllowMe(bad.into()).args(), None, "{bad:?}");
            assert_eq!(Action::Remove(bad.into()).args(), None, "{bad:?}");
        }
        assert!(canonical_id("S-1-5-18") && canonical_id("1000"));
        assert_eq!(
            Action::Everyone.args().as_deref(),
            Some("service access everyone")
        );
        assert_eq!(
            Action::Admins.args().as_deref(),
            Some("service access admins")
        );
    }

    #[test]
    fn the_card_says_in_one_line_whether_this_account_is_served() {
        let served = summary(&info(AccessMode::Admins, true, Some(0)));
        assert_eq!(served.0, "This account uses the Ferrow service");
        let refused = summary(&info(AccessMode::Admins, false, None));
        assert_eq!(refused.0, "This account can't use the Ferrow service");
        assert!(refused.1.contains("own, slower index"));
        let everyone = summary(&info(AccessMode::Everyone, true, None));
        assert!(everyone.1.contains("every file name") && everyone.1.contains("signs in"));
        assert!(
            summary(&info(AccessMode::Listed, true, Some(2)))
                .1
                .contains("2 listed")
        );
    }

    #[test]
    fn an_old_busy_or_missing_service_has_its_own_plain_text() {
        let old = summary(&View::TooOld);
        assert!(old.1.contains("older") && old.1.contains("busy") && old.1.contains("Retry"));
        assert!(summary(&View::NoService).0.contains("isn't running"));
    }

    #[test]
    fn no_service_is_reported_at_once() {
        let started = Instant::now();
        let endpoint = Endpoint::named(&format!("ferrow-access-none-{}", std::process::id()));
        assert_eq!(query_at(&endpoint), View::NoService);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn a_service_that_never_answers_the_request_is_too_old_within_the_deadline() {
        // Stand-in for a service from before access settings: it completes the
        // handshake, then keeps the connection open without answering (it does
        // not know the request, and its worker keeps the handle).
        let endpoint = Endpoint::named(&format!("ferrow-access-mute-{}", std::process::id()));
        let listener = ferrow_ipc::Listener::bind(&endpoint).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            ferrow_ipc::server_handshake(&mut stream).unwrap();
            std::thread::sleep(Duration::from_millis(1500));
        });
        let started = Instant::now();
        assert_eq!(query_at(&endpoint), View::TooOld);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        server.join().unwrap();
    }

    #[test]
    fn each_action_knows_what_the_answer_must_show() {
        let listed = |ids: &[&str]| {
            info_of(
                AccessMode::Listed,
                false,
                Some(ids.iter().map(|i| user(i)).collect()),
            )
        };
        assert!(Action::AllowMe(SID.into()).reflected(&info_of(AccessMode::Listed, true, None)));
        assert!(!Action::AllowMe(SID.into()).reflected(&info_of(AccessMode::Admins, false, None)));
        assert!(Action::Everyone.reflected(&info_of(AccessMode::Everyone, true, None)));
        assert!(!Action::Everyone.reflected(&info_of(AccessMode::Listed, true, None)));
        assert!(Action::Admins.reflected(&info_of(AccessMode::Admins, true, None)));
        assert!(!Action::Admins.reflected(&info_of(AccessMode::Everyone, true, None)));
        assert!(!Action::Remove("1000".into()).reflected(&listed(&["1000", "1001"])));
        assert!(Action::Remove("1000".into()).reflected(&listed(&["1001"])));
        // A standard user cannot see the list: only its own removal shows.
        assert!(Action::Remove("1000".into()).reflected(&info_of(AccessMode::Listed, true, None)));
        assert!(Action::Remove(SID.into()).reflected(&info_of(AccessMode::Listed, false, None)));
        assert!(!Action::Remove(SID.into()).reflected(&info_of(AccessMode::Listed, true, None)));
    }

    /// Answers from a script, and the pauses asked for.
    fn scripted(
        answers: Vec<View>,
    ) -> (
        impl FnMut() -> View + Send + 'static,
        Arc<Mutex<Vec<Duration>>>,
        Arc<AtomicUsize>,
    ) {
        let asked = Arc::new(AtomicUsize::new(0));
        let counter = asked.clone();
        let pauses = Arc::new(Mutex::new(Vec::new()));
        let ask = move || {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            answers[n.min(answers.len() - 1)].clone()
        };
        (ask, pauses, asked)
    }

    #[test]
    fn the_answer_is_asked_again_until_it_shows_the_change_then_stops() {
        let stale = info(AccessMode::Admins, false, Some(0));
        let fresh = info(AccessMode::Everyone, true, Some(0));
        let (ask, pauses, asked) = scripted(vec![stale.clone(), fresh.clone()]);
        let log = pauses.clone();
        let (view, shown) = confirm(&Action::Everyone, ask, move |d| log.lock().unwrap().push(d));
        assert!(shown && view == fresh);
        assert_eq!(
            asked.load(Ordering::SeqCst),
            2,
            "stopped at the first fresh answer"
        );
        assert_eq!(*pauses.lock().unwrap(), [Duration::from_millis(1200)]);
    }

    #[test]
    fn a_change_that_never_shows_is_asked_at_zero_1_2_and_2_4_seconds_only() {
        let (ask, pauses, asked) = scripted(vec![info(AccessMode::Admins, false, Some(0))]);
        let log = pauses.clone();
        let (view, shown) = confirm(&Action::Everyone, ask, move |d| log.lock().unwrap().push(d));
        assert!(!shown && matches!(view, View::Info(_)));
        assert_eq!(asked.load(Ordering::SeqCst), 3);
        let total: Duration = pauses.lock().unwrap().iter().sum();
        assert_eq!(total, Duration::from_millis(2400));
    }

    #[test]
    fn success_is_decided_by_the_answer_and_the_exit_code_only_explains() {
        let shown = info(AccessMode::Everyone, true, Some(0));
        let stale = info(AccessMode::Admins, false, Some(0));
        // The command reported failure, but the service shows the change.
        assert!(outcome(true, &shown, Err("exit 1".into())).starts_with("Done"));
        // The command reported success, but the service does not show it.
        assert!(outcome(false, &stale, Ok(())).contains("does not show it yet"));
        // The service could not be asked at all.
        assert!(outcome(false, &View::TooOld, Ok(())).contains("could not be asked"));
        // The prompt was declined: its own message is kept.
        let declined = "Administrator approval was declined; nothing was changed.";
        assert_eq!(outcome(false, &stale, Err(declined.into())), declined);
    }

    fn wait_done(access: &mut Access) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if access.rx.is_none() {
                return false;
            }
            if access.poll() {
                return true;
            }
            if access.rx.is_none() {
                return false;
            }
            assert!(Instant::now() < deadline, "worker did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn refresh_asks_once_per_visit_and_invalidate_never_cancels_a_running_ask() {
        let mut access = Access::new();
        assert!(access.needs_refresh());
        access.refresh_with(|| {}, || info(AccessMode::Admins, true, Some(0)));
        assert!(
            !access.needs_refresh(),
            "asked: no second query while one runs"
        );
        access.invalidate(); // a visit ending while the answer is pending
        assert!(!access.needs_refresh());
        assert!(!wait_done(&mut access), "a refresh is not a change");
        assert_eq!(access.view, info(AccessMode::Admins, true, Some(0)));
        assert!(!access.needs_refresh(), "answered, so not asked again");
        access.invalidate();
        assert!(access.needs_refresh(), "the next visit asks again");
        access.retry();
        assert_eq!(access.view, View::Checking);
        assert!(access.needs_refresh());
    }

    #[test]
    fn a_change_is_busy_until_polled_and_a_second_one_is_ignored() {
        let mut access = Access::new();
        let ran = Arc::new(AtomicUsize::new(0));
        let counter = ran.clone();
        let (ask, _, _) = scripted(vec![info(AccessMode::Everyone, true, Some(0))]);
        access.run_with(
            Action::Everyone,
            || {},
            move |args| {
                assert_eq!(args, "service access everyone");
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            ask,
            |_| {},
        );
        assert!(access.busy);
        // A second change while busy does nothing.
        let (ask2, _, _) = scripted(vec![info(AccessMode::Admins, true, Some(0))]);
        access.run_with(
            Action::Admins,
            || {},
            |_| panic!("must not run while busy"),
            ask2,
            |_| {},
        );
        assert!(wait_done(&mut access), "the engine is told to look again");
        assert!(!access.busy);
        assert_eq!(ran.load(Ordering::SeqCst), 1);
        assert_eq!(access.view, info(AccessMode::Everyone, true, Some(0)));
        assert!(
            access
                .message
                .as_deref()
                .is_some_and(|m| m.starts_with("Done"))
        );
        // Polling again reports nothing new.
        assert!(!access.poll());
    }

    #[test]
    fn a_declined_prompt_keeps_its_message_and_changes_nothing() {
        let mut access = Access::new();
        let stale = info(AccessMode::Admins, false, Some(0));
        let (ask, _, _) = scripted(vec![stale.clone()]);
        access.run_with(
            Action::AllowMe(SID.into()),
            || {},
            |_| Err("Administrator approval was declined; nothing was changed.".into()),
            ask,
            |_| {},
        );
        assert!(wait_done(&mut access));
        assert_eq!(access.view, stale);
        assert!(
            access
                .message
                .as_deref()
                .is_some_and(|m| m.contains("declined"))
        );
    }

    #[test]
    fn an_invalid_id_never_starts_anything() {
        let mut access = Access::new();
        access.run_with(
            Action::AllowMe("1000 & calc".into()),
            || {},
            |_| panic!("must not run"),
            || View::Checking,
            |_| {},
        );
        assert!(!access.busy && access.rx.is_none());
        assert!(access.message.is_some());
    }
}
