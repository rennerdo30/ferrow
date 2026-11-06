//! `ferrow service install | uninstall` (M2b).
//!
//! Installing a system service needs administrator rights once.
//! When they are missing these functions say exactly what to
//! re-run elevated and never try to elevate themselves.

use std::path::PathBuf;

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
pub fn install() -> Result<String, String> {
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
        launch_arguments: vec![
            "--run-as-service".into(),
            "--service-endpoint".into(),
            "--index".into(),
            index.into_os_string(),
        ],
        dependencies: vec![],
        account_name: None, // LocalSystem
        account_password: None,
    };
    let service = manager
        .create_service(&info, ServiceAccess::CHANGE_CONFIG)
        .map_err(|e| admin_hint("install", &e))?;
    let _ = service
        .set_description("Indexes file names and answers Ferrow searches over a local pipe.");
    Ok(format!(
        "service {SERVICE_NAME} installed (LocalSystem, automatic start). Start it with: sc start {SERVICE_NAME}"
    ))
}

#[cfg(windows)]
pub fn uninstall() -> Result<String, String> {
    use windows_service::service::ServiceAccess;
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|e| admin_hint("uninstall", &e))?;
    let service = manager
        .open_service(
            SERVICE_NAME,
            ServiceAccess::STOP | ServiceAccess::DELETE | ServiceAccess::QUERY_STATUS,
        )
        .map_err(|e| admin_hint("uninstall", &e))?;
    let _ = service.stop();
    service.delete().map_err(|e| admin_hint("uninstall", &e))?;
    Ok(format!("service {SERVICE_NAME} uninstalled"))
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
fn run_cmd(program: &str, args: &[&str]) -> Result<(), String> {
    let out = std::process::Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("{program}: {e}"))?;
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
pub fn install() -> Result<String, String> {
    let _exe = crate::protection::install_binary(&ferrowd_exe()?).map_err(|e| e.to_string())?;
    std::fs::write(UNIT_PATH, systemd_unit()).map_err(|e| root_hint("install", &e))?;
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
    let _ = run_cmd("systemctl", &["disable", "--now", SERVICE_NAME]);
    match std::fs::remove_file(UNIT_PATH) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(format!("service {SERVICE_NAME} is not installed"));
        }
        Err(e) => return Err(root_hint("uninstall", &e)),
    }
    run_cmd("systemctl", &["daemon-reload"])?;
    Ok(format!("service {SERVICE_NAME} uninstalled"))
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
pub fn install() -> Result<String, String> {
    let _exe = crate::protection::install_binary(&ferrowd_exe()?).map_err(|e| e.to_string())?;
    let path = plist_path();
    std::fs::write(&path, launchd_plist()).map_err(|e| root_hint("install", &e))?;
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
    let path = plist_path();
    let _ = run_cmd("launchctl", &["bootout", "system", &path]);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(format!("launchd daemon {LAUNCHD_LABEL} uninstalled")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok(format!("launchd daemon {LAUNCHD_LABEL} is not installed"))
        }
        Err(e) => Err(root_hint("uninstall", &e)),
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
pub fn install() -> Result<String, String> {
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
