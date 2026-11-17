//! Settings → Service access: who may use the Ferrow system service.
//!
//! Everything slow runs on its own thread, so the window never waits: the
//! status query uses a short deadline (an older service does not answer the
//! request at all), and changes go through the elevated `ferrow` command line
//! (`ShellExecuteExW` verb `runas`, Windows). After the change the card asks
//! the service again and the engine looks for the service at once, so a newly
//! allowed account switches over without restarting the app.

use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use ferrow_ipc::{AccessInfo, AccessMode, Client, Endpoint};

/// How long to wait for the service's answer. An older service never answers.
const QUERY_DEADLINE: Duration = Duration::from_millis(800);

#[derive(Clone, Debug, PartialEq)]
pub enum View {
    Checking,
    NoService,
    /// The service is running but predates access settings.
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

impl Action {
    /// The `ferrow` arguments, or `None` when an id is not a plain SID/uid.
    pub fn args(&self) -> Option<String> {
        let id = |id: &str| {
            let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
            let ok = digits(id)
                || id.strip_prefix("S-1-").is_some_and(|rest| {
                    rest.starts_with(|c: char| c.is_ascii_digit())
                        && rest.bytes().all(|b| b.is_ascii_digit() || b == b'-')
                });
            ok.then(|| id.to_owned())
        };
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
}

struct Done {
    view: View,
    message: Option<String>,
    changed: bool,
}

pub struct Access {
    pub view: View,
    pub busy: bool,
    /// The last action's outcome.
    pub message: Option<String>,
    /// "Allow every user" was clicked once and waits for its confirmation.
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

    /// Forget the answer so the next visit asks again.
    pub fn invalidate(&mut self) {
        if self.rx.is_none() {
            self.asked = false;
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
        self.asked = true;
        self.spawn(wake, || Done {
            view: query(),
            message: None,
            changed: false,
        });
    }

    /// Run a change elevated (waits for the approval prompt off the UI thread).
    pub fn run(&mut self, action: Action, wake: impl Fn() + Send + 'static) {
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
            let result = run_elevated(&args);
            Done {
                view: query(),
                changed: result.is_ok(),
                message: Some(match result {
                    Ok(()) => "Done. The service applies it within a second.".into(),
                    Err(e) => e,
                }),
            }
        });
        if self.rx.is_none() {
            self.busy = false;
        }
    }

    /// Take a finished result; true when a change went through, so the
    /// engine should look for the service again.
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
        done.changed
    }

    /// Whether the actions can run from this app (installed Windows copy).
    pub fn can_elevate() -> bool {
        cfg!(windows) && crate::update::installed_sibling("ferrow.exe").is_some()
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
            "The installed service is older than this app",
            "Update Ferrow with Setup to choose who can use the service.",
        ),
        View::Info(info) => {
            let rule = match (info.mode, info.users.as_ref()) {
                (AccessMode::Admins, _) => "Only administrators can use it.".to_owned(),
                (AccessMode::Everyone, _) => {
                    "Every account on this PC can use it and sees every file name on it.".to_owned()
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
    // SAFETY: zeroed POD struct with its size set; every string outlives the call.
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = params.as_ptr();
    info.nShow = 0; // SW_HIDE: the command line has no window to show
    // SAFETY: initialized struct; the shell fills hProcess on success.
    if unsafe { ShellExecuteExW(&raw mut info) } == 0 {
        // SAFETY: reads this thread's last error.
        let code = unsafe { GetLastError() };
        return Err(if code == ERROR_CANCELLED {
            "Administrator approval was declined; nothing was changed.".into()
        } else {
            format!("The change could not be started (Windows error {code}).")
        });
    }
    if info.hProcess.is_null() {
        return Err("The change could not be started.".into());
    }
    let mut code = 1u32;
    // SAFETY: the process handle is ours (NOCLOSEPROCESS) and closed once.
    // The command is short; a bounded wait keeps this thread from hanging on it.
    let timed_out = unsafe {
        let waited = WaitForSingleObject(info.hProcess, 30_000);
        GetExitCodeProcess(info.hProcess, &raw mut code);
        CloseHandle(info.hProcess);
        waited == WAIT_TIMEOUT
    };
    if timed_out {
        Err("The change is taking longer than expected. Check again in a moment.".into())
    } else if code == 0 {
        Ok(())
    } else {
        Err("The change was not applied. Run `ferrow service access show` in a terminal for details.".into())
    }
}

#[cfg(not(windows))]
fn run_elevated(_args: &str) -> Result<(), String> {
    Err("Run the command in a terminal with sudo.".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrow_ipc::AccessUser;

    fn info(mode: AccessMode, allowed: bool, users: Option<usize>) -> View {
        View::Info(AccessInfo {
            mode,
            you_allowed: allowed,
            you: AccessUser {
                id: "S-1-5-21-1-2-3-1001".into(),
                name: String::new(),
            },
            users: users.map(|n| vec![AccessUser::default(); n]),
        })
    }

    #[test]
    fn allow_me_passes_the_standard_users_sid_never_the_current_user() {
        let sid = "S-1-5-21-1-2-3-1001";
        assert_eq!(
            Action::AllowMe(sid.into()).args().as_deref(),
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
    fn only_plain_ids_reach_the_elevated_command_line() {
        for bad in [
            "",
            "S-1-",
            "alice",
            "1000 & calc",
            "S-1-5-21-1\" --x",
            "-1",
            "S-2-5",
        ] {
            assert_eq!(Action::AllowMe(bad.into()).args(), None, "{bad:?}");
            assert_eq!(Action::Remove(bad.into()).args(), None, "{bad:?}");
        }
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
        assert!(everyone.1.contains("every file name"));
        assert!(
            summary(&info(AccessMode::Listed, true, Some(2)))
                .1
                .contains("2 listed")
        );
    }

    #[test]
    fn an_old_or_missing_service_has_its_own_plain_text() {
        assert!(summary(&View::TooOld).1.contains("Update Ferrow"));
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
}
