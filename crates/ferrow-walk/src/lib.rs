//! Portable parallel directory-walk backend.
//!
//! Scans directory trees with `std::fs::read_dir` on the rayon pool and
//! feeds a [`ferrow_index::IndexBuilder`]. Permission and IO errors are
//! counted and sampled into the report, never fatal.

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

/// Normalised comparison key for exclude matching: the lossless bytes
/// of each path component. On Windows components are case-folded, so
/// excludes compare case-insensitively (documented in ADR 0001); on
/// Unix they compare exactly.
fn path_key(path: &Path) -> Vec<Vec<u8>> {
    path.components()
        .map(|component| {
            let mut bytes = Vec::new();
            name_to_bytes(component.as_os_str(), &mut bytes);
            #[cfg(windows)]
            {
                let mut folded = Vec::new();
                ferrow_index::fold_name(&bytes, &mut folded);
                folded
            }
            #[cfg(not(windows))]
            {
                bytes
            }
        })
        .collect()
}

/// True when `key` equals an excluded path or lies under one
/// (component-wise prefix), so an excluded ancestor cannot be
/// bypassed by naming a deeper root explicitly.
fn key_is_excluded(key: &[Vec<u8>], excludes: &[Vec<Vec<u8>>]) -> bool {
    excludes
        .iter()
        .any(|excluded| key.len() >= excluded.len() && key[..excluded.len()] == excluded[..])
}

#[derive(Clone, Copy)]
struct Walker<'a> {
    builder: &'a IndexBuilder,
    excludes: &'a [Vec<Vec<u8>>],
    follow_symlinks: bool,
    errors: &'a Errors,
}

impl<'x> Walker<'x> {
    /// `ancestors` holds the canonical paths of the directory chain
    /// from the root down to and including `dir`. It is only populated
    /// when following symlinks, where it bounds cycles: a directory
    /// whose canonical path is already an ancestor is not descended.
    fn scan_dir<'scope>(
        self,
        scope: &rayon::Scope<'scope>,
        dir: PathBuf,
        dir_id: u32,
        ancestors: Vec<PathBuf>,
    ) where
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
            // file_type() never follows links: a symlinked directory is
            // a leaf unless following is on, in which case fs::metadata
            // (which traverses) decides. DirEntry::metadata() does NOT
            // traverse links, so it cannot be used for this.
            let mut is_dir = file_type.is_dir();
            if is_symlink {
                ent_flags |= flags::SYMLINK;
                if self.follow_symlinks {
                    match std::fs::metadata(&path) {
                        Ok(md) => is_dir = md.is_dir(),
                        Err(e) => {
                            self.errors.add(format!("{}: {e}", path.display()));
                        }
                    }
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
                let mut child_ancestors = ancestors.clone();
                if self.follow_symlinks {
                    match std::fs::canonicalize(&path) {
                        Ok(canonical) => {
                            if ancestors.contains(&canonical) {
                                self.errors.add(format!(
                                    "{}: symlink cycle (already an ancestor)",
                                    path.display()
                                ));
                                continue;
                            }
                            child_ancestors.push(canonical);
                        }
                        Err(e) => {
                            self.errors.add(format!("{}: {e}", path.display()));
                            continue;
                        }
                    }
                }
                scope.spawn(move |s| self.scan_dir(s, path, id, child_ancestors));
            }
        }
    }

    fn is_excluded(&self, path: &Path) -> bool {
        if self.excludes.is_empty() {
            return false;
        }
        match std::path::absolute(path) {
            Ok(abs) => key_is_excluded(&path_key(&abs), self.excludes),
            Err(_) => false,
        }
    }
}

/// Lossless stored name for a root path: the raw `OsStr` bytes via
/// [`name_to_bytes`] (never `to_string_lossy`), with trailing
/// separators removed so path joins stay clean. A bare root (`/`,
/// `E:\`, a UNC share root) keeps its separator. Only real separator
/// bytes are stripped (on Unix only `/`, so a name ending in `\` is
/// preserved); separator bytes are ASCII and cannot be part of a
/// multi-byte WTF-8/UTF-8 sequence, so byte trimming is exact.
fn root_name_bytes(path: &Path) -> Vec<u8> {
    let mut bytes = Vec::new();
    name_to_bytes(path.as_os_str(), &mut bytes);
    if path.parent().is_some() {
        let is_sep = |b: u8| b == std::path::MAIN_SEPARATOR as u8 || (cfg!(windows) && b == b'/');
        while bytes.len() > 1 && is_sep(*bytes.last().unwrap()) {
            bytes.pop();
        }
    }
    bytes
}

/// Scan all roots and freeze the index.
pub fn scan(options: &WalkOptions) -> (Index, ScanReport) {
    let started = Instant::now();
    let builder = IndexBuilder::new();
    let errors = Errors::new();
    let excludes: Vec<Vec<Vec<u8>>> = options
        .excludes
        .iter()
        .filter_map(|p| std::path::absolute(p).ok())
        .map(|p| path_key(&p))
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
        // Roots are made absolute before scanning (without resolving
        // symlinks) so stored paths never depend on the working
        // directory of a later search.
        let abs = match std::path::absolute(root) {
            Ok(p) => p,
            Err(e) => {
                errors.add(format!("{}: {e}", root.display()));
                continue;
            }
        };
        // Excludes apply to roots too, including excluded ancestors:
        // naming a directory explicitly never re-includes it.
        if key_is_excluded(&path_key(&abs), &excludes) {
            continue;
        }
        let name = root_name_bytes(&abs);
        // symlink_metadata first: a root symlink is only followed when
        // the switch is on (fs::metadata would traverse it silently).
        let link_md = match std::fs::symlink_metadata(&abs) {
            Ok(md) => md,
            Err(e) => {
                errors.add(format!("{}: {e}", root.display()));
                continue;
            }
        };
        let is_symlink = link_md.file_type().is_symlink();
        let mut root_flags: u16 = 0;
        if is_symlink {
            root_flags |= flags::SYMLINK;
        }
        let is_dir = if is_symlink && !options.follow_symlinks {
            false
        } else if is_symlink {
            match std::fs::metadata(&abs) {
                Ok(md) => md.is_dir(),
                Err(e) => {
                    // Broken link: counted, kept as a leaf entry.
                    errors.add(format!("{}: {e}", root.display()));
                    false
                }
            }
        } else {
            link_md.is_dir()
        };
        if is_dir {
            root_flags |= flags::DIR;
        }
        let id = builder.add_root(&name, root_flags);
        if is_dir {
            root_dirs.push((abs, id));
        }
    }

    rayon::scope(|scope| {
        for (dir, id) in root_dirs {
            let ancestors = if walker.follow_symlinks {
                match std::fs::canonicalize(&dir) {
                    Ok(canonical) => vec![canonical],
                    Err(e) => {
                        walker.errors.add(format!("{}: {e}", dir.display()));
                        continue;
                    }
                }
            } else {
                Vec::new()
            };
            scope.spawn(move |s| walker.scan_dir(s, dir, id, ancestors));
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

    fn make_symlink_dir(original: &Path, link: &Path) -> bool {
        #[cfg(windows)]
        let result = std::os::windows::fs::symlink_dir(original, link);
        #[cfg(unix)]
        let result = std::os::unix::fs::symlink(original, link);
        match result {
            Ok(()) => true,
            Err(e) => {
                // Windows without Developer Mode refuses symlink
                // creation; skip loudly instead of faking a pass.
                eprintln!("SKIP: symlink creation refused ({e}); test not run");
                false
            }
        }
    }

    #[test]
    fn follow_symlinks_includes_target_children() {
        let root = TestDir::new("links-root");
        let target = TestDir::new("links-target");
        target.file("target-leaf.txt");
        if !make_symlink_dir(&target.0, &root.0.join("link")) {
            return;
        }
        let (idx, report) = scan(&WalkOptions {
            roots: vec![root.0.clone()],
            follow_symlinks: true,
            excludes: Vec::new(),
        });
        assert_eq!(report.errors, 0, "{:?}", report.error_samples);
        assert!(
            (0..idx.len() as u32).any(|id| idx.name(id) == b"target-leaf.txt"),
            "follow=true omitted target children"
        );
        // And with following off, the target children stay out.
        let (idx, _) = scan(&WalkOptions {
            roots: vec![root.0.clone()],
            follow_symlinks: false,
            excludes: Vec::new(),
        });
        assert!(
            !(0..idx.len() as u32).any(|id| idx.name(id) == b"target-leaf.txt"),
            "follow=false descended through a symlink"
        );
    }

    #[test]
    fn symlink_root_is_not_followed_when_disabled() {
        let parent = TestDir::new("root-link-parent");
        let target = TestDir::new("root-link-target");
        target.file("private-leaf.txt");
        let link = parent.0.join("root-link");
        if !make_symlink_dir(&target.0, &link) {
            return;
        }
        let (idx, _) = scan(&WalkOptions {
            roots: vec![link.clone()],
            follow_symlinks: false,
            excludes: Vec::new(),
        });
        assert!(
            !(0..idx.len() as u32).any(|id| idx.name(id) == b"private-leaf.txt"),
            "follow=false descended through root symlink"
        );
        // The root entry itself exists and is marked as a symlink.
        assert_eq!(idx.len(), 1);
        assert_ne!(idx.flags(0) & flags::SYMLINK, 0);
        // With following on, the same root is descended.
        let (idx, _) = scan(&WalkOptions {
            roots: vec![link],
            follow_symlinks: true,
            excludes: Vec::new(),
        });
        assert!(
            (0..idx.len() as u32).any(|id| idx.name(id) == b"private-leaf.txt"),
            "follow=true did not descend through root symlink"
        );
    }

    #[test]
    fn symlink_cycle_is_bounded() {
        let root = TestDir::new("cycle");
        root.file("f.txt");
        root.file("a/g.txt");
        if !make_symlink_dir(&root.0, &root.0.join("a").join("loop")) {
            return;
        }
        let (idx, report) = scan(&WalkOptions {
            roots: vec![root.0.clone()],
            follow_symlinks: true,
            excludes: Vec::new(),
        });
        // root, f.txt, a, g.txt, loop — the loop is a leaf, never descended.
        assert_eq!(idx.len(), 5, "cycle was not bounded");
        assert!(report.errors >= 1, "cycle must be counted: {report:?}");
    }

    #[test]
    fn excluded_root_is_not_indexed() {
        let dir = TestDir::new("excluded-root");
        dir.file("private.txt");
        let (idx, _) = scan(&WalkOptions {
            roots: vec![dir.0.clone()],
            excludes: vec![dir.0.clone()],
            follow_symlinks: false,
        });
        assert!(
            idx.is_empty(),
            "excluded root exposed {} entries",
            idx.len()
        );
    }

    #[test]
    fn excluded_ancestor_is_not_bypassed_by_explicit_root() {
        let dir = TestDir::new("excluded-ancestor");
        dir.file("sub/private.txt");
        let (idx, _) = scan(&WalkOptions {
            roots: vec![dir.0.join("sub")],
            excludes: vec![dir.0.clone()],
            follow_symlinks: false,
        });
        assert!(
            idx.is_empty(),
            "excluded subtree exposed {} entries",
            idx.len()
        );
    }

    #[cfg(windows)]
    #[test]
    fn excludes_compare_case_insensitively_on_windows() {
        let dir = TestDir::new("exclude-case");
        dir.file("KeepDir/keep.txt");
        dir.file("keep.txt");
        // Same directory, different case spelling.
        let (idx, _) = scan(&WalkOptions {
            roots: vec![dir.0.clone()],
            excludes: vec![dir.0.join("KEEPDIR")],
            follow_symlinks: false,
        });
        let names = by_name(&idx);
        assert!(names.contains_key("keep.txt"));
        assert!(
            !names.contains_key("KeepDir"),
            "case-insensitive exclude missed"
        );
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

    #[test]
    fn relative_roots_produce_absolute_paths() {
        static NEXT: AtomicUsize = AtomicUsize::new(500);
        let rel = PathBuf::from(format!(
            "ferrow-rel-root-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        assert!(rel.is_relative());
        std::fs::create_dir_all(&rel).unwrap();
        std::fs::write(rel.join("file.txt"), b"x").unwrap();
        let (idx, report) = scan(&WalkOptions {
            roots: vec![rel.clone()],
            ..WalkOptions::default()
        });
        let expected_root = std::path::absolute(&rel).unwrap();
        let _ = std::fs::remove_dir_all(&rel);
        assert_eq!(report.errors, 0, "{:?}", report.error_samples);
        assert!(
            idx.path(0).is_absolute(),
            "persisted relative path {:?}",
            idx.path(0)
        );
        assert_eq!(idx.path(0), expected_root);
        assert_eq!(idx.path(1), expected_root.join("file.txt"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_drive_root_keeps_its_separator() {
        assert_eq!(root_name_bytes(Path::new("E:\\")), b"E:\\");
        assert_eq!(root_name_bytes(Path::new("E:\\foo\\")), b"E:\\foo");
        assert_eq!(root_name_bytes(Path::new("E:/foo/")), b"E:/foo");
    }

    #[cfg(unix)]
    #[test]
    fn unix_filesystem_root_keeps_its_separator() {
        assert_eq!(root_name_bytes(Path::new("/")), b"/");
        assert_eq!(root_name_bytes(Path::new("/tmp/foo/")), b"/tmp/foo");
    }

    #[cfg(windows)]
    #[test]
    fn windows_root_with_lone_surrogate_is_lossless() {
        use std::os::windows::ffi::OsStringExt;
        let dir = TestDir::new("surrogate");
        let root = dir
            .0
            .join(std::ffi::OsString::from_wide(&[b'r' as u16, 0xd800]));
        std::fs::create_dir_all(&root).unwrap();
        let (idx, report) = scan(&WalkOptions {
            roots: vec![root.clone()],
            ..WalkOptions::default()
        });
        assert_eq!(report.errors, 0, "{:?}", report.error_samples);
        assert_eq!(idx.path(0), root);
    }

    #[cfg(unix)]
    #[test]
    fn unix_root_with_invalid_utf8_name_is_lossless() {
        use std::os::unix::ffi::OsStringExt;
        let dir = TestDir::new("nonutf8");
        let root = dir
            .0
            .join(std::ffi::OsString::from_vec(vec![b'r', 0xff, b'x']));
        if let Err(e) = std::fs::create_dir_all(&root) {
            // macOS (APFS) rejects non-UTF-8 names outright; skip loudly.
            eprintln!("SKIP: non-UTF-8 directory creation refused ({e}); test not run");
            return;
        }
        std::fs::write(root.join("file.txt"), b"x").unwrap();
        let (idx, report) = scan(&WalkOptions {
            roots: vec![root.clone()],
            ..WalkOptions::default()
        });
        assert_eq!(report.errors, 0, "{:?}", report.error_samples);
        assert_eq!(idx.path(0), root);
        assert_eq!(idx.path(1), root.join("file.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn unix_root_name_ending_in_backslash_is_kept() {
        let dir = TestDir::new("backslash");
        // A trailing backslash is a valid part of a Unix file name.
        let root = dir.0.join("dir\\");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("file.txt"), b"x").unwrap();
        let (idx, report) = scan(&WalkOptions {
            roots: vec![root.clone()],
            ..WalkOptions::default()
        });
        assert_eq!(report.errors, 0, "{:?}", report.error_samples);
        assert_eq!(idx.path(0), root);
        assert_eq!(idx.path(1), root.join("file.txt"));
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
