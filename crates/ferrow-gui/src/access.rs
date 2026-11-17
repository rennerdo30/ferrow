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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, OnceLock};
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
    /// Add an account the user looked up (by the id it resolved to, never by
    /// the text that was typed).
    Add(String),
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

/// An id that may go on the elevated command line on this OS: a canonical SID
/// on Windows (a plain number is a Unix uid and means nothing there), a
/// canonical SID or uid elsewhere.
pub fn account_id_ok(id: &str) -> bool {
    canonical_id(id) && (!cfg!(windows) || id.starts_with("S-1-"))
}

/// Whether the service's answer shows a change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Seen {
    /// The answer shows it.
    Yes,
    /// The answer does not show it (yet).
    No,
    /// This account cannot see the part of the answer that would show it
    /// (the list of accounts is hidden from standard users).
    Unknown,
}

/// How the last change ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The service's answer shows the change.
    Done,
    /// The command succeeded but the answer cannot show it to this account.
    Unconfirmed,
    /// The administrator prompt was declined.
    Declined,
    Problem,
}

/// The last change's outcome and what to tell the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub outcome: Outcome,
    pub text: String,
}

impl Notice {
    pub fn new(outcome: Outcome, text: &str) -> Notice {
        Notice {
            outcome,
            text: text.to_owned(),
        }
    }
}

/// Why the elevated command did not run to success.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunError {
    /// The user declined the administrator prompt.
    Declined,
    Failed(String),
}

const DECLINED_TEXT: &str = "Administrator approval was declined; nothing was changed.";

impl Action {
    /// The `ferrow` arguments, or `None` when an id may not go on a command
    /// line.
    pub fn args(&self) -> Option<String> {
        let id = |id: &str| account_id_ok(id).then(|| id.to_owned());
        Some(match self {
            Action::AllowMe(i) => format!("service access add {}", id(i)?),
            Action::Remove(i) => format!("service access remove {}", id(i)?),
            Action::Add(i) => format!("service access add {}", id(i)?),
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
    pub fn seen(&self, info: &AccessInfo) -> Seen {
        let yes = |shown: bool| if shown { Seen::Yes } else { Seen::No };
        match self {
            Action::AllowMe(_) => yes(info.you_allowed),
            Action::Everyone => yes(info.mode == AccessMode::Everyone),
            Action::Admins => yes(info.mode == AccessMode::Admins),
            Action::Add(id) | Action::Remove(id) => {
                let added = matches!(self, Action::Add(_));
                match &info.users {
                    Some(users) => {
                        yes(users.iter().any(|u| u.id.eq_ignore_ascii_case(id)) == added)
                    }
                    // Not an administrator: the list is hidden. This
                    // account's own entry shows in `you_allowed`; any other
                    // cannot be seen, so the answer proves nothing either way.
                    None if info.you.id.eq_ignore_ascii_case(id) => yes(info.you_allowed == added),
                    None => Seen::Unknown,
                }
            }
        }
    }

    /// The change in the past tense, for an outcome that cannot be confirmed.
    fn done_text(&self) -> &'static str {
        match self {
            Action::Add(_) | Action::AllowMe(_) => "Added",
            Action::Remove(_) => "Removed",
            Action::Everyone | Action::Admins => "Changed",
        }
    }
}

/// How long a name lookup may take. A domain account can wait for a domain
/// controller; the lookup runs off the UI thread and is abandoned after this.
const LOOKUP_DEADLINE: Duration = Duration::from_secs(5);
/// The longest account text accepted (a domain, a backslash and a name).
const MAX_ACCOUNT_TEXT: usize = 256;
/// Windows' "no mapping between account names and security IDs".
const ERROR_NONE_MAPPED: u32 = 1332;

/// An account found by name: its id (a SID on Windows, a uid or user name on
/// Unix) and the name Windows gives it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum AddState {
    #[default]
    Idle,
    Looking,
    Found(Resolved),
    Failed(String),
}

/// The "Add account" field: what was typed and what came of it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AddAccount {
    pub text: String,
    pub state: AddState,
    /// Put the cursor in the field (and show it) on the next frame.
    pub focus: bool,
}

impl AddAccount {
    /// A freshly opened field: focused.
    pub fn opened() -> AddAccount {
        AddAccount {
            focus: true,
            ..AddAccount::default()
        }
    }
}

/// What the "found" line of the field says. On Windows the account was
/// really looked up; on Unix only the command's name was checked for safe
/// characters, so it must not look verified.
pub fn found_line(found: &Resolved, verified: bool) -> (bool, String) {
    if verified {
        (true, found.name.clone())
    } else {
        (false, format!("Command for {}", found.name))
    }
}

/// The typed text, cleaned: trimmed, not empty, not long, no control codes.
fn typed_account(text: &str) -> Result<&str, String> {
    let text = text.trim();
    if text.is_empty() {
        Err(r"Type a name (DOMAIN\user or user) or a SID.".into())
    } else if text.len() > MAX_ACCOUNT_TEXT || text.chars().any(char::is_control) {
        Err("That is not a valid account name.".into())
    } else {
        Ok(text)
    }
}

/// An account argument that is safe to show in a terminal command (Unix):
/// letters, digits and `. _ - @`, never starting with `-`.
fn safe_account_arg(text: &str) -> Option<&str> {
    let ok = !text.is_empty()
        && text.len() <= 64
        && !text.starts_with('-')
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@'));
    ok.then_some(text)
}

/// Whether `id` may go on a command line: a canonical SID (or, off Windows,
/// a uid or plain user name).
fn usable_id(id: &str) -> bool {
    account_id_ok(id) || (!cfg!(windows) && safe_account_arg(id).is_some())
}

/// What a failed name lookup tells the user.
#[cfg_attr(not(windows), allow(dead_code))]
fn lookup_error(code: u32, text: &str) -> String {
    if code == ERROR_NONE_MAPPED {
        format!("No account named \"{text}\" was found on this PC or its domain.")
    } else {
        format!("Windows could not look up \"{text}\" (error {code}).")
    }
}

/// Look `text` up with `resolver` on a thread of its own and wait at most
/// `deadline`: a stalled domain lookup is abandoned, not waited for. At most
/// one such thread exists (`running`): while an abandoned one is still stuck,
/// a new lookup is refused instead of piling up threads. The answer must be an
/// id that can go on a command line.
fn lookup(
    text: &str,
    resolver: impl FnOnce(&str) -> Result<Resolved, String> + Send + 'static,
    deadline: Duration,
    running: &Arc<AtomicBool>,
) -> Result<Resolved, String> {
    let text = typed_account(text)?.to_owned();
    if running.swap(true, Ordering::SeqCst) {
        return Err("Still waiting for the last lookup. Try again in a moment.".into());
    }
    let (tx, rx) = channel();
    let asked = text.clone();
    let done = running.clone();
    let started = std::thread::Builder::new()
        .name("ferrow-account-lookup".into())
        .spawn(move || {
            let answer = resolver(&asked);
            done.store(false, Ordering::SeqCst);
            let _ = tx.send(answer);
        });
    if started.is_err() {
        running.store(false, Ordering::SeqCst);
        return Err("The lookup could not be started.".into());
    }
    let found = rx.recv_timeout(deadline).map_err(|_| {
        "Looking up the account took too long. Try again, or type the SID.".to_owned()
    })??;
    if usable_id(&found.id) {
        Ok(found)
    } else {
        Err(format!(
            "\"{text}\" has an account id that can't be used here."
        ))
    }
}

/// The terminal command that adds a looked-up account.
pub fn add_command(found: &Resolved) -> Option<String> {
    if !usable_id(&found.id) {
        return None;
    }
    Some(if cfg!(windows) {
        format!("ferrow service access add {}", found.id)
    } else {
        format!("sudo ferrow service access add {}", found.id)
    })
}

struct Done {
    view: View,
    message: Option<Notice>,
    /// A change was attempted (the engine should look at the service again).
    acted: bool,
}

pub struct Access {
    pub view: View,
    pub busy: bool,
    /// The last action's outcome.
    pub message: Option<Notice>,
    /// "Allow everyone" was clicked once and waits for its confirmation.
    pub confirm_everyone: bool,
    /// The "Add account" field, while it is open.
    pub add: Option<AddAccount>,
    asked: bool,
    rx: Option<Receiver<Done>>,
    lookup_rx: Option<Receiver<Result<Resolved, String>>>,
    /// A lookup thread is still running (possibly stuck on a domain).
    lookup_running: Arc<AtomicBool>,
}

/// Ask the service on a connection of its own.
fn query() -> View {
    // Test builds never talk to the machine's real service: unit tests use
    // `query_at` with their own endpoint, and the screenshot harness must
    // not depend on (or touch) whatever service this PC runs.
    #[cfg(test)]
    return View::NoService;
    #[cfg(not(test))]
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

fn seen_in(action: &Action, view: &View) -> Seen {
    match view {
        View::Info(info) => action.seen(info),
        _ => Seen::No,
    }
}

/// Ask until the answer shows the change or the pauses run out (asking again
/// cannot help when the answer is not visible to this account). Returns the
/// last answer and whether it shows the change.
fn confirm(
    action: &Action,
    mut ask: impl FnMut() -> View,
    mut pause: impl FnMut(Duration),
) -> (View, Seen) {
    let mut view = ask();
    for delay in CONFIRM_PAUSES {
        if seen_in(action, &view) != Seen::No {
            break;
        }
        pause(delay);
        view = ask();
    }
    let seen = seen_in(action, &view);
    (view, seen)
}

/// What to tell the user. The service's answer decides when this account can
/// see it; a change it cannot see is judged by how the command ended, and a
/// declined prompt is never reported as done.
fn outcome(action: &Action, seen: Seen, view: &View, ran: Result<(), RunError>) -> Notice {
    match (seen, ran) {
        (Seen::Yes, _) => Notice::new(
            Outcome::Done,
            "Done. The service applies it within a second.",
        ),
        (_, Err(RunError::Declined)) => Notice::new(Outcome::Declined, DECLINED_TEXT),
        (_, Err(RunError::Failed(why))) => Notice::new(Outcome::Problem, &why),
        (Seen::Unknown, Ok(())) => Notice::new(
            Outcome::Unconfirmed,
            &format!(
                "{} — the list isn't visible to this account, so Ferrow can't confirm it.",
                action.done_text()
            ),
        ),
        (Seen::No, Ok(())) if matches!(view, View::Info(_)) => Notice::new(
            Outcome::Problem,
            "The change ran, but the service does not show it yet. Check again in a moment.",
        ),
        (Seen::No, Ok(())) => Notice::new(
            Outcome::Problem,
            "The change ran, but the service could not be asked to confirm it.",
        ),
    }
}

impl Access {
    pub fn new() -> Access {
        Access {
            view: View::Checking,
            busy: false,
            message: None,
            confirm_everyone: false,
            add: None,
            asked: false,
            rx: None,
            lookup_rx: None,
            lookup_running: Arc::new(AtomicBool::new(false)),
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
        elevate: impl FnOnce(&str) -> Result<(), RunError> + Send + 'static,
        mut ask: impl FnMut() -> View + Send + 'static,
        pause: impl FnMut(Duration) + Send + 'static,
    ) {
        let Some(args) = action.args() else {
            self.message = Some(Notice::new(
                Outcome::Problem,
                "That account id is not valid.",
            ));
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
            let (view, seen) = confirm(&action, &mut ask, pause);
            Done {
                message: Some(outcome(&action, seen, &view, ran)),
                view,
                acted: true,
            }
        });
        if self.rx.is_none() {
            self.busy = false;
        }
    }

    /// Screenshot and unit tests: show a given answer without asking.
    #[cfg(all(test, feature = "screenshots"))]
    pub fn show_for_test(&mut self, view: View, busy: bool, message: Option<Notice>) {
        self.view = view;
        self.busy = busy;
        self.message = message;
        self.asked = true;
    }

    /// Take a finished result (called every frame, next to the updater's).
    /// True when a change was attempted: the engine should look for the
    /// service again.
    pub fn poll(&mut self) -> bool {
        self.poll_lookup();
        let Some(rx) = &self.rx else { return false };
        let Ok(done) = rx.try_recv() else {
            return false;
        };
        self.rx = None;
        self.busy = false;
        self.view = done.view;
        if let Some(notice) = done.message {
            // The account was added: the field has done its job.
            if done.acted && matches!(notice.outcome, Outcome::Done | Outcome::Unconfirmed) {
                self.add = None;
            }
            self.message = Some(notice);
        }
        done.acted
    }

    /// Take a finished lookup. An answer for text that has since been edited
    /// is dropped (editing the field resets it to idle).
    fn poll_lookup(&mut self) {
        let Some(rx) = &self.lookup_rx else { return };
        let Ok(result) = rx.try_recv() else { return };
        self.lookup_rx = None;
        if let Some(add) = &mut self.add
            && add.state == AddState::Looking
        {
            add.state = match result {
                Ok(found) => AddState::Found(found),
                Err(why) => AddState::Failed(why),
            };
        }
    }

    /// Look up the typed account (the Look up button).
    pub fn look_up(&mut self, wake: impl Fn() + Send + 'static) {
        self.look_up_with(wake, os_resolve, LOOKUP_DEADLINE);
    }

    fn look_up_with(
        &mut self,
        wake: impl Fn() + Send + 'static,
        resolver: impl FnOnce(&str) -> Result<Resolved, String> + Send + 'static,
        deadline: Duration,
    ) {
        let Some(add) = &mut self.add else { return };
        if add.state == AddState::Looking {
            return;
        }
        let text = add.text.clone();
        let running = self.lookup_running.clone();
        let (tx, rx) = channel();
        let started = std::thread::Builder::new()
            .name("ferrow-access-lookup".into())
            .spawn(move || {
                let _ = tx.send(lookup(&text, resolver, deadline, &running));
                wake();
            });
        if started.is_ok() {
            add.state = AddState::Looking;
            self.lookup_rx = Some(rx);
        } else {
            add.state = AddState::Failed("The lookup could not be started.".into());
        }
    }

    /// Whether the actions can run from this app (installed Windows copy).
    pub fn can_elevate() -> bool {
        #[cfg(all(test, feature = "screenshots"))]
        if FORCE_ELEVATE.load(std::sync::atomic::Ordering::Relaxed) {
            return true;
        }
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

/// Screenshot tests show the buttons of an installed copy.
#[cfg(all(test, feature = "screenshots"))]
static FORCE_ELEVATE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(all(test, feature = "screenshots"))]
pub fn test_can_elevate(on: bool) {
    FORCE_ELEVATE.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Which accounts the service serves, as one sentence.
pub fn rule(info: &AccessInfo) -> String {
    match (info.mode, info.users.as_ref()) {
        (AccessMode::Admins, _) => "Only administrators can use it.".to_owned(),
        (AccessMode::Everyone, _) => {
            "Every person who signs in to this PC can use it and sees every file name on it."
                .to_owned()
        }
        (AccessMode::Listed, Some(users)) => {
            let n = users.len();
            format!(
                "Administrators and {n} listed {} can use it.",
                if n == 1 { "account" } else { "accounts" }
            )
        }
        (AccessMode::Listed, None) => {
            "Administrators and the listed accounts can use it.".to_owned()
        }
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
            let rule = rule(info);
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

/// The OS lookup: a name or SID to the account's SID and name (Windows).
#[cfg(windows)]
fn os_resolve(text: &str) -> Result<Resolved, String> {
    use windows_sys::Win32::Foundation::{GetLastError, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSidToSidW,
    };
    use windows_sys::Win32::Security::{LookupAccountNameW, LookupAccountSidW, PSID, SidTypeUser};
    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(Some(0)).collect() };
    let mut sid_buffer = [0usize; 16]; // 128 bytes, above the 68-byte SID maximum
    let mut owned_sid: PSID = std::ptr::null_mut();
    let sid: PSID = if text
        .get(..4)
        .is_some_and(|p| p.eq_ignore_ascii_case("S-1-"))
    {
        // SAFETY: NUL-terminated input; the SID is freed below.
        if unsafe { ConvertStringSidToSidW(wide(text).as_ptr(), &raw mut owned_sid) } == 0 {
            return Err(format!("\"{text}\" is not a valid SID."));
        }
        owned_sid
    } else {
        let name = wide(text);
        let mut sid_len = size_of_val(&sid_buffer) as u32;
        let mut domain = [0u16; 512];
        let mut domain_len = domain.len() as u32;
        let mut kind = 0;
        // SAFETY: NUL-terminated name and buffers whose lengths are passed in.
        let ok = unsafe {
            LookupAccountNameW(
                std::ptr::null(),
                name.as_ptr(),
                sid_buffer.as_mut_ptr().cast(),
                &raw mut sid_len,
                domain.as_mut_ptr(),
                &raw mut domain_len,
                &raw mut kind,
            )
        };
        if ok == 0 {
            // SAFETY: reads this thread's last error, right after the call.
            return Err(lookup_error(unsafe { GetLastError() }, text));
        }
        sid_buffer.as_mut_ptr().cast()
    };
    let result = (|| {
        let mut string: windows_sys::core::PWSTR = std::ptr::null_mut();
        // SAFETY: a valid SID; the string is allocated by the call, freed below.
        if unsafe { ConvertSidToStringSidW(sid, &raw mut string) } == 0 || string.is_null() {
            return Err("Windows returned an account id that can't be read.".to_owned());
        }
        // SAFETY: NUL-terminated string from the call above, freed once.
        let id = unsafe {
            let mut len = 0;
            while *string.add(len) != 0 {
                len += 1;
            }
            let id = String::from_utf16_lossy(std::slice::from_raw_parts(string, len));
            LocalFree(string.cast());
            id
        };
        let (mut name, mut domain) = ([0u16; 512], [0u16; 512]);
        let (mut name_len, mut domain_len) = (name.len() as u32, domain.len() as u32);
        let mut kind = 0;
        // SAFETY: valid SID and buffers whose lengths are passed in.
        let named = unsafe {
            LookupAccountSidW(
                std::ptr::null(),
                sid,
                name.as_mut_ptr(),
                &raw mut name_len,
                domain.as_mut_ptr(),
                &raw mut domain_len,
                &raw mut kind,
            )
        } != 0;
        if !named {
            // A SID nobody owns (a deleted account) can still be listed.
            return Ok(Resolved {
                name: id.clone(),
                id,
            });
        }
        if kind != SidTypeUser {
            return Err(format!(
                "\"{text}\" is not a user account. Add individual users, or use Allow everyone."
            ));
        }
        let name = String::from_utf16_lossy(&name[..name_len as usize]);
        let domain = String::from_utf16_lossy(&domain[..domain_len as usize]);
        Ok(Resolved {
            id,
            name: if domain.is_empty() {
                name
            } else {
                format!("{domain}\\{name}")
            },
        })
    })();
    if !owned_sid.is_null() {
        // SAFETY: frees the SID allocated by ConvertStringSidToSidW.
        unsafe { LocalFree(owned_sid.cast()) };
    }
    result
}

/// Unix: the account is named in a command for the administrator to run;
/// nothing is looked up here.
#[cfg(not(windows))]
fn os_resolve(text: &str) -> Result<Resolved, String> {
    match safe_account_arg(text) {
        Some(arg) => Ok(Resolved {
            id: arg.to_owned(),
            name: arg.to_owned(),
        }),
        None => Err("Use a plain user name (letters, digits and . _ - @) or a numeric id.".into()),
    }
}

#[cfg(windows)]
fn run_elevated(args: &str) -> Result<(), RunError> {
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
        return Err(RunError::Failed("ferrow.exe is not installed next to this app. Run the command in an elevated terminal.".into()));
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
            RunError::Declined
        } else {
            RunError::Failed(format!(
                "The change could not be started (Windows error {code})."
            ))
        })
    } else if info.hProcess.is_null() {
        Err(RunError::Failed(
            "The change could not be started.".to_owned(),
        ))
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
            Err(RunError::Failed(
                "The change is taking longer than expected. Check again in a moment.".to_owned(),
            ))
        } else if exit == 0 {
            Ok(())
        } else {
            Err(RunError::Failed("The change was not applied. Run `ferrow service access show` in a terminal for details.".to_owned()))
        }
    };
    if com {
        // SAFETY: matches the successful CoInitializeEx above.
        unsafe { CoUninitialize() };
    }
    result
}

#[cfg(not(windows))]
fn run_elevated(_args: &str) -> Result<(), RunError> {
    Err(RunError::Failed(
        "Run the command in a terminal with sudo.".into(),
    ))
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_builds_never_ask_the_real_service() {
        // The real service on a developer PC would answer (or time out);
        // a test build must report no service without connecting.
        assert_eq!(super::query(), super::View::NoService);
    }

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
            // A plain number is a Unix uid: not an account id on Windows.
            (!cfg!(windows)).then_some(true)
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
    fn the_rule_line_is_what_the_summary_shows() {
        let view = info(AccessMode::Everyone, true, None);
        let View::Info(i) = &view else { unreachable!() };
        assert_eq!(summary(&view).1, rule(i));
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
        let (yes, no, unknown) = (Seen::Yes, Seen::No, Seen::Unknown);
        let allow = Action::AllowMe(SID.into());
        assert_eq!(allow.seen(&info_of(AccessMode::Listed, true, None)), yes);
        assert_eq!(allow.seen(&info_of(AccessMode::Admins, false, None)), no);
        assert_eq!(
            Action::Everyone.seen(&info_of(AccessMode::Everyone, true, None)),
            yes
        );
        assert_eq!(
            Action::Everyone.seen(&info_of(AccessMode::Listed, true, None)),
            no
        );
        assert_eq!(
            Action::Admins.seen(&info_of(AccessMode::Admins, true, None)),
            yes
        );
        assert_eq!(
            Action::Admins.seen(&info_of(AccessMode::Everyone, true, None)),
            no
        );
        let remove = |id: &str| Action::Remove(id.into());
        assert_eq!(remove("1000").seen(&listed(&["1000", "1001"])), no);
        assert_eq!(remove("1000").seen(&listed(&["1001"])), yes);
        // A standard user cannot see the list: only its own entry shows.
        assert_eq!(
            remove(SID).seen(&info_of(AccessMode::Listed, false, None)),
            yes
        );
        assert_eq!(
            remove(SID).seen(&info_of(AccessMode::Listed, true, None)),
            no
        );
        // Another account's removal or addition cannot be seen at all.
        assert_eq!(
            remove("1000").seen(&info_of(AccessMode::Listed, true, None)),
            unknown
        );
        let add = |id: &str| Action::Add(id.into());
        assert_eq!(add("1002").seen(&listed(&["1001"])), no);
        assert_eq!(add("1002").seen(&listed(&["1001", "1002"])), yes);
        assert_eq!(
            add("1002").seen(&info_of(AccessMode::Listed, true, None)),
            unknown
        );
        assert_eq!(add(SID).seen(&info_of(AccessMode::Listed, true, None)), yes);
        assert_eq!(add(SID).seen(&info_of(AccessMode::Listed, false, None)), no);
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
        let (view, seen) = confirm(&Action::Everyone, ask, move |d| log.lock().unwrap().push(d));
        assert!(seen == Seen::Yes && view == fresh);
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
        let (view, seen) = confirm(&Action::Everyone, ask, move |d| log.lock().unwrap().push(d));
        assert!(seen == Seen::No && matches!(view, View::Info(_)));
        assert_eq!(asked.load(Ordering::SeqCst), 3);
        let total: Duration = pauses.lock().unwrap().iter().sum();
        assert_eq!(total, Duration::from_millis(2400));
    }

    fn failed(text: &str) -> Result<(), RunError> {
        Err(RunError::Failed(text.into()))
    }

    #[test]
    fn success_is_decided_by_the_answer_and_the_exit_code_only_explains() {
        let shown = info(AccessMode::Everyone, true, Some(0));
        let stale = info(AccessMode::Admins, false, Some(0));
        let act = Action::Everyone;
        let kind = |seen, view: &View, ran| outcome(&act, seen, view, ran).outcome;
        // The command reported failure, but the service shows the change.
        assert_eq!(kind(Seen::Yes, &shown, failed("exit 1")), Outcome::Done);
        // The command reported success, but the service does not show it.
        let n = outcome(&act, Seen::No, &stale, Ok(()));
        assert!(n.outcome == Outcome::Problem && n.text.contains("does not show it yet"));
        // The service could not be asked at all.
        let n = outcome(&act, Seen::No, &View::TooOld, Ok(()));
        assert!(n.text.contains("could not be asked"));
        // The prompt was declined: never done, whatever the answer can show.
        for seen in [Seen::No, Seen::Unknown] {
            let n = outcome(&act, seen, &stale, Err(RunError::Declined));
            assert_eq!(n.outcome, Outcome::Declined, "{seen:?}");
            assert_eq!(n.text, DECLINED_TEXT);
        }
        assert_eq!(
            outcome(&act, Seen::No, &stale, failed("boom")),
            Notice::new(Outcome::Problem, "boom")
        );
    }

    #[test]
    fn a_change_this_account_cannot_see_is_judged_by_how_the_command_ended() {
        // A standard user approving the prompt with administrator
        // credentials cannot read the list: the answer proves nothing.
        let hidden = info_of(AccessMode::Listed, true, None);
        let add = Action::Add("S-1-5-21-1-2-3-1002".into());
        assert_eq!(add.seen(&hidden), Seen::Unknown);
        let view = View::Info(hidden);
        // Prompt declined or command failed: never reported as done.
        let n = outcome(&add, Seen::Unknown, &view, Err(RunError::Declined));
        assert_eq!(n.outcome, Outcome::Declined);
        let n = outcome(&add, Seen::Unknown, &view, failed("exit 1"));
        assert_eq!(n.outcome, Outcome::Problem);
        // Command succeeded: said so, and said it cannot be confirmed.
        let n = outcome(&add, Seen::Unknown, &view, Ok(()));
        assert_eq!(n.outcome, Outcome::Unconfirmed);
        assert_eq!(
            n.text,
            "Added — the list isn't visible to this account, so Ferrow can't confirm it."
        );
        let remove = Action::Remove("S-1-5-21-1-2-3-1002".into());
        let n = outcome(&remove, Seen::Unknown, &view, Ok(()));
        assert!(n.text.starts_with("Removed — the list isn't visible"));
    }

    #[test]
    fn an_unseen_change_is_asked_about_once_not_three_times() {
        let hidden = info_of(AccessMode::Listed, true, None);
        let (ask, pauses, asked) = scripted(vec![View::Info(hidden)]);
        let log = pauses.clone();
        let add = Action::Add("S-1-5-21-1-2-3-1002".into());
        let (_, seen) = confirm(&add, ask, move |d| log.lock().unwrap().push(d));
        assert_eq!(seen, Seen::Unknown);
        assert_eq!(asked.load(Ordering::SeqCst), 1);
        assert!(pauses.lock().unwrap().is_empty());
    }

    #[test]
    fn a_declined_add_by_a_standard_user_is_not_done() {
        // The whole path: the answer is Listed/allowed with the list hidden,
        // and the administrator prompt is declined.
        let mut access = Access::new();
        let hidden = info_of(AccessMode::Listed, true, None);
        let (ask, _, _) = scripted(vec![View::Info(hidden)]);
        access.add = Some(AddAccount::opened());
        access.run_with(
            Action::Add("S-1-5-21-1-2-3-1002".into()),
            || {},
            |_| Err(RunError::Declined),
            ask,
            |_| {},
        );
        assert!(wait_done(&mut access));
        let outcome = access.message.as_ref().map(|m| m.outcome);
        assert_eq!(outcome, Some(Outcome::Declined));
        assert!(access.add.is_some(), "kept for another try");
    }

    #[test]
    fn the_list_rule_uses_the_right_plural() {
        for (n, want) in [
            (0, "0 listed accounts"),
            (1, "1 listed account can"),
            (2, "2 listed accounts"),
        ] {
            let text = summary(&info(AccessMode::Listed, true, Some(n))).1;
            assert!(text.contains(want), "{text}");
        }
        assert!(
            !summary(&info(AccessMode::Listed, true, Some(2)))
                .1
                .contains("(s)")
        );
    }

    fn found(id: &str, name: &str) -> Resolved {
        Resolved {
            id: id.into(),
            name: name.into(),
        }
    }

    const QUICK: Duration = Duration::from_secs(5);

    fn lookup_t(
        text: &str,
        resolver: impl FnOnce(&str) -> Result<Resolved, String> + Send + 'static,
        deadline: Duration,
    ) -> Result<Resolved, String> {
        lookup(text, resolver, deadline, &Arc::new(AtomicBool::new(false)))
    }

    #[test]
    fn a_lookup_returns_what_the_resolver_found_and_passes_its_errors_on() {
        let ok = lookup_t(
            r"  ALEX-PC\Sam ",
            |text| {
                assert_eq!(text, r"ALEX-PC\Sam", "trimmed before it is looked up");
                Ok(found(SID, r"ALEX-PC\Sam"))
            },
            QUICK,
        );
        assert_eq!(ok, Ok(found(SID, r"ALEX-PC\Sam")));
        let unknown = lookup_t("nobody", |t| Err(lookup_error(ERROR_NONE_MAPPED, t)), QUICK);
        assert_eq!(
            unknown.unwrap_err(),
            "No account named \"nobody\" was found on this PC or its domain."
        );
        assert!(lookup_error(5, "x").contains("error 5"));
    }

    #[test]
    fn the_typed_text_is_checked_before_anything_looks_it_up() {
        let never = |_: &str| -> Result<Resolved, String> { panic!("must not look up") };
        assert!(
            lookup_t("   ", never, QUICK)
                .unwrap_err()
                .starts_with("Type a name")
        );
        assert!(lookup_t("a\u{1b}[2Jb", never, QUICK).is_err());
        assert!(lookup_t(&"x".repeat(MAX_ACCOUNT_TEXT + 1), never, QUICK).is_err());
    }

    #[test]
    fn an_answer_that_is_not_a_canonical_id_is_refused() {
        // Whatever the resolver says, only a usable id can be offered for the
        // elevated command line.
        for bad in [
            "1000 & calc",
            "S-1-5-21-1\" --x",
            "",
            "-rf",
            "a b",
            "S-2-5-18",
        ] {
            let r = lookup_t("x", move |_| Ok(found(bad, "x")), QUICK);
            assert!(r.is_err(), "{bad:?}");
        }
        assert!(lookup_t("x", |_| Ok(found(SID, "x")), QUICK).is_ok());
        assert_eq!(add_command(&found("1000 & calc", "x")), None);
        assert!(
            add_command(&found(SID, "x"))
                .is_some_and(|c| c.ends_with(&format!("service access add {SID}")))
        );
    }

    #[test]
    fn a_lookup_that_stalls_is_abandoned_at_the_deadline() {
        let started = Instant::now();
        let r = lookup_t(
            "slow",
            |_| {
                std::thread::sleep(Duration::from_millis(1500));
                Ok(found(SID, "slow"))
            },
            Duration::from_millis(40),
        );
        assert!(r.unwrap_err().contains("took too long"));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn only_a_canonical_id_reaches_the_elevated_add_command() {
        assert_eq!(
            Action::Add(SID.into()).args().as_deref(),
            Some("service access add S-1-5-21-1-2-3-1001")
        );
        // A typed name, or anything shell-like, never becomes an argument.
        for typed in [r"ALEX-PC\Sam", "Sam", "1000 & calc", "S-1-5-21-1\" --x", ""] {
            assert_eq!(Action::Add(typed.into()).args(), None, "{typed:?}");
        }
        let mut access = Access::new();
        access.run_with(
            Action::Add(r"ALEX-PC\Sam".into()),
            || {},
            |_| panic!("must not run"),
            || View::Checking,
            |_| {},
        );
        assert!(!access.busy && access.rx.is_none());
    }

    #[test]
    fn a_plain_number_is_not_an_account_id_on_windows() {
        // A decimal id is a Unix uid; on Windows it means nothing and must
        // never reach the elevated command line.
        assert_eq!(account_id_ok("1000"), !cfg!(windows));
        assert_eq!(Action::Add("1000".into()).args().is_some(), !cfg!(windows));
        assert_eq!(usable_id("1000"), !cfg!(windows));
        assert!(account_id_ok(SID) && usable_id(SID));
        assert!(!account_id_ok("S-1-5-21-1-2-3-1001 & calc"));
        assert_eq!(add_command(&found("1000", "x")).is_some(), !cfg!(windows));
    }

    #[test]
    fn a_stuck_lookup_is_the_only_one_and_a_new_one_says_so() {
        let running = Arc::new(AtomicBool::new(false));
        let (release_tx, release_rx) = channel::<()>();
        // The first resolver hangs until released: the call gives up at the
        // deadline but its thread is still alive.
        let first = lookup(
            "slow",
            move |_| {
                let _ = release_rx.recv_timeout(Duration::from_secs(5));
                Ok(found(SID, "slow"))
            },
            Duration::from_millis(40),
            &running,
        );
        assert!(first.unwrap_err().contains("took too long"));
        assert!(running.load(Ordering::SeqCst), "still stuck");
        let second = lookup(
            "other",
            |_| panic!("no second thread while one is stuck"),
            QUICK,
            &running,
        );
        assert_eq!(
            second.unwrap_err(),
            "Still waiting for the last lookup. Try again in a moment."
        );
        // Once the stuck one ends, lookups work again.
        release_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while running.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "the stuck lookup never ended");
            std::thread::sleep(Duration::from_millis(5));
        }
        let third = lookup("ok", |_| Ok(found(SID, "ok")), QUICK, &running);
        assert!(third.is_ok());
        assert!(!running.load(Ordering::SeqCst));
    }

    #[test]
    fn a_found_line_only_looks_verified_when_the_account_was_looked_up() {
        let r = found("sam", "sam");
        assert_eq!(found_line(&r, true), (true, "sam".to_owned()));
        // Unix: only a command is shown, nothing was checked against the OS.
        assert_eq!(found_line(&r, false), (false, "Command for sam".to_owned()));
    }

    /// The real Windows lookup refuses every well-known group, with no domain
    /// controller involved.
    #[cfg(windows)]
    #[test]
    fn the_windows_lookup_refuses_well_known_sids_that_are_not_users() {
        for (sid, what) in [
            ("S-1-1-0", "Everyone"),
            ("S-1-5-11", "Authenticated Users"),
            ("S-1-5-18", "SYSTEM"),
            ("S-1-5-7", "Anonymous"),
            ("S-1-5-32-544", "Administrators"),
        ] {
            let err = os_resolve(sid).unwrap_err();
            assert!(err.contains("is not a user account"), "{what}: {err}");
        }
    }

    fn wait_lookup(access: &mut Access) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while access.lookup_rx.is_some() {
            access.poll();
            assert!(Instant::now() < deadline, "lookup did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn the_add_field_looks_up_once_and_keeps_the_answer_for_its_text() {
        let mut access = Access::new();
        access.look_up_with(|| {}, |_| panic!("no field open"), QUICK);
        assert!(
            access.lookup_rx.is_none(),
            "nothing to look up without a field"
        );
        access.add = Some(AddAccount {
            text: "sam".into(),
            state: AddState::Idle,
            ..Default::default()
        });
        access.look_up_with(|| {}, |_| Ok(found(SID, "Sam")), QUICK);
        assert_eq!(access.add.as_ref().unwrap().state, AddState::Looking);
        // Pressed again while looking: ignored (the resolver would panic).
        access.look_up_with(|| {}, |_| panic!("second lookup"), QUICK);
        wait_lookup(&mut access);
        assert_eq!(
            access.add.as_ref().unwrap().state,
            AddState::Found(found(SID, "Sam"))
        );
        // Typing resets the answer; a late answer for old text is dropped.
        access.add = Some(AddAccount {
            text: "ghost".into(),
            state: AddState::Idle,
            ..Default::default()
        });
        access.look_up_with(|| {}, |t| Err(lookup_error(ERROR_NONE_MAPPED, t)), QUICK);
        access.add.as_mut().unwrap().state = AddState::Idle; // the user typed on
        wait_lookup(&mut access);
        assert_eq!(access.add.as_ref().unwrap().state, AddState::Idle);
    }

    #[test]
    fn a_failed_lookup_is_shown_and_a_finished_change_closes_the_field() {
        let mut access = Access::new();
        access.add = Some(AddAccount {
            text: "ghost".into(),
            state: AddState::Idle,
            ..Default::default()
        });
        access.look_up_with(|| {}, |t| Err(lookup_error(ERROR_NONE_MAPPED, t)), QUICK);
        wait_lookup(&mut access);
        assert!(
            matches!(&access.add.as_ref().unwrap().state, AddState::Failed(m) if m.contains("No account named"))
        );
        // A declined prompt keeps the field for another try.
        let stale = info(AccessMode::Admins, false, Some(0));
        let (ask, _, _) = scripted(vec![stale]);
        access.run_with(
            Action::Add(SID.into()),
            || {},
            |_| Err(RunError::Declined),
            ask,
            |_| {},
        );
        assert!(access.busy, "no second change while this one runs");
        assert!(wait_done(&mut access));
        assert!(access.add.is_some());
        // A change that shows in the answer closes it.
        let shown = info_of(AccessMode::Listed, true, Some(vec![user(SID)]));
        let (ask, _, _) = scripted(vec![View::Info(shown)]);
        access.run_with(Action::Add(SID.into()), || {}, |_| Ok(()), ask, |_| {});
        assert!(wait_done(&mut access));
        assert!(access.add.is_none());
    }

    #[test]
    fn unix_account_names_are_safe_to_show_in_a_command() {
        for ok in ["sam", "sam.k_x-1", "a@b", "1000"] {
            assert_eq!(safe_account_arg(ok), Some(ok));
        }
        for bad in ["", "-x", "a b", "a;b", "a$(b)", "a\nb", "a\"b"] {
            assert_eq!(safe_account_arg(bad), None, "{bad:?}");
        }
    }

    /// The real Windows lookup, on inputs that need no domain controller.
    #[cfg(windows)]
    #[test]
    fn the_windows_lookup_refuses_groups_and_malformed_sids() {
        // BUILTIN\Users: a group grants nobody who signs in.
        let group = os_resolve("S-1-5-32-545").unwrap_err();
        assert!(group.contains("is not a user account"), "{group}");
        let bad = os_resolve("S-1-banana").unwrap_err();
        assert!(bad.contains("not a valid SID"), "{bad}");
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
        assert_eq!(
            access.message.as_ref().map(|m| m.outcome),
            Some(Outcome::Done)
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
            |_| Err(RunError::Declined),
            ask,
            |_| {},
        );
        assert!(wait_done(&mut access));
        assert_eq!(access.view, stale);
        assert_eq!(
            access.message.as_ref().map(|m| m.outcome),
            Some(Outcome::Declined)
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
