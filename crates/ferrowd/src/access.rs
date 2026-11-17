//! Who the system service answers: the access policy (`access.json`).
//!
//! The file sits next to the service index in the protected state folder and
//! is validated like the index (`protection::state_file`). A missing file
//! means administrators only, exactly the rule before this existed. An
//! unprotected, unreadable, oversized or malformed file also means
//! administrators only; it is never read as a wider grant. Authorization uses
//! the account `id` (Windows SID, Unix uid); `name` is display only.
//!
//! The service keeps the policy in an `ArcSwap` and re-checks the file at
//! most once a second. Whoever wins a `try_lock` does the check (protection
//! validation, then a metadata compare, then a reload on change); every other
//! request uses the current policy at once and never waits for it. A failed
//! check is retried on the next tick; nothing is remembered about it.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, TryLockError};
use std::time::{Duration, Instant};

use crate::eventlog::Level;
use arc_swap::{ArcSwap, Guard};
use ferrow_ipc::protocol::{MAX_ACCESS_USERS, MAX_ACCOUNT_ID, MAX_ACCOUNT_NAME};
use ferrow_ipc::{AccessInfo, AccessMode, AccessUser, CallerIdentity};
use serde::{Deserialize, Serialize};

pub const FILE_NAME: &str = "access.json";
/// Largest policy file read; anything bigger is ignored.
pub const MAX_POLICY_BYTES: u64 = 64 * 1024;
/// Least time between two checks of the policy file.
pub const RECHECK: Duration = Duration::from_secs(1);

/// The OS facts the decision needs; real callers and test doubles both
/// provide them. Windows ids are SIDs, Unix ids are decimal uids.
pub trait Caller {
    /// SYSTEM, root, or an unsandboxed member of Administrators.
    fn admin(&self) -> bool;
    /// A usable identity that is not restricted, AppContainer or low integrity.
    fn unsandboxed(&self) -> bool;
    /// A person who signs in to this PC: unsandboxed and not a service,
    /// virtual, network-logon or guest account.
    fn person(&self) -> bool;
    fn id(&self) -> Option<String>;
}

impl Caller for CallerIdentity {
    fn admin(&self) -> bool {
        self.is_admin_account()
    }
    fn unsandboxed(&self) -> bool {
        self.is_unsandboxed()
    }
    fn person(&self) -> bool {
        self.is_person()
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

/// A canonical Windows SID (`S-1-<authority>(-<subauthority>)+`, every part a
/// plain decimal number) or a canonical decimal uid, within the wire limits.
pub fn valid_id(id: &str) -> bool {
    let number = |s: &str, max_len: usize| {
        !s.is_empty() && s.len() <= max_len && s.bytes().all(|b| b.is_ascii_digit())
    };
    if id.len() > MAX_ACCOUNT_ID {
        return false;
    }
    if let Some(rest) = id.strip_prefix("S-1-") {
        let mut parts = rest.split('-');
        let authority = parts.next().is_some_and(|a| number(a, 15));
        let subs: Vec<&str> = parts.collect();
        return authority && (1..=15).contains(&subs.len()) && subs.iter().all(|s| number(s, 10));
    }
    id.parse::<u32>().is_ok_and(|n| n.to_string() == id)
}

/// Replace control characters so a name cannot move the cursor or recolour a
/// terminal when printed.
pub fn printable(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_control() {
                c.escape_unicode().to_string()
            } else {
                c.to_string()
            }
        })
        .collect()
}

impl Policy {
    /// Parse a policy file's bytes; any problem is an error (callers then
    /// use the administrators-only default).
    pub fn parse(bytes: &[u8]) -> Result<Policy, String> {
        if bytes.len() as u64 > MAX_POLICY_BYTES {
            return Err("policy file is too large".into());
        }
        // Editors on Windows like to add a UTF-8 byte order mark.
        let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
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
    /// are always answered. `admins` is the rule that existed before the
    /// policy. `everyone` means people who sign in to this PC (not service,
    /// virtual, network-logon or guest accounts). `listed` admits exactly the
    /// accounts an administrator chose, but never a sandboxed caller.
    pub fn allows(&self, caller: &impl Caller) -> bool {
        match self.mode {
            AccessMode::Admins => caller.admin(),
            AccessMode::Everyone => caller.admin() || caller.person(),
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

fn read_parse(path: &Path) -> Result<Policy, String> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|f| f.take(MAX_POLICY_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Policy::parse(&bytes).map_err(|e| format!("{}: {e}", path.display()))
}

fn load_with(path: &Path, validate: Validate) -> Result<Policy, String> {
    match validate(path) {
        Ok(()) => read_parse(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Policy::default()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Read and validate the policy file like the service does. `Err` carries
/// why the service would ignore it.
pub fn load_file(path: &Path) -> Result<Policy, String> {
    load_with(path, crate::protection::state_file)
}

/// Size, last write and file identity of the policy file at one moment: any
/// replacement of the file shows. `None` inside means the file is missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStamp(Option<(u64, u64, u64)>);

impl FileStamp {
    pub fn of(path: &Path) -> FileStamp {
        let Ok(meta) = std::fs::symlink_metadata(path) else {
            return FileStamp(None);
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let nanos = (meta.mtime() as u64).wrapping_mul(1_000_000_000);
            FileStamp(Some((
                meta.len(),
                nanos.wrapping_add(meta.mtime_nsec() as u64),
                meta.ino(),
            )))
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            FileStamp(Some((
                meta.len(),
                meta.last_write_time(),
                meta.creation_time(),
            )))
        }
    }
}

/// Read the policy to change it, together with the file's stamp taken first:
/// `write` refuses when the file changed in between. The policy is the
/// administrators-only default when the file is unusable (then `Err` says why).
pub fn load_for_update(path: &Path) -> (FileStamp, Result<Policy, String>) {
    (FileStamp::of(path), load_file(path))
}

/// Write the policy atomically: a temporary file in the same protected
/// folder (so it inherits only the protected grants), validated, then renamed
/// over the old one, and made durable. Refuses when the file changed since
/// `expected` was read. Needs administrator rights; creates the folder.
pub fn write(path: &Path, policy: &Policy, expected: &FileStamp) -> io::Result<()> {
    write_with(
        path,
        policy,
        expected,
        crate::protection::state_directory,
        adopt_state_owner,
        crate::protection::state_file,
    )
}

/// Windows: make the new file's owner Administrators even when the elevated
/// token's default owner is the user (the state check refuses user-owned
/// files). Best effort: the validation after it decides.
fn adopt_state_owner(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        let _ = crate::protection::windows::own_by_administrators(path);
    }
    #[cfg(not(windows))]
    let _ = path;
    Ok(())
}

fn write_with(
    path: &Path,
    policy: &Policy,
    expected: &FileStamp,
    folder: Validate,
    adopt: Validate,
    validate: Validate,
) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("policy path has no folder"))?;
    let text = policy.to_json();
    // The service must be able to read back exactly what is written.
    Policy::parse(text.as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("not writable: {e}")))?;
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
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        adopt(&temporary)?;
        validate(&temporary)?;
        if FileStamp::of(path) != *expected {
            return Err(io::Error::other(
                "the access list changed meanwhile, try again",
            ));
        }
        std::fs::rename(&temporary, path)?;
        make_durable(path, parent)?;
        validate(path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// After the rename: Unix syncs the folder (the rename itself), Windows
/// flushes the file's buffers.
fn make_durable(path: &Path, parent: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        let _ = path;
        std::fs::File::open(parent)?.sync_all()
    }
    #[cfg(windows)]
    {
        let _ = parent;
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)?
            .sync_all()
    }
}

/// What the last check learned. Only touched by the thread that holds the lock.
#[derive(Default)]
struct State {
    /// Stamp of the content currently loaded, when the last check succeeded.
    good: Option<FileStamp>,
    /// Stamp of content that parsed badly (not read again until it changes).
    bad: Option<FileStamp>,
    last_error: Option<String>,
}

type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// The live policy of the system service.
pub struct PolicyStore {
    path: PathBuf,
    validate: Validate,
    interval_ms: u64,
    /// Milliseconds on a monotonic clock.
    clock: Clock,
    /// Time of the last claimed check.
    checked: AtomicU64,
    policy: ArcSwap<Policy>,
    state: Mutex<State>,
}

impl PolicyStore {
    /// The service's store: the fixed protected file, checked every second.
    pub fn open(path: PathBuf) -> PolicyStore {
        let start = Instant::now();
        let clock: Clock = Arc::new(move || start.elapsed().as_millis() as u64);
        Self::with(path, crate::protection::state_file, RECHECK, clock)
    }

    fn with(path: PathBuf, validate: Validate, interval: Duration, clock: Clock) -> PolicyStore {
        let store = PolicyStore {
            path,
            validate,
            interval_ms: interval.as_millis() as u64,
            checked: AtomicU64::new(clock()),
            clock,
            policy: ArcSwap::from_pointee(Policy::default()),
            state: Mutex::new(State::default()),
        };
        let mut logs = Vec::new();
        store.check(
            &mut store.state.lock().unwrap_or_else(|e| e.into_inner()),
            &mut logs,
        );
        print(logs);
        store
    }

    /// The policy now in force. When a check is due, one caller does it and
    /// everyone else gets the current policy immediately: a slow disk or a
    /// hung validation can delay that one request, never the others.
    pub fn current(&self) -> Guard<Arc<Policy>> {
        let now = (self.clock)();
        if now.saturating_sub(self.checked.load(Ordering::Relaxed)) >= self.interval_ms {
            let mut logs = Vec::new();
            match self.state.try_lock() {
                Ok(mut state) => self.claim(now, &mut state, &mut logs),
                Err(TryLockError::Poisoned(p)) => self.claim(now, &mut p.into_inner(), &mut logs),
                Err(TryLockError::WouldBlock) => {}
            }
            print(logs); // after the lock is released
        }
        self.policy.load()
    }

    fn claim(&self, now: u64, state: &mut State, logs: &mut Vec<(Level, String)>) {
        // Another thread may have finished a check while this one waited.
        if now.saturating_sub(self.checked.load(Ordering::Relaxed)) >= self.interval_ms {
            self.checked.store(now, Ordering::Relaxed);
            self.check(state, logs);
        }
    }

    /// Validate protection, compare the stamp, reload on change. A failure
    /// changes the policy to the default and is retried at the next check.
    fn check(&self, state: &mut State, logs: &mut Vec<(Level, String)>) {
        let stamp = FileStamp::of(&self.path);
        match (self.validate)(&self.path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                *state = State {
                    good: Some(stamp),
                    ..State::default()
                };
                self.set(Policy::default(), logs);
            }
            Err(e) => {
                state.good = None;
                state.bad = None;
                self.fail(state, format!("{}: {e}", self.path.display()), logs);
            }
            Ok(()) => {
                if state.good.as_ref() == Some(&stamp) || state.bad.as_ref() == Some(&stamp) {
                    return;
                }
                match read_parse(&self.path) {
                    Ok(policy) => {
                        *state = State {
                            good: Some(stamp),
                            ..State::default()
                        };
                        self.set(policy, logs);
                    }
                    Err(error) => {
                        state.good = None;
                        state.bad = Some(stamp);
                        self.fail(state, error, logs);
                    }
                }
            }
        }
    }

    fn fail(&self, state: &mut State, error: String, logs: &mut Vec<(Level, String)>) {
        self.set(Policy::default(), logs);
        if state.last_error.as_ref() != Some(&error) {
            logs.push((
                Level::Warn,
                format!("access policy ignored (administrators only): {error}"),
            ));
            state.last_error = Some(error);
        }
    }

    fn set(&self, policy: Policy, logs: &mut Vec<(Level, String)>) {
        if **self.policy.load() != policy {
            logs.push((
                Level::Info,
                format!(
                    "access policy: {} ({} listed)",
                    policy.mode.as_str(),
                    policy.users.len()
                ),
            ));
            self.policy.store(Arc::new(policy));
        }
    }
}

fn print(logs: Vec<(Level, String)>) {
    for (level, line) in logs {
        crate::eventlog::record(level, line);
    }
}

#[cfg(test)]
#[path = "access_tests.rs"]
mod tests;
