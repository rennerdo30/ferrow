//! Host controller for enabling/disabling Windows Search integration and
//! attaching `ferrow_winsearch.dll` to `SearchHost.exe`.

use std::ffi::c_void;
use std::io;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_SUCCESS, FreeLibrary, GENERIC_EXECUTE, GENERIC_READ, GENERIC_WRITE, HANDLE,
    HLOCAL, HMODULE, LocalFree, WAIT_OBJECT_0,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSidToSidW, EXPLICIT_ACCESS_W, GRANT_ACCESS, GetNamedSecurityInfoW, SE_FILE_OBJECT,
    SetEntriesInAclW, SetNamedSecurityInfoW, TRUSTEE_IS_SID, TRUSTEE_W,
};
use windows::Win32::Security::{
    ACL, CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, NO_INHERITANCE, OBJECT_INHERIT_ACE,
    PSECURITY_DESCRIPTOR, PSID,
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
    PROCESS_ACCESS_RIGHTS, WaitForSingleObject,
};
use windows::core::{PCWSTR, PWSTR, s, w};

use crate::ipc;

const DLL_NAME: &str = "ferrow_winsearch.dll";
/// `PROCESS_CREATE_THREAD | PROCESS_QUERY_INFORMATION | PROCESS_VM_OPERATION | PROCESS_VM_WRITE | PROCESS_VM_READ | SYNCHRONIZE`
const ATTACH_ACCESS: u32 = 0x10043A | 0x0010;

static LAST_ATTACHED_PID: AtomicU32 = AtomicU32::new(0);

/// Synchronize the live Windows Search integration state with the user's setting.
pub fn sync_state(data_dir: &Path, enabled: bool) -> Result<(), String> {
    if !enabled {
        let marker = ipc::enabled_marker_path(data_dir);
        if marker.exists() {
            std::fs::remove_file(&marker)
                .map_err(|e| format!("Could not disable Windows Search integration: {e}"))?;
        }
        return Ok(());
    }

    let ws_dir = ipc::winsearch_dir(data_dir);
    std::fs::create_dir_all(&ws_dir)
        .map_err(|e| format!("Could not create Windows Search state folder: {e}"))?;

    grant_appcontainer_access(data_dir, false)
        .map_err(|e| format!("Could not grant AppContainer read access on data folder: {e}"))?;
    grant_appcontainer_access(&ws_dir, true)
        .map_err(|e| format!("Could not grant AppContainer access on winsearch folder: {e}"))?;

    let index_path = data_dir.join("index.bin");
    if index_path.is_file() {
        let _ = grant_appcontainer_access(&index_path, false);
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

    Ok(())
}

/// Synchronously stage `ferrow_winsearch.dll` and attach it to `SearchHost.exe`.
pub fn attach_searchhost_now(data_dir: &Path) -> Result<bool, String> {
    let ws_dir = ipc::winsearch_dir(data_dir);
    let staged_dll = stage_attachment_dll(&ws_dir)
        .map_err(|e| format!("Could not stage ferrow_winsearch.dll: {e}"))?;
    attach_to_searchhost(&staged_dll, data_dir)
        .map_err(|e| format!("Could not attach to SearchHost.exe: {e}"))
}

/// Grant `ALL APPLICATION PACKAGES` (`S-1-15-2-1`) Read+Execute (or Modify when
/// `writable` is true) on `path`, with container/object inheritance for folders.
pub fn grant_appcontainer_access(path: &Path, writable: bool) -> io::Result<()> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut sid = PSID::default();
    // SAFETY: S-1-15-2-1 is a well-known constant SID; freed via LocalFree below.
    unsafe {
        ConvertStringSidToSidW(w!("S-1-15-2-1"), &mut sid).map_err(io::Error::from)?;
    }

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
        unsafe {
            let _ = LocalFree(Some(HLOCAL(sid.0)));
        }
        return Err(io::Error::from_raw_os_error(get_err.0 as i32));
    }

    let perms = if writable {
        GENERIC_READ.0 | GENERIC_WRITE.0 | GENERIC_EXECUTE.0 | DELETE.0
    } else {
        GENERIC_READ.0 | GENERIC_EXECUTE.0
    };
    let inheritance = if path.is_dir() {
        CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE
    } else {
        NO_INHERITANCE
    };

    let ea = EXPLICIT_ACCESS_W {
        grfAccessPermissions: perms,
        grfAccessMode: GRANT_ACCESS,
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
            let _ = LocalFree(Some(HLOCAL(sid.0)));
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
        let _ = LocalFree(Some(HLOCAL(sid.0)));
    }

    if set_err != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(set_err.0 as i32));
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
        let _ = grant_appcontainer_access(&staged, false);
        Ok(staged)
    } else if let Some(src) = find_source_dll() {
        let _ = grant_appcontainer_access(&src, false);
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
        return Ok(false);
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
    let mut proc_machine = IMAGE_FILE_MACHINE(0);
    let mut native_machine = IMAGE_FILE_MACHINE(0);
    unsafe {
        IsWow64Process2(proc_handle, &mut proc_machine, Some(&mut native_machine))
            .map_err(io::Error::from)?;
    }
    if proc_machine.0 != 0 || native_machine.0 != 0x8664 {
        return Ok(false);
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
