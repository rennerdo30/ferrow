//! `ferrowd` binary: the Ferrow background index service.
//!
//! ```text
//! ferrowd [--endpoint <name> | --service-endpoint]
//!         [--index <file>] [--root <path>]... [--exclude <path>]...
//!         [--follow-symlinks]
//! ```
//!
//! With no endpoint flag the per-user development endpoint is
//! used, so a plain `ferrowd` run never needs privileges. The
//! installed system service passes `--service-endpoint` (and, on
//! Windows, `--run-as-service`).

use std::path::PathBuf;
use std::process::ExitCode;

use ferrow_ipc::Endpoint;
use ferrowd::{DaemonConfig, Shutdown, SnapshotStore, initial_snapshot, serve};

#[cfg(windows)]
mod service_windows;

const USAGE: &str = "\
ferrowd - Ferrow background index service

Usage:
  ferrowd [--endpoint <name> | --service-endpoint]
          [--index <file>] [--root <path>]... [--exclude <path>]...
          [--follow-symlinks]
  ferrowd --help | --version
";

fn run(argv: Vec<std::ffi::OsString>) -> Result<(), String> {
    let mut endpoint = Endpoint::user_service();
    let mut index_path: Option<PathBuf> = None;
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut excludes: Vec<PathBuf> = Vec::new();
    let mut follow_symlinks = false;
    let mut run_as_service = false;

    let mut i = 0;
    while i < argv.len() {
        let arg = argv[i].to_string_lossy().into_owned();
        let value = |i: &mut usize| -> Result<PathBuf, String> {
            let v = argv
                .get(*i + 1)
                .ok_or_else(|| format!("flag {arg} needs a value"))?;
            *i += 1;
            Ok(PathBuf::from(v.clone()))
        };
        match arg.as_str() {
            "--help" | "-h" => {
                print!("{USAGE}");
                return Ok(());
            }
            "--version" | "-V" => {
                println!("ferrowd {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--endpoint" => {
                endpoint = Endpoint::named(
                    value(&mut i)?
                        .to_str()
                        .ok_or("--endpoint name is not UTF-8")?,
                );
            }
            "--service-endpoint" => endpoint = Endpoint::default_service(),
            "--index" => index_path = Some(value(&mut i)?),
            "--root" => roots.push(value(&mut i)?),
            "--exclude" => excludes.push(value(&mut i)?),
            "--follow-symlinks" => follow_symlinks = true,
            "--run-as-service" => run_as_service = true,
            other => return Err(format!("unknown argument: {other}\n\n{USAGE}")),
        }
        i += 1;
    }

    #[cfg(windows)]
    if run_as_service {
        let config = service_config(endpoint, index_path, roots, excludes, follow_symlinks)?;
        return service_windows::run(config).map_err(|e| e.to_string());
    }
    #[cfg(not(windows))]
    if run_as_service {
        return Err("--run-as-service is only supported on Windows".into());
    }

    if endpoint == Endpoint::default_service() && index_path.is_none() {
        index_path = Some(ferrowd::default_service_index_path().map_err(|e| e.to_string())?);
    }
    let config = DaemonConfig {
        endpoint,
        index_path,
        roots,
        excludes,
        follow_symlinks,
    };
    let store = SnapshotStore::new(initial_snapshot(&config).map_err(|e| e.to_string())?);
    eprintln!("ferrowd: {}", store.get().report);
    serve(&config, store, Shutdown::new()).map_err(|e| e.to_string())
}

#[cfg(windows)]
fn service_config(
    endpoint: Endpoint,
    index_path: Option<PathBuf>,
    roots: Vec<PathBuf>,
    excludes: Vec<PathBuf>,
    follow_symlinks: bool,
) -> Result<DaemonConfig, String> {
    Ok(DaemonConfig {
        index_path: Some(match index_path {
            Some(path) => path,
            None => ferrowd::default_service_index_path().map_err(|e| e.to_string())?,
        }),
        endpoint,
        roots,
        excludes,
        follow_symlinks,
    })
}

fn main() -> ExitCode {
    let argv: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    match run(argv) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("error: {msg}");
            ExitCode::FAILURE
        }
    }
}
