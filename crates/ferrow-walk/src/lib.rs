//! Portable parallel directory-walk backend.
//!
//! Scans directory trees with `std::fs::read_dir` on the rayon pool and
//! feeds a [`ferrow_index::IndexBuilder`]. Permission and IO errors are
//! counted and sampled into the report, never fatal.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use ferrow_index::osname::name_to_bytes;
use ferrow_index::{Index, IndexBuilder, flags};

/// What to scan.
#[derive(Debug, Clone, Default)]
pub struct WalkOptions {
    /// Roots to include. Each becomes one root entry in the index.
    pub roots: Vec<PathBuf>,
    /// Paths to exclude (files or whole subtrees), compared as absolute paths.
    pub excludes: Vec<PathBuf>,
    /// Descend into symlinked directories. Off by default.
    pub follow_symlinks: bool,
}

/// Outcome of a scan.
#[derive(Debug, Clone)]
pub struct ScanReport {
    pub mode: String,
    pub threads: usize,
    pub entries: usize,
    pub errors: usize,
    /// Up to 10 sampled error descriptions.
    pub error_samples: Vec<String>,
    pub elapsed: Duration,
}

impl ScanReport {
    /// One-line summary, e.g. `walk: parallel read_dir, 8 threads, 1234
    /// entries, 2 errors, 0.42 s`.
    pub fn summary(&self) -> String {
        format!(
            "walk: {}, {} threads, {} entries, {} errors, {:.2} s",
            self.mode,
            self.threads,
            self.entries,
            self.errors,
            self.elapsed.as_secs_f64()
        )
    }
}

struct Errors {
    count: AtomicUsize,
    samples: Mutex<Vec<String>>,
}

impl Errors {
    fn new() -> Errors {
        Errors {
            count: AtomicUsize::new(0),
            samples: Mutex::new(Vec::new()),
        }
    }

    fn add(&self, what: String) {
        let n = self.count.fetch_add(1, Ordering::Relaxed);
        if n < 10 {
            self.samples
                .lock()
                .expect("error samples poisoned")
                .push(what);
        }
    }
}

fn is_hidden(name: &[u8], entry: &std::fs::DirEntry) -> bool {
    #[cfg(windows)]
    {
        let _ = name;
        use std::os::windows::fs::MetadataExt;
        if let Ok(md) = entry.metadata() {
            // FILE_ATTRIBUTE_HIDDEN
            return md.file_attributes() & 0x2 != 0;
        }
        false
    }
    #[cfg(not(windows))]
    {
        let _ = entry;
        name.first() == Some(&b'.') && name.len() > 1
    }
}

#[derive(Clone, Copy)]
struct Walker<'a> {
    builder: &'a IndexBuilder,
    excludes: &'a HashSet<PathBuf>,
    follow_symlinks: bool,
    errors: &'a Errors,
}

impl<'x> Walker<'x> {
    fn scan_dir<'scope>(self, scope: &rayon::Scope<'scope>, dir: PathBuf, dir_id: u32)
    where
        'x: 'scope,
    {
        let read = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(e) => {
                self.errors.add(format!("{}: {e}", dir.display()));
                return;
            }
        };
        for item in read {
            let entry = match item {
                Ok(e) => e,
                Err(e) => {
                    self.errors.add(format!("{}: {e}", dir.display()));
                    continue;
                }
            };
            let path = entry.path();
            if self.is_excluded(&path) {
                continue;
            }
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(e) => {
                    self.errors.add(format!("{}: {e}", path.display()));
                    continue;
                }
            };
            let mut name = Vec::new();
            name_to_bytes(&entry.file_name(), &mut name);
            let mut ent_flags: u16 = 0;
            let is_symlink = file_type.is_symlink();
            let mut is_dir = file_type.is_dir();
            if is_symlink {
                ent_flags |= flags::SYMLINK;
                if self.follow_symlinks {
                    // metadata() follows the link; a broken link stays a leaf.
                    is_dir = entry.metadata().map(|m| m.is_dir()).unwrap_or(false);
                }
            }
            if is_dir {
                ent_flags |= flags::DIR;
            }
            if is_hidden(&name, &entry) {
                ent_flags |= flags::HIDDEN;
            }
            let id = self.builder.add(dir_id, &name, ent_flags);
            if is_dir {
                scope.spawn(move |s| self.scan_dir(s, path, id));
            }
        }
    }

    fn is_excluded(&self, path: &Path) -> bool {
        if self.excludes.is_empty() {
            return false;
        }
        match std::path::absolute(path) {
            Ok(abs) => self.excludes.contains(&abs),
            Err(_) => false,
        }
    }
}

/// Scan all roots and freeze the index.
pub fn scan(options: &WalkOptions) -> (Index, ScanReport) {
    let started = Instant::now();
    let builder = IndexBuilder::new();
    let errors = Errors::new();
    let excludes: HashSet<PathBuf> = options
        .excludes
        .iter()
        .filter_map(|p| std::path::absolute(p).ok())
        .collect();
    let walker = Walker {
        builder: &builder,
        excludes: &excludes,
        follow_symlinks: options.follow_symlinks,
        errors: &errors,
    };

    // Roots are added sequentially (cheap), their subtrees in parallel.
    let mut root_dirs: Vec<(PathBuf, u32)> = Vec::new();
    for root in &options.roots {
        // Store the root without a trailing separator (except a bare "/"
        // or drive root) so path joins stay clean.
        let display = root.to_string_lossy();
        let trimmed = display.trim_end_matches(['/', '\\']);
        let root_str: &str = if trimmed.is_empty() {
            &display
        } else {
            trimmed
        };
        let (name_bytes, is_dir) = match std::fs::metadata(root) {
            Ok(md) => (root_str.as_bytes().to_vec(), md.is_dir()),
            Err(e) => {
                errors.add(format!("{}: {e}", root.display()));
                continue;
            }
        };
        let root_flags = if is_dir { flags::DIR } else { 0 };
        let id = builder.add_root(&name_bytes, root_flags);
        if is_dir {
            root_dirs.push((root.clone(), id));
        }
    }

    rayon::scope(|scope| {
        for (dir, id) in root_dirs {
            scope.spawn(move |s| walker.scan_dir(s, dir, id));
        }
    });

    let entries = builder.len();
    let index = builder.freeze();
    let report = ScanReport {
        mode: "parallel read_dir".to_string(),
        threads: rayon::current_num_threads(),
        entries,
        errors: errors.count.load(Ordering::Relaxed),
        error_samples: errors.samples.into_inner().expect("error samples poisoned"),
        elapsed: started.elapsed(),
    };
    (index, report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrow_index::ROOT_PARENT;
    use std::collections::HashMap;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(tag: &str) -> TestDir {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "ferrow-walk-{tag}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            TestDir(dir)
        }

        fn file(&self, rel: &str) {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"x").unwrap();
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn by_name(idx: &Index) -> HashMap<String, u32> {
        let mut map = HashMap::new();
        for id in 0..idx.len() as u32 {
            if idx.parent(id) != ROOT_PARENT {
                map.insert(String::from_utf8(idx.name(id).to_vec()).unwrap(), id);
            }
        }
        map
    }

    #[test]
    fn finds_every_entry_with_right_parent() {
        let dir = TestDir::new("tree");
        dir.file("a.txt");
        dir.file("sub/b.log");
        dir.file("sub/deep/c.md");
        dir.file("sub2/d.txt");

        let (idx, report) = scan(&WalkOptions {
            roots: vec![dir.0.clone()],
            ..WalkOptions::default()
        });
        assert_eq!(report.errors, 0, "{:?}", report.error_samples);
        // root + a.txt + sub + b.log + deep + c.md + sub2 + d.txt
        assert_eq!(idx.len(), 8);

        let names = by_name(&idx);
        let sub = names["sub"];
        let deep = names["deep"];
        assert_eq!(idx.parent(names["a.txt"]), 0);
        assert_eq!(idx.parent(names["b.log"]), sub);
        assert_eq!(idx.parent(deep), sub);
        assert_eq!(idx.parent(names["c.md"]), deep);
        assert_eq!(idx.parent(names["d.txt"]), names["sub2"]);
        assert_ne!(idx.flags(sub) & flags::DIR, 0);
        assert_eq!(idx.flags(names["a.txt"]) & flags::DIR, 0);

        // Rebuilt paths point at the real files.
        let rebuilt = idx.path(names["c.md"]);
        assert_eq!(rebuilt, dir.0.join("sub").join("deep").join("c.md"));
        assert!(report.summary().contains("parallel read_dir"));
    }

    #[test]
    fn excludes_subtree() {
        let dir = TestDir::new("exclude");
        dir.file("keep.txt");
        dir.file("skip/gone.txt");
        let (idx, _) = scan(&WalkOptions {
            roots: vec![dir.0.clone()],
            excludes: vec![dir.0.join("skip")],
            follow_symlinks: false,
        });
        let names = by_name(&idx);
        assert!(names.contains_key("keep.txt"));
        assert!(!names.contains_key("gone.txt"));
        assert!(!names.contains_key("skip"));
    }

    #[test]
    fn missing_root_is_counted_not_fatal() {
        let (idx, report) = scan(&WalkOptions {
            roots: vec![PathBuf::from("ferrow-no-such-dir-xyz")],
            ..WalkOptions::default()
        });
        assert_eq!(idx.len(), 0);
        assert_eq!(report.errors, 1);
        assert_eq!(report.error_samples.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_directory_is_counted_not_fatal() {
        use std::os::unix::fs::PermissionsExt;
        // Root (as in some CI containers) ignores permission bits; skip then.
        let dir = TestDir::new("unreadable");
        dir.file("open.txt");
        dir.file("locked/secret.txt");
        let locked = dir.0.join("locked");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let can_read = std::fs::read_dir(&locked).is_ok();
        let (idx, report) = scan(&WalkOptions {
            roots: vec![dir.0.clone()],
            ..WalkOptions::default()
        });
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(by_name(&idx).contains_key("open.txt"));
        if !can_read {
            assert!(report.errors >= 1, "expected a counted error");
            assert!(!by_name(&idx).contains_key("secret.txt"));
        }
    }
}
