//! In-app update check (Settings → Updates).
//!
//! Checks run only while the app is open: once when it opens (installed
//! copies, at most every 24 hours) and on **Check now**. The request runs
//! on its own thread through the system `curl` with a hard time limit, so a
//! slow or missing network never touches the UI. Installing hands the
//! version to the elevated `ferrow-update.exe` next to the app (Windows);
//! it downloads, verifies and runs the setup itself.

use std::sync::mpsc::{Receiver, channel};

use ferrow_update::{Channel, Release, Version};

/// What the Updates card shows.
#[derive(Clone, Debug, PartialEq)]
pub enum UpdateState {
    Idle,
    Checking,
    UpToDate,
    Available(Release),
    CheckFailed(String),
    /// Waiting for administrator approval and the updater.
    Installing,
    InstallFailed(String),
}

enum Msg {
    Checked(Result<Option<Release>, String>),
    Installed(Result<(), String>),
}

pub struct Updater {
    pub state: UpdateState,
    /// The once-per-launch check has been considered.
    pub opened: bool,
    rx: Option<Receiver<Msg>>,
}

/// The version this build reports (`CARGO_PKG_VERSION`).
pub fn current_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or(Version {
        major: 0,
        minor: 0,
        patch: 0,
        pre: vec!["dev".into()],
    })
}

/// True for a copy installed by setup, which can update itself. Other
/// builds (development, portable) only link to the release page.
pub fn can_install() -> bool {
    #[cfg(windows)]
    {
        let Ok(exe) = std::env::current_exe() else {
            return false;
        };
        let Some(dir) = exe.parent() else {
            return false;
        };
        let Some(program_files) = std::env::var_os("ProgramW6432")
            .or_else(|| std::env::var_os("ProgramFiles"))
            .map(std::path::PathBuf::from)
        else {
            return false;
        };
        dir.eq_ignore_ascii_case_path(&program_files.join("Ferrow"))
            && dir.join("ferrow-update.exe").is_file()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

#[cfg(windows)]
trait PathEq {
    fn eq_ignore_ascii_case_path(&self, other: &std::path::Path) -> bool;
}
#[cfg(windows)]
impl PathEq for std::path::Path {
    fn eq_ignore_ascii_case_path(&self, other: &std::path::Path) -> bool {
        self.as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&other.as_os_str().to_string_lossy())
    }
}

impl Updater {
    pub fn new() -> Updater {
        Updater {
            state: UpdateState::Idle,
            opened: false,
            rx: None,
        }
    }

    pub fn busy(&self) -> bool {
        matches!(self.state, UpdateState::Checking | UpdateState::Installing)
    }

    /// Start a check on its own thread; `wake` repaints when it is done.
    pub fn check(&mut self, channel: Channel, wake: impl Fn() + Send + 'static) {
        if self.busy() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("ferrow-update-check".into())
            .spawn(move || {
                let result = fetch(channel);
                let _ = tx.send(Msg::Checked(result));
                wake();
            });
        if spawned.is_ok() {
            self.state = UpdateState::Checking;
            self.rx = Some(rx);
        }
    }

    /// Ask for administrator approval and run the updater (Windows).
    pub fn install(&mut self, version: &Version, wake: impl Fn() + Send + 'static) {
        if self.busy() {
            return;
        }
        let (tx, rx) = channel();
        let version = version.to_string();
        let spawned = std::thread::Builder::new()
            .name("ferrow-update-install".into())
            .spawn(move || {
                let _ = tx.send(Msg::Installed(run_updater(&version)));
                wake();
            });
        if spawned.is_ok() {
            self.state = UpdateState::Installing;
            self.rx = Some(rx);
        }
    }

    /// Take a finished result. Returns true when a check completed.
    pub fn poll(&mut self) -> bool {
        let Some(rx) = &self.rx else { return false };
        let Ok(msg) = rx.try_recv() else { return false };
        self.rx = None;
        match msg {
            Msg::Checked(result) => {
                let current = current_version();
                self.state = match result {
                    Ok(Some(release)) if release.version > current => {
                        UpdateState::Available(release)
                    }
                    Ok(_) => UpdateState::UpToDate,
                    Err(e) => UpdateState::CheckFailed(e),
                };
                true
            }
            Msg::Installed(result) => {
                self.state = match result {
                    // Setup normally closes this window first; if it is
                    // still open, the update finished without restarting it.
                    Ok(()) => UpdateState::UpToDate,
                    Err(e) => UpdateState::InstallFailed(e),
                };
                false
            }
        }
    }
}

/// Ask GitHub for the release this channel offers.
fn fetch(channel: Channel) -> Result<Option<Release>, String> {
    let url = match channel {
        Channel::Stable => ferrow_update::latest_api_url(),
        Channel::Beta => ferrow_update::list_api_url(),
    };
    let mut command = std::process::Command::new(curl_path());
    command.args([
        "--proto",
        "=https",
        "--fail",
        "--silent",
        "--show-error",
        "--location",
        "--connect-timeout",
        "10",
        "--max-time",
        "20",
        "--max-filesize",
        "4194304",
        "--header",
        "Accept: application/vnd.github+json",
        "--user-agent",
        "ferrow-gui",
        &url,
    ]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = command
        .output()
        .map_err(|e| format!("Could not run curl: {e}"))?;
    if !out.status.success() {
        let text = String::from_utf8_lossy(&out.stderr);
        // A repository without any stable release answers 404 on latest.
        if text.contains("404") {
            return Ok(None);
        }
        return Err(format!("Could not reach GitHub: {}", text.trim()));
    }
    ferrow_update::pick(&String::from_utf8_lossy(&out.stdout), channel)
}

fn curl_path() -> std::path::PathBuf {
    #[cfg(windows)]
    {
        let windir = std::env::var_os("SystemRoot")
            .or_else(|| std::env::var_os("WINDIR"))
            .map_or_else(|| std::path::PathBuf::from(r"C:\Windows"), Into::into);
        windir.join("System32").join("curl.exe")
    }
    #[cfg(not(windows))]
    {
        "curl".into()
    }
}

#[cfg(windows)]
fn run_updater(version: &str) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_CANCELLED, GetLastError};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, INFINITE, WaitForSingleObject,
    };
    use windows_sys::Win32::UI::Shell::{
        SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
    };
    let exe = std::env::current_exe()
        .map_err(|e| e.to_string())?
        .with_file_name("ferrow-update.exe");
    let wide = |s: &std::ffi::OsStr| -> Vec<u16> { s.encode_wide().chain(Some(0)).collect() };
    let verb = wide("runas".as_ref());
    let file = wide(exe.as_os_str());
    let params = wide(format!("--version {version}").as_ref());
    // SAFETY: zeroed POD struct with its size set; every string outlives the call.
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = params.as_ptr();
    info.nShow = 0; // SW_HIDE: the updater has no window
    // SAFETY: initialized struct; the shell fills hProcess on success.
    if unsafe { ShellExecuteExW(&raw mut info) } == 0 {
        // SAFETY: reads this thread's last error.
        let code = unsafe { GetLastError() };
        return Err(if code == ERROR_CANCELLED {
            "Administrator approval was declined; nothing was changed.".into()
        } else {
            format!("The updater could not be started (Windows error {code}).")
        });
    }
    if info.hProcess.is_null() {
        return Err("The updater could not be started.".into());
    }
    let mut code = 1u32;
    // SAFETY: the process handle is ours (NOCLOSEPROCESS) and closed once.
    unsafe {
        // The updater bounds its own work (downloads 5 min, setup 3 min);
        // this is a background thread, so waiting never blocks the window.
        WaitForSingleObject(info.hProcess, INFINITE);
        GetExitCodeProcess(info.hProcess, &raw mut code);
        CloseHandle(info.hProcess);
    }
    if code == 0 {
        Ok(())
    } else {
        Err(ferrow_update::exit::message(code).into())
    }
}

#[cfg(not(windows))]
fn run_updater(_version: &str) -> Result<(), String> {
    Err("Install updates through your package manager.".into())
}
