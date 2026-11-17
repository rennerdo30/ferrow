//! Controlling and repairing the installed service: what `ferrow service
//! start | stop | restart | repair` do. The callers (the CLI) check for
//! administrator rights first; Ferrow never elevates itself.
//!
//! Nothing here starts a process on Windows (the Service Control Manager API
//! is used). On Linux and macOS the manager command (`systemctl`,
//! `launchctl`) runs with a timeout and its PID is recorded on stderr, as
//! `install` does. Every wait is bounded.
//!
//! Repair never copies a binary and never takes a path from the user: the
//! registration is rebuilt only from the fixed installed path
//! (`protection::binary_path`), which must exist and pass the binary
//! protection check. The index is never deleted: one that does not load is
//! renamed to `index.bin.bad-<time>` so the service rebuilds it.

use std::io;
use std::path::Path;
#[cfg(any(windows, test))]
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::install::SERVICE_NAME;
use crate::{access, eventlog, protection};

/// How long a start, a stop or the wait for an answer may take.
pub const WAIT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Start,
    Stop,
    Restart,
}

/// Poll `probe` every `step` until it is true; false once `limit` has passed.
pub fn wait_until(limit: Duration, step: Duration, mut probe: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        if probe() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(step);
    }
}

/// The service's health report, or `None` when it does not answer one (not
/// running, or older than `Health`). One connection, a one-second budget.
pub fn health_of(endpoint: &ferrow_ipc::Endpoint) -> Option<ferrow_ipc::health::Health> {
    let deadline = Instant::now() + Duration::from_secs(1);
    ferrow_ipc::Client::connect_deadline(endpoint, deadline)
        .and_then(|mut client| client.health())
        .ok()
}

/// Whether the service at `endpoint` answers: a `Health` request, or `Status`
/// for an installed service older than `Health` (it never answers that kind). A
/// refusal counts: the service is up. One connection each, a one-second budget.
pub fn answers(endpoint: &ferrow_ipc::Endpoint) -> bool {
    fn up<T>(
        endpoint: &ferrow_ipc::Endpoint,
        ask: impl FnOnce(&mut ferrow_ipc::Client<ferrow_ipc::Stream>) -> io::Result<T>,
    ) -> bool {
        let deadline = Instant::now() + Duration::from_secs(1);
        match ferrow_ipc::Client::connect_deadline(endpoint, deadline).and_then(|mut c| ask(&mut c))
        {
            Ok(_) => true,
            Err(error) => ferrow_ipc::error_code(&error).is_some(),
        }
    }
    up(endpoint, |c| c.health()) || up(endpoint, |c| c.status())
}

// ---- start, stop, restart ---------------------------------------------------

#[cfg(windows)]
pub fn control(action: Action) -> Result<String, String> {
    use windows_service::service::{ServiceAccess, ServiceState};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|e| e.to_string())?;
    let service = manager
        .open_service(
            SERVICE_NAME,
            ServiceAccess::START | ServiceAccess::STOP | ServiceAccess::QUERY_STATUS,
        )
        .map_err(|e| {
            format!(
                "service {SERVICE_NAME}: {e} (not installed? `ferrow service repair` registers it)"
            )
        })?;
    let state = || {
        service
            .query_status()
            .map(|s| s.current_state)
            .map_err(|e| e.to_string())
    };
    let reached = |want: ServiceState| {
        wait_until(WAIT, Duration::from_millis(100), || {
            state().is_ok_and(|s| s == want)
        })
    };
    let before = state()?;
    if action == Action::Start && before == ServiceState::Running {
        return Ok(format!("service {SERVICE_NAME} is already running"));
    }
    if action == Action::Stop && before == ServiceState::Stopped {
        return Ok(format!("service {SERVICE_NAME} is already stopped"));
    }
    if action != Action::Start || before == ServiceState::StopPending {
        if !matches!(before, ServiceState::Stopped | ServiceState::StopPending) {
            service.stop().map_err(|e| e.to_string())?;
        }
        if !reached(ServiceState::Stopped) {
            return Err(format!(
                "service {SERVICE_NAME} did not stop within {} s (it saves the index first); it was not killed",
                WAIT.as_secs()
            ));
        }
    }
    if action == Action::Stop {
        return Ok(format!("service {SERVICE_NAME} stopped"));
    }
    if state()? != ServiceState::StartPending {
        service.start::<&str>(&[]).map_err(|e| e.to_string())?;
    }
    if !reached(ServiceState::Running) {
        return Err(format!(
            "service {SERVICE_NAME} did not reach the running state within {} s",
            WAIT.as_secs()
        ));
    }
    Ok(format!("service {SERVICE_NAME} started"))
}

/// Run the platform manager command; stop and restart may wait for the final
/// index save, so they get longer than a start. (`command_within` resolves the
/// tool to its absolute path.)
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn manage(args: &[&str], timeout: Duration) -> Result<std::process::Output, String> {
    let program = if cfg!(target_os = "linux") {
        "systemctl"
    } else {
        "launchctl"
    };
    crate::install::command_within(program, args, timeout)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn manager_ok(args: &[&str], timeout: Duration) -> Result<(), String> {
    let out = manage(args, timeout)?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

#[cfg(target_os = "linux")]
pub fn control(action: Action) -> Result<String, String> {
    let (verb, done, timeout) = match action {
        Action::Start => ("start", "started", Duration::from_secs(15)),
        Action::Stop => ("stop", "stopped", Duration::from_secs(45)),
        Action::Restart => ("restart", "restarted", Duration::from_secs(45)),
    };
    manager_ok(&[verb, SERVICE_NAME], timeout)?;
    Ok(format!("service {SERVICE_NAME} {done}"))
}

/// launchd: stop is `bootout` (SIGTERM, so the service saves its index) and
/// start is `bootstrap` (or `kickstart` when still loaded). A restart is both;
/// `kickstart -k` would SIGKILL and skip the final save.
#[cfg(target_os = "macos")]
pub fn control(action: Action) -> Result<String, String> {
    use crate::install::{LAUNCHD_LABEL, plist_path};
    let target = format!("system/{LAUNCHD_LABEL}");
    let loaded =
        || manage(&["print", &target], Duration::from_secs(10)).is_ok_and(|o| o.status.success());
    let long = Duration::from_secs(45);
    // The plist's ExitTimeOut is 60 s: launchd waits that long for the final
    // save, then kills the job. Wait a little longer than that.
    let stop_wait = Duration::from_secs(70);
    if matches!(action, Action::Stop | Action::Restart) && loaded() {
        manager_ok(&["bootout", &target], stop_wait)?;
        if !wait_until(stop_wait, Duration::from_millis(200), || !loaded()) {
            return Err(format!(
                "service {SERVICE_NAME} is still loaded after {} s (launchd kills a service that has not exited after 60 s)",
                stop_wait.as_secs()
            ));
        }
    }
    if matches!(action, Action::Start | Action::Restart) {
        if loaded() {
            manager_ok(&["kickstart", &target], long)?;
        } else {
            manager_ok(&["bootstrap", "system", &plist_path()], long)?;
        }
    }
    let done = match action {
        Action::Start => "started",
        Action::Stop => "stopped",
        Action::Restart => "restarted",
    };
    Ok(format!("service {SERVICE_NAME} {done}"))
}

// ---- registration -----------------------------------------------------------

/// How to get the service binary back where repair needs it.
fn reinstall_advice() -> &'static str {
    if cfg!(windows) {
        "reinstall Ferrow with its Setup (repair never copies a binary from anywhere else)"
    } else {
        "run `sudo ferrow service install` from the Ferrow package (repair never copies a binary from anywhere else)"
    }
}

/// The installed service binary: it must exist and pass the binary
/// protection check. Repair never copies one in.
fn installed_binary() -> Result<std::path::PathBuf, String> {
    let exe = protection::binary_path().map_err(|e| e.to_string())?;
    if !exe.try_exists().map_err(|e| e.to_string())? {
        return Err(format!(
            "the installed service binary is missing at {}; {}",
            exe.display(),
            reinstall_advice()
        ));
    }
    protection::validate_binary(&exe)
        .map_err(|e| format!("{}: {e}; {}", exe.display(), reinstall_advice()))?;
    Ok(exe)
}

/// Words of a Windows command line: a quoted word keeps its spaces.
#[cfg(any(windows, test))]
fn split_command_line(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let (mut word, mut quoted, mut any) = (String::new(), false, false);
    for c in line.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            c if c.is_whitespace() && !quoted => {
                if any {
                    words.push(std::mem::take(&mut word));
                    any = false;
                }
            }
            c => {
                word.push(c);
                any = true;
            }
        }
    }
    if any {
        words.push(word);
    }
    words
}

/// Whether a registered command line is exactly the one `install` writes for
/// `exe`: the program, the service flags and the fixed index path.
#[cfg(any(windows, test))]
fn registration_matches(words: &[String], exe: &Path, index: &Path) -> bool {
    let same = |a: &str, b: &Path| a.eq_ignore_ascii_case(&b.to_string_lossy());
    let at = |flag: &str| words.iter().position(|w| w == flag);
    words.first().is_some_and(|program| same(program, exe))
        && at("--run-as-service").is_some()
        && at("--service-endpoint").is_some()
        && at("--index")
            .and_then(|i| words.get(i + 1))
            .is_some_and(|value| same(value, index))
}

/// The `--root` values of a registered command line (kept on re-register).
#[cfg(any(windows, test))]
fn registered_roots(words: &[String]) -> Vec<PathBuf> {
    words
        .windows(2)
        .filter(|pair| pair[0] == "--root")
        .map(|pair| PathBuf::from(&pair[1]))
        .collect()
}

/// What checking (or repairing) the registration found.
#[derive(Debug, PartialEq, Eq)]
pub enum Registration {
    /// Registered exactly for the installed binary.
    Fine(String),
    /// Missing or pointing elsewhere (only when not asked to fix it).
    Needed(String),
    /// Missing or pointing elsewhere; now restored.
    Fixed(String),
}

/// Check the registration without changing anything.
pub fn check_registration() -> Result<Registration, String> {
    registration(false)
}

/// Restore the registration from the fixed installed path when it is missing
/// or points elsewhere. A service whose binary is still installed is
/// re-registered; one whose binary is gone cannot be repaired.
pub fn repair_registration() -> Result<Registration, String> {
    registration(true)
}

#[cfg(windows)]
fn registration(fix: bool) -> Result<Registration, String> {
    use windows_service::service::{ServiceAccess, ServiceStartType, ServiceType};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    use windows_sys::Win32::Foundation::ERROR_SERVICE_DOES_NOT_EXIST;
    let exe = installed_binary()?;
    let index = crate::default_service_index_path().map_err(|e| e.to_string())?;
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .map_err(|e| e.to_string())?;
    let service = match manager.open_service(
        SERVICE_NAME,
        ServiceAccess::QUERY_CONFIG | ServiceAccess::CHANGE_CONFIG,
    ) {
        Ok(service) => service,
        Err(windows_service::Error::Winapi(e))
            if e.raw_os_error() == Some(ERROR_SERVICE_DOES_NOT_EXIST as i32) =>
        {
            let text = format!(
                "the service was not registered; registering it from {} (every local fixed drive)",
                exe.display()
            );
            if !fix {
                return Ok(Registration::Needed(text));
            }
            let info = crate::install::service_info(exe.clone(), index, &[])?;
            let service = manager
                .create_service(&info, ServiceAccess::CHANGE_CONFIG)
                .map_err(|e| e.to_string())?;
            crate::install::configure(&service)?;
            return Ok(Registration::Fixed(text));
        }
        Err(e) => return Err(e.to_string()),
    };
    let config = service.query_config().map_err(|e| e.to_string())?;
    let words = split_command_line(&config.executable_path.to_string_lossy());
    let system = config
        .account_name
        .as_ref()
        .is_none_or(|a| a.to_string_lossy().eq_ignore_ascii_case("LocalSystem"));
    if registration_matches(&words, &exe, &index)
        && config.start_type == ServiceStartType::AutoStart
        && config.service_type == ServiceType::OWN_PROCESS
        && system
    {
        return Ok(Registration::Fine(format!(
            "registered from {}",
            exe.display()
        )));
    }
    let text = format!(
        "registration pointed at {} (it was {})",
        exe.display(),
        words.first().map_or("empty", String::as_str)
    );
    if !fix {
        return Ok(Registration::Needed(text));
    }
    let mut info = crate::install::service_info(exe.clone(), index, &registered_roots(&words))?;
    info.account_name = Some("LocalSystem".into());
    service.change_config(&info).map_err(|e| e.to_string())?;
    crate::install::configure(&service)?;
    Ok(Registration::Fixed(text))
}

/// How systemd sees the unit.
#[cfg(any(target_os = "linux", test))]
#[derive(Debug, PartialEq, Eq)]
enum Enablement {
    Enabled,
    Disabled,
    Masked,
}

/// `systemctl is-enabled` output. Only a unit we can enable ourselves is
/// "disabled": a masked one is the administrator's decision.
#[cfg(any(target_os = "linux", test))]
fn classify_is_enabled(stdout: &str) -> Enablement {
    match stdout.trim() {
        "enabled" | "enabled-runtime" => Enablement::Enabled,
        "masked" | "masked-runtime" => Enablement::Masked,
        _ => Enablement::Disabled,
    }
}

/// The systemd registration, with the machine behind closures so the decision
/// is testable: the unit text must be current *and* the unit enabled.
#[cfg(any(target_os = "linux", test))]
struct Systemd<'a> {
    /// The unit file is a safe file with exactly the expected text.
    current: bool,
    /// `systemctl is-enabled`'s output.
    is_enabled: &'a dyn Fn() -> Result<String, String>,
    /// Write the unit (refuses an unsafe existing definition).
    write: &'a dyn Fn() -> Result<(), String>,
    /// Run `systemctl <args>`.
    systemctl: &'a dyn Fn(&[&str]) -> Result<(), String>,
}

#[cfg(any(target_os = "linux", test))]
fn systemd_registration(
    unit: &Systemd<'_>,
    path: &str,
    exe: &Path,
    fix: bool,
) -> Result<Registration, String> {
    let masked = || {
        format!(
            "{SERVICE_NAME} is masked; an administrator decided that. If it is not wanted, run `sudo systemctl unmask {SERVICE_NAME}` and repair again"
        )
    };
    let state = classify_is_enabled(&(unit.is_enabled)()?);
    if state == Enablement::Masked {
        return Err(masked());
    }
    if unit.current && state == Enablement::Enabled {
        return Ok(Registration::Fine(format!("{path} runs {}", exe.display())));
    }
    let note = if unit.current {
        format!("{path} is current but {SERVICE_NAME} is not enabled")
    } else {
        format!("{path} must be rewritten to run {}", exe.display())
    };
    if !fix {
        return Ok(Registration::Needed(note));
    }
    if !unit.current {
        (unit.write)()?;
        (unit.systemctl)(&["daemon-reload"])?;
    }
    (unit.systemctl)(&["enable", SERVICE_NAME])?;
    Ok(Registration::Fixed(if unit.current {
        format!("enabled {SERVICE_NAME}")
    } else {
        format!("rewrote {path} to run {} and enabled it", exe.display())
    }))
}

#[cfg(target_os = "linux")]
fn registration(fix: bool) -> Result<Registration, String> {
    use crate::install::{UNIT_PATH, command_within, run_cmd, systemd_unit};
    let exe = installed_binary()?;
    let text = systemd_unit();
    // Equal text only counts from a root-owned, single-link regular file that
    // is not a symbolic link and has no extra ACL: the same policy `write` uses.
    let current = crate::definition::is_current(Path::new(UNIT_PATH), &text, 0);
    let is_enabled = || {
        // Disabled units exit non-zero; the text is what matters.
        command_within(
            "systemctl",
            &["is-enabled", SERVICE_NAME],
            Duration::from_secs(10),
        )
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let write = || {
        crate::definition::write(Path::new(UNIT_PATH), &text)
            .map_err(|e| format!("{UNIT_PATH}: {e}"))
    };
    let systemctl = |args: &[&str]| run_cmd("systemctl", args);
    systemd_registration(
        &Systemd {
            current,
            is_enabled: &is_enabled,
            write: &write,
            systemctl: &systemctl,
        },
        UNIT_PATH,
        &exe,
        fix,
    )
}

#[cfg(target_os = "macos")]
fn registration(fix: bool) -> Result<Registration, String> {
    let exe = installed_binary()?;
    let (path, text) = (
        crate::install::plist_path(),
        crate::install::launchd_plist(),
    );
    // Equal text only counts from a root-owned, single-link regular file that
    // is not a symbolic link and has no extra ACL: the same policy `write` uses.
    if crate::definition::is_current(Path::new(&path), &text, 0) {
        return Ok(Registration::Fine(format!("{path} runs {}", exe.display())));
    }
    let note = format!("{path} must be rewritten to run {}", exe.display());
    if !fix {
        return Ok(Registration::Needed(note));
    }
    // `definition::write` refuses an unsafe existing definition.
    crate::definition::write(Path::new(&path), &text).map_err(|e| format!("{path}: {e}"))?;
    Ok(Registration::Fixed(format!(
        "rewrote {path} to run {}",
        exe.display()
    )))
}

// ---- state folder and index -------------------------------------------------

/// Rename `path` to `<name>.bad-<time>` (a numbered variant when that exists)
/// in the same folder. The file is never deleted or overwritten: the new name
/// is made with a hard link, which fails when the name exists, and only then
/// is the old name removed.
pub fn move_aside(path: &Path, now_ms: u64) -> io::Result<std::path::PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("no file name"))?
        .to_string_lossy();
    let base = format!("{name}.bad-{}", ferrow_ipc::health::compact_utc(now_ms));
    let mut n = 1;
    loop {
        let target = if n == 1 {
            path.with_file_name(&base)
        } else {
            path.with_file_name(format!("{base}-{n}"))
        };
        match std::fs::hard_link(path, &target) {
            Ok(()) => {
                if let Err(e) = std::fs::remove_file(path) {
                    // Two names for one file: take back our new one.
                    let _ = std::fs::remove_file(&target);
                    return Err(e);
                }
                return Ok(target);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => n += 1,
            Err(e) => return Err(e),
        }
    }
}

/// The protection rules for the state folder and the files in it.
pub struct Rules {
    /// Validates the folder; production also creates it when missing.
    pub directory: fn(&Path) -> io::Result<()>,
    pub file: fn(&Path) -> io::Result<()>,
}

pub const PROTECTION: Rules = Rules {
    directory: protection::state_directory,
    file: protection::state_file,
};

/// What the state folder check found.
#[derive(Debug, PartialEq, Eq)]
pub struct StateCheck {
    /// Nothing needs repair (or, when fixing, nothing is left).
    pub clean: bool,
    pub text: String,
    /// What was found beyond the folder itself (moved logs, an `access.json`
    /// that fails its check), also contained in `text`.
    pub notes: Vec<String>,
}

/// The state folder `dir` and the files in it. The folder must pass the
/// protection check (it cannot be repaired automatically; a missing one is
/// created only when `fix`). A log file that fails is moved aside when `fix`
/// (so logging resumes); `access.json` is only reported (the service already
/// ignores one that fails, which means administrators only).
pub fn state_folder_in(
    dir: &Path,
    now_ms: u64,
    fix: bool,
    rules: &Rules,
) -> Result<StateCheck, String> {
    let exists = dir.try_exists().unwrap_or(false);
    if !exists && !fix {
        return Ok(StateCheck {
            clean: false,
            text: format!("{} does not exist yet", dir.display()),
            notes: Vec::new(),
        });
    }
    (rules.directory)(dir).map_err(|e| {
        format!(
            "{}: {e}; its protection cannot be repaired automatically (it must be owned by SYSTEM/root with only administrator access)",
            dir.display()
        )
    })?;
    let (mut clean, mut notes) = (true, Vec::new());
    for name in [eventlog::FILE_NAME, eventlog::ROTATED_NAME] {
        let path = dir.join(name);
        if path.try_exists().unwrap_or(false)
            && let Err(e) = (rules.file)(&path)
        {
            if fix {
                let to = move_aside(&path, now_ms).map_err(|m| format!("{name}: {e}; and {m}"))?;
                notes.push(format!(
                    "{name} failed its check ({e}); moved to {}",
                    to.display()
                ));
            } else {
                clean = false;
                notes.push(format!("{name} fails its check ({e})"));
            }
        }
    }
    let policy = dir.join(access::FILE_NAME);
    if policy.try_exists().unwrap_or(false)
        && let Err(e) = (rules.file)(&policy)
    {
        notes.push(format!(
            "{} fails its check ({e}); the service ignores it (administrators only)",
            access::FILE_NAME
        ));
    }
    let mut text = format!("{} is protected", dir.display());
    for note in &notes {
        text.push_str("; ");
        text.push_str(note);
    }
    Ok(StateCheck { clean, text, notes })
}

/// The service's state folder, checked (and with `fix` repaired) with the
/// real protection code.
pub fn check_state_folder(now_ms: u64, fix: bool) -> Result<StateCheck, String> {
    let index = crate::default_service_index_path().map_err(|e| e.to_string())?;
    let dir = index.parent().ok_or("the index has no folder")?;
    state_folder_in(dir, now_ms, fix, &PROTECTION)
}

#[derive(Debug, PartialEq, Eq)]
pub enum IndexCheck {
    /// No saved index: the service builds one.
    Missing,
    /// The index loads (what it holds).
    Loads(String),
    /// It was damaged; moved here so the service rebuilds it.
    MovedAside { to: std::path::PathBuf, why: String },
}

/// Why a saved index did not load.
#[derive(Debug, PartialEq, Eq)]
pub enum IndexProblem {
    /// The file's content is bad (wrong magic, truncated, checksum, corrupt):
    /// safe to move aside and rebuild.
    Damaged(String),
    /// Something else (a read error, a version this build does not read, a
    /// protection failure): the file may be fine, so it is left alone.
    Unusable(String),
}

/// Load-check the saved index with `load`. Only a damaged one is moved aside
/// (never deleted); anything else stops the repair with the reason. When even
/// the move fails the file stays where it is.
pub fn check_index(
    path: &Path,
    now_ms: u64,
    load: impl FnOnce(&Path) -> Result<String, IndexProblem>,
) -> Result<IndexCheck, String> {
    check_index_with(path, now_ms, load, move_aside)
}

pub fn check_index_with(
    path: &Path,
    now_ms: u64,
    load: impl FnOnce(&Path) -> Result<String, IndexProblem>,
    mover: impl FnOnce(&Path, u64) -> io::Result<std::path::PathBuf>,
) -> Result<IndexCheck, String> {
    if !path
        .try_exists()
        .map_err(|e| format!("{}: {e}", path.display()))?
    {
        return Ok(IndexCheck::Missing);
    }
    match load(path) {
        Ok(what) => Ok(IndexCheck::Loads(what)),
        Err(IndexProblem::Unusable(why)) => Err(format!(
            "the saved index cannot be checked ({why}); it was not moved. Fix the cause, or move it aside yourself, and run repair again"
        )),
        Err(IndexProblem::Damaged(why)) => match mover(path, now_ms) {
            Ok(to) => Ok(IndexCheck::MovedAside { to, why }),
            Err(e) => Err(format!(
                "the saved index is damaged ({why}) and could not be moved aside ({e}); it was left as it is"
            )),
        },
    }
}

/// Check an index file without decoding it (header, length and the whole-file
/// checksum over reads of at most `VERIFY_BUFFER`): a full load would need
/// memory for a second copy of the index next to the running service's own.
pub fn load_index_file(path: &Path) -> Result<String, IndexProblem> {
    use ferrow_index::LoadError;
    ferrow_index::Index::verify_file(path)
        .map(|v| format!("{} entries, checksum ok", v.entries))
        .map_err(|e| match e {
            LoadError::WrongMagic
            | LoadError::Truncated
            | LoadError::ChecksumMismatch
            | LoadError::Corrupt(_) => IndexProblem::Damaged(e.to_string()),
            LoadError::Io(_) | LoadError::UnsupportedVersion(_) => {
                IndexProblem::Unusable(e.to_string())
            }
        })
}

/// What the service does at start: the protection check, then the load.
pub fn load_index(path: &Path) -> Result<String, IndexProblem> {
    protection::state_file(path).map_err(|e| IndexProblem::Unusable(format!("protection: {e}")))?;
    load_index_file(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("ferrow-control-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn damaged(_: &Path) -> Result<String, IndexProblem> {
        Err(IndexProblem::Damaged("bad magic".into()))
    }

    #[test]
    fn a_damaged_index_is_moved_aside_and_never_deleted() {
        let dir = dir("aside");
        let index = dir.join("index.bin");
        std::fs::write(&index, b"precious but unreadable").unwrap();
        let outcome = check_index(&index, 1_709_210_096_000, damaged).unwrap();
        let IndexCheck::MovedAside { to, why } = outcome else {
            panic!("{outcome:?}")
        };
        assert_eq!(why, "bad magic");
        assert_eq!(to.file_name().unwrap(), "index.bin.bad-20240229T123456Z");
        assert!(
            !index.exists(),
            "the service must find no index and rebuild"
        );
        assert_eq!(std::fs::read(&to).unwrap(), b"precious but unreadable");
        // A second failure in the same second never overwrites the first.
        std::fs::write(&index, b"second").unwrap();
        let IndexCheck::MovedAside { to: second, .. } =
            check_index(&index, 1_709_210_096_000, damaged).unwrap()
        else {
            panic!()
        };
        assert_ne!(second, to);
        assert_eq!(std::fs::read(&to).unwrap(), b"precious but unreadable");
        assert_eq!(std::fs::read(&second).unwrap(), b"second");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_index_that_may_be_fine_is_reported_and_never_moved() {
        let dir = dir("unusable");
        let index = dir.join("index.bin");
        std::fs::write(&index, b"maybe a newer version, maybe a busy disk").unwrap();
        let error = check_index(&index, 0, |_| {
            Err(IndexProblem::Unusable(
                "reading index: access denied".into(),
            ))
        })
        .unwrap_err();
        assert!(error.contains("was not moved"), "{error}");
        assert!(error.contains("access denied"), "{error}");
        assert_eq!(
            std::fs::read(&index).unwrap(),
            b"maybe a newer version, maybe a busy disk"
        );
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            1,
            "nothing renamed"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_move_leaves_the_file_where_it_is_and_says_so() {
        let dir = dir("nomove");
        let index = dir.join("index.bin");
        std::fs::write(&index, b"damaged").unwrap();
        let error = check_index_with(&index, 0, damaged, |_, _| {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "locked"))
        })
        .unwrap_err();
        assert!(
            error.contains("left as it is") && error.contains("locked"),
            "{error}"
        );
        assert_eq!(std::fs::read(&index).unwrap(), b"damaged");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_good_or_missing_index_is_left_alone() {
        let dir = dir("fine");
        let index = dir.join("index.bin");
        assert_eq!(
            check_index(&index, 0, |_| panic!("no file to load")),
            Ok(IndexCheck::Missing)
        );
        // A real index, written and read by the real code.
        let builder = ferrow_index::IndexBuilder::new();
        let root = builder.add_root(b"root", ferrow_index::flags::DIR);
        builder.add(root, b"a.txt", 0);
        builder.freeze().save(&index).unwrap();
        let before = std::fs::read(&index).unwrap();
        let checked = check_index(&index, 0, load_index_file);
        assert!(
            matches!(&checked, Ok(IndexCheck::Loads(what)) if what.ends_with("checksum ok")),
            "{checked:?}"
        );
        assert_eq!(std::fs::read(&index).unwrap(), before);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_real_loader_tells_damage_from_other_failures() {
        let dir = dir("real");
        let index = dir.join("index.bin");
        std::fs::write(&index, b"this is not an index").unwrap();
        assert!(matches!(
            load_index_file(&index),
            Err(IndexProblem::Damaged(_))
        ));
        // A real index cut short is damaged too; a missing file is an I/O error.
        let builder = ferrow_index::IndexBuilder::new();
        let root = builder.add_root(b"root", ferrow_index::flags::DIR);
        builder.add(root, b"a.txt", 0);
        builder.freeze().save(&index).unwrap();
        let bytes = std::fs::read(&index).unwrap();
        std::fs::write(&index, &bytes[..bytes.len() / 2]).unwrap();
        assert!(matches!(
            load_index_file(&index),
            Err(IndexProblem::Damaged(_))
        ));
        assert!(matches!(
            load_index_file(&dir.join("absent.bin")),
            Err(IndexProblem::Unusable(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn accept(_: &Path) -> io::Result<()> {
        Ok(())
    }
    fn refuse(_: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unprotected",
        ))
    }
    /// Refuses only files named like logs and the policy; the folder passes.
    fn refuse_files(path: &Path) -> io::Result<()> {
        if path.file_name().is_some_and(|n| n == "index.bin") {
            Ok(())
        } else {
            refuse(path)
        }
    }

    #[test]
    fn a_state_folder_that_fails_its_protection_is_refused_untouched() {
        let dir = dir("folder-bad");
        std::fs::write(dir.join(eventlog::FILE_NAME), b"log").unwrap();
        let rules = Rules {
            directory: refuse,
            file: accept,
        };
        let error = state_folder_in(&dir, 0, true, &rules).unwrap_err();
        assert!(
            error.contains("cannot be repaired automatically"),
            "{error}"
        );
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "nothing moved");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_log_that_fails_its_check_is_moved_aside_and_the_policy_is_only_reported() {
        let dir = dir("folder-logs");
        std::fs::write(dir.join(eventlog::FILE_NAME), b"log lines").unwrap();
        std::fs::write(dir.join(access::FILE_NAME), b"{}").unwrap();
        let rules = Rules {
            directory: accept,
            file: refuse_files,
        };
        // Checking only: reports, changes nothing.
        let check = state_folder_in(&dir, 1_709_210_096_000, false, &rules).unwrap();
        assert!(!check.clean, "{check:?}");
        assert!(dir.join(eventlog::FILE_NAME).exists());
        // Fixing: the log is kept under a new name, the policy stays.
        let fixed = state_folder_in(&dir, 1_709_210_096_000, true, &rules).unwrap();
        assert!(fixed.clean && fixed.text.contains("moved to"), "{fixed:?}");
        assert!(!dir.join(eventlog::FILE_NAME).exists());
        let moved = dir.join("service.log.bad-20240229T123456Z");
        assert_eq!(std::fs::read(&moved).unwrap(), b"log lines");
        assert_eq!(std::fs::read(dir.join(access::FILE_NAME)).unwrap(), b"{}");
        assert!(
            fixed.text.contains("the service ignores it"),
            "{}",
            fixed.text
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_state_folder_is_only_created_when_fixing() {
        let base = dir("folder-missing");
        let state = base.join("state");
        let rules = Rules {
            directory: |p| std::fs::create_dir_all(p),
            file: accept,
        };
        let check = state_folder_in(&state, 0, false, &rules).unwrap();
        assert!(!check.clean && !state.exists(), "{check:?}");
        assert!(state_folder_in(&state, 0, true, &rules).unwrap().clean);
        assert!(state.exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn waits_are_bounded_and_stop_at_the_first_success() {
        let mut calls = 0;
        assert!(wait_until(Duration::from_secs(5), Duration::ZERO, || {
            calls += 1;
            calls == 3
        }));
        assert_eq!(calls, 3);
        let started = Instant::now();
        assert!(!wait_until(
            Duration::from_millis(60),
            Duration::from_millis(5),
            || false
        ));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn a_registered_command_line_is_judged_against_the_fixed_path() {
        let exe = Path::new(r"C:\Program Files\Ferrow\service\ferrowd.exe");
        let index = Path::new(r"C:\ProgramData\Ferrow\index.bin");
        let line = r#""C:\Program Files\Ferrow\service\ferrowd.exe" --run-as-service --service-endpoint --index C:\ProgramData\Ferrow\index.bin --root "D:\My Files""#;
        let words = split_command_line(line);
        assert_eq!(words.len(), 7);
        assert!(registration_matches(&words, exe, index));
        assert_eq!(registered_roots(&words), [PathBuf::from(r"D:\My Files")]);
        // Another program, a missing flag or another index: not the installed service.
        let other = split_command_line(
            &line.replace(r"C:\Program Files\Ferrow\service", r"C:\Users\x\Downloads"),
        );
        assert!(!registration_matches(&other, exe, index));
        assert!(!registration_matches(&words[..2], exe, index));
        let elsewhere = split_command_line(&line.replace(r"C:\ProgramData", r"C:\Temp"));
        assert!(!registration_matches(&elsewhere, exe, index));
        assert!(!registration_matches(&[], exe, index));
    }

    #[test]
    fn is_enabled_output_is_classified_and_masking_is_left_to_the_administrator() {
        for (text, want) in [
            ("enabled\n", Enablement::Enabled),
            ("enabled-runtime", Enablement::Enabled),
            ("disabled\n", Enablement::Disabled),
            ("not-found", Enablement::Disabled),
            ("linked", Enablement::Disabled),
            ("", Enablement::Disabled),
            ("masked\n", Enablement::Masked),
            ("masked-runtime", Enablement::Masked),
        ] {
            assert_eq!(classify_is_enabled(text), want, "{text:?}");
        }
    }

    /// Runs the systemd decision with the machine replaced by closures and
    /// returns the outcome and the commands that would have run.
    fn systemd(
        current: bool,
        is_enabled: &str,
        fix: bool,
    ) -> (Result<Registration, String>, Vec<String>) {
        use std::cell::RefCell;
        let calls = RefCell::new(Vec::new());
        let state = || Ok(is_enabled.to_string());
        let write = || {
            calls.borrow_mut().push("write unit".to_string());
            Ok(())
        };
        let systemctl = |args: &[&str]| {
            calls
                .borrow_mut()
                .push(format!("systemctl {}", args.join(" ")));
            Ok(())
        };
        let result = systemd_registration(
            &Systemd {
                current,
                is_enabled: &state,
                write: &write,
                systemctl: &systemctl,
            },
            "/etc/systemd/system/ferrowd.service",
            Path::new("/usr/local/libexec/ferrow/ferrowd"),
            fix,
        );
        let calls = calls.into_inner();
        (result, calls)
    }

    #[test]
    fn a_current_unit_that_is_not_enabled_is_not_fine() {
        // Current and enabled: fine, nothing run.
        let (result, calls) = systemd(true, "enabled", true);
        assert!(matches!(result, Ok(Registration::Fine(_))), "{result:?}");
        assert!(calls.is_empty());
        // Current but disabled: the check says repair is needed, and does nothing.
        let (result, calls) = systemd(true, "disabled", false);
        assert!(
            matches!(&result, Ok(Registration::Needed(t)) if t.contains("not enabled")),
            "{result:?}"
        );
        assert!(calls.is_empty());
        // Repair enables it, without rewriting the unit.
        let (result, calls) = systemd(true, "disabled", true);
        assert!(matches!(result, Ok(Registration::Fixed(_))), "{result:?}");
        assert_eq!(calls, ["systemctl enable ferrowd"]);
        // A unit that must be rewritten: write, reload, then enable.
        let (result, calls) = systemd(false, "not-found", true);
        assert!(matches!(result, Ok(Registration::Fixed(_))), "{result:?}");
        assert_eq!(
            calls,
            [
                "write unit",
                "systemctl daemon-reload",
                "systemctl enable ferrowd"
            ]
        );
        let (result, calls) = systemd(false, "not-found", false);
        assert!(matches!(result, Ok(Registration::Needed(_))), "{result:?}");
        assert!(calls.is_empty());
    }

    #[test]
    fn a_masked_unit_is_reported_and_never_unmasked_silently() {
        for fix in [false, true] {
            for current in [false, true] {
                let (result, calls) = systemd(current, "masked", fix);
                let error = result.unwrap_err();
                assert!(error.contains("sudo systemctl unmask ferrowd"), "{error}");
                assert!(
                    calls.is_empty(),
                    "nothing runs for a masked unit: {calls:?}"
                );
            }
        }
    }

    #[test]
    fn the_state_folder_notes_include_a_policy_file_that_fails_its_check() {
        let dir = dir("folder-notes");
        std::fs::write(dir.join(access::FILE_NAME), b"{}").unwrap();
        let rules = Rules {
            directory: accept,
            file: refuse,
        };
        let check = state_folder_in(&dir, 0, false, &rules).unwrap();
        assert!(
            check.clean,
            "the policy file does not make the folder unclean"
        );
        assert_eq!(check.notes.len(), 1, "{check:?}");
        assert!(check.notes[0].contains("access.json"), "{check:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn answers_falls_back_to_status_for_a_service_older_than_health() {
        use ferrow_ipc::{
            Endpoint, Listener, Response, read_message, serve_stream, server_handshake,
        };
        let endpoint = Endpoint::named(&format!("ctl-old-{}", std::process::id()));
        let listener = Listener::bind(&endpoint).unwrap();
        let server = std::thread::spawn(move || {
            // First connection (Health): an old service drops it without a reply.
            let (mut stream, _) = listener.accept().unwrap();
            server_handshake(&mut stream).unwrap();
            let _ = read_message(&mut stream);
            drop(stream);
            // Second connection (Status): it answers.
            let (stream, _) = listener.accept().unwrap();
            let _ = serve_stream(stream, |_| {
                vec![Response::Status {
                    entries: 1,
                    mode: "old".into(),
                    report: String::new(),
                }]
            });
        });
        assert!(
            answers(&endpoint),
            "an older service that answers Status is up"
        );
        server.join().unwrap();
    }

    #[test]
    fn a_service_that_answers_nothing_is_not_up_within_the_budget() {
        use ferrow_ipc::{Endpoint, Listener, read_message, server_handshake};
        let endpoint = Endpoint::named(&format!("ctl-mute-{}", std::process::id()));
        let listener = Listener::bind(&endpoint).unwrap();
        let server = std::thread::spawn(move || {
            // Each connection gets its own thread that reads the request and
            // never answers (longer than the probe's one-second budget).
            let mut handlers = Vec::new();
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                handlers.push(std::thread::spawn(move || {
                    server_handshake(&mut stream).unwrap();
                    let _ = read_message(&mut stream);
                    std::thread::sleep(Duration::from_millis(1500));
                }));
            }
            for handler in handlers {
                handler.join().unwrap();
            }
        });
        let started = Instant::now();
        assert!(!answers(&endpoint));
        assert!(
            started.elapsed() < Duration::from_secs(6),
            "{:?}",
            started.elapsed()
        );
        server.join().unwrap();
    }
}
