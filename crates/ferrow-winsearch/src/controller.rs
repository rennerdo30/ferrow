//! Host controller for enabling/disabling Windows Search integration and
//! attaching `ferrow_winsearch.dll` to `SearchHost.exe`.
//!
//! A watcher thread blocks on the `SearchHost.exe` process handle (0% CPU
//! while waiting) and re-attaches whenever Windows restarts the shell
//! search host, so the integration survives shell restarts for as long as
//! `ferrow-gui` runs. Attachment state is exposed via [`attach_status`].

use std::ffi::c_void;
use std::io;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
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
use windows::Win32::System::SystemInformation::IMAGE_FILE_MACHINE;
use windows::Win32::System::Threading::{
    CreateRemoteThread, GetCurrentProcessId, GetExitCodeThread, IsWow64Process2, OpenProcess,
    OpenProcessToken, PROCESS_ACCESS_RIGHTS, PROCESS_NAME_FORMAT, QueryFullProcessImageNameW,
    WaitForSingleObject,
};
use windows::core::{PCWSTR, PWSTR, s, w};

use crate::ipc;

const DLL_NAME: &str = "ferrow_winsearch.dll";
/// `PROCESS_CREATE_THREAD | PROCESS_QUERY_INFORMATION | PROCESS_VM_OPERATION | PROCESS_VM_WRITE | PROCESS_VM_READ | SYNCHRONIZE`
const ATTACH_ACCESS: u32 = 0x10043A | 0x0010;

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
        revoke_searchhost_access(data_dir);
        record_attach_error(None);
        return Ok(());
    }

    let ws_dir = ipc::winsearch_dir(data_dir);
    std::fs::create_dir_all(&ws_dir)
        .map_err(|e| format!("Could not create Windows Search state folder: {e}"))?;

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

/// File in the winsearch folder recording which AppContainer SID was
/// granted bridge access, so disable can revoke exactly that grant.
const GRANTED_SID_FILE: &str = "granted-sid";
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

/// Verify an opened process handle really is this session's
/// `SearchHost.exe`. Checking identity from the handle (not the earlier
/// pid snapshot) closes the pid-reuse race between enumeration and use.
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
    let image = String::from_utf16_lossy(&buffer[..len as usize]);
    let file_name = image.rsplit(['\\', '/']).next().unwrap_or("");
    if !file_name.eq_ignore_ascii_case("SearchHost.exe") {
        return Err(io::Error::other(format!(
            "pid {pid} is {file_name}, not SearchHost.exe"
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
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
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

/// Grant the SearchHost package (and only it) the bridge access, record
/// its SID for later revocation, and drop any legacy all-packages grant.
fn grant_searchhost_access(data_dir: &Path, sid: &[u8], sid_text: &str) -> io::Result<()> {
    for (path, kind) in bridge_paths(data_dir) {
        if path.exists() {
            apply_sid_access(&path, sid, Some(kind))?;
        }
    }
    let ws_dir = ipc::winsearch_dir(data_dir);
    let _ = std::fs::write(ws_dir.join(GRANTED_SID_FILE), sid_text);
    if let Ok(legacy) = sid_string_to_bytes(LEGACY_ALL_PACKAGES_SID) {
        for (path, _) in bridge_paths(data_dir) {
            if path.exists() {
                let _ = apply_sid_access(&path, &legacy, None);
            }
        }
        let staged = ws_dir.join(DLL_NAME);
        if staged.exists() {
            let _ = apply_sid_access(&staged, &legacy, None);
        }
    }
    Ok(())
}

/// Best-effort undo of [`grant_searchhost_access`] on disable: revoke
/// the recorded package grant (and the legacy one) and remove the
/// staged DLL copy. The loaded copy inside SearchHost stays inert
/// behind the removed marker until the shell restarts it.
fn revoke_searchhost_access(data_dir: &Path) {
    let ws_dir = ipc::winsearch_dir(data_dir);
    let mut sids: Vec<Vec<u8>> = Vec::new();
    if let Ok(text) = std::fs::read_to_string(ws_dir.join(GRANTED_SID_FILE))
        && let Ok(bytes) = sid_string_to_bytes(text.trim())
    {
        sids.push(bytes);
    }
    if let Ok(legacy) = sid_string_to_bytes(LEGACY_ALL_PACKAGES_SID) {
        sids.push(legacy);
    }
    let staged = ws_dir.join(DLL_NAME);
    for sid in &sids {
        for (path, _) in bridge_paths(data_dir) {
            if path.exists() {
                let _ = apply_sid_access(&path, sid, None);
            }
        }
        if staged.exists() {
            let _ = apply_sid_access(&staged, sid, None);
        }
    }
    let _ = std::fs::remove_file(ws_dir.join(GRANTED_SID_FILE));
    let _ = std::fs::remove_file(&staged);
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
    Ok(pids)
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
    let Some(&pid) = pids.first() else {
        return Ok(false);
    };

    let dll_name_lower = dll_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(DLL_NAME)
        .to_ascii_lowercase();

    let remote_mods = list_remote_modules(pid)?;
    let already_loaded = remote_mods.iter().any(|(n, _, _)| *n == dll_name_lower);
    if already_loaded && LAST_ATTACHED_PID.load(Ordering::Acquire) == pid {
        return Ok(true);
    }

    let proc_handle = unsafe { OpenProcess(PROCESS_ACCESS_RIGHTS(ATTACH_ACCESS), false, pid) }
        .map_err(io::Error::from)?;

    let res =
        unsafe { attach_to_process_handle(proc_handle, pid, dll_path, &dll_name_lower, data_dir) };
    unsafe {
        let _ = CloseHandle(proc_handle);
    }
    if res.is_ok() {
        LAST_ATTACHED_PID.store(pid, Ordering::Release);
    }
    res
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

    /// When SearchHost runs here, its token must yield a package SID —
    /// the narrow bridge grant depends on it. Hosts without a shell
    /// search process (CI runners) skip like the catalog test does.
    #[test]
    fn resolves_searchhost_package_sid_when_running() {
        match resolve_searchhost_sid() {
            Ok((bytes, text)) => {
                assert!(!bytes.is_empty());
                assert!(text.starts_with("S-1-15-2-"), "unexpected SID: {text}");
                assert_ne!(text, LEGACY_ALL_PACKAGES_SID);
                let round_trip = sid_string_to_bytes(&text).expect("SID round-trips");
                assert_eq!(round_trip, bytes);
            }
            Err(error) => {
                eprintln!("skipping: SearchHost AppContainer SID unavailable here: {error}");
            }
        }
    }
}
