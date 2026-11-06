#![cfg(unix)]
use ferrow_ipc::{Client, Endpoint};
use std::os::unix::fs::MetadataExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn stop(child: &mut OwnedChild) {
    // SAFETY: this test just spawned and still owns this exact child process.
    assert_eq!(unsafe { libc::kill(child.0.id() as i32, libc::SIGTERM) }, 0);
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(status.success());
            return;
        }
        assert!(
            Instant::now() < deadline,
            "SIGTERM did not produce a bounded exit"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn sigterm_saves_and_restart_reloads_the_index() {
    let root = std::env::temp_dir().join(format!("fr-signal-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let tree = root.join("tree");
    std::fs::create_dir(&tree).unwrap();
    std::fs::write(tree.join("signal-needle.txt"), b"x").unwrap();
    let index = root.join("index.bin");
    let name = format!("fr-signal-{}", std::process::id());
    let endpoint = Endpoint::named(&name);
    for first in [true, false] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ferrowd"));
        command
            .args(["--endpoint", &name, "--index"])
            .arg(&index)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        if first {
            command.arg("--root").arg(&tree);
        }
        let mut child = OwnedChild(command.spawn().unwrap());
        eprintln!("owned daemon PID {}", child.0.id());
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Ok(mut client) =
                Client::connect_deadline(&endpoint, Instant::now() + Duration::from_millis(100))
            {
                assert_eq!(client.count("signal-needle").unwrap(), 1);
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        let before = std::fs::metadata(&index).unwrap().ino();
        stop(&mut child);
        assert_ne!(
            std::fs::metadata(&index).unwrap().ino(),
            before,
            "shutdown did not save"
        );
        assert!(ferrowd::Snapshot::from_file(&index).is_ok());
    }
    std::fs::remove_dir_all(root).unwrap();
}
