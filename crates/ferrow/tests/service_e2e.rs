//! CLI <-> service end-to-end: `ferrow service run` unprivileged
//! on a per-test endpoint, then plain `ferrow search`/`status`
//! must use the service while no in-process index exists, and
//! `--index` must bypass it.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Fixture {
    base: PathBuf,
    tree: PathBuf,
    endpoint_name: String,
}

impl Fixture {
    fn new() -> Fixture {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!("ferrow-svc-e2e-{}-{n}", std::process::id()));
        let tree = base.join("tree");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(tree.join("service-needle.txt"), b"x").unwrap();
        std::fs::write(tree.join("other.txt"), b"x").unwrap();
        Fixture {
            endpoint_name: format!("ferrow-svc-test-{}-{n}", std::process::id()),
            base,
            tree,
        }
    }

    /// A ferrow invocation with the test endpoint pinned and the
    /// per-user data dir pointed at an empty directory, so the
    /// only way search can succeed is through the service.
    fn ferrow(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ferrow"));
        cmd.env("FERROW_ENDPOINT_NAME", &self.endpoint_name);
        let empty_home = self.base.join("home");
        std::fs::create_dir_all(&empty_home).unwrap();
        if cfg!(windows) {
            cmd.env("LOCALAPPDATA", &empty_home);
        } else {
            cmd.env("HOME", &empty_home).env_remove("XDG_DATA_HOME");
        }
        cmd
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn search_uses_service_and_index_flag_bypasses_it() {
    let fx = Fixture::new();
    let _daemon = KillOnDrop(
        fx.ferrow()
            .args(["service", "run", fx.tree.to_str().unwrap()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ferrow service run"),
    );

    // Wait until the service answers (bounded).
    let endpoint = ferrow_ipc::Endpoint::named(&fx.endpoint_name);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        if let Ok(mut client) = ferrow_ipc::Client::connect(&endpoint)
            && client.status().is_ok()
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "service never came up"
        );
        std::thread::sleep(std::time::Duration::from_millis(25));
    }

    let out = fx.ferrow().args(["status"]).output().unwrap();
    let status = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(status.contains("mode: service"), "status: {status}");

    let out = fx
        .ferrow()
        .args(["search", "service-needle"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let hits = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(hits.contains("service-needle.txt"), "hits: {hits}");

    let out = fx
        .ferrow()
        .args(["search", "service-needle", "--count"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "1");

    for (format, expected) in [
        ("--json", b"[]\n".as_slice()),
        ("--null", b"".as_slice()),
        ("--count", b"0\n".as_slice()),
    ] {
        let out = fx
            .ferrow()
            .args(["search", "service-needle", "--limit", "0", format])
            .output()
            .unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout, expected, "explicit zero with {format}");
    }

    // An explicit --index bypasses the service: this file does
    // not exist, so the command must fail instead of answering
    // from the daemon.
    let missing = fx.base.join("no-such-index.bin");
    let out = fx
        .ferrow()
        .args([
            "search",
            "service-needle",
            "--index",
            missing.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!out.status.success(), "--index must bypass the service");
}

#[test]
fn discovery_retains_protocol_refusal_reason() {
    let fx = Fixture::new();
    let listener =
        ferrow_ipc::Listener::bind(&ferrow_ipc::Endpoint::named(&fx.endpoint_name)).unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let hello = ferrow_ipc::read_message(&mut stream).unwrap().unwrap();
        ferrow_ipc::write_message(
            &mut stream,
            hello.id,
            &ferrow_ipc::Message::Response(ferrow_ipc::Response::Error {
                code: ferrow_ipc::ErrorCode::VersionMismatch,
                msg: "different service protocol".into(),
            }),
        )
        .unwrap();
    });
    let output = fx.ferrow().args(["status"]).output().unwrap();
    server.join().unwrap();
    assert!(String::from_utf8_lossy(&output.stdout).contains("VersionMismatch"));
}

#[test]
fn stalled_hello_falls_back_within_discovery_budget() {
    let fx = Fixture::new();
    let listener =
        ferrow_ipc::Listener::bind(&ferrow_ipc::Endpoint::named(&fx.endpoint_name)).unwrap();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        ferrow_ipc::read_message(&mut stream).unwrap().unwrap();
        let _ = release_rx.recv_timeout(std::time::Duration::from_secs(2));
    });
    let started = std::time::Instant::now();
    let output = fx.ferrow().args(["status"]).output().unwrap();
    let elapsed = started.elapsed();
    let _ = release_tx.send(());
    server.join().unwrap();
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "selection stalled for {elapsed:?}"
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("deadline"));
}

/// The CLI contract the app relies on: `service health --json` is the report,
/// `rescan` without a live maintainer exits 5 with its reason on stdout, and no
/// service at all exits 3.
#[test]
fn service_health_and_rescan_follow_the_documented_exit_codes() {
    let fx = Fixture::new();
    let nothing = fx.ferrow().args(["service", "health"]).output().unwrap();
    assert_eq!(nothing.status.code(), Some(3), "no service: exit 3");
    assert!(String::from_utf8_lossy(&nothing.stderr).contains("no Ferrow service is reachable"));

    let _daemon = KillOnDrop(
        fx.ferrow()
            .args(["service", "run", fx.tree.to_str().unwrap()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ferrow service run"),
    );
    let endpoint = ferrow_ipc::Endpoint::named(&fx.endpoint_name);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        if let Ok(mut client) = ferrow_ipc::Client::connect(&endpoint)
            && client.status().is_ok()
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "service never came up"
        );
        std::thread::sleep(std::time::Duration::from_millis(25));
    }

    let out = fx
        .ferrow()
        .args(["service", "health", "--json"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert_eq!(text.matches('\n').count(), 1, "one line: {text}");
    let report = ferrow_ipc::health::Health::from_json(&text).unwrap();
    assert!(report.pid != 0 && !report.version.is_empty(), "{text}");
    assert!(
        report
            .events
            .iter()
            .any(|e| e.text.contains("listening on")),
        "{text}"
    );

    let human = fx.ferrow().args(["service", "health"]).output().unwrap();
    assert_eq!(human.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&human.stdout).contains("Ferrow service"));

    let rescan = fx.ferrow().args(["service", "rescan"]).output().unwrap();
    assert_eq!(rescan.status.code(), Some(5), "not started: exit 5");
    assert!(
        !String::from_utf8_lossy(&rescan.stdout).trim().is_empty(),
        "the reason is on stdout"
    );

    let bad = fx
        .ferrow()
        .args(["service", "health", "--yaml"])
        .output()
        .unwrap();
    assert_eq!(bad.status.code(), Some(1));
}

/// A service that answers every request with `Unauthorized` (as one does for
/// an account its access policy refuses): the CLI must exit 4 for both
/// `health` and `rescan`, with a message that points at the access policy.
#[test]
fn a_refusing_service_gives_exit_code_4() {
    use ferrow_ipc::{ErrorCode, Listener, Response, serve_stream};
    let fx = Fixture::new();
    let endpoint = ferrow_ipc::Endpoint::named(&fx.endpoint_name);
    let listener = Listener::bind(&endpoint).unwrap();
    let server = std::thread::spawn(move || {
        // Two connections: one for `health`, one for `rescan`.
        for _ in 0..2 {
            let (stream, _) = listener.accept().unwrap();
            let _ = serve_stream(stream, |_| {
                vec![Response::Error {
                    code: ErrorCode::Unauthorized,
                    msg: "not allowed".into(),
                }]
            });
        }
    });
    let health = fx.ferrow().args(["service", "health"]).output().unwrap();
    assert_eq!(health.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&health.stderr).contains("does not answer this account"));
    let rescan = fx.ferrow().args(["service", "rescan"]).output().unwrap();
    assert_eq!(rescan.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&rescan.stderr).contains("only administrators"));
    server.join().unwrap();
}
