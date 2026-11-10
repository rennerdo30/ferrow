//! `ferrow service install | uninstall` (M2b).
//!
//! Installing a system service needs administrator rights once.
//! When they are missing these functions say exactly what to
//! re-run elevated and never try to elevate themselves.

use std::path::PathBuf;

fn confirm_stopped(
    mut probe: impl FnMut() -> Result<bool, String>,
    mut before_deadline: impl FnMut() -> bool,
    mut park: impl FnMut(),
) -> Result<(), String> {
    loop {
        if probe()? {
            return Ok(());
        }
        if !before_deadline() {
            return Err("timed out waiting for the recorded service instance to stop".into());
        }
        park();
    }
}

/// The service's registered name on every platform.
pub const SERVICE_NAME: &str = "ferrowd";
/// launchd label / plist base name.
#[cfg(target_os = "macos")]
const LAUNCHD_LABEL: &str = "com.github.rennerdo30.ferrowd";

/// The `ferrowd` binary to register: the current executable when
/// it is `ferrowd`, else a sibling of it.
fn ferrowd_exe() -> Result<PathBuf, String> {
    let current = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe_name = if cfg!(windows) {
        "ferrowd.exe"
    } else {
        "ferrowd"
    };
    if current.file_name().is_some_and(|n| n == exe_name) {
        return Ok(current);
    }
    let sibling = current.with_file_name(exe_name);
    if sibling.exists() {
        Ok(sibling)
    } else {
        Err(format!(
            "ferrowd binary not found next to {} (looked for {})",
            current.display(),
            sibling.display()
        ))
    }
}

#[cfg(windows)]
fn admin_hint(action: &str, e: &dyn std::fmt::Display) -> String {
    format!(
        "{action}: {e}\nInstalling or removing the service needs administrator rights once. \
         Re-run `ferrow service {action}` from an elevated terminal. Ferrow never elevates itself."
    )
}

#[cfg(windows)]
pub fn install(roots: &[std::path::PathBuf]) -> Result<String, String> {
    use windows_service::service::{
        ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType, ServiceType,
    };
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    let exe = crate::protection::install_binary(&ferrowd_exe()?).map_err(|e| e.to_string())?;
    let index = crate::default_service_index_path().map_err(|e| admin_hint("install", &e))?;
    if let Some(parent) = index.parent() {
        crate::protection::state_directory(parent).map_err(|e| admin_hint("install", &e))?;
    }
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .map_err(|e| admin_hint("install", &e))?;
    if manager
        .open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS)
        .is_ok()
    {
        return Err(format!(
            "service {SERVICE_NAME} is already installed; uninstall it before installing the protected executable"
        ));
    }
    let info = ServiceInfo {
        name: SERVICE_NAME.into(),
        display_name: "Ferrow index service".into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe,
        launch_arguments: {
            let mut args: Vec<std::ffi::OsString> = vec![
                "--run-as-service".into(),
                "--service-endpoint".into(),
                "--index".into(),
                index.into_os_string(),
            ];
            // No roots: every local fixed drive (see ferrowd's main).
            for root in roots {
                let root = std::path::absolute(root).map_err(|e| e.to_string())?;
                args.push("--root".into());
                args.push(root.into_os_string());
            }
            args
        },
        dependencies: vec![],
        account_name: None, // LocalSystem
        account_password: None,
    };
    let service = manager
        .create_service(&info, ServiceAccess::CHANGE_CONFIG | ServiceAccess::START)
        .map_err(|e| admin_hint("install", &e))?;
    let _ = service
        .set_description("Indexes file names and answers Ferrow searches over a local pipe.");
    // Restart after a crash: 5 s, 15 s, then 60 s; the count resets daily.
    let restart = |seconds| windows_service::service::ServiceAction {
        action_type: windows_service::service::ServiceActionType::Restart,
        delay: std::time::Duration::from_secs(seconds),
    };
    service
        .update_failure_actions(windows_service::service::ServiceFailureActions {
            reset_period: windows_service::service::ServiceFailureResetPeriod::After(
                std::time::Duration::from_secs(86_400),
            ),
            reboot_msg: None,
            command: None,
            actions: Some(vec![restart(5), restart(15), restart(60)]),
        })
        .map_err(|e| admin_hint("install", &e))?;
    Ok(format!(
        "service {SERVICE_NAME} installed (LocalSystem, automatic start). Start it with: sc start {SERVICE_NAME}"
    ))
}

#[cfg(windows)]
pub fn uninstall() -> Result<String, String> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_service::service::{ServiceAccess, ServiceState};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    use windows_sys::Win32::{
        Foundation::{
            ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SERVICE_MARKED_FOR_DELETE, WAIT_OBJECT_0,
        },
        System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, WaitForSingleObject},
    };
    let absent = |error: &windows_service::Error| matches!(error, windows_service::Error::Winapi(e) if e.raw_os_error() == Some(ERROR_SERVICE_DOES_NOT_EXIST as i32));
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|e| admin_hint("uninstall", &e))?;
    let service = match manager.open_service(
        SERVICE_NAME,
        ServiceAccess::STOP | ServiceAccess::DELETE | ServiceAccess::QUERY_STATUS,
    ) {
        Ok(service) => service,
        Err(e) if absent(&e) => return Ok(format!("service {SERVICE_NAME} is not installed")),
        Err(e) => return Err(admin_hint("uninstall", &e)),
    };
    let status = service.query_status().map_err(|e| e.to_string())?;
    let process = if status.current_state == ServiceState::Stopped {
        None
    } else {
        let pid = status
            .process_id
            .ok_or("running service has no process id")?;
        // SAFETY: open the exact SCM-reported process and retain its kernel
        // identity through verification; no process-name or PID-reuse guess.
        let handle = unsafe { OpenProcess(0x00100000 | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error().to_string());
        }
        // SAFETY: OpenProcess returned a uniquely owned valid handle.
        Some(unsafe { OwnedHandle::from_raw_handle(handle) })
    };
    if !matches!(
        status.current_state,
        ServiceState::Stopped | ServiceState::StopPending
    ) {
        service.stop().map_err(|e| admin_hint("uninstall", &e))?;
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    confirm_stopped(
        || {
            let stopped = service
                .query_status()
                .map_err(|e| e.to_string())?
                .current_state
                == ServiceState::Stopped;
            // SAFETY: retained process handle is live; zero wait checks its exit state.
            let exited = process.as_ref().is_none_or(
                |p| unsafe { WaitForSingleObject(p.as_raw_handle(), 0) } == WAIT_OBJECT_0,
            );
            Ok(stopped && exited)
        },
        || std::time::Instant::now() < deadline,
        || std::thread::sleep(std::time::Duration::from_millis(50)),
    )?;
    service.delete().map_err(|e| admin_hint("uninstall", &e))?;
    drop(service);
    confirm_stopped(
        || match manager.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS) {
            Err(e) if absent(&e) => Ok(true),
            Err(windows_service::Error::Winapi(e))
                if e.raw_os_error() == Some(ERROR_SERVICE_MARKED_FOR_DELETE as i32) =>
            {
                Ok(false)
            }
            Err(e) => Err(e.to_string()),
            Ok(_) => Ok(false),
        },
        || std::time::Instant::now() < deadline,
        || std::thread::sleep(std::time::Duration::from_millis(50)),
    )?;
    Ok(format!("service {SERVICE_NAME} stopped and uninstalled"))
}

#[cfg(target_os = "linux")]
const SYSTEMD_TEMPLATE: &str = "[Unit]\nDescription=Ferrow file-name index service\nAfter=local-fs.target\nWants=local-fs.target\n\n[Service]\nType=simple\nExecStart=@FERROWD@ --service-endpoint --index /var/lib/ferrow/index.bin\nRestart=on-failure\nRestartSec=2s\n# Only the capabilities the indexer needs: read every directory\n# (live backends in later milestones use CAP_SYS_ADMIN).\nCapabilityBoundingSet=CAP_DAC_READ_SEARCH CAP_SYS_ADMIN\nAmbientCapabilities=CAP_DAC_READ_SEARCH CAP_SYS_ADMIN\nRuntimeDirectory=ferrow\nStateDirectory=ferrow\nStateDirectoryMode=0700\n# Hardening. ProtectHome stays off: indexing home directories is\n# the point. Writes are confined to the state directory.\nProtectSystem=strict\nReadWritePaths=/var/lib/ferrow\nPrivateTmp=true\nProtectKernelTunables=true\nProtectKernelModules=true\nProtectControlGroups=true\nLockPersonality=true\nRestrictSUIDSGID=true\nRestrictRealtime=true\n\n[Install]\nWantedBy=multi-user.target\n";

#[cfg(target_os = "linux")]
fn systemd_unit() -> String {
    SYSTEMD_TEMPLATE.replace("@FERROWD@", "\"/usr/local/libexec/ferrow/ferrowd\"")
}

#[cfg(target_os = "linux")]
const UNIT_PATH: &str = "/etc/systemd/system/ferrowd.service";

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn command(program: &str, args: &[&str]) -> Result<std::process::Output, String> {
    use std::io::Read;
    let mut child = std::process::Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    eprintln!("ferrow: started {program} PID {}", child.id());
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let out = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .take(64 * 1024)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let err = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr
            .take(64 * 1024)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut timed_out = false;
    let status = loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) => break status,
            None if std::time::Instant::now() >= deadline => {
                timed_out = true;
                child.kill().map_err(|e| e.to_string())?;
                break child.wait().map_err(|e| e.to_string())?;
            }
            None => std::thread::sleep(std::time::Duration::from_millis(20)),
        }
    };
    let stdout = out
        .join()
        .map_err(|_| "manager stdout worker failed")?
        .map_err(|e| e.to_string())?;
    let stderr = err
        .join()
        .map_err(|_| "manager stderr worker failed")?
        .map_err(|e| e.to_string())?;
    if timed_out {
        return Err(format!(
            "{program} timed out; owned PID {} was stopped",
            child.id()
        ));
    }
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run_cmd(program: &str, args: &[&str]) -> Result<(), String> {
    let out = command(program, args)?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn root_hint(action: &str, e: &std::io::Error) -> String {
    format!(
        "{action}: {e}\nInstalling or removing the service needs root once. \
         Re-run with sudo: sudo ferrow service {action}. Ferrow never elevates itself."
    )
}

#[cfg(target_os = "linux")]
pub fn install(roots: &[std::path::PathBuf]) -> Result<String, String> {
    if !roots.is_empty() {
        return Err("choosing index roots at install is only supported on Windows".into());
    }
    let _exe = crate::protection::install_binary(&ferrowd_exe()?).map_err(|e| e.to_string())?;
    crate::definition::write(std::path::Path::new(UNIT_PATH), &systemd_unit())
        .map_err(|e| root_hint("install", &e))?;
    crate::protection::state_directory(
        crate::default_service_index_path()
            .map_err(|e| e.to_string())?
            .parent()
            .unwrap(),
    )
    .map_err(|e| root_hint("install", &e))?;
    run_cmd("systemctl", &["daemon-reload"])?;
    run_cmd("systemctl", &["enable", SERVICE_NAME])?;
    Ok(format!(
        "service {SERVICE_NAME} installed ({UNIT_PATH}, enabled). Start it with: sudo systemctl start {SERVICE_NAME}"
    ))
}

#[cfg(target_os = "linux")]
pub fn uninstall() -> Result<String, String> {
    fn state() -> Result<(bool, u32), String> {
        let output = command(
            "systemctl",
            &[
                "show",
                SERVICE_NAME,
                "--property=LoadState,ActiveState,MainPID",
            ],
        )?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).into());
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let active = text
            .lines()
            .find_map(|line| line.strip_prefix("ActiveState="))
            .ok_or("missing ActiveState")?;
        let pid = text
            .lines()
            .find_map(|line| line.strip_prefix("MainPID="))
            .ok_or("missing MainPID")?
            .parse::<u32>()
            .map_err(|e| e.to_string())?;
        Ok((matches!(active, "inactive" | "failed"), pid))
    }
    fn start_time(pid: u32) -> Result<Option<String>, String> {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => Ok(Some(
                stat.rsplit_once(") ")
                    .and_then(|(_, fields)| fields.split_whitespace().nth(19))
                    .ok_or("malformed process identity")?
                    .into(),
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }
    let (already_stopped, pid) = state()?;
    let identity = if pid == 0 { None } else { start_time(pid)? };
    if !already_stopped || pid != 0 {
        run_cmd("systemctl", &["stop", SERVICE_NAME])?;
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    confirm_stopped(
        || {
            let (stopped, current) = state()?;
            let exited = if let Some(original) = &identity {
                start_time(pid)?.as_ref() != Some(original)
            } else {
                pid == 0
            };
            Ok(stopped && current == 0 && exited)
        },
        || std::time::Instant::now() < deadline,
        || std::thread::sleep(std::time::Duration::from_millis(50)),
    )?;
    if !std::path::Path::new(UNIT_PATH).exists() {
        return Ok(format!("service {SERVICE_NAME} is not installed"));
    }
    run_cmd("systemctl", &["disable", SERVICE_NAME])?;
    std::fs::remove_file(UNIT_PATH).map_err(|e| root_hint("uninstall", &e))?;
    run_cmd("systemctl", &["daemon-reload"])?;
    Ok(format!("service {SERVICE_NAME} stopped and uninstalled"))
}

#[cfg(target_os = "macos")]
fn launchd_plist() -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n\t<key>Label</key>\n\t<string>{LAUNCHD_LABEL}</string>\n\t<key>ProgramArguments</key>\n\t<array>\n\t\t<string>{}</string>\n\t\t<string>--service-endpoint</string>\n\t\t<string>--index</string>\n\t\t<string>/private/var/lib/ferrow/index.bin</string>\n\t</array>\n\t<key>RunAtLoad</key>\n\t<true/>\n\t<key>KeepAlive</key>\n\t<true/>\n\t<key>WorkingDirectory</key>\n\t<string>/private/var/lib/ferrow</string>\n\t<key>StandardOutPath</key>\n\t<string>/var/log/ferrowd.log</string>\n\t<key>StandardErrorPath</key>\n\t<string>/var/log/ferrowd.log</string>\n</dict>\n</plist>\n",
        "/usr/local/libexec/ferrow/ferrowd"
    )
}

#[cfg(target_os = "macos")]
fn plist_path() -> String {
    format!("/Library/LaunchDaemons/{LAUNCHD_LABEL}.plist")
}

#[cfg(target_os = "macos")]
pub fn install(roots: &[std::path::PathBuf]) -> Result<String, String> {
    if !roots.is_empty() {
        return Err("choosing index roots at install is only supported on Windows".into());
    }
    let _exe = crate::protection::install_binary(&ferrowd_exe()?).map_err(|e| e.to_string())?;
    let path = plist_path();
    crate::definition::write(std::path::Path::new(&path), &launchd_plist())
        .map_err(|e| root_hint("install", &e))?;
    crate::protection::state_directory(
        crate::default_service_index_path()
            .map_err(|e| e.to_string())?
            .parent()
            .unwrap(),
    )
    .map_err(|e| root_hint("install", &e))?;
    Ok(format!(
        "launchd daemon {LAUNCHD_LABEL} installed ({path}). Load it with: sudo launchctl bootstrap system {path}\n\
         Grant ferrowd Full Disk Access in System Settings > Privacy & Security, or home directories stay unindexed."
    ))
}

#[cfg(target_os = "macos")]
pub fn uninstall() -> Result<String, String> {
    fn process_identity(pid: u32) -> Result<Option<(u64, u64)>, String> {
        // SAFETY: zero is a valid initial output structure; the buffer is
        // correctly aligned and exactly sized for PROC_PIDTBSDINFO.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        // SAFETY: live output buffer, checked byte count before reading fields.
        let bytes = unsafe {
            libc::proc_pidinfo(
                pid as i32,
                libc::PROC_PIDTBSDINFO,
                0,
                (&raw mut info).cast(),
                size_of::<libc::proc_bsdinfo>() as i32,
            )
        };
        if bytes == 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(None);
            }
            return Err(error.to_string());
        }
        if bytes as usize != size_of::<libc::proc_bsdinfo>() {
            return Err("incomplete service process identity".into());
        }
        Ok(Some((info.pbi_start_tvsec, info.pbi_start_tvusec)))
    }
    let target = format!("system/{LAUNCHD_LABEL}");
    let current = command("launchctl", &["print", &target])?;
    let text = String::from_utf8_lossy(&current.stdout);
    let pid = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("pid = "))
        .map(str::parse::<u32>)
        .transpose()
        .map_err(|e| e.to_string())?;
    if pid.is_none() && text.lines().any(|line| line.trim() == "state = running") {
        return Err("running launchd service has no verifiable process id".into());
    }
    let original = pid.map(process_identity).transpose()?.flatten();
    if current.status.success() {
        run_cmd("launchctl", &["bootout", &target])?;
    } else if current.status.code() != Some(113) {
        return Err(format!(
            "cannot establish launchd service state: {}",
            String::from_utf8_lossy(&current.stderr)
        ));
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    confirm_stopped(
        || {
            let output = command("launchctl", &["print", &target])?;
            if output.status.success() {
                Ok(false)
            } else if output.status.code() == Some(113) {
                match (pid, original) {
                    (Some(pid), Some(identity)) => Ok(process_identity(pid)? != Some(identity)),
                    _ => Ok(true),
                }
            } else {
                Err(format!(
                    "cannot verify launchd stop: {}",
                    String::from_utf8_lossy(&output.stderr)
                ))
            }
        },
        || std::time::Instant::now() < deadline,
        || std::thread::sleep(std::time::Duration::from_millis(50)),
    )?;
    let path = plist_path();
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(format!(
            "launchd daemon {LAUNCHD_LABEL} stopped and uninstalled"
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok(format!("launchd daemon {LAUNCHD_LABEL} is not installed"))
        }
        Err(e) => Err(root_hint("uninstall", &e)),
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
pub fn install(roots: &[std::path::PathBuf]) -> Result<String, String> {
    if !roots.is_empty() {
        return Err("choosing index roots at install is only supported on Windows".into());
    }
    Err("service install is not supported on this platform".into())
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
pub fn uninstall() -> Result<String, String> {
    Err("service uninstall is not supported on this platform".into())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    #[test]
    fn generated_unit_matches_packaging_file() {
        let generated = super::systemd_unit();
        assert_eq!(
            generated,
            include_str!("../../../packaging/systemd/ferrowd.service")
        );
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    #[test]
    fn generated_plist_matches_packaging_file() {
        let generated = super::launchd_plist();
        assert_eq!(
            generated,
            include_str!("../../../packaging/launchd/com.github.rennerdo30.ferrowd.plist")
        );
    }
}

#[cfg(test)]
mod stop_tests {
    use super::*;
    #[test]
    fn failed_stop_and_pending_timeout_never_report_success() {
        assert_eq!(
            confirm_stopped(|| Err("stop denied".into()), || true, || {}),
            Err("stop denied".into())
        );
        assert!(
            confirm_stopped(|| Ok(false), || false, || {})
                .unwrap_err()
                .contains("timed out")
        );
        let mut states = [false, false, true].into_iter();
        let ticks = std::cell::Cell::new(0);
        confirm_stopped(
            || Ok(states.next().unwrap()),
            || ticks.get() < 3,
            || ticks.set(ticks.get() + 1),
        )
        .unwrap();
        assert_eq!(ticks.get(), 2);
        confirm_stopped(
            || Ok(true),
            || false,
            || panic!("already stopped must not wait"),
        )
        .unwrap();
    }
}
