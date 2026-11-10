use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

pub struct Fixture {
    pub path: PathBuf,
    links: Vec<(PathBuf, bool)>,
}

impl Fixture {
    pub fn new(tag: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "ferrow-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self {
            path,
            links: Vec::new(),
        }
    }

    pub fn link(&mut self, target: &Path, link: &Path) {
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(target, link).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).unwrap();
        self.links.push((link.to_path_buf(), true));
    }

    pub fn file_link(&mut self, target: &Path, link: &Path) {
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(target, link).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).unwrap();
        self.links.push((link.to_path_buf(), false));
    }

    #[cfg(windows)]
    pub fn junction(&mut self, target: &Path, link: &Path) {
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};
        let mut child = Command::new("cmd")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        eprintln!("junction fixture PID {pid}, timeout 10 seconds");
        let started = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if started.elapsed() > Duration::from_secs(10) {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("junction fixture PID {pid} timed out");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(status.success(), "junction fixture PID {pid}: {status}");
        self.links.push((link.to_path_buf(), true));
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Unlink our recorded aliases before recursive cleanup, especially junctions.
        for (link, _is_dir) in self.links.iter().rev() {
            #[cfg(windows)]
            let result = if *_is_dir {
                std::fs::remove_dir(link)
            } else {
                std::fs::remove_file(link)
            };
            #[cfg(unix)]
            let result = std::fs::remove_file(link);
            if let Err(error) = result {
                eprintln!("retaining fixture {}: {error}", self.path.display());
                return;
            }
        }
        std::fs::remove_dir_all(&self.path).unwrap();
    }
}

pub fn run(root: &Path, excludes: Vec<PathBuf>, follow: bool) -> ferrow_index::Index {
    let (index, report) = ferrow_walk::scan(&ferrow_walk::WalkOptions {
        columns: Default::default(),
        roots: vec![root.to_path_buf()],
        excludes,
        follow_symlinks: follow,
        ntfs: ferrow_walk::NtfsMode::Off,
    });
    assert_eq!(report.errors, 0, "{report:?}");
    index
}

pub fn contains(index: &ferrow_index::Index, name: &[u8]) -> bool {
    (0..index.len() as u32).any(|id| index.name(id) == name)
}
