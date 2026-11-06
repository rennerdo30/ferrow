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
