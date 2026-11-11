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
    CloseHandle, ERROR_SUCCESS, GENERIC_EXECUTE, GENERIC_READ, GENERIC_WRITE, HANDLE, HLOCAL,
    LocalFree, WAIT_OBJECT_0,
};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSidToSidW, EXPLICIT_ACCESS_W, GetNamedSecurityInfoW,
    REVOKE_ACCESS, SE_FILE_OBJECT, SET_ACCESS, SetEntriesInAclW, SetNamedSecurityInfoW,
    TRUSTEE_IS_SID, TRUSTEE_W,
};
use windows::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACL, CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, EqualSid, GetAce,
    GetLengthSid, GetTokenInformation, NO_INHERITANCE, OBJECT_INHERIT_ACE, PSECURITY_DESCRIPTOR,
    PSID, TOKEN_APPCONTAINER_INFORMATION, TOKEN_QUERY, TokenAppContainerSid,
};
use windows::Win32::Storage::FileSystem::DELETE;
use windows::Win32::System::Diagnostics::Debug::{ReadProcessMemory, WriteProcessMemory};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, MODULEENTRY32W, Module32FirstW, Module32NextW, PROCESSENTRY32W,
    Process32FirstW, Process32NextW, TH32CS_SNAPMODULE, TH32CS_SNAPMODULE32, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
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

/// `INHERITED_ACE` flag in an ACE header. Inherited ACEs come from the
/// parent folder (for example Windows' own `ALL APPLICATION PACKAGES`
/// read access under `Program Files`); they are never grants Ferrow
/// made and cannot be removed by editing the child's DACL.
const INHERITED_ACE_FLAG: u8 = 0x10;

/// Whether `dacl` carries an explicit (non-inherited) plain
/// allowed/denied ACE for `sid` — the only kind Ferrow ever writes.
fn acl_has_explicit_sid(dacl: *mut ACL, sid: PSID) -> bool {
    if dacl.is_null() {
        return false;
    }
    // SAFETY: `dacl` is a live DACL from GetNamedSecurityInfoW and
    // `sid` a valid SID for this call; ACE bodies are read only
    // through their header-typed layout.
    unsafe {
        let count = (*dacl).AceCount as u32;
        for index in 0..count {
            let mut ace: *mut c_void = ptr::null_mut();
            if GetAce(dacl, index, &mut ace).is_err() || ace.is_null() {
                continue;
            }
            let plain = &*ace.cast::<ACCESS_ALLOWED_ACE>();
            // ACCESS_ALLOWED_ACE_TYPE / ACCESS_DENIED_ACE_TYPE only;
            // object and callback ACEs have a different SID offset
            // and are never written by this code.
            if !matches!(plain.Header.AceType, 0 | 1)
                || plain.Header.AceFlags & INHERITED_ACE_FLAG != 0
            {
                continue;
            }
            let ace_sid = PSID(ptr::addr_of!(plain.SidStart).cast_mut().cast::<c_void>());
            if EqualSid(ace_sid, sid).is_ok() {
                return true;
            }
        }
        false
    }
}

/// Whether `path` currently carries an explicit ACE for `sid_bytes`.
/// Read-only: succeeds on paths an unelevated process may inspect but
/// not rewrite (for example the installed DLL under `Program Files`).
fn has_explicit_sid_ace(path: &Path, sid_bytes: &[u8]) -> io::Result<bool> {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: sid_bytes holds a valid self-relative SID for this call.
    let sid = PSID(sid_bytes.as_ptr().cast_mut().cast::<c_void>());
    let mut dacl: *mut ACL = ptr::null_mut();
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: out-params follow the GetNamedSecurityInfoW contract;
    // the returned descriptor is freed below.
    let get_err = unsafe {
        GetNamedSecurityInfoW(
            PCWSTR(wide.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&mut dacl),
            None,
            &mut sd,
        )
    };
    if get_err != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(get_err.0 as i32));
    }
    let found = acl_has_explicit_sid(dacl, sid);
    unsafe {
        let _ = LocalFree(Some(HLOCAL(sd.0)));
    }
    Ok(found)
}

/// Grant (`kind = Some`) or revoke (`kind = None`) access for `sid` on
/// `path`, with container/object inheritance for folders.
fn apply_sid_access(path: &Path, sid_bytes: &[u8], kind: Option<AccessKind>) -> io::Result<()> {
    if sid_bytes.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty SID"));
    }
    // Revoking a SID with no explicit ACE on the path is a no-op
    // success, decided without a DACL write. Without this, cleaning
    // up paths an unelevated GUI cannot rewrite (the installed DLL
    // under `Program Files`, whose only AppContainer ACE is Windows'
    // inherited one) failed with Access denied and blocked enabling.
    if kind.is_none() && !has_explicit_sid_ace(path, sid_bytes)? {
        return Ok(());
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

    // SYNCHRONIZE (0x0010_0000) | FILE_TRAVERSE (0x20) | FILE_READ_ATTRIBUTES (0x80):
    // reach children by full path and let `std::fs::metadata` / `Path::is_dir`
    // (`CreateFileW` with `FILE_FLAG_BACKUP_SEMANTICS`, which requests `SYNCHRONIZE`)
    // succeed under a LowBox AppContainer token without listing the folder or
    // propagating any ACE to sibling files (`NO_INHERITANCE` + `SET_ACCESS`).
    let (perms, mode, inheritance) = match kind {
        Some(AccessKind::Traverse) => (0x0010_0000u32 | 0x20 | 0x80, SET_ACCESS, NO_INHERITANCE),
        Some(AccessKind::ReadExecute) => (
            GENERIC_READ.0 | GENERIC_EXECUTE.0,
            SET_ACCESS,
            if path.is_dir() {
                CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE
            } else {
                NO_INHERITANCE
            },
        ),
        Some(AccessKind::Modify) => (
            GENERIC_READ.0 | GENERIC_WRITE.0 | GENERIC_EXECUTE.0 | DELETE.0,
            SET_ACCESS,
            if path.is_dir() {
                CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE
            } else {
                NO_INHERITANCE
            },
        ),
        None => (0, REVOKE_ACCESS, NO_INHERITANCE),
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
    record
        .dll_paths
        .retain(|path| path == dll_path || path.exists());
    if !record.dll_paths.iter().any(|path| path == dll_path) {
        record.dll_paths.push(dll_path.to_path_buf());
    }
    write_grant_record(data_dir, &record)
}

fn is_staged_dll_filename(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == DLL_NAME
        || lower == "ferrow_winsearch.dll.old"
        || (lower.starts_with("ferrow_winsearch-") && lower.ends_with(".dll"))
}

fn staged_dll_files(ws_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(ws_dir) else {
        return Vec::new();
    };
    let src_norm = find_source_dll().map(|src| normalize_windows_path(&src));
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !is_staged_dll_filename(name) {
            continue;
        }
        if src_norm
            .as_deref()
            .is_some_and(|src| src == normalize_windows_path(&path))
        {
            continue;
        }
        files.push(path);
    }
    files
}

fn access_paths(data_dir: &Path, extra_dll_paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = bridge_paths(data_dir)
        .into_iter()
        .map(|(path, _)| path)
        .collect();
    let ws_dir = ipc::winsearch_dir(data_dir);
    let src_norm = find_source_dll().map(|src| normalize_windows_path(&src));
    let staged = ws_dir.join(DLL_NAME);
    if src_norm
        .as_deref()
        .is_none_or(|src| src != normalize_windows_path(&staged))
        && !paths.contains(&staged)
    {
        paths.push(staged);
    }
    for path in staged_dll_files(&ws_dir) {
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    // Deliberately no `find_source_dll()`: granted DLL paths are
    // revoked from the grant record (`extra_dll_paths`), and in the
    // installed layout the source DLL is admin-owned under
    // `Program Files` — its only AppContainer ACE is Windows' own
    // inherited one, never a grant of ours to revoke.
    for path in extra_dll_paths {
        if src_norm
            .as_deref()
            .is_some_and(|src| src == normalize_windows_path(path))
        {
            continue;
        }
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
            first_error = Some(io::Error::new(
                error.kind(),
                format!("{}: {error}", path.display()),
            ));
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
    let ws_dir = ipc::winsearch_dir(data_dir);
    let _ = std::fs::remove_file(ws_dir.join("granted-sid"));
    for staged in staged_dll_files(&ws_dir) {
        let _ = std::fs::remove_file(staged);
    }
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

fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Stage `src` under a content-addressed filename (`ferrow_winsearch-<hash>.dll`)
/// inside `ws_dir` and opportunistically sweep older unmapped staged copies.
///
/// Using a distinct module filename per DLL build avoids Windows' PEB module-name
/// collision when `SearchHost.exe` still has an older `ferrow_winsearch*.dll`
/// mapped from before an in-place Ferrow upgrade.
fn stage_dll_from_source(src: &Path, ws_dir: &Path) -> io::Result<PathBuf> {
    std::fs::create_dir_all(ws_dir)?;
    let bytes = std::fs::read(src)?;
    let staged_name = format!("ferrow_winsearch-{:016x}.dll", fnv1a_64(&bytes));
    let staged = ws_dir.join(&staged_name);

    if normalize_windows_path(src) != normalize_windows_path(&staged) {
        let already_matches = std::fs::read(&staged)
            .map(|existing| existing == bytes)
            .unwrap_or(false);
        if !already_matches {
            let tmp = ws_dir.join(format!(".{staged_name}.tmp.{}", std::process::id()));
            std::fs::write(&tmp, &bytes)?;
            if let Err(rename_err) = std::fs::rename(&tmp, &staged) {
                let _ = std::fs::remove_file(&tmp);
                let now_matches = std::fs::read(&staged)
                    .map(|existing| existing == bytes)
                    .unwrap_or(false);
                if !now_matches {
                    return Err(rename_err);
                }
            }
        }
    }

    let keep_norm = normalize_windows_path(&staged);
    for old in staged_dll_files(ws_dir) {
        if normalize_windows_path(&old) != keep_norm {
            let _ = std::fs::remove_file(old);
        }
    }

    Ok(staged)
}

fn stage_attachment_dll(ws_dir: &Path) -> io::Result<PathBuf> {
    if let Some(src) = find_source_dll() {
        return stage_dll_from_source(&src, ws_dir);
    }
    let fallback = ws_dir.join(DLL_NAME);
    if fallback.is_file() {
        Ok(fallback)
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

fn is_ferrow_winsearch_module(mod_name_lower: &str) -> bool {
    mod_name_lower == DLL_NAME
        || (mod_name_lower.starts_with("ferrow_winsearch-") && mod_name_lower.ends_with(".dll"))
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

    // Detach any older `ferrow_winsearch*.dll` still mapped from a prior
    // build so `tquery.dll`'s vtable slot returns to `tquery.dll` before
    // the newly staged DLL installs its hook.
    for (mod_name, old_base, old_size) in &remote_mods {
        if mod_name != dll_name_lower && is_ferrow_winsearch_module(mod_name) {
            let detach_rva = remote_export_rva(proc_handle, *old_base, *old_size, b"FerrowDetach")?;
            let detach_addr = old_base
                .checked_add(detach_rva)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "detach RVA overflow"))?;
            let detach_code = unsafe { run_remote_thread(proc_handle, detach_addr, None, 10_000)? };
            if detach_code != 0 {
                return Err(io::Error::other(format!(
                    "FerrowDetach on prior {mod_name} returned non-zero status: {detach_code:#x}"
                )));
            }
        }
    }

    if !remote_mods.iter().any(|(n, _, _)| n == dll_name_lower) {
        let load_lib_addr = resolve_remote_load_library(pid, &remote_mods)?;
        let dll_wide: Vec<u16> = dll_path.as_os_str().encode_wide().chain(Some(0)).collect();
        let _ = unsafe { run_remote_thread(proc_handle, load_lib_addr, Some(&dll_wide), 10_000)? };
    }

    let updated_mods = list_remote_modules(pid)?;
    let Some(&(_, remote_dll_base, remote_dll_size)) =
        updated_mods.iter().find(|(n, _, _)| n == dll_name_lower)
    else {
        return Err(io::Error::other(
            "ferrow_winsearch.dll did not load in SearchHost.exe",
        ));
    };

    let attach_rva = remote_export_rva(
        proc_handle,
        remote_dll_base,
        remote_dll_size,
        b"FerrowAttach",
    )?;
    let attach_addr = remote_dll_base
        .checked_add(attach_rva)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "attach RVA overflow"))?;
    let data_wide: Vec<u16> = data_dir.as_os_str().encode_wide().chain(Some(0)).collect();
    let exit_code =
        unsafe { run_remote_thread(proc_handle, attach_addr, Some(&data_wide), 10_000)? };
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

fn read_remote_bytes(
    proc_handle: HANDLE,
    mod_base: usize,
    mod_size: usize,
    rva: usize,
    out: &mut [u8],
) -> io::Result<()> {
    let end = rva
        .checked_add(out.len())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "PE RVA overflow"))?;
    if end > mod_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "PE read out of module bounds",
        ));
    }
    let addr = mod_base
        .checked_add(rva)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "remote address overflow"))?;
    let mut read = 0usize;
    unsafe {
        ReadProcessMemory(
            proc_handle,
            addr as *const c_void,
            out.as_mut_ptr().cast(),
            out.len(),
            Some(&mut read),
        )
        .map_err(io::Error::from)?;
    }
    if read != out.len() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "short ReadProcessMemory",
        ));
    }
    Ok(())
}

fn read_remote_u16(
    proc_handle: HANDLE,
    mod_base: usize,
    mod_size: usize,
    rva: usize,
) -> io::Result<u16> {
    let mut bytes = [0u8; 2];
    read_remote_bytes(proc_handle, mod_base, mod_size, rva, &mut bytes)?;
    Ok(u16::from_le_bytes(bytes))
}

fn read_remote_u32(
    proc_handle: HANDLE,
    mod_base: usize,
    mod_size: usize,
    rva: usize,
) -> io::Result<u32> {
    let mut bytes = [0u8; 4];
    read_remote_bytes(proc_handle, mod_base, mod_size, rva, &mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn remote_cstr_equals(
    proc_handle: HANDLE,
    mod_base: usize,
    mod_size: usize,
    name_rva: usize,
    expected: &[u8],
) -> io::Result<bool> {
    let avail = mod_size.saturating_sub(name_rva);
    if avail <= expected.len() {
        return Ok(false);
    }
    let mut buf = vec![0u8; expected.len() + 1];
    read_remote_bytes(proc_handle, mod_base, mod_size, name_rva, &mut buf)?;
    Ok(&buf[..expected.len()] == expected && buf[expected.len()] == 0)
}

/// Resolve an exported function RVA directly from the mapped PE32+ image in
/// `proc_handle`. Reading the remote module's own export directory avoids
/// mapping anything from the bridge folder into `ferrow-gui` and eliminates
/// disk-vs-memory RVA skew across in-place upgrades.
fn remote_export_rva(
    proc_handle: HANDLE,
    mod_base: usize,
    mod_size: usize,
    symbol: &[u8],
) -> io::Result<usize> {
    let _ = mod_base
        .checked_add(mod_size)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "remote module span overflow"))?;
    if read_remote_u16(proc_handle, mod_base, mod_size, 0)? != 0x5A4D {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "remote module missing MZ header",
        ));
    }
    let pe = read_remote_u32(proc_handle, mod_base, mod_size, 0x3C)? as usize;
    if read_remote_u32(proc_handle, mod_base, mod_size, pe)? != 0x0000_4550 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "remote module missing PE signature",
        ));
    }
    let coff = pe
        .checked_add(4)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "PE header overflow"))?;
    if read_remote_u16(proc_handle, mod_base, mod_size, coff)? != 0x8664 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "remote module is not x86-64",
        ));
    }
    let opt_size = read_remote_u16(proc_handle, mod_base, mod_size, coff + 16)? as usize;
    let opt = coff
        .checked_add(20)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "optional header overflow"))?;
    if opt_size < 120
        || opt
            .checked_add(opt_size)
            .is_none_or(|opt_end| opt_end > mod_size)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "remote optional header truncated",
        ));
    }
    if read_remote_u16(proc_handle, mod_base, mod_size, opt)? != 0x020B {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "remote module is not PE32+",
        ));
    }
    if read_remote_u32(proc_handle, mod_base, mod_size, opt + 108)? < 1 {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "remote module has no export data directory",
        ));
    }
    let export_dir_rva = read_remote_u32(proc_handle, mod_base, mod_size, opt + 112)? as usize;
    let export_dir_size = read_remote_u32(proc_handle, mod_base, mod_size, opt + 116)? as usize;
    if export_dir_rva == 0 || export_dir_size < 40 {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "remote module has an empty export directory",
        ));
    }
    let export_dir_end = export_dir_rva
        .checked_add(export_dir_size)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "export directory overflow"))?;
    if export_dir_end > mod_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "export directory out of module bounds",
        ));
    }

    let number_of_functions =
        read_remote_u32(proc_handle, mod_base, mod_size, export_dir_rva + 20)? as usize;
    let number_of_names =
        read_remote_u32(proc_handle, mod_base, mod_size, export_dir_rva + 24)? as usize;
    let addr_of_functions =
        read_remote_u32(proc_handle, mod_base, mod_size, export_dir_rva + 28)? as usize;
    let addr_of_names =
        read_remote_u32(proc_handle, mod_base, mod_size, export_dir_rva + 32)? as usize;
    let addr_of_name_ordinals =
        read_remote_u32(proc_handle, mod_base, mod_size, export_dir_rva + 36)? as usize;

    if number_of_names > 65_536 || number_of_functions > 65_536 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "remote export table exceeds sanity cap",
        ));
    }

    let mut name_rvas = vec![0u8; number_of_names * 4];
    read_remote_bytes(
        proc_handle,
        mod_base,
        mod_size,
        addr_of_names,
        &mut name_rvas,
    )?;
    let mut ordinals = vec![0u8; number_of_names * 2];
    read_remote_bytes(
        proc_handle,
        mod_base,
        mod_size,
        addr_of_name_ordinals,
        &mut ordinals,
    )?;

    for index in 0..number_of_names {
        let name_rva =
            u32::from_le_bytes(name_rvas[index * 4..index * 4 + 4].try_into().unwrap()) as usize;
        if !remote_cstr_equals(proc_handle, mod_base, mod_size, name_rva, symbol)? {
            continue;
        }
        let ordinal_index =
            u16::from_le_bytes(ordinals[index * 2..index * 2 + 2].try_into().unwrap()) as usize;
        if ordinal_index >= number_of_functions {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "export ordinal out of bounds",
            ));
        }
        let fn_slot_rva = addr_of_functions
            .checked_add(ordinal_index * 4)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "function table overflow"))?;
        let fn_rva = read_remote_u32(proc_handle, mod_base, mod_size, fn_slot_rva)? as usize;
        if fn_rva == 0 || fn_rva >= mod_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "export RVA out of module bounds",
            ));
        }
        if fn_rva >= export_dir_rva && fn_rva < export_dir_end {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "forwarded export cannot be called directly",
            ));
        }
        return Ok(fn_rva);
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("export {} missing", String::from_utf8_lossy(symbol)),
    ))
}

unsafe fn run_remote_thread(
    proc_handle: HANDLE,
    start_addr: usize,
    utf16_arg: Option<&[u16]>,
    timeout_ms: u32,
) -> io::Result<u32> {
    let remote_mem = if let Some(units) = utf16_arg {
        let byte_len = units.len() * 2;
        let mem = unsafe {
            VirtualAllocEx(
                proc_handle,
                None,
                byte_len,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            )
        };
        if mem.is_null() {
            return Err(io::Error::last_os_error());
        }

        let mut written = 0usize;
        let write_ok = unsafe {
            WriteProcessMemory(
                proc_handle,
                mem,
                units.as_ptr().cast(),
                byte_len,
                Some(&mut written),
            )
        };
        if write_ok.is_err() || written != byte_len {
            unsafe {
                let _ = VirtualFreeEx(proc_handle, mem, 0, MEM_RELEASE);
            }
            return Err(io::Error::other("WriteProcessMemory failed"));
        }
        Some(mem)
    } else {
        None
    };

    let start_routine: unsafe extern "system" fn(*mut c_void) -> u32 =
        unsafe { std::mem::transmute(start_addr) };
    let thread = unsafe {
        CreateRemoteThread(
            proc_handle,
            None,
            0,
            Some(start_routine),
            remote_mem.map(|mem| mem as *const c_void),
            0,
            None,
        )
    };
    let Ok(thread) = thread else {
        let err = io::Error::last_os_error();
        if let Some(mem) = remote_mem {
            unsafe {
                let _ = VirtualFreeEx(proc_handle, mem, 0, MEM_RELEASE);
            }
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
        if let Some(mem) = remote_mem {
            let _ = VirtualFreeEx(proc_handle, mem, 0, MEM_RELEASE);
        }
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

    /// Regression for beta.5 on the installed layout: enabling failed
    /// with "Access is denied" because legacy cleanup tried to rewrite
    /// the DACL of `C:\Program Files\Ferrow\ferrow_winsearch.dll`,
    /// which carries only Windows' inherited `ALL APPLICATION
    /// PACKAGES` ACE. Revoking a SID with no explicit ACE must be a
    /// no-op success, decided by reading the DACL only.
    #[test]
    fn revoking_ungranted_or_inherited_only_sid_is_a_no_op() {
        let bogus = sid_string_to_bytes("S-1-15-2-987654321-123456789").unwrap();

        // A system file this SID has no ACE on at all: an unelevated
        // revoke must not attempt the DACL write Windows would deny.
        let windows = windows_directory().expect("OS reports its Windows directory");
        let target = windows.join("System32").join("kernel32.dll");
        assert!(target.is_file(), "missing {}", target.display());
        assert!(!has_explicit_sid_ace(&target, &bogus).unwrap());
        apply_sid_access(&target, &bogus, None).expect("absent-SID revoke is a no-op");

        // An inherited-only ACE (the installed-DLL shape): grant
        // inheritable access on a folder, then create the file so its
        // ACE arrives by inheritance. Revoking on the file is a no-op
        // and leaves the folder's explicit grant untouched.
        let dir =
            std::env::temp_dir().join(format!("ferrow-winsearch-inherit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let legacy = sid_string_to_bytes(LEGACY_ALL_PACKAGES_SID).unwrap();
        apply_sid_access(&dir, &legacy, Some(AccessKind::Modify)).unwrap();
        assert!(has_explicit_sid_ace(&dir, &legacy).unwrap());
        let child = dir.join("child.txt");
        std::fs::write(&child, b"x").unwrap();
        assert!(
            !has_explicit_sid_ace(&child, &legacy).unwrap(),
            "child ACE must be inherited-only"
        );
        apply_sid_access(&child, &legacy, None).expect("inherited-only revoke is a no-op");
        assert!(has_explicit_sid_ace(&dir, &legacy).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grant_and_revoke_round_trip_on_temp_file() {
        let file =
            std::env::temp_dir().join(format!("ferrow-winsearch-acl-{}", std::process::id()));
        let _ = std::fs::remove_file(&file);
        std::fs::write(&file, b"x").unwrap();
        let sid = sid_string_to_bytes("S-1-15-2-987654321-123456789").unwrap();

        assert!(!has_explicit_sid_ace(&file, &sid).unwrap());
        apply_sid_access(&file, &sid, Some(AccessKind::Modify)).unwrap();
        assert!(has_explicit_sid_ace(&file, &sid).unwrap());
        apply_sid_access(&file, &sid, None).unwrap();
        assert!(!has_explicit_sid_ace(&file, &sid).unwrap());
        // A second revoke is an idempotent no-op success.
        apply_sid_access(&file, &sid, None).unwrap();
        let _ = std::fs::remove_file(&file);
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

    #[test]
    fn remote_export_rva_matches_getprocaddress_and_rejects_forwarders() {
        use windows::Win32::System::Threading::GetCurrentProcess;

        let proc_handle = unsafe { GetCurrentProcess() };
        let mods = list_remote_modules(unsafe { GetCurrentProcessId() }).unwrap();
        let &(_, k32_base, k32_size) = mods
            .iter()
            .find(|(n, _, _)| n == "kernel32.dll")
            .expect("kernel32.dll is mapped in every Win32 process");

        let k32 = unsafe { GetModuleHandleW(w!("kernel32.dll")).unwrap() };
        let expected_addr =
            unsafe { GetProcAddress(k32, s!("LoadLibraryW")).expect("LoadLibraryW exists") }
                as usize;
        let resolved_rva =
            remote_export_rva(proc_handle, k32_base, k32_size, b"LoadLibraryW").unwrap();
        assert_eq!(k32_base + resolved_rva, expected_addr);

        let missing = remote_export_rva(
            proc_handle,
            k32_base,
            k32_size,
            b"FerrowDefinitelyMissingSymbol",
        )
        .unwrap_err();
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);

        // `kernel32!HeapAlloc` is a PE forwarder string (`NTDLL.RtlAllocateHeap`);
        // calling its RVA directly via `CreateRemoteThread` would jump into ASCII
        // data, so `remote_export_rva` must reject it.
        let forwarded =
            remote_export_rva(proc_handle, k32_base, k32_size, b"HeapAlloc").unwrap_err();
        assert_eq!(forwarded.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn content_addressed_dll_staging_sweeps_stale_copies_and_prunes_record() {
        let base =
            std::env::temp_dir().join(format!("ferrow-winsearch-stage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let src_dir = base.join("src");
        let data_dir = base.join("data");
        let ws_dir = ipc::winsearch_dir(&data_dir);
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::create_dir_all(&ws_dir).unwrap();

        let src_dll = src_dir.join(DLL_NAME);
        let legacy_dll = ws_dir.join(DLL_NAME);
        let legacy_old = ws_dir.join("ferrow_winsearch.dll.old");
        std::fs::write(&legacy_dll, b"beta6").unwrap();
        std::fs::write(&legacy_old, b"beta5").unwrap();

        std::fs::write(&src_dll, b"build-v1-payload").unwrap();
        let staged_v1 = stage_dll_from_source(&src_dll, &ws_dir).unwrap();
        assert!(staged_v1.is_file());
        assert_eq!(std::fs::read(&staged_v1).unwrap(), b"build-v1-payload");
        assert!(!legacy_dll.exists(), "unlocked legacy DLL should be swept");
        assert!(
            !legacy_old.exists(),
            "unlocked .old fallback DLL should be swept"
        );

        let sid_text = "S-1-15-2-222-333";
        let sid_bytes = sid_string_to_bytes(sid_text).unwrap();
        record_granted_dll_path(&data_dir, sid_text, &sid_bytes, &staged_v1).unwrap();

        // Idempotent re-stage of identical bytes keeps the exact same filename.
        let staged_v1_again = stage_dll_from_source(&src_dll, &ws_dir).unwrap();
        assert_eq!(staged_v1_again, staged_v1);

        // New build payload stages under a distinct filename and sweeps unmapped v1.
        std::fs::write(&src_dll, b"build-v2-payload-updated").unwrap();
        let staged_v2 = stage_dll_from_source(&src_dll, &ws_dir).unwrap();
        assert_ne!(staged_v2, staged_v1);
        assert!(staged_v2.is_file());
        assert!(!staged_v1.exists(), "unmapped v1 DLL should be swept");
        assert_eq!(
            std::fs::read(&staged_v2).unwrap(),
            b"build-v2-payload-updated"
        );

        // Recording v2 prunes deleted v1 from the grant record so it does not grow unboundedly.
        record_granted_dll_path(&data_dir, sid_text, &sid_bytes, &staged_v2).unwrap();
        let rec = read_grant_record(&data_dir).unwrap().unwrap();
        assert_eq!(rec.dll_paths, vec![staged_v2]);

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Regression for beta.6 (`FerrowAttach returned non-zero status: 0x80070057`):
    /// `AccessKind::Traverse` omitted `SYNCHRONIZE` (`0x0010_0000`), causing
    /// `Path::is_dir()` (`CreateFileW(..., FILE_FLAG_BACKUP_SEMANTICS)`) inside a
    /// real LowBox AppContainer token to fail with `ERROR_ACCESS_DENIED` and
    /// `attach_in_process` to return `E_INVALIDARG` (`0x80070057`), while its
    /// `(OI)(CI)` flags also propagated `(X,RA)` to sibling files in `data_dir`.
    #[test]
    fn lowbox_token_can_stat_data_dir_and_use_bridge_without_reading_siblings() {
        use windows::Win32::Security::Authorization::GRANT_ACCESS;
        use windows::Win32::Security::{
            FreeSid, ImpersonateLoggedOnUser, PROTECTED_DACL_SECURITY_INFORMATION, RevertToSelf,
            TOKEN_ACCESS_MASK, TOKEN_USER, TokenUser,
        };
        use windows::Win32::System::Threading::GetCurrentProcess;

        #[link(name = "userenv")]
        unsafe extern "system" {
            fn CreateAppContainerProfile(
                psz_app_container_name: PCWSTR,
                psz_display_name: PCWSTR,
                psz_description: PCWSTR,
                p_capabilities: *const c_void,
                dw_capability_count: u32,
                pp_sid_app_container_sid: *mut PSID,
            ) -> windows::core::HRESULT;
            fn DeleteAppContainerProfile(psz_app_container_name: PCWSTR) -> windows::core::HRESULT;
        }

        #[link(name = "ntdll")]
        unsafe extern "system" {
            fn NtCreateLowBoxToken(
                token_handle: *mut HANDLE,
                existing_token_handle: HANDLE,
                desired_access: u32,
                object_attributes: *const c_void,
                package_sid: PSID,
                capability_count: u32,
                capabilities: *const c_void,
                handle_count: u32,
                handles: *const HANDLE,
            ) -> i32;
        }

        struct Cleanup {
            profile_wide: Vec<u16>,
            root_dir: PathBuf,
            lowbox_token: HANDLE,
            base_token: HANDLE,
        }
        impl Drop for Cleanup {
            fn drop(&mut self) {
                unsafe {
                    if !self.lowbox_token.is_invalid() && !self.lowbox_token.0.is_null() {
                        let _ = CloseHandle(self.lowbox_token);
                    }
                    if !self.base_token.is_invalid() && !self.base_token.0.is_null() {
                        let _ = CloseHandle(self.base_token);
                    }
                    let _ = DeleteAppContainerProfile(PCWSTR(self.profile_wide.as_ptr()));
                }
                let _ = std::fs::remove_dir_all(&self.root_dir);
            }
        }

        struct RevertGuard;
        impl Drop for RevertGuard {
            fn drop(&mut self) {
                unsafe {
                    let _ = RevertToSelf();
                }
            }
        }

        fn run_as_lowbox<R: Send + 'static>(
            token_raw: usize,
            f: impl FnOnce() -> R + Send + 'static,
        ) -> R {
            std::thread::spawn(move || {
                let token = HANDLE(token_raw as *mut c_void);
                unsafe {
                    ImpersonateLoggedOnUser(token).expect("ImpersonateLoggedOnUser must succeed");
                }
                let _revert = RevertGuard;
                f()
            })
            .join()
            .expect("impersonated worker thread panicked")
        }

        let pid = std::process::id();
        let profile_name = format!("ferrow.winsearch.lowbox.{pid}");
        let profile_wide: Vec<u16> = profile_name.encode_utf16().chain(Some(0)).collect();
        let root_dir = std::env::temp_dir().join(format!("ferrow-winsearch-lowbox-{pid}"));
        let _ = std::fs::remove_dir_all(&root_dir);
        std::fs::create_dir_all(&root_dir).unwrap();

        unsafe {
            let _ = DeleteAppContainerProfile(PCWSTR(profile_wide.as_ptr()));
        }

        let mut cleanup = Cleanup {
            profile_wide: profile_wide.clone(),
            root_dir: root_dir.clone(),
            lowbox_token: HANDLE::default(),
            base_token: HANDLE::default(),
        };

        // Lock `root_dir` down to a protected current-user-only DACL so
        // `%TEMP%`'s default ACL cannot mask missing AppContainer grants.
        unsafe {
            const TOKEN_ALL_ACCESS_MASK: u32 = 0x000F_01FF;
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_ACCESS_MASK(TOKEN_ALL_ACCESS_MASK),
                &mut cleanup.base_token,
            )
            .unwrap();

            let mut needed = 0u32;
            let _ = GetTokenInformation(cleanup.base_token, TokenUser, None, 0, &mut needed);
            let mut user_buf = vec![0u8; needed as usize];
            GetTokenInformation(
                cleanup.base_token,
                TokenUser,
                Some(user_buf.as_mut_ptr().cast()),
                needed,
                &mut needed,
            )
            .unwrap();
            let user_sid = (*user_buf.as_ptr().cast::<TOKEN_USER>()).User.Sid;

            let ea = EXPLICIT_ACCESS_W {
                grfAccessPermissions: 0x1000_0000, // GENERIC_ALL
                grfAccessMode: SET_ACCESS,
                grfInheritance: CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE,
                Trustee: TRUSTEE_W {
                    TrusteeForm: TRUSTEE_IS_SID,
                    ptstrName: PWSTR(user_sid.0.cast()),
                    ..Default::default()
                },
            };
            let mut locked_dacl: *mut ACL = ptr::null_mut();
            let acl_err = SetEntriesInAclW(Some(&[ea]), None, &mut locked_dacl);
            assert_eq!(acl_err, ERROR_SUCCESS);
            let mut root_wide: Vec<u16> =
                root_dir.as_os_str().encode_wide().chain(Some(0)).collect();
            let set_err = SetNamedSecurityInfoW(
                PWSTR(root_wide.as_mut_ptr()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(locked_dacl),
                None,
            );
            let _ = LocalFree(Some(HLOCAL(locked_dacl.cast())));
            assert_eq!(set_err, ERROR_SUCCESS);

            let mut raw_ac_sid = PSID::default();
            let hr = CreateAppContainerProfile(
                PCWSTR(profile_wide.as_ptr()),
                PCWSTR(profile_wide.as_ptr()),
                PCWSTR(profile_wide.as_ptr()),
                ptr::null(),
                0,
                &mut raw_ac_sid,
            );
            assert!(hr.is_ok(), "CreateAppContainerProfile failed: {hr:?}");
            let sid_len = GetLengthSid(raw_ac_sid) as usize;
            let sid_bytes = std::slice::from_raw_parts(raw_ac_sid.0.cast::<u8>(), sid_len).to_vec();
            let status = NtCreateLowBoxToken(
                &mut cleanup.lowbox_token,
                cleanup.base_token,
                TOKEN_ALL_ACCESS_MASK,
                ptr::null(),
                raw_ac_sid,
                0,
                ptr::null(),
                0,
                ptr::null(),
            );
            let _ = FreeSid(raw_ac_sid);
            assert!(status >= 0, "NtCreateLowBoxToken failed: {status:#x}");

            let sid_text = sid_bytes_to_string(&sid_bytes).unwrap();
            let data_dir = root_dir.join("Ferrow");
            let ws_dir = ipc::winsearch_dir(&data_dir);
            let src_dir = root_dir.join("install");
            std::fs::create_dir_all(&ws_dir).unwrap();
            std::fs::create_dir_all(&src_dir).unwrap();

            let settings_path = data_dir.join("settings.json");
            let index_path = data_dir.join("index.bin");
            let src_dll = src_dir.join(DLL_NAME);
            std::fs::write(&settings_path, b"{\"secret\":true}").unwrap();
            std::fs::write(&index_path, b"FIDX-test").unwrap();
            std::fs::write(&src_dll, b"MZ-fake-dll").unwrap();

            // 1. Seed `root_dir` with traverse-without-inheritance so the LowBox
            // token can reach `data_dir`, then seed `data_dir` with beta.6's
            // buggy `(OI)(CI)(0x20 | 0x80)` ACE (missing `SYNCHRONIZE`).
            apply_sid_access(&root_dir, &sid_bytes, Some(AccessKind::Traverse)).unwrap();
            {
                let ea_beta6 = EXPLICIT_ACCESS_W {
                    grfAccessPermissions: 0x20 | 0x80,
                    grfAccessMode: GRANT_ACCESS,
                    grfInheritance: CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE,
                    Trustee: TRUSTEE_W {
                        TrusteeForm: TRUSTEE_IS_SID,
                        ptstrName: PWSTR(sid_bytes.as_ptr().cast_mut().cast()),
                        ..Default::default()
                    },
                };
                let mut data_wide: Vec<u16> =
                    data_dir.as_os_str().encode_wide().chain(Some(0)).collect();
                let mut old_dacl: *mut ACL = ptr::null_mut();
                let mut sd = PSECURITY_DESCRIPTOR::default();
                assert_eq!(
                    GetNamedSecurityInfoW(
                        PCWSTR(data_wide.as_ptr()),
                        SE_FILE_OBJECT,
                        DACL_SECURITY_INFORMATION,
                        None,
                        None,
                        Some(&mut old_dacl),
                        None,
                        &mut sd,
                    ),
                    ERROR_SUCCESS
                );
                let mut beta6_dacl: *mut ACL = ptr::null_mut();
                assert_eq!(
                    SetEntriesInAclW(Some(&[ea_beta6]), Some(old_dacl), &mut beta6_dacl),
                    ERROR_SUCCESS
                );
                assert_eq!(
                    SetNamedSecurityInfoW(
                        PWSTR(data_wide.as_mut_ptr()),
                        SE_FILE_OBJECT,
                        DACL_SECURITY_INFORMATION,
                        None,
                        None,
                        Some(beta6_dacl),
                        None,
                    ),
                    ERROR_SUCCESS
                );
                let _ = LocalFree(Some(HLOCAL(beta6_dacl.cast())));
                let _ = LocalFree(Some(HLOCAL(sd.0)));
            }

            let token_raw = cleanup.lowbox_token.0 as usize;
            let dd_check = data_dir.clone();
            assert!(
                !run_as_lowbox(token_raw, move || dd_check.is_dir()),
                "beta.6 (0x20 | 0x80) without SYNCHRONIZE must fail Path::is_dir under LowBox"
            );

            // 2. Apply the real bridge grant + DLL staging and verify upgrade + least privilege.
            grant_searchhost_access(&data_dir, &sid_bytes, &sid_text).unwrap();
            let staged_dll = stage_dll_from_source(&src_dll, &ws_dir).unwrap();
            record_granted_dll_path(&data_dir, &sid_text, &sid_bytes, &staged_dll).unwrap();
            apply_sid_access(&staged_dll, &sid_bytes, Some(AccessKind::ReadExecute)).unwrap();

            let dd = data_dir.clone();
            let ws = ws_dir.clone();
            let settings = settings_path.clone();
            let grant_rec = grant_record_path(&data_dir);
            let idx = index_path.clone();
            let dll = staged_dll.clone();
            run_as_lowbox(token_raw, move || {
                assert!(dd.is_dir(), "LowBox token must stat data_dir (`is_dir()`)");
                assert!(
                    std::fs::read_dir(&dd).is_err(),
                    "LowBox token must not list data_dir"
                );
                assert!(
                    std::fs::metadata(&settings).is_err(),
                    "stale inherited ACE on settings.json must be stripped"
                );
                assert!(
                    std::fs::read(&settings).is_err(),
                    "LowBox token must not read settings.json"
                );
                assert!(
                    std::fs::read(&grant_rec).is_err(),
                    "LowBox token must not read winsearch-granted-sid"
                );
                assert_eq!(std::fs::read(&idx).unwrap(), b"FIDX-test");
                assert!(
                    std::fs::write(&idx, b"tamper").is_err(),
                    "LowBox token must not write index.bin"
                );
                assert_eq!(std::fs::read(&dll).unwrap(), b"MZ-fake-dll");

                let req_tmp = ws.join("req-1.tmp");
                let req_final = ws.join("req-1.q");
                std::fs::write(&req_tmp, b"query").unwrap();
                std::fs::rename(&req_tmp, &req_final).unwrap();
                assert_eq!(std::fs::read(&req_final).unwrap(), b"query");
                std::fs::remove_file(&req_final).unwrap();
            });

            // 3. Revoke access on disable and prove LowBox access is gone again.
            revoke_searchhost_access(&data_dir).unwrap();
            assert!(!grant_record_path(&data_dir).exists());
            assert!(!staged_dll.exists());

            let dd_after = data_dir.clone();
            let ws_after = ws_dir.clone();
            let idx_after = index_path.clone();
            run_as_lowbox(token_raw, move || {
                assert!(!dd_after.is_dir(), "revoked data_dir must fail is_dir()");
                assert!(std::fs::read(&idx_after).is_err());
                assert!(std::fs::write(ws_after.join("probe"), b"x").is_err());
            });
        }
    }
}
