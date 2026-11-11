//! Host controller for enabling/disabling Windows Search integration and
//! attaching `ferrow_winsearch.dll` to `SearchHost.exe`.
//!
//! A watcher thread blocks on the `SearchHost.exe` process handle (0% CPU
//! while waiting) and re-attaches whenever Windows restarts the shell
//! search host, so the integration survives shell restarts for as long as
//! `ferrow-gui` runs. Attachment state is exposed via [`attach_status`].

use std::ffi::{OsString, c_void};
use std::io;
use std::mem::size_of;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use windows::Win32::Foundation::{
    CloseHandle, ERROR_SUCCESS, FreeLibrary, GENERIC_EXECUTE, GENERIC_READ, GENERIC_WRITE, HANDLE,
    HLOCAL, HMODULE, LocalFree, WAIT_OBJECT_0,
};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSidToSidW, EXPLICIT_ACCESS_W, GRANT_ACCESS,
    GetNamedSecurityInfoW, REVOKE_ACCESS, SE_FILE_OBJECT, SetEntriesInAclW, SetNamedSecurityInfoW,
    TRUSTEE_IS_SID, TRUSTEE_W,
};
use windows::Win32::Security::{
    ACL, CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, GetLengthSid, GetTokenInformation,
    NO_INHERITANCE, OBJECT_INHERIT_ACE, PSECURITY_DESCRIPTOR, PSID, TOKEN_APPCONTAINER_INFORMATION,
    TOKEN_QUERY, TokenAppContainerSid,
};
use windows::Win32::Storage::FileSystem::DELETE;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, MODULEENTRY32W, Module32FirstW, Module32NextW, PROCESSENTRY32W,
    Process32FirstW, Process32NextW, TH32CS_SNAPMODULE, TH32CS_SNAPMODULE32, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::LibraryLoader::{
    DONT_RESOLVE_DLL_REFERENCES, GetModuleHandleW, GetProcAddress, LoadLibraryExW,
};
use windows::Win32::System::Memory::{
    MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAllocEx, VirtualFreeEx,
};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::SystemInformation::{GetWindowsDirectoryW, IMAGE_FILE_MACHINE};
use windows::Win32::System::Threading::{
    CreateRemoteThread, GetCurrentProcessId, GetExitCodeThread, IsWow64Process2, OpenProcess,
    OpenProcessToken, PROCESS_ACCESS_RIGHTS, PROCESS_NAME_FORMAT, QueryFullProcessImageNameW,
    WaitForSingleObject,
};
use windows::core::{PCWSTR, PWSTR, s, w};

use crate::ipc;

const DLL_NAME: &str = "ferrow_winsearch.dll";
const SEARCHHOST_PACKAGE_DIR: &str = "MicrosoftWindows.Client.CBS_cw5n1h2txyewy";
/// `PROCESS_CREATE_THREAD | PROCESS_QUERY_INFORMATION | PROCESS_VM_OPERATION | PROCESS_VM_WRITE | PROCESS_VM_READ | SYNCHRONIZE`
const ATTACH_ACCESS: u32 = 0x10043A | 0x0010;
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

static LAST_ATTACHED_PID: AtomicU32 = AtomicU32::new(0);
static WATCHER_STARTED: AtomicBool = AtomicBool::new(false);
static ATTACH_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
static LAST_ERROR: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn attach_lock() -> &'static Mutex<()> {
    ATTACH_MUTEX.get_or_init(|| Mutex::new(()))
}

fn last_error_slot() -> &'static Mutex<Option<String>> {
    LAST_ERROR.get_or_init(|| Mutex::new(None))
}

fn record_attach_error(error: Option<String>) {
    if let Ok(mut guard) = last_error_slot().lock() {
        *guard = error;
    }
}

/// `(attached SearchHost pid, most recent attachment failure)` for the
/// Settings status line. Attachment is best-effort and retried by the
/// watcher, so an error here is informational, not fatal.
pub fn attach_status() -> (Option<u32>, Option<String>) {
    let pid = LAST_ATTACHED_PID.load(Ordering::Acquire);
    let error = last_error_slot().lock().ok().and_then(|g| g.clone());
    (if pid == 0 { None } else { Some(pid) }, error)
}

/// Synchronize the live Windows Search integration state with the user's setting.
pub fn sync_state(data_dir: &Path, enabled: bool) -> Result<(), String> {
    if !enabled {
        let marker = ipc::enabled_marker_path(data_dir);
        if marker.exists() {
            std::fs::remove_file(&marker)
                .map_err(|e| format!("Could not disable Windows Search integration: {e}"))?;
        }
        revoke_searchhost_access(data_dir)
            .map_err(|e| format!("Could not revoke SearchHost bridge access: {e}"))?;
        record_attach_error(None);
        return Ok(());
    }

    let ws_dir = ipc::winsearch_dir(data_dir);
    std::fs::create_dir_all(&ws_dir)
        .map_err(|e| format!("Could not create Windows Search state folder: {e}"))?;

    // Remove the broad grant made by older builds before anything can
    // route queries again. This must happen even when SearchHost is
    // not currently running and no replacement grant can be made yet.
    revoke_legacy_all_packages_access(data_dir)
        .map_err(|e| format!("Could not remove the legacy all-AppContainer bridge access: {e}"))?;

    // Bridge access goes to the SearchHost package alone, never to all
    // AppContainer apps. If SearchHost is not running yet, attachment
    // applies the same grants when it opens the verified process.
    if let Ok((sid, sid_text)) = resolve_searchhost_sid() {
        grant_searchhost_access(data_dir, &sid, &sid_text)
            .map_err(|e| format!("Could not grant SearchHost access on the bridge folders: {e}"))?;
    }

    ipc::ensure_responder_started(data_dir);

    std::fs::write(ipc::enabled_marker_path(data_dir), b"1")
        .map_err(|e| format!("Could not write Windows Search enabled marker: {e}"))?;

    let dir_owned = data_dir.to_path_buf();
    let _ = std::thread::Builder::new()
        .name("ferrow-winsearch-attach".into())
        .spawn(move || {
            let _ = attach_searchhost_now(&dir_owned);
        });
    ensure_watcher_started(data_dir);

    Ok(())
}

/// Start the `SearchHost.exe` lifetime watcher once per process. The
/// watcher attaches (when enabled), blocks on the process handle until
/// the shell search host exits, then attaches to its successor.
fn ensure_watcher_started(data_dir: &Path) {
    if WATCHER_STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    let dir_owned = data_dir.to_path_buf();
    let _ = std::thread::Builder::new()
        .name("ferrow-winsearch-watch".into())
        .spawn(move || watcher_loop(&dir_owned));
}

fn watcher_loop(data_dir: &Path) {
    loop {
        let pids = find_searchhost_pids().unwrap_or_default();
        let Some(&pid) = pids.first() else {
            // SearchHost is not running (rare outside logon): the only
            // waiting done by sleeping, and only until it appears.
            std::thread::sleep(Duration::from_secs(1));
            continue;
        };
        if ipc::is_enabled(data_dir) {
            let _ = attach_searchhost_now(data_dir);
        }
        wait_for_process_exit(pid);
        if LAST_ATTACHED_PID.load(Ordering::Acquire) == pid {
            LAST_ATTACHED_PID.store(0, Ordering::Release);
        }
    }
}

/// Block until `pid` exits. Prefers a kernel wait on the process handle
/// (0% CPU); if the handle cannot be opened, falls back to noticing the
/// pid disappear.
fn wait_for_process_exit(pid: u32) {
    const SYNCHRONIZE: u32 = 0x0010_0000;
    let opened = unsafe { OpenProcess(PROCESS_ACCESS_RIGHTS(SYNCHRONIZE), false, pid) };
    match opened {
        Ok(handle) => unsafe {
            // INFINITE: block until SearchHost exits.
            let _ = WaitForSingleObject(handle, u32::MAX);
            let _ = CloseHandle(handle);
        },
        Err(_) => loop {
            std::thread::sleep(Duration::from_secs(2));
            match find_searchhost_pids() {
                Ok(pids) if pids.contains(&pid) => continue,
                _ => break,
            }
        },
    }
}

/// Synchronously stage `ferrow_winsearch.dll` and attach it to `SearchHost.exe`.
/// Serialized across the immediate attach and the watcher; the outcome is
/// recorded for [`attach_status`].
pub fn attach_searchhost_now(data_dir: &Path) -> Result<bool, String> {
    let _guard = match attach_lock().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let result = (|| -> Result<bool, String> {
        let ws_dir = ipc::winsearch_dir(data_dir);
        let staged_dll = stage_attachment_dll(&ws_dir)
            .map_err(|e| format!("Could not stage ferrow_winsearch.dll: {e}"))?;
        attach_to_searchhost(&staged_dll, data_dir)
            .map_err(|e| format!("Could not attach to SearchHost.exe: {e}"))
    })();
    match &result {
        Ok(_) => record_attach_error(None),
        Err(error) => record_attach_error(Some(error.clone())),
    }
    result
}

/// File in the app data folder recording which AppContainer SID and DLL
/// paths were granted bridge access. It deliberately lives outside the
/// Modify-granted bridge folder so the AppContainer cannot alter it.
const GRANTED_SID_FILE: &str = "winsearch-granted-sid";
/// Legacy pre-review grant trustee. Revoked opportunistically so folders
/// enabled by older builds do not stay open to every AppContainer app.
const LEGACY_ALL_PACKAGES_SID: &str = "S-1-15-2-1";

/// How much access the SearchHost AppContainer gets on one path.
#[derive(Clone, Copy)]
enum AccessKind {
    /// Pass through the folder without reading its contents.
    Traverse,
    ReadExecute,
    Modify,
}

/// Grant (`kind = Some`) or revoke (`kind = None`) access for `sid` on
/// `path`, with container/object inheritance for folders.
fn apply_sid_access(path: &Path, sid_bytes: &[u8], kind: Option<AccessKind>) -> io::Result<()> {
    if sid_bytes.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty SID"));
    }
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: sid_bytes holds a valid self-relative SID for this call.
    let sid = PSID(sid_bytes.as_ptr().cast_mut().cast::<c_void>());

    let mut old_dacl: *mut ACL = ptr::null_mut();
    let mut sd = PSECURITY_DESCRIPTOR::default();
    let get_err = unsafe {
        GetNamedSecurityInfoW(
            PCWSTR(wide.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&mut old_dacl),
            None,
            &mut sd,
        )
    };
    if get_err != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(get_err.0 as i32));
    }

    // FILE_TRAVERSE (0x20) | FILE_READ_ATTRIBUTES (0x80): reach children
    // by full path without listing or reading the folder itself.
    let (perms, mode) = match kind {
        Some(AccessKind::Traverse) => (0x20u32 | 0x80, GRANT_ACCESS),
        Some(AccessKind::ReadExecute) => (GENERIC_READ.0 | GENERIC_EXECUTE.0, GRANT_ACCESS),
        Some(AccessKind::Modify) => (
            GENERIC_READ.0 | GENERIC_WRITE.0 | GENERIC_EXECUTE.0 | DELETE.0,
            GRANT_ACCESS,
        ),
        None => (0, REVOKE_ACCESS),
    };
    let inheritance = if kind.is_some() && path.is_dir() {
        CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE
    } else {
        NO_INHERITANCE
    };

    let ea = EXPLICIT_ACCESS_W {
        grfAccessPermissions: perms,
        grfAccessMode: mode,
        grfInheritance: inheritance,
        Trustee: TRUSTEE_W {
            TrusteeForm: TRUSTEE_IS_SID,
            ptstrName: PWSTR(sid.0.cast()),
            ..Default::default()
        },
    };

    let mut new_dacl: *mut ACL = ptr::null_mut();
    let acl_err = unsafe { SetEntriesInAclW(Some(&[ea]), Some(old_dacl), &mut new_dacl) };
    if acl_err != ERROR_SUCCESS {
        unsafe {
            let _ = LocalFree(Some(HLOCAL(sd.0)));
        }
        return Err(io::Error::from_raw_os_error(acl_err.0 as i32));
    }

    let set_err = unsafe {
        SetNamedSecurityInfoW(
            PWSTR(wide.as_mut_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(new_dacl),
            None,
        )
    };

    unsafe {
        let _ = LocalFree(Some(HLOCAL(new_dacl.cast())));
        let _ = LocalFree(Some(HLOCAL(sd.0)));
    }

    if set_err != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(set_err.0 as i32));
    }
    Ok(())
}

fn sid_bytes_to_string(sid_bytes: &[u8]) -> io::Result<String> {
    // SAFETY: sid_bytes holds a valid SID; the returned buffer is freed.
    unsafe {
        let mut text = PWSTR::null();
        ConvertSidToStringSidW(PSID(sid_bytes.as_ptr().cast_mut().cast()), &mut text)
            .map_err(io::Error::from)?;
        let value = text.to_string();
        let _ = LocalFree(Some(HLOCAL(text.as_ptr().cast())));
        value.map_err(|e| io::Error::other(e.to_string()))
    }
}

fn sid_string_to_bytes(text: &str) -> io::Result<Vec<u8>> {
    let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    // SAFETY: the allocated SID is copied out and freed before returning.
    unsafe {
        let mut sid = PSID::default();
        ConvertStringSidToSidW(PCWSTR(wide.as_ptr()), &mut sid).map_err(io::Error::from)?;
        let len = GetLengthSid(sid) as usize;
        let bytes = std::slice::from_raw_parts(sid.0.cast::<u8>(), len).to_vec();
        let _ = LocalFree(Some(HLOCAL(sid.0)));
        Ok(bytes)
    }
}

/// The Windows directory from the OS — never from the process
/// environment, because it anchors the trusted SearchHost image path.
/// `None` (fail closed) when the OS does not answer.
fn windows_directory() -> Option<PathBuf> {
    // SAFETY: both calls follow the GetWindowsDirectoryW contract; a
    // return larger than the buffer is the required size.
    let mut stack = [0u16; 260];
    let len = unsafe { GetWindowsDirectoryW(Some(&mut stack)) };
    if len == 0 {
        return None;
    }
    if (len as usize) < stack.len() {
        return Some(PathBuf::from(String::from_utf16_lossy(
            &stack[..len as usize],
        )));
    }
    let mut heap = vec![0u16; len as usize + 1];
    let written = unsafe { GetWindowsDirectoryW(Some(&mut heap)) };
    (written > 0 && (written as usize) < heap.len())
        .then(|| PathBuf::from(String::from_utf16_lossy(&heap[..written as usize])))
}

fn normalize_windows_path(path: &Path) -> String {
    let mut normalized = path.to_string_lossy().replace('/', "\\").to_lowercase();
    for prefix in [r"\\?\unc\", r"\\?\", r"\??\"] {
        if let Some(stripped) = normalized.strip_prefix(prefix) {
            normalized = if prefix == r"\\?\unc\" {
                format!(r"\\{stripped}")
            } else {
                stripped.to_owned()
            };
            break;
        }
    }
    while normalized.ends_with('\\') && !normalized.ends_with(":\\") {
        normalized.pop();
    }
    normalized
}

/// Only the inbox CBS package image below the protected `SystemApps`
/// directory is trusted. A same-session process elsewhere that merely
/// calls itself `SearchHost.exe` is rejected before any grant/injection.
fn is_trusted_searchhost_image(image: &Path, windows_dir: &Path) -> bool {
    let expected = windows_dir
        .join("SystemApps")
        .join(SEARCHHOST_PACKAGE_DIR)
        .join("SearchHost.exe");
    normalize_windows_path(image) == normalize_windows_path(&expected)
}

fn process_image_path(proc_handle: HANDLE) -> io::Result<PathBuf> {
    let mut buffer = [0u16; 1024];
    let mut len = buffer.len() as u32;
    // SAFETY: buffer/len follow the QueryFullProcessImageNameW contract.
    unsafe {
        QueryFullProcessImageNameW(
            proc_handle,
            PROCESS_NAME_FORMAT(0),
            PWSTR(buffer.as_mut_ptr()),
            &mut len,
        )
        .map_err(io::Error::from)?;
    }
    Ok(PathBuf::from(String::from_utf16_lossy(
        &buffer[..len as usize],
    )))
}

/// Verify an opened process handle really is this session's trusted
/// system `SearchHost.exe`. Checking the full image from the handle
/// (not the earlier pid snapshot or basename) closes both the pid-reuse
/// race and same-name decoy processes.
fn verify_searchhost_identity(proc_handle: HANDLE, pid: u32) -> io::Result<()> {
    let mut my_session = 0u32;
    let mut proc_session = 0u32;
    unsafe {
        ProcessIdToSessionId(GetCurrentProcessId(), &mut my_session).map_err(io::Error::from)?;
        ProcessIdToSessionId(pid, &mut proc_session).map_err(io::Error::from)?;
    }
    if proc_session != my_session {
        return Err(io::Error::other(
            "SearchHost candidate is in another session",
        ));
    }
    let Some(windows_dir) = windows_directory() else {
        return Err(io::Error::other(
            "could not determine the Windows directory; refusing to trust any SearchHost image",
        ));
    };
    let image = process_image_path(proc_handle)?;
    if !is_trusted_searchhost_image(&image, &windows_dir) {
        return Err(io::Error::other(format!(
            "pid {pid} image {} is not the trusted system SearchHost.exe",
            image.display()
        )));
    }
    Ok(())
}

/// The AppContainer SID of the process behind an already-verified
/// handle, as `(sid bytes, sid string)`. Fails closed when the process
/// is not an AppContainer: bridge access is then never granted broadly.
fn appcontainer_sid_of_process(proc_handle: HANDLE) -> io::Result<(Vec<u8>, String)> {
    // SAFETY: token handle is closed on every path; the two-call
    // GetTokenInformation sizing pattern follows the API contract.
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(proc_handle, TOKEN_QUERY, &mut token).map_err(io::Error::from)?;
        let mut needed = 0u32;
        let _ = GetTokenInformation(token, TokenAppContainerSid, None, 0, &mut needed);
        if needed == 0 {
            let _ = CloseHandle(token);
            return Err(io::Error::other(
                "SearchHost token reports no AppContainer SID",
            ));
        }
        let mut buffer = vec![0u8; needed as usize];
        let fetched = GetTokenInformation(
            token,
            TokenAppContainerSid,
            Some(buffer.as_mut_ptr().cast()),
            needed,
            &mut needed,
        );
        let _ = CloseHandle(token);
        fetched.map_err(io::Error::from)?;
        let info = &*buffer.as_ptr().cast::<TOKEN_APPCONTAINER_INFORMATION>();
        if info.TokenAppContainer.0.is_null() {
            return Err(io::Error::other(
                "SearchHost token reports no AppContainer SID",
            ));
        }
        let len = GetLengthSid(info.TokenAppContainer) as usize;
        let bytes = std::slice::from_raw_parts(info.TokenAppContainer.0.cast::<u8>(), len).to_vec();
        let text = sid_bytes_to_string(&bytes)?;
        Ok((bytes, text))
    }
}

/// Resolve the running SearchHost's AppContainer SID by opening and
/// verifying its process handle. `NotFound` while it is not running.
fn resolve_searchhost_sid() -> io::Result<(Vec<u8>, String)> {
    for pid in find_searchhost_pids()? {
        let handle = unsafe {
            OpenProcess(
                PROCESS_ACCESS_RIGHTS(PROCESS_QUERY_LIMITED_INFORMATION | 0x0400),
                false,
                pid,
            )
        };
        let Ok(handle) = handle else { continue };
        let result = verify_searchhost_identity(handle, pid)
            .and_then(|()| appcontainer_sid_of_process(handle));
        unsafe {
            let _ = CloseHandle(handle);
        }
        if result.is_ok() {
            return result;
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "SearchHost.exe is not running",
    ))
}

/// The bridge paths the SearchHost package may touch, narrowest first:
/// pass through the data folder, use the bridge folder, read the index.
fn bridge_paths(data_dir: &Path) -> Vec<(PathBuf, AccessKind)> {
    vec![
        (data_dir.to_path_buf(), AccessKind::Traverse),
        (ipc::winsearch_dir(data_dir), AccessKind::Modify),
        (data_dir.join("index.bin"), AccessKind::ReadExecute),
    ]
}

#[derive(Debug, PartialEq, Eq)]
struct GrantRecord {
    sid_text: String,
    sid_bytes: Vec<u8>,
    dll_paths: Vec<PathBuf>,
}

fn grant_record_path(data_dir: &Path) -> PathBuf {
    data_dir.join(GRANTED_SID_FILE)
}

fn encode_path_for_record(path: &Path) -> String {
    path.as_os_str()
        .encode_wide()
        .map(|unit| format!("{unit:04x}"))
        .collect()
}

fn decode_path_from_record(text: &str) -> io::Result<PathBuf> {
    if !text.len().is_multiple_of(4) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid encoded DLL path in grant record",
        ));
    }
    let mut units = Vec::with_capacity(text.len() / 4);
    for chunk in text.as_bytes().as_chunks::<4>().0 {
        let digits = std::str::from_utf8(chunk).map_err(io::Error::other)?;
        units.push(u16::from_str_radix(digits, 16).map_err(io::Error::other)?);
    }
    Ok(PathBuf::from(OsString::from_wide(&units)))
}

fn write_grant_record(data_dir: &Path, record: &GrantRecord) -> io::Result<()> {
    let mut text = format!("{}\n", record.sid_text);
    for path in &record.dll_paths {
        text.push_str("dll=");
        text.push_str(&encode_path_for_record(path));
        text.push('\n');
    }
    std::fs::write(grant_record_path(data_dir), text)
}

fn read_grant_record(data_dir: &Path) -> io::Result<Option<GrantRecord>> {
    let path = grant_record_path(data_dir);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut lines = text.lines();
    let sid_text = lines.next().unwrap_or_default().trim().to_owned();
    if !sid_text.starts_with("S-1-15-2-") || sid_text == LEGACY_ALL_PACKAGES_SID {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "grant record does not contain a package SID",
        ));
    }
    let sid_bytes = sid_string_to_bytes(&sid_text)?;
    let mut dll_paths = Vec::new();
    for line in lines {
        let Some(encoded) = line.strip_prefix("dll=") else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unknown line in grant record",
            ));
        };
        let path = decode_path_from_record(encoded.trim())?;
        if !dll_paths.contains(&path) {
            dll_paths.push(path);
        }
    }
    Ok(Some(GrantRecord {
        sid_text,
        sid_bytes,
        dll_paths,
    }))
}

fn record_granted_dll_path(
    data_dir: &Path,
    sid_text: &str,
    sid_bytes: &[u8],
    dll_path: &Path,
) -> io::Result<()> {
    let mut record = match read_grant_record(data_dir)? {
        Some(record) if record.sid_text == sid_text => record,
        Some(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "grant record belongs to a different SearchHost SID",
            ));
        }
        None => GrantRecord {
            sid_text: sid_text.to_owned(),
            sid_bytes: sid_bytes.to_vec(),
            dll_paths: Vec::new(),
        },
    };
    if !record.dll_paths.iter().any(|path| path == dll_path) {
        record.dll_paths.push(dll_path.to_path_buf());
    }
    write_grant_record(data_dir, &record)
}

fn access_paths(data_dir: &Path, extra_dll_paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = bridge_paths(data_dir)
        .into_iter()
        .map(|(path, _)| path)
        .collect();
    let staged = ipc::winsearch_dir(data_dir).join(DLL_NAME);
    if !paths.contains(&staged) {
        paths.push(staged);
    }
    if let Some(source) = find_source_dll()
        && !paths.contains(&source)
    {
        paths.push(source);
    }
    for path in extra_dll_paths {
        if !paths.contains(path) {
            paths.push(path.clone());
        }
    }
    paths
}

fn revoke_sid_from_paths(paths: &[PathBuf], sid_bytes: &[u8]) -> io::Result<()> {
    let mut first_error = None;
    for path in paths {
        if path.exists()
            && let Err(error) = apply_sid_access(path, sid_bytes, None)
            && first_error.is_none()
        {
            first_error = Some(error);
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Remove grants made to every AppContainer by builds before the
/// package-scoped grant existed. This is independent of whether a
/// SearchHost process can currently be resolved.
fn revoke_legacy_all_packages_access(data_dir: &Path) -> io::Result<()> {
    let legacy = sid_string_to_bytes(LEGACY_ALL_PACKAGES_SID)?;
    revoke_sid_from_paths(&access_paths(data_dir, &[]), &legacy)
}

/// Grant the SearchHost package (and only it) the bridge access, record
/// its SID for later revocation, and drop any legacy all-packages grant.
/// The record is written first so a partially applied grant can still
/// be cleaned up on disable.
fn grant_searchhost_access(data_dir: &Path, sid: &[u8], sid_text: &str) -> io::Result<()> {
    if !sid_text.starts_with("S-1-15-2-") || sid_text == LEGACY_ALL_PACKAGES_SID {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to grant bridge access to a non-package SID",
        ));
    }
    let mut preserved_dll_paths = Vec::new();
    if let Some(previous) = read_grant_record(data_dir)? {
        if previous.sid_text == sid_text {
            preserved_dll_paths = previous.dll_paths;
        } else {
            revoke_sid_from_paths(
                &access_paths(data_dir, &previous.dll_paths),
                &previous.sid_bytes,
            )?;
        }
    }
    write_grant_record(
        data_dir,
        &GrantRecord {
            sid_text: sid_text.to_owned(),
            sid_bytes: sid.to_vec(),
            dll_paths: preserved_dll_paths,
        },
    )?;

    for (path, kind) in bridge_paths(data_dir) {
        if path.exists() {
            apply_sid_access(&path, sid, Some(kind))?;
        }
    }
    // The first package-scoped build kept its record inside the
    // AppContainer-writable bridge folder; it is not authoritative.
    let _ = std::fs::remove_file(ipc::winsearch_dir(data_dir).join("granted-sid"));
    revoke_legacy_all_packages_access(data_dir)
}

/// Undo [`grant_searchhost_access`] on disable. Every recorded path is
/// attempted; the grant record is removed only after every revocation
/// succeeds, so a failed cleanup remains retryable and visible. The
/// loaded copy inside SearchHost stays inert behind the removed
/// marker until the shell restarts it.
fn revoke_searchhost_access(data_dir: &Path) -> io::Result<()> {
    // A damaged record must not stop revocation of the grants that can
    // still be identified (running SearchHost, legacy trustee); the
    // damage is reported after every known path was attempted.
    let (record, record_error) = match read_grant_record(data_dir) {
        Ok(record) => (record, None),
        Err(error) => (None, Some(error)),
    };
    let recorded_dll_paths = record
        .as_ref()
        .map(|record| record.dll_paths.clone())
        .unwrap_or_default();
    let paths = access_paths(data_dir, &recorded_dll_paths);

    let mut sids: Vec<Vec<u8>> = Vec::new();
    if let Some(record) = &record {
        sids.push(record.sid_bytes.clone());
    }
    // If the record is missing for any reason, the verified running
    // SearchHost still identifies the package whose grants must go.
    if let Ok((current, _)) = resolve_searchhost_sid()
        && !sids.contains(&current)
    {
        sids.push(current);
    }
    sids.push(sid_string_to_bytes(LEGACY_ALL_PACKAGES_SID)?);

    let mut first_error = record_error;
    for sid in &sids {
        if let Err(error) = revoke_sid_from_paths(&paths, sid)
            && first_error.is_none()
        {
            first_error = Some(error);
        }
    }
    if let Some(error) = first_error {
        return Err(error);
    }

    if record.is_some() {
        std::fs::remove_file(grant_record_path(data_dir))?;
    }
    // Remove the short-lived bridge-folder record written by the first
    // package-scoped build; it is no longer authoritative.
    let _ = std::fs::remove_file(ipc::winsearch_dir(data_dir).join("granted-sid"));
    let staged = ipc::winsearch_dir(data_dir).join(DLL_NAME);
    let _ = std::fs::remove_file(staged);
    Ok(())
}

fn find_source_dll() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let candidate = exe.with_file_name(DLL_NAME);
    if candidate.is_file() {
        return Some(candidate);
    }
    let parent_candidate = exe.parent()?.parent()?.join(DLL_NAME);
    if parent_candidate.is_file() {
        return Some(parent_candidate);
    }
    None
}

fn stage_attachment_dll(ws_dir: &Path) -> io::Result<PathBuf> {
    let staged = ws_dir.join(DLL_NAME);
    if let Some(src) = find_source_dll() {
        let needs_copy = match (std::fs::metadata(&src), std::fs::metadata(&staged)) {
            (Ok(s_meta), Ok(d_meta)) => {
                s_meta.len() != d_meta.len() || s_meta.modified().ok() != d_meta.modified().ok()
            }
            (Ok(_), Err(_)) => true,
            _ => false,
        };
        if needs_copy && src != staged {
            if staged.is_file() && std::fs::copy(&src, &staged).is_err() {
                let old = ws_dir.join("ferrow_winsearch.dll.old");
                let _ = std::fs::remove_file(&old);
                let _ = std::fs::rename(&staged, &old);
                let _ = std::fs::copy(&src, &staged);
            } else {
                let _ = std::fs::copy(&src, &staged);
            }
        }
    }
    if staged.is_file() {
        Ok(staged)
    } else if let Some(src) = find_source_dll() {
        Ok(src)
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "ferrow_winsearch.dll not found next to executable",
        ))
    }
}

fn find_searchhost_pids() -> io::Result<Vec<u32>> {
    let mut my_session = 0u32;
    unsafe {
        let _ = ProcessIdToSessionId(GetCurrentProcessId(), &mut my_session);
    }
    let snap =
        unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }.map_err(io::Error::from)?;
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut pids = Vec::new();
    unsafe {
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                let len = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
                if name.eq_ignore_ascii_case("SearchHost.exe") {
                    let mut proc_session = 0u32;
                    if ProcessIdToSessionId(entry.th32ProcessID, &mut proc_session).is_ok()
                        && proc_session == my_session
                    {
                        pids.push(entry.th32ProcessID);
                    }
                }
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }

    // The snapshot only supplies candidates. Keep only processes whose
    // opened handle verifies as the trusted CBS SearchHost image.
    let mut trusted = Vec::new();
    for pid in pids {
        let handle = unsafe {
            OpenProcess(
                PROCESS_ACCESS_RIGHTS(PROCESS_QUERY_LIMITED_INFORMATION),
                false,
                pid,
            )
        };
        let Ok(handle) = handle else { continue };
        if verify_searchhost_identity(handle, pid).is_ok() {
            trusted.push(pid);
        }
        unsafe {
            let _ = CloseHandle(handle);
        }
    }
    Ok(trusted)
}

fn list_remote_modules(pid: u32) -> io::Result<Vec<(String, usize, usize)>> {
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, pid) }
        .map_err(io::Error::from)?;
    let mut entry = MODULEENTRY32W {
        dwSize: size_of::<MODULEENTRY32W>() as u32,
        ..Default::default()
    };
    let mut mods = Vec::new();
    unsafe {
        if Module32FirstW(snap, &mut entry).is_ok() {
            loop {
                let len = entry
                    .szModule
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szModule.len());
                let name = String::from_utf16_lossy(&entry.szModule[..len]).to_ascii_lowercase();
                mods.push((name, entry.modBaseAddr as usize, entry.modBaseSize as usize));
                if Module32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    Ok(mods)
}

fn attach_to_searchhost(dll_path: &Path, data_dir: &Path) -> io::Result<bool> {
    if size_of::<*const c_void>() != 8 {
        return Err(io::Error::other(
            "Windows Search integration needs a 64-bit Ferrow build",
        ));
    }
    let pids = find_searchhost_pids()?;
    if pids.is_empty() {
        return Ok(false);
    }

    let dll_name_lower = dll_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(DLL_NAME)
        .to_ascii_lowercase();

    let mut last_error = None;
    for pid in pids {
        let result = (|| -> io::Result<bool> {
            let remote_mods = list_remote_modules(pid)?;
            let already_loaded = remote_mods.iter().any(|(n, _, _)| *n == dll_name_lower);
            if already_loaded && LAST_ATTACHED_PID.load(Ordering::Acquire) == pid {
                return Ok(true);
            }

            let proc_handle = unsafe {
                OpenProcess(PROCESS_ACCESS_RIGHTS(ATTACH_ACCESS), false, pid)
                    .map_err(io::Error::from)?
            };
            let result = unsafe {
                attach_to_process_handle(proc_handle, pid, dll_path, &dll_name_lower, data_dir)
            };
            unsafe {
                let _ = CloseHandle(proc_handle);
            }
            result
        })();
        match result {
            Ok(true) => {
                LAST_ATTACHED_PID.store(pid, Ordering::Release);
                return Ok(true);
            }
            Ok(false) => {}
            Err(error) => last_error = Some(error),
        }
    }
    match last_error {
        Some(error) => Err(error),
        None => Ok(false),
    }
}

unsafe fn attach_to_process_handle(
    proc_handle: HANDLE,
    pid: u32,
    dll_path: &Path,
    dll_name_lower: &str,
    data_dir: &Path,
) -> io::Result<bool> {
    // Identity and bridge access come from THIS handle, never from the
    // pid snapshot taken earlier: a reused pid must not be injected,
    // and only the verified SearchHost package gets bridge access.
    verify_searchhost_identity(proc_handle, pid)?;
    let (sid, sid_text) = appcontainer_sid_of_process(proc_handle)?;
    grant_searchhost_access(data_dir, &sid, &sid_text)?;
    record_granted_dll_path(data_dir, &sid_text, &sid, dll_path)?;
    apply_sid_access(dll_path, &sid, Some(AccessKind::ReadExecute))?;

    let mut proc_machine = IMAGE_FILE_MACHINE(0);
    let mut native_machine = IMAGE_FILE_MACHINE(0);
    unsafe {
        IsWow64Process2(proc_handle, &mut proc_machine, Some(&mut native_machine))
            .map_err(io::Error::from)?;
    }
    if proc_machine.0 != 0 || native_machine.0 != 0x8664 {
        return Err(io::Error::other(
            "Windows Search integration supports 64-bit x86 Windows only",
        ));
    }

    let remote_mods = list_remote_modules(pid)?;
    if !remote_mods.iter().any(|(n, _, _)| n == dll_name_lower) {
        let load_lib_addr = resolve_remote_load_library(pid, &remote_mods)?;
        let dll_wide: Vec<u16> = dll_path.as_os_str().encode_wide().chain(Some(0)).collect();
        let _ = unsafe { run_remote_thread(proc_handle, load_lib_addr, &dll_wide, 10_000)? };
    }

    let updated_mods = list_remote_modules(pid)?;
    let Some(&(_, remote_dll_base, _)) = updated_mods.iter().find(|(n, _, _)| n == dll_name_lower)
    else {
        return Err(io::Error::other(
            "ferrow_winsearch.dll did not load in SearchHost.exe",
        ));
    };

    let attach_rva = export_rva(dll_path)?;
    let data_wide: Vec<u16> = data_dir.as_os_str().encode_wide().chain(Some(0)).collect();
    let exit_code = unsafe {
        run_remote_thread(
            proc_handle,
            remote_dll_base + attach_rva,
            &data_wide,
            10_000,
        )?
    };
    if exit_code != 0 {
        return Err(io::Error::other(format!(
            "FerrowAttach returned non-zero status: {exit_code:#x}"
        )));
    }
    Ok(true)
}

fn resolve_remote_load_library(
    _pid: u32,
    remote_mods: &[(String, usize, usize)],
) -> io::Result<usize> {
    let local_mods = list_remote_modules(unsafe { GetCurrentProcessId() })?;
    let k32 = unsafe { GetModuleHandleW(w!("kernel32.dll")).map_err(io::Error::from)? };
    let proc_addr = unsafe { GetProcAddress(k32, s!("LoadLibraryW")) }
        .ok_or_else(|| io::Error::other("LoadLibraryW not found"))? as usize;

    let Some((owner_name, local_base, _)) = local_mods
        .iter()
        .find(|(_, base, len)| proc_addr >= *base && proc_addr < *base + *len)
    else {
        return Err(io::Error::other("owner module for LoadLibraryW not found"));
    };
    let Some((_, remote_base, _)) = remote_mods.iter().find(|(n, _, _)| n == owner_name) else {
        return Err(io::Error::other("remote kernel32 module not found"));
    };
    Ok(remote_base + (proc_addr - local_base))
}

fn export_rva(dll_path: &Path) -> io::Result<usize> {
    let wide: Vec<u16> = dll_path.as_os_str().encode_wide().chain(Some(0)).collect();
    let local: HMODULE = unsafe {
        LoadLibraryExW(PCWSTR(wide.as_ptr()), None, DONT_RESOLVE_DLL_REFERENCES)
            .map_err(io::Error::from)?
    };
    let res = unsafe { GetProcAddress(local, s!("FerrowAttach")) }
        .map(|fn_ptr| (fn_ptr as usize) - (local.0 as usize))
        .ok_or_else(|| io::Error::other("FerrowAttach export missing"));
    unsafe {
        let _ = FreeLibrary(local);
    }
    res
}

unsafe fn run_remote_thread(
    proc_handle: HANDLE,
    start_addr: usize,
    utf16_arg: &[u16],
    timeout_ms: u32,
) -> io::Result<u32> {
    let byte_len = utf16_arg.len() * 2;
    let remote_mem = unsafe {
        VirtualAllocEx(
            proc_handle,
            None,
            byte_len,
            MEM_COMMIT | MEM_RESERVE,
            PAGE_READWRITE,
        )
    };
    if remote_mem.is_null() {
        return Err(io::Error::last_os_error());
    }

    let mut written = 0usize;
    let write_ok = unsafe {
        windows::Win32::System::Diagnostics::Debug::WriteProcessMemory(
            proc_handle,
            remote_mem,
            utf16_arg.as_ptr().cast(),
            byte_len,
            Some(&mut written),
        )
    };
    if write_ok.is_err() || written != byte_len {
        unsafe {
            let _ = VirtualFreeEx(proc_handle, remote_mem, 0, MEM_RELEASE);
        }
        return Err(io::Error::other("WriteProcessMemory failed"));
    }

    let start_routine: unsafe extern "system" fn(*mut c_void) -> u32 =
        unsafe { std::mem::transmute(start_addr) };
    let thread = unsafe {
        CreateRemoteThread(
            proc_handle,
            None,
            0,
            Some(start_routine),
            Some(remote_mem),
            0,
            None,
        )
    };
    let Ok(thread) = thread else {
        let err = io::Error::last_os_error();
        unsafe {
            let _ = VirtualFreeEx(proc_handle, remote_mem, 0, MEM_RELEASE);
        }
        return Err(err);
    };

    let wait_status = unsafe { WaitForSingleObject(thread, timeout_ms) };
    if wait_status != WAIT_OBJECT_0 {
        unsafe {
            let _ = CloseHandle(thread);
        }
        // Leave `remote_mem` allocated if the remote thread is still running.
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "remote thread timed out",
        ));
    }

    let mut exit_code = 0u32;
    let _ = unsafe { GetExitCodeThread(thread, &mut exit_code) };
    unsafe {
        let _ = CloseHandle(thread);
        let _ = VirtualFreeEx(proc_handle, remote_mem, 0, MEM_RELEASE);
    }
    Ok(exit_code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trusts_only_the_cbs_systemapps_searchhost_image() {
        let windows = Path::new(r"C:\Windows");
        let trusted = Path::new(
            r"\\?\C:\WINDOWS\SystemApps\MicrosoftWindows.Client.CBS_cw5n1h2txyewy\SearchHost.exe",
        );
        assert!(is_trusted_searchhost_image(trusted, windows));

        for decoy in [
            r"C:\Program Files\WindowsApps\Contoso.Search\SearchHost.exe",
            r"C:\Users\someone\AppData\Local\Temp\SearchHost.exe",
            r"C:\Windows\SystemApps\Some.Other_package\SearchHost.exe",
            r"C:\Windows\SystemApps\MicrosoftWindows.Client.CBS_cw5n1h2txyewy\NotSearchHost.exe",
        ] {
            assert!(
                !is_trusted_searchhost_image(Path::new(decoy), windows),
                "decoy must be rejected: {decoy}"
            );
        }
    }

    #[test]
    fn windows_directory_comes_from_the_os() {
        let dir = windows_directory().expect("OS reports its Windows directory");
        assert!(dir.is_dir(), "not a directory: {}", dir.display());
        assert!(dir.join("System32").is_dir());
        // A spoofed environment must not move the trust anchor.
        assert_ne!(dir, Path::new(r"C:\definitely-not-windows"));
        let expected = std::env::var_os("SystemRoot").map(PathBuf::from);
        if let Some(expected) = expected {
            assert_eq!(
                normalize_windows_path(&dir),
                normalize_windows_path(&expected)
            );
        }
    }

    #[test]
    fn grant_record_lives_outside_bridge_and_round_trips_paths() {
        let data_dir =
            std::env::temp_dir().join(format!("ferrow-winsearch-record-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data_dir);
        std::fs::create_dir_all(&data_dir).unwrap();
        let sid_text = "S-1-15-2-123-456";
        let sid_bytes = sid_string_to_bytes(sid_text).unwrap();
        let dll = Path::new(r"C:\Program Files\Ferrow\ferrow_winsearch.dll").to_path_buf();
        let record = GrantRecord {
            sid_text: sid_text.to_owned(),
            sid_bytes,
            dll_paths: vec![dll],
        };
        write_grant_record(&data_dir, &record).unwrap();

        assert!(grant_record_path(&data_dir).starts_with(&data_dir));
        assert!(!grant_record_path(&data_dir).starts_with(ipc::winsearch_dir(&data_dir)));
        assert_eq!(read_grant_record(&data_dir).unwrap(), Some(record));

        // A replaced or corrupted record is an error, never silently
        // treated as authority to revoke an arbitrary SID.
        std::fs::write(grant_record_path(&data_dir), "S-1-5-21-1\n").unwrap();
        assert!(read_grant_record(&data_dir).is_err());
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    /// When a verified SearchHost runs here, its token must yield a
    /// package SID — the narrow bridge grant depends on it. Skip only
    /// when there is no trusted candidate at all (for example CI).
    #[test]
    fn resolves_searchhost_package_sid_when_running() {
        let candidates = find_searchhost_pids().expect("candidate scan cannot fail the test");
        if candidates.is_empty() {
            eprintln!("skipping: no verified SearchHost process on this host");
            return;
        }
        let (bytes, text) = resolve_searchhost_sid()
            .expect("verified SearchHost candidate exists, so SID resolution must succeed");
        assert!(!bytes.is_empty());
        assert!(text.starts_with("S-1-15-2-"), "unexpected SID: {text}");
        assert_ne!(text, LEGACY_ALL_PACKAGES_SID);
        let round_trip = sid_string_to_bytes(&text).expect("SID round-trips");
        assert_eq!(round_trip, bytes);
    }
}
