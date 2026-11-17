//! Who the system service answers: the access policy (`access.json`).
//!
//! The file sits next to the service index in the protected state folder and
//! is validated like the index (`protection::state_file`). A missing file
//! means administrators only, exactly the rule before this existed. An
//! unprotected, unreadable, oversized or malformed file also means
//! administrators only; it is never read as a wider grant. Authorization uses
//! the account `id` (Windows SID, Unix uid); `name` is display only.
//!
//! The service keeps the policy in an `ArcSwap` and re-checks the file's
//! metadata at most once a second, on whichever request thread claims the
//! check (one atomic compare-exchange; every other request only loads the
//! current policy). No thread, no per-request file read, no blocking lock.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arc_swap::{ArcSwap, Guard};
use ferrow_ipc::protocol::{MAX_ACCESS_USERS, MAX_ACCOUNT_ID, MAX_ACCOUNT_NAME};
use ferrow_ipc::{AccessInfo, AccessMode, AccessUser, CallerIdentity};
use serde::{Deserialize, Serialize};

pub const FILE_NAME: &str = "access.json";
/// Largest policy file read; anything bigger is ignored.
pub const MAX_POLICY_BYTES: u64 = 64 * 1024;
/// Least time between two metadata checks of the policy file.
pub const RECHECK: Duration = Duration::from_secs(1);

/// The OS facts the decision needs; real callers and test doubles both
/// provide them. Windows ids are SIDs, Unix ids are decimal uids.
pub trait Caller {
    /// SYSTEM, root, or an unsandboxed member of Administrators.
    fn admin(&self) -> bool;
    /// A usable identity that is not restricted, AppContainer or low integrity.
    fn unsandboxed(&self) -> bool;
    fn id(&self) -> Option<String>;
}

impl Caller for CallerIdentity {
    fn admin(&self) -> bool {
        self.is_admin_account()
    }
    fn unsandboxed(&self) -> bool {
        self.is_unsandboxed()
    }
    fn id(&self) -> Option<String> {
        self.account_id()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Policy {
    pub mode: AccessMode,
    pub users: Vec<AccessUser>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileUser {
    id: String,
    #[serde(default)]
    name: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileShape {
    version: u32,
    mode: String,
    #[serde(default)]
    users: Vec<FileUser>,
}

/// A Windows SID (`S-1-…`) or a decimal uid, within the wire limits.
pub fn valid_id(id: &str) -> bool {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    id.len() <= MAX_ACCOUNT_ID
        && (digits(id)
            || id
                .strip_prefix("S-1-")
                .is_some_and(|rest| digits(&rest.replace('-', ""))))
}

impl Policy {
    /// Parse a policy file's bytes; any problem is an error (callers then
    /// use the administrators-only default).
    pub fn parse(bytes: &[u8]) -> Result<Policy, String> {
        if bytes.len() as u64 > MAX_POLICY_BYTES {
            return Err("policy file is too large".into());
        }
        let file: FileShape = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        if file.version != 1 {
            return Err(format!("unsupported policy version {}", file.version));
        }
        let mode = AccessMode::parse(&file.mode).ok_or("unknown mode")?;
        if file.users.len() > MAX_ACCESS_USERS {
            return Err("too many users".into());
        }
        let mut users = Vec::with_capacity(file.users.len());
        for user in file.users {
            if !valid_id(&user.id) || user.name.len() > MAX_ACCOUNT_NAME {
                return Err("invalid user entry".into());
            }
            users.push(AccessUser {
                id: user.id,
                name: user.name,
            });
        }
        Ok(Policy { mode, users })
    }

    pub fn to_json(&self) -> String {
        let file = FileShape {
            version: 1,
            mode: self.mode.as_str().into(),
            users: self
                .users
                .iter()
                .map(|u| FileUser {
                    id: u.id.clone(),
                    name: u.name.clone(),
                })
                .collect(),
        };
        let mut text = serde_json::to_string_pretty(&file).expect("plain data serializes");
        text.push('\n');
        text
    }

    pub fn listed(&self, id: &str) -> Option<&AccessUser> {
        self.users.iter().find(|u| u.id.eq_ignore_ascii_case(id))
    }

    /// Whether the service answers this caller. Administrators and SYSTEM
    /// are always answered; everyone else must be unsandboxed, and in
    /// `listed` mode also listed. `admins` is the rule that existed before
    /// the policy: administrators only.
    pub fn allows(&self, caller: &impl Caller) -> bool {
        match self.mode {
            AccessMode::Admins => caller.admin(),
            // Every administrator is unsandboxed, so this includes them.
            AccessMode::Everyone => caller.unsandboxed(),
            AccessMode::Listed => {
                caller.admin()
                    || (caller.unsandboxed()
                        && caller.id().is_some_and(|id| self.listed(&id).is_some()))
            }
        }
    }

    /// The `AccessInfo` reply: every caller learns the mode, whether they are
    /// answered and who they are; only an allowed administrator gets the list.
    pub fn info(&self, caller: &impl Caller) -> AccessInfo {
        let allowed = self.allows(caller);
        let id = caller.id().unwrap_or_default();
        let name = self.listed(&id).map(|u| u.name.clone()).unwrap_or_default();
        AccessInfo {
            mode: self.mode,
            you_allowed: allowed,
            you: AccessUser { id, name },
            users: (allowed && caller.admin()).then(|| self.users.clone()),
        }
    }
}

/// The policy file next to the service index.
pub fn default_path() -> io::Result<PathBuf> {
    Ok(crate::default_service_index_path()?.with_file_name(FILE_NAME))
}

type Validate = fn(&Path) -> io::Result<()>;

fn load_with(path: &Path, validate: Validate) -> Result<Policy, String> {
    match validate(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Policy::default()),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|f| f.take(MAX_POLICY_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Policy::parse(&bytes).map_err(|e| format!("{}: {e}", path.display()))
}

/// Read and validate the policy file like the service does. `Err` carries
/// why the service would ignore it.
pub fn load_file(path: &Path) -> Result<Policy, String> {
    load_with(path, crate::protection::state_file)
}

/// Write the policy atomically: a temporary file in the same protected
/// folder (so it inherits only the protected grants), validated, then renamed
/// over the old one. Needs administrator rights; creates the protected folder.
pub fn write(path: &Path, policy: &Policy) -> io::Result<()> {
    write_with(
        path,
        policy,
        crate::protection::state_directory,
        crate::protection::state_file,
    )
}

fn write_with(
    path: &Path,
    policy: &Policy,
    folder: Validate,
    validate: Validate,
) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("policy path has no folder"))?;
    folder(parent)?;
    let temporary = parent.join(format!("access-{}.new", std::process::id()));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(policy.to_json().as_bytes())?;
        file.sync_all()?;
        drop(file);
        validate(&temporary)?;
        std::fs::rename(&temporary, path)?;
        validate(path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// Size, last write and file identity: any replacement of the file shows.
type Stamp = Option<(u64, u64, u64)>;

fn stamp(path: &Path) -> Stamp {
    let meta = std::fs::symlink_metadata(path).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let nanos = (meta.mtime() as u64).wrapping_mul(1_000_000_000);
        Some((
            meta.len(),
            nanos.wrapping_add(meta.mtime_nsec() as u64),
            meta.ino(),
        ))
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        Some((meta.len(), meta.last_write_time(), meta.creation_time()))
    }
}

struct Seen {
    stamp: Stamp,
    error: Option<String>,
}

/// The live policy of the system service.
pub struct PolicyStore {
    path: PathBuf,
    validate: Validate,
    interval_ms: u64,
    start: Instant,
    /// Milliseconds since `start` of the last claimed check.
    checked: AtomicU64,
    policy: ArcSwap<Policy>,
    seen: Mutex<Seen>,
}

impl PolicyStore {
    /// The service's store: the fixed protected file, checked every second.
    pub fn open(path: PathBuf) -> PolicyStore {
        Self::with(path, crate::protection::state_file, RECHECK)
    }

    fn with(path: PathBuf, validate: Validate, interval: Duration) -> PolicyStore {
        let store = PolicyStore {
            path,
            validate,
            interval_ms: interval.as_millis() as u64,
            start: Instant::now(),
            checked: AtomicU64::new(0),
            policy: ArcSwap::from_pointee(Policy::default()),
            seen: Mutex::new(Seen {
                stamp: None,
                error: None,
            }),
        };
        store.reload(true);
        store
    }

    /// The policy now in force; a change of the file shows within the
    /// recheck interval. Never blocks on another request.
    pub fn current(&self) -> Guard<Arc<Policy>> {
        let now = self.start.elapsed().as_millis() as u64;
        let last = self.checked.load(Ordering::Relaxed);
        if now.saturating_sub(last) >= self.interval_ms
            && self
                .checked
                .compare_exchange(last, now, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
        {
            self.reload(false);
        }
        self.policy.load()
    }

    fn reload(&self, force: bool) {
        let now = stamp(&self.path);
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        if !force && seen.stamp == now {
            return;
        }
        seen.stamp = now;
        let policy = match load_with(&self.path, self.validate) {
            Ok(policy) => {
                seen.error = None;
                policy
            }
            Err(error) => {
                if seen.error.as_ref() != Some(&error) {
                    eprintln!("ferrowd: access policy ignored (administrators only): {error}");
                    seen.error = Some(error);
                }
                Policy::default()
            }
        };
        if **self.policy.load() != policy {
            eprintln!(
                "ferrowd: access policy: {} ({} listed)",
                policy.mode.as_str(),
                policy.users.len()
            );
            self.policy.store(Arc::new(policy));
        }
    }
}

#[cfg(test)]
#[path = "access_tests.rs"]
mod tests;
