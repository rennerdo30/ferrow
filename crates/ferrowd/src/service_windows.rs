//! Windows Service Control Manager host (M2b). The service is
//! created by `ferrow service install` with `--run-as-service
//! --service-endpoint` in its command line and runs as
//! LocalSystem. Stop/shutdown are signalled through the
//! daemon's [`Shutdown`] wake-up, never by killing threads.

use std::ffi::OsString;
use std::sync::OnceLock;
use std::time::Duration;

use ferrowd::{DaemonConfig, Shutdown, SnapshotStore, initial_snapshot, serve};
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::{define_windows_service, service_dispatcher};

pub const SERVICE_NAME: &str = "ferrowd";

static CONFIG: OnceLock<DaemonConfig> = OnceLock::new();

define_windows_service!(ffi_service_main, service_main);

/// Entry point when launched with `--run-as-service` by the SCM.
pub fn run(config: DaemonConfig) -> Result<(), windows_service::Error> {
    let _ = CONFIG.set(config);
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
}

fn service_main(_arguments: Vec<OsString>) {
    let _ = run_service();
}

fn run_service() -> Result<(), windows_service::Error> {
    let Some(config) = CONFIG.get().cloned() else {
        return Ok(());
    };
    let shutdown = Shutdown::new();
    let stop = shutdown.clone();
    let endpoint = config.endpoint.clone();
    let handler = move |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            stop.request(&endpoint);
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let status_handle = service_control_handler::register(SERVICE_NAME, handler)?;
    let set_state = |state: ServiceState, accept: ServiceControlAccept| {
        status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: accept,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        })
    };
    set_state(
        ServiceState::Running,
        ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
    )?;
    let store =
        SnapshotStore::new(initial_snapshot(&config).map_err(windows_service::Error::Winapi)?);
    let result = serve(&config, store, shutdown);
    set_state(ServiceState::Stopped, ServiceControlAccept::empty())?;
    result.map_err(|e| {
        eprintln!("ferrowd service: {e}");
        windows_service::Error::Winapi(e)
    })
}
