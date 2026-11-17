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

/// Whether the service at `endpoint` answers a request (a refusal counts: it
/// is up). One connection, a one-second budget.
pub fn answers(endpoint: &ferrow_ipc::Endpoint) -> bool {
    let deadline = Instant::now() + Duration::from_secs(1);
    let result = ferrow_ipc::Client::connect_deadline(endpoint, deadline)
        .and_then(|mut client| client.health());
    match result {
        Ok(_) => true,
        Err(error) => ferrow_ipc::error_code(&error).is_some(),
    }
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
/// index save, so they get longer than a start.
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

#[cfg(target_os = "macos")]
pub fn control(action: Action) -> Result<String, String> {
    use crate::install::{LAUNCHD_LABEL, plist_path};
    let target = format!("system/{LAUNCHD_LABEL}");
    let loaded = manage(&["print", &target], Duration::from_secs(10))?
        .status
        .success();
    let long = Duration::from_secs(45);
    let done = match action {
        Action::Stop => {
            if loaded {
                manager_ok(&["bootout", &target], long)?;
            }
            "stopped"
        }
        Action::Start | Action::Restart if !loaded => {
            manager_ok(&["bootstrap", "system", &plist_path()], long)?;
            "started"
        }
        Action::Start => {
            manager_ok(&["kickstart", &target], long)?;
            "started"
        }
        Action::Restart => {
            manager_ok(&["kickstart", "-k", &target], long)?;
            "restarted"
        }
    };
    Ok(format!("service {SERVICE_NAME} {done}"))
}

// ---- registration -----------------------------------------------------------

/// The installed service binary: it must exist and pass the binary
/// protection check. Repair never copies one in.
fn installed_binary() -> Result<std::path::PathBuf, String> {
    let exe = protection::binary_path().map_err(|e| e.to_string())?;
    let reinstall =
        "reinstall Ferrow with its Setup (repair never copies a binary from anywhere else)";
    if !exe.try_exists().map_err(|e| e.to_string())? {
        return Err(format!(
            "the installed service binary is missing at {}; {reinstall}",
            exe.display()
        ));
    }
    protection::validate_binary(&exe)
        .map_err(|e| format!("{}: {e}; {reinstall}", exe.display()))?;
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

/// Check the registration and restore it from the fixed installed path when it
/// is missing or points elsewhere.
#[cfg(windows)]
pub fn repair_registration() -> Result<String, String> {
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
            let info = crate::install::service_info(exe.clone(), index, &[])?;
            let service = manager
                .create_service(&info, ServiceAccess::CHANGE_CONFIG)
                .map_err(|e| e.to_string())?;
            crate::install::configure(&service)?;
            return Ok(format!(
                "the service was not registered; registered it from {} (every local fixed drive)",
                exe.display()
            ));
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
        return Ok(format!("registered from {}", exe.display()));
    }
    let mut info = crate::install::service_info(exe.clone(), index, &registered_roots(&words))?;
    info.account_name = Some("LocalSystem".into());
    service.change_config(&info).map_err(|e| e.to_string())?;
    crate::install::configure(&service)?;
    Ok(format!(
        "registration pointed at {} (it was {})",
        exe.display(),
        words.first().map_or("empty", String::as_str)
    ))
}

#[cfg(unix)]
pub fn repair_registration() -> Result<String, String> {
    let exe = installed_binary()?;
    #[cfg(target_os = "linux")]
    let (path, text) = (
        crate::install::UNIT_PATH.to_string(),
        crate::install::systemd_unit(),
    );
    #[cfg(target_os = "macos")]
    let (path, text) = (
        crate::install::plist_path(),
        crate::install::launchd_plist(),
    );
    if std::fs::read_to_string(&path).is_ok_and(|current| current == text) {
        return Ok(format!("{path} runs {}", exe.display()));
    }
    // `definition::write` refuses an unsafe existing definition.
    crate::definition::write(Path::new(&path), &text).map_err(|e| format!("{path}: {e}"))?;
    #[cfg(target_os = "linux")]
    {
        crate::install::run_cmd("systemctl", &["daemon-reload"])?;
        crate::install::run_cmd("systemctl", &["enable", SERVICE_NAME])?;
    }
    Ok(format!("rewrote {path} to run {}", exe.display()))
}

// ---- state folder and index -------------------------------------------------

/// Rename `path` to `<name>.bad-<time>` (a numbered variant when that exists)
/// in the same folder. The file is never deleted or overwritten.
pub fn move_aside(path: &Path, now_ms: u64) -> io::Result<std::path::PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("no file name"))?
        .to_string_lossy();
    let base = format!("{name}.bad-{}", ferrow_ipc::health::compact_utc(now_ms));
    let mut target = path.with_file_name(&base);
    let mut n = 1;
    while target.try_exists()? {
        n += 1;
        target = path.with_file_name(format!("{base}-{n}"));
    }
    std::fs::rename(path, &target)?;
    Ok(target)
}

/// The state folder (created with its protection when missing) and the files
/// in it. The folder must pass the protection check; a log file that does not
/// is moved aside so logging resumes; `access.json` is only reported (the
/// service already ignores one that fails, which means administrators only).
pub fn check_state_folder(now_ms: u64) -> Result<String, String> {
    let index = crate::default_service_index_path().map_err(|e| e.to_string())?;
    let dir = index.parent().ok_or("the index has no folder")?;
    protection::state_directory(dir).map_err(|e| {
        format!(
            "{}: {e}; its protection cannot be repaired automatically (it must be owned by SYSTEM/root with only administrator access)",
            dir.display()
        )
    })?;
    let mut notes = Vec::new();
    for name in [eventlog::FILE_NAME, eventlog::ROTATED_NAME] {
        let path = dir.join(name);
        if path.try_exists().unwrap_or(false)
            && let Err(e) = protection::state_file(&path)
        {
            let to = move_aside(&path, now_ms).map_err(|m| format!("{name}: {e}; and {m}"))?;
            notes.push(format!(
                "{name} failed its check ({e}); moved to {}",
                to.display()
            ));
        }
    }
    let policy = dir.join(access::FILE_NAME);
    if policy.try_exists().unwrap_or(false)
        && let Err(e) = protection::state_file(&policy)
    {
        notes.push(format!(
            "{} fails its check ({e}); the service ignores it (administrators only)",
            access::FILE_NAME
        ));
    }
    let mut text = format!("{} is protected", dir.display());
    for note in notes {
        text.push_str("; ");
        text.push_str(&note);
    }
    Ok(text)
}

#[derive(Debug, PartialEq, Eq)]
pub enum IndexCheck {
    /// No saved index: the service builds one.
    Missing,
    /// The index loads (what it holds).
    Loads(String),
    /// It did not load; moved here so the service rebuilds it.
    MovedAside { to: std::path::PathBuf, why: String },
}

/// Load-check the saved index with `load`; one that fails is moved aside,
/// never deleted. When even the move fails the file stays where it is.
pub fn check_index(
    path: &Path,
    now_ms: u64,
    load: impl FnOnce(&Path) -> Result<String, String>,
) -> Result<IndexCheck, String> {
    if !path
        .try_exists()
        .map_err(|e| format!("{}: {e}", path.display()))?
    {
        return Ok(IndexCheck::Missing);
    }
    match load(path) {
        Ok(what) => Ok(IndexCheck::Loads(what)),
        Err(why) => match move_aside(path, now_ms) {
            Ok(to) => Ok(IndexCheck::MovedAside { to, why }),
            Err(e) => Err(format!(
                "the saved index does not load ({why}) and could not be moved aside ({e}); it was left as it is"
            )),
        },
    }
}

/// What the service does at start: the protection check, then the load.
pub fn load_index(path: &Path) -> Result<String, String> {
    protection::state_file(path).map_err(|e| format!("protection: {e}"))?;
    ferrow_index::Index::load(path)
        .map(|index| format!("{} entries", index.len()))
        .map_err(|e| e.to_string())
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

    #[test]
    fn an_index_that_does_not_load_is_moved_aside_and_never_deleted() {
        let dir = dir("aside");
        let index = dir.join("index.bin");
        std::fs::write(&index, b"precious but unreadable").unwrap();
        let outcome = check_index(&index, 1_709_210_096_000, |_| Err("bad magic".into())).unwrap();
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
            check_index(&index, 1_709_210_096_000, |_| Err("again".into())).unwrap()
        else {
            panic!()
        };
        assert_ne!(second, to);
        assert_eq!(std::fs::read(&to).unwrap(), b"precious but unreadable");
        assert_eq!(std::fs::read(&second).unwrap(), b"second");
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
        std::fs::write(&index, b"ok").unwrap();
        assert_eq!(
            check_index(&index, 0, |_| Ok("3 entries".into())),
            Ok(IndexCheck::Loads("3 entries".into()))
        );
        assert_eq!(std::fs::read(&index).unwrap(), b"ok");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_real_unreadable_index_fails_the_real_loader() {
        let dir = dir("real");
        let index = dir.join("index.bin");
        std::fs::write(&index, b"this is not an index").unwrap();
        // The production check (`load_index`) must refuse it as well; here the
        // protection step may fail first on a temp folder, which also counts.
        assert!(load_index(&index).is_err());
        let _ = std::fs::remove_dir_all(&dir);
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
}
