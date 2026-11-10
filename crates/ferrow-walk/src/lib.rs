//! Portable parallel directory-walk backend.
//!
//! Scans directory trees with `std::fs::read_dir` on the rayon pool and
//! feeds a [`ferrow_index::IndexBuilder`]. Permission and IO errors are
//! counted and sampled into the report, never fatal — with one
//! exception: an existing exclude that cannot be fully resolved aborts
//! the scan before any root is walked (fail closed, N6), reported in
//! [`ScanReport::fatal_error`].

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use ferrow_index::osname::name_to_bytes;
use ferrow_index::{ColumnSet, EntryMetadata, Index, IndexBuilder, flags, metadata_timestamp};

mod identity;
pub mod ntfs;

use identity::{FileId, file_id};

/// What to scan.
#[derive(Debug, Clone, Default)]
pub struct WalkOptions {
    /// Optional metadata arrays; defaults to size and modified time.
    pub columns: ColumnSet,
    /// Roots to include. Each becomes one root entry in the index.
    pub roots: Vec<PathBuf>,
    /// Paths to exclude (files or whole subtrees), matched by
    /// filesystem identity, so any spelling or alias of the same
    /// object excludes it.
    pub excludes: Vec<PathBuf>,
    /// Descend into symlinked directories. Off by default.
    pub follow_symlinks: bool,
    /// Read NTFS volumes' master file table directly when possible
    /// (needs administrator rights; seconds instead of minutes on whole
    /// drives). Roots it cannot serve are walked. Off by default.
    pub ntfs: NtfsMode,
}

/// When to read an NTFS volume's MFT instead of walking a root.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum NtfsMode {
    /// Always walk.
    #[default]
    Off,
    /// Roots that are whole volumes (`C:\`, a mounted volume's folder).
    /// The MFT read costs the same however small the root is, so folder
    /// roots are walked.
    VolumeRoots,
    /// Every root on an NTFS volume (tests).
    AllRoots,
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
    /// Set when the scan aborted before walking any root because an
    /// existing exclude could not be fully resolved. The index is
    /// empty and must not be persisted as a scan result.
    pub fatal_error: Option<String>,
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

fn from_std_metadata(md: &std::fs::Metadata, set: ColumnSet) -> EntryMetadata {
    let mut result = EntryMetadata::default();
    if set.contains(ColumnSet::SIZE) {
        result.size = Some(md.len());
    }
    if set.contains(ColumnSet::MODIFIED) {
        result.modified = md.modified().ok().and_then(metadata_timestamp);
    }
    if set.contains(ColumnSet::CREATED) {
        result.created = md.created().ok().and_then(metadata_timestamp);
    }
    if set.contains(ColumnSet::ACCESSED) {
        result.accessed = md.accessed().ok().and_then(metadata_timestamp);
    }
    if set.contains(ColumnSet::ATTRIBUTES) {
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            result.attributes = Some(md.file_attributes());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            result.attributes = Some(md.mode());
        }
    }
    result
}

fn collect_metadata(
    set: ColumnSet,
    read: impl FnOnce() -> std::io::Result<std::fs::Metadata>,
    path: &Path,
    errors: &Errors,
) -> EntryMetadata {
    if set == ColumnSet::NONE {
        return EntryMetadata::default();
    }
    match read() {
        Ok(md) => from_std_metadata(&md, set),
        Err(error) => {
            errors.add(format!("metadata {}: {error}", path.display()));
            EntryMetadata::default()
        }
    }
}

/// Excluded objects resolved to filesystem identities at scan start.
///
/// Directory excludes match by identity wherever the walk reaches
/// them — any spelling (normal, verbatim, differently cased) or
/// alias (symlink, junction) of the same directory is skipped with
/// everything below it. File excludes match by identity inside the
/// directory that contains the resolved exclude path, so identity lookups
/// stay at one per directory (plus files in such a directory).
#[derive(Default)]
struct ExcludeSet {
    dirs: HashSet<FileId>,
    files: HashSet<FileId>,
    files_by_parent: HashMap<FileId, HashSet<FileId>>,
    ancestors: Mutex<HashMap<FileId, bool>>,
}

impl ExcludeSet {
    fn is_empty(&self) -> bool {
        self.dirs.is_empty() && self.files.is_empty()
    }

    /// A real location is inside an excluded directory when any resolved
    /// ancestor has that identity. Cache only complete, successful results;
    /// never cache an IO failure as a negative match.
    fn excluded_ancestors(&self, resolved: &Path) -> std::io::Result<bool> {
        let mut visited = Vec::new();
        let mut excluded = false;
        for ancestor in resolved.ancestors() {
            let id = file_id(ancestor)?;
            if self.dirs.contains(&id) {
                excluded = true;
                break;
            }
            if let Some(cached) = self
                .ancestors
                .lock()
                .expect("ancestor cache poisoned")
                .get(&id)
            {
                excluded = *cached;
                break;
            }
            visited.push(id);
        }
        let mut cache = self.ancestors.lock().expect("ancestor cache poisoned");
        for id in visited {
            cache.insert(id, excluded);
        }
        Ok(excluded)
    }

    /// Shared location check for roots and aliases, including aliases kept
    /// as leaves. Canonical strings locate ancestors; only identities match.
    fn excluded_location(&self, path: &Path, id: Option<FileId>) -> std::io::Result<bool> {
        if self.is_empty() {
            return Ok(false);
        }
        if let Some(id) = id
            && (self.dirs.contains(&id) || self.files.contains(&id))
        {
            return Ok(true);
        }
        if self.dirs.is_empty() {
            return Ok(false);
        }
        // Check the containing location even if a trailing slash makes
        // metadata classify an alias root as its target. Resolve the parent
        // only for a named entry, never for a final `..`: that would make a
        // directory we are leaving look like an ancestor of the location.
        if matches!(
            path.components().next_back(),
            Some(std::path::Component::Normal(_))
        ) && let Some(parent) = path.parent()
            && self.excluded_ancestors(&std::fs::canonicalize(parent)?)?
        {
            return Ok(true);
        }
        if let Some(id) = id
            && let Some(cached) = self
                .ancestors
                .lock()
                .expect("ancestor cache poisoned")
                .get(&id)
        {
            return Ok(*cached);
        }
        let resolved = std::fs::canonicalize(path)?;
        // Only directories have a unique ancestor chain. A file identity
        // may have hard-linked names in different containing directories;
        // never cache a location result under that file's identity.
        let location = if std::fs::metadata(&resolved)?.is_dir() {
            resolved.as_path()
        } else {
            resolved.parent().unwrap_or(&resolved)
        };
        self.excluded_ancestors(location)
    }
}

/// Resolve every exclude path that exists to its identity. A path
/// that does not exist at scan start is reported as
/// `exclude not found: …` and otherwise ignored.
///
/// Fail closed (N6): any other failure to fully resolve an existing
/// exclude — its metadata, identity, canonical location, or (for a
/// file exclude) its parent identity — is an `Err` naming the exclude.
/// The caller must not scan with a partial set: a dropped or
/// parentless exclude would silently admit excluded entries.
fn resolve_excludes(paths: &[PathBuf], errors: &Errors) -> Result<ExcludeSet, String> {
    let mut set = ExcludeSet::default();
    for path in paths {
        // Establish existence independently of identity opening: a provider
        // may fail an identity query with NotFound even for an existing path.
        let md = match std::fs::metadata(path) {
            Ok(md) => md,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                errors.add(format!("exclude not found: {}", path.display()));
                continue;
            }
            Err(e) => return Err(format!("exclude {}: {e}", path.display())),
        };
        let id = match file_id(path) {
            Ok(id) => id,
            Err(e) => return Err(format!("exclude identity {}: {e}", path.display())),
        };
        if md.is_dir() {
            set.dirs.insert(id);
        } else {
            set.files.insert(id);
            let resolved = match std::fs::canonicalize(path) {
                Ok(resolved) => resolved,
                Err(e) => return Err(format!("exclude location {}: {e}", path.display())),
            };
            let parent = match resolved.parent() {
                Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
                _ => PathBuf::from("."),
            };
            match file_id(&parent) {
                Ok(parent_id) => {
                    set.files_by_parent.entry(parent_id).or_default().insert(id);
                }
                Err(e) => {
                    return Err(format!(
                        "exclude parent identity {} (parent {}): {e}",
                        path.display(),
                        parent.display()
                    ));
                }
            }
        }
    }
    Ok(set)
}

/// Windows junctions and other name-surrogate reparse points are aliases,
/// even when FileType::is_symlink does not classify them as symlinks.
fn is_link(path: &Path, md: &std::fs::Metadata) -> std::io::Result<bool> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if md.file_attributes() & 0x400 == 0 {
            return Ok(false);
        }
        identity::is_name_surrogate(path)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Ok(md.file_type().is_symlink())
    }
}

#[derive(Clone, Copy)]
struct Walker<'a> {
    columns: ColumnSet,
    builder: &'a IndexBuilder,
    excludes: &'a ExcludeSet,
    follow_symlinks: bool,
    errors: &'a Errors,
}

impl<'x> Walker<'x> {
    /// The single exclude decision for one enumerated candidate,
    /// shared by ordinary directories, links and aliases (followed
    /// or leaf) and the parent-scoped file case. Returns whether the
    /// candidate must be skipped before it is added, plus its
    /// resolved identity when one was obtained (for descent, cycle
    /// detection and contained file excludes).
    ///
    /// Fail-closed (N5, owner decision 2026-11-06): while excludes
    /// are active, an identity or location error means exclusion
    /// cannot be determined, so the candidate is skipped and the
    /// error is counted exactly once here. With no excludes the
    /// lookup only feeds symlink-cycle detection; an error is
    /// counted and the candidate is kept (as a leaf when following,
    /// handled by the caller).
    fn candidate_excluded(
        self,
        path: &Path,
        is_dir: bool,
        is_symlink: bool,
        dir_identity: Option<FileId>,
    ) -> (bool, Option<FileId>) {
        if is_dir || is_symlink {
            // Directories are resolved once, before descending: the
            // same lookup serves the exclude check, symlink-cycle
            // detection and contained file excludes in the child.
            if !(self.follow_symlinks || !self.excludes.is_empty()) {
                return (false, None);
            }
            match file_id(path) {
                Ok(id) => {
                    if is_symlink {
                        match self.excludes.excluded_location(path, Some(id)) {
                            Ok(excluded) => (excluded, Some(id)),
                            Err(e) => {
                                self.errors.add(format!("{}: {e}", path.display()));
                                (true, Some(id))
                            }
                        }
                    } else {
                        // A real parent-to-child edge inherits the
                        // already-checked parent chain: one lookup.
                        (self.excludes.dirs.contains(&id), Some(id))
                    }
                }
                Err(e) => {
                    self.errors.add(format!("{}: {e}", path.display()));
                    (!self.excludes.is_empty(), None)
                }
            }
        } else if let Some(parent_id) = dir_identity
            && let Some(files) = self.excludes.files_by_parent.get(&parent_id)
        {
            // Files are only resolved when their containing
            // directory holds a file exclude, so excludes are
            // active here by construction: an identity error
            // fails closed.
            match file_id(path) {
                Ok(id) => (files.contains(&id), None),
                Err(e) => {
                    self.errors.add(format!("{}: {e}", path.display()));
                    (true, None)
                }
            }
        } else {
            (false, None)
        }
    }

    /// `ancestors` holds the filesystem identities of the directory
    /// chain from the root down to and including `dir`. It is only
    /// populated when following symlinks, where it bounds cycles: a
    /// directory whose identity is already an ancestor is not
    /// descended. `dir_identity` is `dir`'s own identity when it was
    /// resolved (for exclude matching or following), reused here for
    /// contained file excludes so no second lookup happens.
    fn scan_dir<'scope>(
        self,
        scope: &rayon::Scope<'scope>,
        dir: PathBuf,
        dir_id: u32,
        ancestors: Vec<FileId>,
        dir_identity: Option<FileId>,
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
            #[cfg(windows)]
            let is_symlink = match entry.metadata() {
                Ok(md) => match is_link(&path, &md) {
                    Ok(value) => value,
                    Err(e) => {
                        self.errors.add(format!("{}: {e}", path.display()));
                        continue;
                    }
                },
                Err(e) => {
                    self.errors.add(format!("{}: {e}", path.display()));
                    continue;
                }
            };
            #[cfg(not(windows))]
            let is_symlink = file_type.is_symlink();
            // file_type() never follows links: a symlinked directory is
            // a leaf unless following is on, in which case fs::metadata
            // (which traverses) decides. DirEntry::metadata() does NOT
            // traverse links, so it cannot be used for this.
            let mut is_dir = file_type.is_dir() && !is_symlink;
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
            // Exclude matching by filesystem identity, through the
            // single fail-closed decision (see `candidate_excluded`).
            let (skip, child_identity) =
                self.candidate_excluded(&path, is_dir, is_symlink, dir_identity);
            if skip {
                continue;
            }
            let metadata = collect_metadata(self.columns, || entry.metadata(), &path, self.errors);
            let id = self
                .builder
                .add_with_metadata(dir_id, &name, ent_flags, metadata);
            if is_dir {
                let mut child_ancestors = ancestors.clone();
                if self.follow_symlinks {
                    match child_identity {
                        Some(identity) => {
                            if ancestors.contains(&identity) {
                                self.errors.add(format!(
                                    "{}: symlink cycle (already an ancestor)",
                                    path.display()
                                ));
                                continue;
                            }
                            child_ancestors.push(identity);
                        }
                        // Identity error already counted above; the
                        // entry stays a leaf.
                        None => continue,
                    }
                }
                scope.spawn(move |s| self.scan_dir(s, path, id, child_ancestors, child_identity));
            }
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

/// Which index entries an NTFS volume's MFT records became, so a live
/// updater can apply the volume's change journal to the scan result.
#[derive(Debug, Clone, Default)]
pub struct VolumeMap {
    /// `\\?\Volume{…}` device path.
    pub device: String,
    pub serial: u64,
    /// Change journal id and the position to continue reading from.
    pub journal: Option<(u64, i64)>,
    pub record_size: usize,
    /// `(record number, entry id)` for every entry from this volume,
    /// including root entries. A record with several names (hard links)
    /// appears once per name.
    pub entries: Vec<(u64, u32)>,
    /// Record numbers of excluded files and directories on this volume.
    pub excluded: Vec<u64>,
    /// Root entries filled from this volume: `(root record, entry id)`.
    pub roots: Vec<(u64, u32)>,
}

/// Scan all roots and freeze the index.
pub fn scan(options: &WalkOptions) -> (Index, ScanReport) {
    let (index, report, _) = scan_with_maps(options);
    (index, report)
}

/// [`scan`], also returning the record-to-entry maps of every volume
/// that was read through its MFT.
pub fn scan_with_maps(options: &WalkOptions) -> (Index, ScanReport, Vec<VolumeMap>) {
    let started = Instant::now();
    let builder = IndexBuilder::with_columns(options.columns);
    let errors = Errors::new();
    let excludes = match resolve_excludes(&options.excludes, &errors) {
        Ok(set) => set,
        Err(fatal) => {
            // Fail closed (N6): an existing exclude could not be fully
            // resolved, so no entry's exclusion status is known. Index
            // nothing; do not walk any root.
            errors.add(fatal.clone());
            let report = ScanReport {
                mode: "parallel read_dir".to_string(),
                threads: rayon::current_num_threads(),
                entries: 0,
                errors: errors.count.load(Ordering::Relaxed),
                error_samples: errors.samples.into_inner().expect("error samples poisoned"),
                fatal_error: Some(fatal),
                elapsed: started.elapsed(),
            };
            return (builder.freeze(), report, Vec::new());
        }
    };
    let walker = Walker {
        columns: options.columns,
        builder: &builder,
        excludes: &excludes,
        follow_symlinks: options.follow_symlinks,
        errors: &errors,
    };

    // Roots are added sequentially (cheap), their subtrees in parallel.
    let mut root_dirs: Vec<(PathBuf, u32, Option<FileId>)> = Vec::new();
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
        // naming a directory explicitly never re-includes it. The
        // root identity (resolved when excludes or following need it)
        // is reused for the subtree walk and cycle detection.
        let need_identity = options.follow_symlinks || !excludes.is_empty();
        let identity_result = need_identity.then(|| file_id(&abs));
        let root_identity = identity_result
            .as_ref()
            .and_then(|r| r.as_ref().ok())
            .copied();
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
        if let Some(Err(e)) = &identity_result {
            // The root exists but its identity could not be resolved;
            // excludes and cycle detection cannot see it. Counted,
            // never silent. With excludes active this fails closed
            // (N5): the root is not indexed. Without excludes the
            // root is kept; when following, the missing identity
            // still stops descent below.
            errors.add(format!("{}: {e}", root.display()));
            if !excludes.is_empty() {
                continue;
            }
        }
        let is_symlink = match is_link(&abs, &link_md) {
            Ok(value) => value,
            Err(e) => {
                errors.add(format!("{}: {e}", root.display()));
                continue;
            }
        };
        match excludes.excluded_location(&abs, root_identity) {
            Ok(true) => continue,
            Ok(false) => {}
            Err(e) => {
                errors.add(format!("{}: {e}", root.display()));
                continue;
            }
        }
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
        let metadata = from_std_metadata(&link_md, options.columns);
        let id = builder.add_with_metadata(ferrow_index::ROOT_PARENT, &name, root_flags, metadata);
        if is_dir {
            root_dirs.push((abs, id, root_identity));
        }
    }

    // Fast path: roots on NTFS volumes are filled from the volume's MFT
    // when it can be read (administrator or SYSTEM); the rest are walked.
    let mut modes: Vec<String> = Vec::new();
    #[allow(unused_mut, reason = "only filled on Windows")]
    let mut maps: Vec<VolumeMap> = Vec::new();
    #[cfg(windows)]
    let root_dirs = if options.follow_symlinks || options.ntfs == NtfsMode::Off {
        root_dirs
    } else {
        let (mft, walk): (Vec<_>, Vec<_>) = root_dirs
            .into_iter()
            .partition(|(dir, _, _)| options.ntfs == NtfsMode::AllRoots || is_volume_root(dir));
        let mut rest = scan_from_mft(mft, &walker, &mut modes, &mut maps);
        rest.extend(walk);
        rest
    };
    if !root_dirs.is_empty() {
        modes.push("parallel read_dir".to_string());
    }

    rayon::scope(|scope| {
        for (dir, id, identity) in root_dirs {
            let ancestors = if walker.follow_symlinks {
                match identity {
                    Some(identity) => vec![identity],
                    // Identity error already counted; cannot bound
                    // cycles below this root, so do not descend.
                    None => continue,
                }
            } else {
                Vec::new()
            };
            scope.spawn(move |s| walker.scan_dir(s, dir, id, ancestors, identity));
        }
    });

    let entries = builder.len();
    let index = builder.freeze();
    if modes.is_empty() {
        modes.push("parallel read_dir".to_string());
    }
    let report = ScanReport {
        mode: modes.join(" + "),
        threads: rayon::current_num_threads(),
        entries,
        errors: errors.count.load(Ordering::Relaxed),
        error_samples: errors.samples.into_inner().expect("error samples poisoned"),
        fatal_error: None,
        elapsed: started.elapsed(),
    };
    (index, report, maps)
}

/// Fill every root on a readable NTFS volume from its MFT (one read per
/// volume) and return the roots that still need the directory walk.
#[cfg(windows)]
fn scan_from_mft(
    root_dirs: Vec<(PathBuf, u32, Option<FileId>)>,
    walker: &Walker<'_>,
    modes: &mut Vec<String>,
    maps: &mut Vec<VolumeMap>,
) -> Vec<(PathBuf, u32, Option<FileId>)> {
    let mut pending: Vec<(PathBuf, u32, Option<FileId>)> = root_dirs;
    let mut walk = Vec::new();
    while let Some((dir, id, identity)) = pending.pop() {
        let started = Instant::now();
        let (device, volume) = match ntfs::read_volume(&dir) {
            Ok(v) => v,
            Err(reason) => {
                modes.push(format!("walk {} ({reason})", dir.display()));
                walk.push((dir, id, identity));
                continue;
            }
        };
        let read = started.elapsed();
        // Every other pending root on the same volume reuses this read.
        let mut here = vec![(dir, id, identity)];
        let mut rest = Vec::new();
        for root in pending.drain(..) {
            if ntfs_device_of(&root.0).as_deref() == Some(device.as_str()) {
                here.push(root);
            } else {
                rest.push(root);
            }
        }
        pending = rest;
        let mut entries = 0usize;
        let mut map = VolumeMap {
            device: device.clone(),
            serial: volume.serial,
            journal: volume.journal,
            record_size: volume.record_size,
            excluded: walker
                .excludes
                .dirs
                .iter()
                .chain(&walker.excludes.files)
                .filter(|id| id.kind == 1 && id.volume == volume.serial)
                .map(|id| (id.id as u64) & 0x0000_FFFF_FFFF_FFFF)
                .collect(),
            ..VolumeMap::default()
        };
        for (dir, id, identity) in here {
            let identity = match identity.map(Ok).unwrap_or_else(|| file_id(&dir)) {
                Ok(identity) => identity,
                Err(e) => {
                    walker.errors.add(format!("{}: {e}", dir.display()));
                    walk.push((dir, id, None));
                    continue;
                }
            };
            if identity.kind != 1 || identity.volume != volume.serial {
                walk.push((dir, id, Some(identity)));
                continue;
            }
            let record = (identity.id as u64) & 0x0000_FFFF_FFFF_FFFF;
            if volume.info(record).is_none() {
                walk.push((dir, id, Some(identity)));
                continue;
            }
            map.roots.push((record, id));
            map.entries.push((record, id));
            entries += emit_mft_subtree(&volume, record, id, &dir, walker, &mut map.entries);
        }
        if !map.roots.is_empty() {
            maps.push(map);
        }
        modes.push(format!(
            "mft {device} ({} records read in {:.2} s, {entries} entries)",
            volume.infos.len(),
            read.as_secs_f64()
        ));
    }
    walk
}

/// Whether `path` is the root of a volume (a drive or a mounted volume).
#[cfg(windows)]
pub fn is_volume_root(path: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetVolumePathNameW;
    let path_w: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut mount = [0u16; 1024];
    // SAFETY: NUL-terminated input; output buffer and its length match.
    if unsafe { GetVolumePathNameW(path_w.as_ptr(), mount.as_mut_ptr(), mount.len() as u32) } == 0 {
        return false;
    }
    let len = mount.iter().position(|&c| c == 0).unwrap_or(0);
    let trim = |s: &[u16]| {
        let mut s = s.to_vec();
        while s
            .last()
            .is_some_and(|&c| c == u16::from(b'\\') || c == u16::from(b'/'))
        {
            s.pop();
        }
        String::from_utf16_lossy(&s).to_lowercase()
    };
    let given: Vec<u16> = path.as_os_str().encode_wide().collect();
    trim(&mount[..len]) == trim(&given)
}

#[cfg(windows)]
fn ntfs_device_of(path: &Path) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        GetVolumeNameForVolumeMountPointW, GetVolumePathNameW,
    };
    let path_w: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut mount = [0u16; 1024];
    let mut guid = [0u16; 128];
    // SAFETY: NUL-terminated inputs; outputs and their lengths match.
    let ok = unsafe {
        GetVolumePathNameW(path_w.as_ptr(), mount.as_mut_ptr(), mount.len() as u32) != 0
            && GetVolumeNameForVolumeMountPointW(
                mount.as_ptr(),
                guid.as_mut_ptr(),
                guid.len() as u32,
            ) != 0
    };
    if !ok {
        return None;
    }
    let len = guid.iter().position(|&c| c == 0).unwrap_or(0);
    let mut device = String::from_utf16_lossy(&guid[..len]);
    while device.ends_with('\\') {
        device.pop();
    }
    Some(device)
}

/// Add everything below directory `record` (already added as `root_id`)
/// to the builder, applying excludes like the walk does. Returns the
/// number of entries added.
#[cfg(windows)]
fn emit_mft_subtree(
    volume: &ntfs::Volume,
    record: u64,
    root_id: u32,
    root_path: &Path,
    walker: &Walker<'_>,
    pairs: &mut Vec<(u64, u32)>,
) -> usize {
    let excluding = !walker.excludes.is_empty();
    let mut added = 0usize;
    // Paths are only needed to resolve link targets against excludes.
    let mut stack: Vec<(u64, u32, Option<PathBuf>)> =
        vec![(record, root_id, excluding.then(|| root_path.to_path_buf()))];
    while let Some((dir, dir_id, dir_path)) = stack.pop() {
        for (name, bytes) in volume.children(dir) {
            let Some(info) = volume.info(name.record) else {
                continue;
            };
            let surrogate = info.is_name_surrogate();
            let is_dir = info.is_descended_dir();
            let ent_flags = info.entry_flags();
            let child_path = dir_path
                .as_ref()
                .map(|p| p.join(ferrow_index::osname::bytes_to_os(bytes)));
            if excluding {
                let identity = FileId {
                    kind: 1,
                    volume: volume.serial,
                    id: (name.record | (info.sequence as u64) << 48) as u128,
                };
                let skip = if is_dir {
                    walker.excludes.dirs.contains(&identity)
                } else if surrogate {
                    let path = child_path.as_deref().expect("paths kept while excluding");
                    walker.candidate_excluded(path, false, true, None).0
                } else {
                    walker.excludes.files.contains(&identity)
                };
                if skip {
                    continue;
                }
            }
            let id = walker.builder.add_with_metadata(
                dir_id,
                bytes,
                ent_flags,
                info.metadata(walker.columns),
            );
            added += 1;
            pairs.push((name.record, id));
            if is_dir {
                stack.push((name.record, id, child_path));
            }
        }
    }
    added
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrow_index::ROOT_PARENT;

    #[cfg(windows)]
    #[test]
    fn volume_roots_are_recognised() {
        let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        assert!(is_volume_root(Path::new(&format!("{system}\\"))));
        assert!(is_volume_root(Path::new(&system.to_lowercase())));
        assert!(!is_volume_root(&std::env::temp_dir()));
    }
    use std::collections::HashMap;

    #[test]
    fn metadata_disabled_never_requests_a_record_and_errors_stay_unknown() {
        let errors = Errors::new();
        let metadata = collect_metadata(
            ColumnSet::NONE,
            || panic!("disabled metadata requested"),
            Path::new("unused"),
            &errors,
        );
        assert_eq!(metadata, EntryMetadata::default());
        let metadata = collect_metadata(
            ColumnSet::ALL,
            || Err(std::io::Error::other("mock unavailable")),
            Path::new("mock"),
            &errors,
        );
        assert_eq!(metadata, EntryMetadata::default());
        assert_eq!(errors.count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn enabled_columns_match_filesystem_metadata() {
        let tree = TestDir::new("metadata");
        let path = tree.0.join("metadata.txt");
        std::fs::write(&path, b"metadata bytes").unwrap();
        let expected =
            from_std_metadata(&std::fs::symlink_metadata(&path).unwrap(), ColumnSet::ALL);
        for columns in [ColumnSet::NONE, ColumnSet::DEFAULT, ColumnSet::ALL] {
            let (index, report) = scan(&WalkOptions {
                roots: vec![path.clone()],
                columns,
                ..WalkOptions::default()
            });
            assert_eq!(report.errors, 0);
            assert_eq!(index.columns_present(), columns);
            let metadata = index.metadata(0);
            if columns == ColumnSet::NONE {
                assert_eq!(metadata, EntryMetadata::default());
                assert_eq!(index.metadata_heap_bytes(), 0);
            } else {
                assert_eq!(metadata.size, Some(14));
                assert_eq!(metadata.modified, expected.modified);
            }
            if columns == ColumnSet::ALL {
                assert_eq!(metadata, expected);
            }
        }
        let (index, report) = scan(&WalkOptions {
            roots: vec![tree.0.clone()],
            ..WalkOptions::default()
        });
        assert_eq!(report.errors, 0);
        let id = (0..index.len() as u32)
            .find(|&id| index.name(id) == b"metadata.txt")
            .unwrap();
        assert_eq!(index.metadata(id).size, Some(14));
        assert_eq!(index.metadata(id).modified, expected.modified);
    }

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

    fn make_symlink_dir(original: &Path, link: &Path) {
        #[cfg(windows)]
        let result = std::os::windows::fs::symlink_dir(original, link);
        #[cfg(unix)]
        let result = std::os::unix::fs::symlink(original, link);
        // Fixture refusals fail locally and on CI. A real unsupported
        // platform must use cfg or an explicit ignored test instead.
        result.expect("symlink fixture creation refused");
    }

    #[test]
    fn follow_symlinks_includes_target_children() {
        let root = TestDir::new("links-root");
        let target = TestDir::new("links-target");
        target.file("target-leaf.txt");
        make_symlink_dir(&target.0, &root.0.join("link"));
        let (idx, report) = scan(&WalkOptions {
            columns: Default::default(),
            roots: vec![root.0.clone()],
            follow_symlinks: true,
            excludes: Vec::new(),
            ntfs: NtfsMode::Off,
        });
        assert_eq!(report.errors, 0, "{:?}", report.error_samples);
        assert!(
            (0..idx.len() as u32).any(|id| idx.name(id) == b"target-leaf.txt"),
            "follow=true omitted target children"
        );
        // And with following off, the target children stay out.
        let (idx, _) = scan(&WalkOptions {
            columns: Default::default(),
            roots: vec![root.0.clone()],
            follow_symlinks: false,
            excludes: Vec::new(),
            ntfs: NtfsMode::Off,
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
        make_symlink_dir(&target.0, &link);
        let (idx, _) = scan(&WalkOptions {
            columns: Default::default(),
            roots: vec![link.clone()],
            follow_symlinks: false,
            excludes: Vec::new(),
            ntfs: NtfsMode::Off,
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
            columns: Default::default(),
            roots: vec![link],
            follow_symlinks: true,
            excludes: Vec::new(),
            ntfs: NtfsMode::Off,
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
        make_symlink_dir(&root.0, &root.0.join("a").join("loop"));
        let (idx, report) = scan(&WalkOptions {
            columns: Default::default(),
            roots: vec![root.0.clone()],
            follow_symlinks: true,
            excludes: Vec::new(),
            ntfs: NtfsMode::Off,
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
            columns: Default::default(),
            roots: vec![dir.0.clone()],
            excludes: vec![dir.0.clone()],
            follow_symlinks: false,
            ntfs: NtfsMode::Off,
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
            columns: Default::default(),
            roots: vec![dir.0.join("sub")],
            excludes: vec![dir.0.clone()],
            follow_symlinks: false,
            ntfs: NtfsMode::Off,
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
            columns: Default::default(),
            roots: vec![dir.0.clone()],
            excludes: vec![dir.0.join("KEEPDIR")],
            follow_symlinks: false,
            ntfs: NtfsMode::Off,
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
            columns: Default::default(),
            roots: vec![dir.0.clone()],
            excludes: vec![dir.0.join("skip")],
            follow_symlinks: false,
            ntfs: NtfsMode::Off,
        });
        let names = by_name(&idx);
        assert!(names.contains_key("keep.txt"));
        assert!(!names.contains_key("gone.txt"));
        assert!(!names.contains_key("skip"));
    }

    #[cfg(windows)]
    #[test]
    fn excludes_match_verbatim_and_normal_windows_paths() {
        let dir = TestDir::new("verbatim-excluded");
        dir.file("private.txt");
        let verbatim = std::fs::canonicalize(&dir.0).unwrap();
        assert!(verbatim.to_string_lossy().starts_with(r"\\?\"));
        // Both spelling directions name the same directory.
        for (root, exclude) in [(verbatim.clone(), dir.0.clone()), (dir.0.clone(), verbatim)] {
            let (idx, report) = scan(&WalkOptions {
                columns: Default::default(),
                roots: vec![root],
                excludes: vec![exclude],
                follow_symlinks: false,
                ntfs: NtfsMode::Off,
            });
            assert!(
                idx.is_empty(),
                "excluded root exposed {} entries: {report:?}",
                idx.len()
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn excludes_fold_unicode_even_in_wtf8_components() {
        use std::os::windows::ffi::OsStringExt;
        let parent = TestDir::new("wtf8-excluded");
        // Ü (U+00DC) with a lone surrogate, vs ü (U+00FC) likewise.
        // Both creation spellings, both root/exclude directions.
        for (i, (real, alternate)) in [
            ([0x00dcu16, 0xd800], [0x00fcu16, 0xd800]),
            ([0x00fcu16, 0xd800], [0x00dcu16, 0xd800]),
        ]
        .iter()
        .enumerate()
        {
            let case = parent.0.join(format!("case{i}"));
            let root = case.join(std::ffi::OsString::from_wide(real));
            let alt = case.join(std::ffi::OsString::from_wide(alternate));
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(root.join("private.txt"), b"x").unwrap();
            // Precondition: the filesystem resolves both spellings to
            // the same directory. Without it this test proves nothing.
            assert!(
                alt.join("private.txt").exists(),
                "precondition: OS resolves both spellings"
            );
            for (scan_root, exclude) in [(root.clone(), alt.clone()), (alt, root)] {
                let (idx, report) = scan(&WalkOptions {
                    columns: Default::default(),
                    roots: vec![scan_root],
                    excludes: vec![exclude],
                    follow_symlinks: false,
                    ntfs: NtfsMode::Off,
                });
                assert!(
                    idx.is_empty(),
                    "excluded root exposed {} entries: {report:?}",
                    idx.len()
                );
            }
        }
    }

    #[test]
    fn missing_exclude_is_reported_and_ignored() {
        let dir = TestDir::new("missing-exclude");
        dir.file("keep.txt");
        let (idx, report) = scan(&WalkOptions {
            columns: Default::default(),
            roots: vec![dir.0.clone()],
            excludes: vec![dir.0.join("no-such-exclude")],
            follow_symlinks: false,
            ntfs: NtfsMode::Off,
        });
        assert!(by_name(&idx).contains_key("keep.txt"));
        assert_eq!(report.fatal_error, None, "missing exclude must not abort");
        assert!(
            report
                .error_samples
                .iter()
                .any(|s| s.contains("exclude not found:")),
            "missing exclude not reported: {report:?}"
        );
    }

    #[test]
    fn excludes_single_file() {
        let dir = TestDir::new("exclude-file");
        dir.file("keep.txt");
        dir.file("secret.txt");
        let (idx, _) = scan(&WalkOptions {
            columns: Default::default(),
            roots: vec![dir.0.clone()],
            excludes: vec![dir.0.join("secret.txt")],
            follow_symlinks: false,
            ntfs: NtfsMode::Off,
        });
        let names = by_name(&idx);
        assert!(names.contains_key("keep.txt"));
        assert!(!names.contains_key("secret.txt"));
    }

    /// Serializes the N5 identity-refusal seam across parallel tests
    /// and always clears it, even on panic.
    struct IdentityRefusal {
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl IdentityRefusal {
        fn new(inject: bool) -> IdentityRefusal {
            static LOCK: Mutex<()> = Mutex::new(());
            let lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
            identity::TEST_IDENTITY_REFUSAL.store(inject, Ordering::Relaxed);
            IdentityRefusal { _lock: lock }
        }
    }

    impl Drop for IdentityRefusal {
        fn drop(&mut self) {
            identity::TEST_IDENTITY_REFUSAL.store(false, Ordering::Relaxed);
        }
    }

    fn make_symlink_file(original: &Path, link: &Path) {
        #[cfg(windows)]
        let result = std::os::windows::fs::symlink_file(original, link);
        #[cfg(unix)]
        let result = std::os::unix::fs::symlink(original, link);
        result.expect("symlink fixture creation refused");
    }

    /// N5 fixture: a `deny-identity` directory under `root` (holding
    /// `private.txt`), the same directory reached as alias
    /// `aliases/deny-identity`, and an exclude spelled through a
    /// separate alias (`exclude-alias`) so exclude resolution at scan
    /// start succeeds while candidate identity queries are refused.
    struct N5Fixture {
        _root_dir: TestDir,
        _alias_dir: TestDir,
        _exclude_dir: TestDir,
        root: PathBuf,
        aliases: PathBuf,
        exclude: PathBuf,
    }

    fn n5_fixture(tag: &str) -> N5Fixture {
        let root_dir = TestDir::new(&format!("n5-{tag}-root"));
        let private = root_dir.0.join(identity::TEST_REFUSED_NAME);
        std::fs::create_dir_all(&private).unwrap();
        std::fs::write(private.join("private.txt"), b"x").unwrap();
        let exclude_dir = TestDir::new(&format!("n5-{tag}-exclude"));
        let exclude = exclude_dir.0.join("exclude-alias");
        make_symlink_dir(&private, &exclude);
        let alias_dir = TestDir::new(&format!("n5-{tag}-aliases"));
        make_symlink_dir(&private, &alias_dir.0.join(identity::TEST_REFUSED_NAME));
        N5Fixture {
            root: root_dir.0.clone(),
            aliases: alias_dir.0.clone(),
            exclude,
            _root_dir: root_dir,
            _alias_dir: alias_dir,
            _exclude_dir: exclude_dir,
        }
    }

    #[test]
    fn identity_refusal_fails_closed_for_excluded_directory() {
        let _refusal = IdentityRefusal::new(true);
        let fx = n5_fixture("dir");
        for follow in [false, true] {
            let (idx, report) = scan(&WalkOptions {
                columns: Default::default(),
                roots: vec![fx.root.clone()],
                excludes: vec![fx.exclude.clone()],
                follow_symlinks: follow,
                ntfs: NtfsMode::Off,
            });
            assert_eq!(
                report.errors, 1,
                "follow={follow}: only the candidate identity is refused: {report:?}"
            );
            assert_eq!(
                idx.len(),
                1,
                "follow={follow}: excluded directory admitted after identity error"
            );
        }
    }

    #[test]
    fn identity_refusal_fails_closed_for_excluded_alias() {
        let _refusal = IdentityRefusal::new(true);
        let fx = n5_fixture("alias");
        for follow in [false, true] {
            let (idx, report) = scan(&WalkOptions {
                columns: Default::default(),
                roots: vec![fx.aliases.clone()],
                excludes: vec![fx.exclude.clone()],
                follow_symlinks: follow,
                ntfs: NtfsMode::Off,
            });
            assert_eq!(
                report.errors, 1,
                "follow={follow}: only the candidate identity is refused: {report:?}"
            );
            assert_eq!(
                idx.len(),
                1,
                "follow={follow}: excluded alias admitted after identity error"
            );
        }
    }

    #[test]
    fn identity_refusal_fails_closed_for_excluded_file() {
        let _refusal = IdentityRefusal::new(true);
        let root_dir = TestDir::new("n5-file-root");
        let file = root_dir.0.join(identity::TEST_REFUSED_NAME);
        std::fs::write(&file, b"x").unwrap();
        let exclude_dir = TestDir::new("n5-file-exclude");
        let exclude = exclude_dir.0.join("exclude-file");
        make_symlink_file(&file, &exclude);
        for follow in [false, true] {
            let (idx, report) = scan(&WalkOptions {
                columns: Default::default(),
                roots: vec![root_dir.0.clone()],
                excludes: vec![exclude.clone()],
                follow_symlinks: follow,
                ntfs: NtfsMode::Off,
            });
            assert_eq!(
                report.errors, 1,
                "follow={follow}: only the candidate identity is refused: {report:?}"
            );
            assert_eq!(
                idx.len(),
                1,
                "follow={follow}: excluded file admitted after identity error"
            );
        }
    }

    #[test]
    fn identity_refusal_control_without_refusal_is_unchanged() {
        let _refusal = IdentityRefusal::new(false);
        let fx = n5_fixture("control");
        for root in [fx.root.clone(), fx.aliases.clone()] {
            for follow in [false, true] {
                let (idx, report) = scan(&WalkOptions {
                    columns: Default::default(),
                    roots: vec![root.clone()],
                    excludes: vec![fx.exclude.clone()],
                    follow_symlinks: follow,
                    ntfs: NtfsMode::Off,
                });
                assert_eq!(report.errors, 0, "follow={follow}: {report:?}");
                assert_eq!(idx.len(), 1, "follow={follow}");
            }
        }
    }

    /// N6: the exclude itself is a `deny-identity` alias of
    /// `root/private`, so existence metadata succeeds but resolving
    /// the exclude's identity is refused. An existing exclude that
    /// cannot be fully resolved aborts the scan: nothing is indexed,
    /// alone or together with another valid exclude, with following
    /// off or on, for the containing root and the excluded root.
    #[test]
    fn unresolvable_directory_exclude_aborts_scan() {
        let _refusal = IdentityRefusal::new(false);
        let base = TestDir::new("n6-exclude-identity");
        let root = base.0.join("root");
        let private = root.join("private");
        std::fs::create_dir_all(&private).unwrap();
        std::fs::write(private.join("secret.txt"), b"x").unwrap();
        let exclude = base.0.join(identity::TEST_REFUSED_NAME);
        make_symlink_dir(&private, &exclude);
        let unrelated = base.0.join("unrelated");
        std::fs::create_dir(&unrelated).unwrap();
        let set_refusal = |on: bool| identity::TEST_IDENTITY_REFUSAL.store(on, Ordering::Relaxed);
        for extra in [false, true] {
            for follow in [false, true] {
                let excludes = if extra {
                    vec![exclude.clone(), unrelated.clone()]
                } else {
                    vec![exclude.clone()]
                };
                // Control: without refusal the exclude resolves and
                // only the root itself is indexed.
                set_refusal(false);
                let (idx, report) = scan(&WalkOptions {
                    columns: Default::default(),
                    roots: vec![root.clone()],
                    excludes: excludes.clone(),
                    follow_symlinks: follow,
                    ntfs: NtfsMode::Off,
                });
                assert_eq!(
                    report.errors, 0,
                    "extra={extra}, follow={follow}: {report:?}"
                );
                assert_eq!(idx.len(), 1, "extra={extra}, follow={follow}");
                set_refusal(true);
                for scan_root in [root.clone(), private.clone()] {
                    let (idx, report) = scan(&WalkOptions {
                        columns: Default::default(),
                        roots: vec![scan_root.clone()],
                        excludes: excludes.clone(),
                        follow_symlinks: follow,
                        ntfs: NtfsMode::Off,
                    });
                    assert_eq!(
                        report.errors, 1,
                        "extra={extra}, follow={follow}, root={scan_root:?}: {report:?}"
                    );
                    assert!(
                        report.error_samples[0].starts_with("exclude identity ")
                            && report.error_samples[0].contains(&exclude.display().to_string()),
                        "extra={extra}, follow={follow}: {report:?}"
                    );
                    assert!(
                        idx.is_empty(),
                        "extra={extra}, follow={follow}, root={scan_root:?}: \
                         unresolvable exclude admitted {} entries",
                        idx.len()
                    );
                    assert_eq!(
                        report.fatal_error.as_deref(),
                        Some(report.error_samples[0].as_str()),
                        "extra={extra}, follow={follow}: abort must be fatal"
                    );
                    assert_eq!(report.entries, 0);
                }
            }
        }
    }

    /// N6: a file exclude inside a `deny-identity` directory resolves
    /// to its file identity, but the identity of its canonical parent
    /// is refused. Losing the parent scope must abort the scan, not
    /// silently disable matching for the same directory reached
    /// through another spelling or alias.
    #[test]
    fn unresolvable_file_exclude_parent_identity_aborts_scan() {
        let _refusal = IdentityRefusal::new(false);
        let base = TestDir::new("n6-exclude-parent");
        let private = base.0.join(identity::TEST_REFUSED_NAME);
        std::fs::create_dir(&private).unwrap();
        let secret = private.join("secret.txt");
        std::fs::write(&secret, b"x").unwrap();
        let alias = base.0.join("scan-alias");
        make_symlink_dir(&private, &alias);
        let set_refusal = |on: bool| identity::TEST_IDENTITY_REFUSAL.store(on, Ordering::Relaxed);
        for follow in [false, true] {
            #[cfg(windows)]
            let root = if follow {
                alias.clone()
            } else {
                base.0.join("DENY-IDENTITY")
            };
            #[cfg(unix)]
            let root = alias.clone();
            #[cfg(windows)]
            assert_eq!(
                std::fs::canonicalize(&root).unwrap(),
                std::fs::canonicalize(&private).unwrap(),
                "precondition: root spelling resolves to the same directory"
            );
            set_refusal(false);
            let (idx, report) = scan(&WalkOptions {
                columns: Default::default(),
                roots: vec![root.clone()],
                excludes: vec![secret.clone()],
                follow_symlinks: follow,
                ntfs: NtfsMode::Off,
            });
            assert_eq!(report.errors, 0, "follow={follow}: {report:?}");
            assert_eq!(idx.len(), 1, "follow={follow}: control");
            set_refusal(true);
            let (idx, report) = scan(&WalkOptions {
                columns: Default::default(),
                roots: vec![root],
                excludes: vec![secret.clone()],
                follow_symlinks: follow,
                ntfs: NtfsMode::Off,
            });
            assert_eq!(report.errors, 1, "follow={follow}: {report:?}");
            assert!(
                report.error_samples[0].starts_with("exclude parent identity ")
                    && report.error_samples[0].contains(&secret.display().to_string()),
                "follow={follow}: {report:?}"
            );
            assert!(
                idx.is_empty(),
                "follow={follow}: file exclude lost its parent scope, {} entries",
                idx.len()
            );
            assert_eq!(
                report.fatal_error.as_deref(),
                Some(report.error_samples[0].as_str()),
                "follow={follow}: abort must be fatal"
            );
            assert_eq!(report.entries, 0);
        }
    }

    #[cfg(unix)]
    #[test]
    fn exclude_via_symlinked_alias_applies_when_following() {
        let root = TestDir::new("alias-root");
        let target = TestDir::new("alias-target");
        target.file("target-leaf.txt");
        let link = root.0.join("link");
        std::os::unix::fs::symlink(&target.0, &link).unwrap();
        // The exclude may be spelled through the alias or through the
        // real target path; both must keep the target out.
        for exclude in [link, target.0.clone()] {
            let (idx, _) = scan(&WalkOptions {
                columns: Default::default(),
                roots: vec![root.0.clone()],
                excludes: vec![exclude],
                follow_symlinks: true,
                ntfs: NtfsMode::Off,
            });
            assert!(
                !(0..idx.len() as u32).any(|id| idx.name(id) == b"target-leaf.txt"),
                "excluded symlinked target was indexed"
            );
        }
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

    // APFS rejects non-UTF-8 names outright, so on macOS this is a
    // real ignored test in CI output, not a silent pass. Everywhere
    // else a creation error fails the test.
    #[cfg(unix)]
    #[cfg_attr(target_os = "macos", ignore = "APFS rejects non-UTF-8 file names")]
    #[test]
    fn unix_root_with_invalid_utf8_name_is_lossless() {
        use std::os::unix::ffi::OsStringExt;
        let dir = TestDir::new("nonutf8");
        let root = dir
            .0
            .join(std::ffi::OsString::from_vec(vec![b'r', 0xff, b'x']));
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
        // Privileged processes (e.g. root in CI containers) can ignore mode
        // bits. In that case only the permission-denial assertions below
        // are omitted, with a visible reason under --nocapture (ADR 0001).
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
        } else {
            eprintln!(
                "IGNORED permission-denial assertions: process can read mode-000 directory (privileged execution or filesystem ignores mode bits)"
            );
        }
    }
}
