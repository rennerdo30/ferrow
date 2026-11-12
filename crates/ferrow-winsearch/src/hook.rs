//! Vtable hook on `tquery.dll`'s `ICommand::Execute` (`CLSID_CollatorDataSource`)
//! inside `SearchHost.exe`.
//!
//! Uses `VirtualProtect` to atomically swap slot 4 (`Execute`) of `tquery.dll`'s
//! `ICommandText` vtable without any external hooking library. Routing is
//! dynamically gated by `<data_dir>/winsearch/enabled` on each query.

use std::ffi::c_void;
use std::mem::size_of;
use std::path::PathBuf;
use std::sync::atomic::{AtomicPtr, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::{E_FAIL, E_INVALIDARG, E_POINTER, E_UNEXPECTED, HMODULE, S_OK};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize,
};
use windows::Win32::System::LibraryLoader::{
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_PIN,
    GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, GetModuleHandleExW, GetModuleHandleW,
};
use windows::Win32::System::Memory::{PAGE_PROTECTION_FLAGS, PAGE_READWRITE, VirtualProtect};
use windows::Win32::System::Search::{
    DBPARAMS, ICommandText, IDBCreateCommand, IDBCreateSession, IDBInitialize, IDBProperties,
    IRowset,
};
use windows::core::{Error, GUID, HRESULT, IUnknown, Interface, PCWSTR, PWSTR, Result, w};

use crate::{ipc, rowset, sql};

const CLSID_COLLATOR_DATA_SOURCE: GUID = GUID::from_u128(0x9e175b8b_f52a_11d8_b9a5_505054503030);

type ExecuteFn = unsafe extern "system" fn(
    *mut c_void,
    *mut c_void,
    *const GUID,
    *mut DBPARAMS,
    *mut isize,
    *mut *mut c_void,
) -> HRESULT;

static ORIGINAL: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static SLOT_PTR: AtomicPtr<*mut c_void> = AtomicPtr::new(std::ptr::null_mut());
static FERROW_ROUTES: AtomicU32 = AtomicU32::new(0);
static NATIVE_ROUTES: AtomicU32 = AtomicU32::new(0);
static DATA_DIR: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
static HOOK_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();

fn data_dir_slot() -> &'static Mutex<Option<PathBuf>> {
    DATA_DIR.get_or_init(|| Mutex::new(None))
}

fn hook_lock() -> &'static Mutex<()> {
    HOOK_MUTEX.get_or_init(|| Mutex::new(()))
}

fn current_data_dir() -> Option<PathBuf> {
    data_dir_slot().lock().ok().and_then(|g| g.clone())
}

/// Counts of `(ferrow_routed, native_fallback)` queries in this process.
pub fn route_counts() -> (u32, u32) {
    (
        FERROW_ROUTES.load(Ordering::Relaxed),
        NATIVE_ROUTES.load(Ordering::Relaxed),
    )
}

/// Largest `diag.log` the DLL appends to; later lines are dropped.
const DIAG_MAX_BYTES: u64 = 1024 * 1024;

/// Opt-in DLL diagnostics: while `<data_dir>/winsearch/diag` exists, append
/// one line per event to `<data_dir>/winsearch/diag.log`. Search text is
/// never written; SQL is logged with every quoted literal blanked.
fn diag(data_dir: Option<&std::path::Path>, line: impl FnOnce() -> String) {
    let Some(data_dir) = data_dir else { return };
    let ws_dir = ipc::winsearch_dir(data_dir);
    if !ws_dir.join("diag").is_file() {
        return;
    }
    let path = ws_dir.join("diag.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > DIAG_MAX_BYTES) {
        return;
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        use std::io::Write;
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        let _ = writeln!(
            file,
            "{ms} pid={} module={:p} {}",
            std::process::id(),
            detour as *const c_void,
            line()
        );
    }
}

/// True while the opt-in DLL diagnostics marker exists.
pub(crate) fn diag_enabled() -> bool {
    current_data_dir().is_some_and(|dir| ipc::winsearch_dir(&dir).join("diag").is_file())
}

/// [`diag`] for the data folder this DLL is attached to.
pub(crate) fn diag_attached(line: impl FnOnce() -> String) {
    diag(current_data_dir().as_deref(), line);
}

/// `sql` with every word inside `'...'` literals replaced by `x`s, except
/// the full-text operators `AND`, `OR`, `NOT` and `NEAR`, so the query
/// shape can be logged without the user's search text.
pub(crate) fn redact_sql(sql: &str) -> String {
    fn flush(word: &mut String, out: &mut String) {
        if ["AND", "OR", "NOT", "NEAR"]
            .iter()
            .any(|op| word.eq_ignore_ascii_case(op))
        {
            out.push_str(word);
        } else {
            out.extend(word.chars().map(|_| 'x'));
        }
        word.clear();
    }
    let mut out = String::with_capacity(sql.len());
    let mut word = String::new();
    let mut in_literal = false;
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        if in_literal && c.is_alphanumeric() {
            word.push(c);
            continue;
        }
        flush(&mut word, &mut out);
        if c == '\'' {
            if in_literal && chars.peek() == Some(&'\'') {
                chars.next(); // escaped quote inside the literal
                out.push_str("''");
                continue;
            }
            in_literal = !in_literal;
        }
        out.push(c);
    }
    flush(&mut word, &mut out);
    out
}

unsafe extern "system" fn detour(
    this: *mut c_void,
    outer: *mut c_void,
    iid: *const GUID,
    params: *mut DBPARAMS,
    affected: *mut isize,
    output: *mut *mut c_void,
) -> HRESULT {
    let attempt = std::panic::catch_unwind(|| -> Result<bool> {
        let data_dir = current_data_dir();
        if !outer.is_null()
            || iid.is_null()
            || output.is_null()
            || !params.is_null()
            || unsafe { *iid != IRowset::IID && *iid != IUnknown::IID }
        {
            diag(data_dir.as_deref(), || {
                let iid = if iid.is_null() {
                    "null".to_owned()
                } else {
                    format!("{:?}", unsafe { *iid })
                };
                format!(
                    "native: unsupported call outer={} iid={iid} params={} output={}",
                    !outer.is_null(),
                    !params.is_null(),
                    !output.is_null()
                )
            });
            return Ok(false);
        }
        let Some(data_dir) = data_dir else {
            return Ok(false);
        };
        if !ipc::is_enabled(&data_dir) {
            diag(Some(&data_dir), || {
                "native: integration disabled".to_owned()
            });
            return Ok(false);
        }

        // Execute is inherited by ICommandText at the same COM interface pointer.
        let command = unsafe { ICommandText::from_raw_borrowed(&this) }
            .ok_or_else(|| Error::from_hresult(E_POINTER))?;
        let mut raw_sql = PWSTR::null();
        unsafe {
            command.GetCommandText(None, &mut raw_sql)?;
        }
        if raw_sql.is_null() {
            diag(Some(&data_dir), || "native: no command text".to_owned());
            return Ok(false);
        }
        let sql_res = unsafe { raw_sql.to_string() };
        unsafe {
            CoTaskMemFree(Some(raw_sql.0.cast()));
        }
        let sql_str = sql_res?;

        let Some(parsed) = sql::parse_windows_search_sql(&sql_str) else {
            diag(Some(&data_dir), || {
                format!("native: SQL not understood: {}", redact_sql(&sql_str))
            });
            return Ok(false);
        };
        let start = ipc::StartQuery::from_sql(&parsed);
        let Some(hits) = ipc::query_hits(&data_dir, &start) else {
            diag(Some(&data_dir), || {
                format!(
                    "native: no answer from Ferrow (limit={}): {}",
                    parsed.limit,
                    redact_sql(&sql_str)
                )
            });
            return Ok(false);
        };
        if hits.is_empty() {
            // Fall back to Windows Search when Ferrow has zero matches so
            // non-indexed locations and non-file items still appear.
            diag(Some(&data_dir), || {
                format!("native: Ferrow found 0 hits: {}", redact_sql(&sql_str))
            });
            return Ok(false);
        }
        let hit_count = hits.len();

        let rows = rowset::build_for_hits(&parsed.columns, hits).inspect_err(|e| {
            diag(Some(&data_dir), || format!("native: rowset failed: {e}"));
        })?;
        diag(Some(&data_dir), || {
            format!("ferrow: answered {hit_count} hits")
        });
        unsafe {
            output.write(rows.into_raw());
            if !affected.is_null() {
                affected.write(-1);
            }
        }
        FERROW_ROUTES.fetch_add(1, Ordering::Relaxed);
        Ok(true)
    });

    if matches!(attempt, Ok(Ok(true))) {
        return S_OK;
    }

    NATIVE_ROUTES.fetch_add(1, Ordering::Relaxed);
    let orig_ptr = ORIGINAL.load(Ordering::Acquire);
    let original: ExecuteFn = unsafe { std::mem::transmute(orig_ptr) };
    unsafe { original(this, outer, iid, params, affected, output) }
}

/// Activate `CLSID_CollatorDataSource` and create a fresh `ICommandText`.
pub fn create_native_command() -> Result<ICommandText> {
    unsafe {
        let source: IDBProperties =
            CoCreateInstance(&CLSID_COLLATOR_DATA_SOURCE, None, CLSCTX_INPROC_SERVER)?;
        source.cast::<IDBInitialize>()?.Initialize()?;
        let session = source
            .cast::<IDBCreateSession>()?
            .CreateSession(None, &IDBCreateCommand::IID)?;
        session
            .cast::<IDBCreateCommand>()?
            .CreateCommand(None, &ICommandText::IID)?
            .cast()
    }
}

/// Install the vtable hook on `tquery.dll`'s `ICommandText::Execute` and point
/// query routing at `data_dir`. Safe and idempotent to call multiple times.
pub fn attach_in_process(data_dir: PathBuf) -> Result<()> {
    if !data_dir.is_absolute() || !data_dir.is_dir() {
        return Err(E_INVALIDARG.into());
    }
    let _guard = hook_lock()
        .lock()
        .map_err(|_| Error::from_hresult(E_FAIL))?;
    if let Ok(mut slot) = data_dir_slot().lock() {
        *slot = Some(data_dir.clone());
    }

    unsafe {
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        let initialized_com = hr.is_ok();
        let res = install_vtable_hook();
        if initialized_com {
            CoUninitialize();
        }
        if res.is_ok() {
            // Warm the closed-app index cache in the background so the
            // query path never loads from disk synchronously.
            ipc::preload_disk_index(&data_dir);
        }
        res
    }
}

unsafe fn install_vtable_hook() -> Result<()> {
    unsafe {
        // Pin our DLL so a late detour entrant can never execute unloaded code.
        let mut module = HMODULE::default();
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_PIN,
            PCWSTR(detour as *const u16),
            &mut module,
        )?;

        let command = create_native_command()?;
        let slot_addr =
            (&raw const command.vtable().base__.Execute).cast::<*mut c_void>() as *mut *mut c_void;
        let current_target = *slot_addr;

        if current_target == detour as *mut c_void {
            return Ok(());
        }

        let mut owner = HMODULE::default();
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_PIN,
            PCWSTR(current_target.cast()),
            &mut owner,
        )?;
        let _ = GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT;
        if owner != GetModuleHandleW(w!("tquery.dll"))? {
            return Err(E_UNEXPECTED.into());
        }

        ORIGINAL.store(current_target, Ordering::Release);
        SLOT_PTR.store(slot_addr, Ordering::Release);

        let mut old_prot = PAGE_PROTECTION_FLAGS(0);
        VirtualProtect(
            slot_addr.cast(),
            size_of::<*mut c_void>(),
            PAGE_READWRITE,
            &mut old_prot,
        )?;

        let atomic_slot = &*(slot_addr as *const AtomicPtr<c_void>);
        atomic_slot.store(detour as *mut c_void, Ordering::Release);

        let mut dummy = PAGE_PROTECTION_FLAGS(0);
        let _ = VirtualProtect(
            slot_addr.cast(),
            size_of::<*mut c_void>(),
            old_prot,
            &mut dummy,
        );

        Ok(())
    }
}

/// Restore the original `tquery.dll` `Execute` pointer in the vtable
/// if and only if this module's detour currently owns the slot.
pub fn detach_in_process() -> Result<()> {
    let _guard = hook_lock()
        .lock()
        .map_err(|_| Error::from_hresult(E_FAIL))?;
    let slot_addr = SLOT_PTR.load(Ordering::Acquire);
    let orig = ORIGINAL.load(Ordering::Acquire);
    if slot_addr.is_null() || orig.is_null() {
        return Ok(());
    }
    unsafe {
        let atomic_slot = &*(slot_addr as *const AtomicPtr<c_void>);
        if atomic_slot.load(Ordering::Acquire) != detour as *mut c_void {
            SLOT_PTR.store(std::ptr::null_mut(), Ordering::Release);
            return Ok(());
        }
        let mut old_prot = PAGE_PROTECTION_FLAGS(0);
        VirtualProtect(
            slot_addr.cast(),
            size_of::<*mut c_void>(),
            PAGE_READWRITE,
            &mut old_prot,
        )?;
        let _ = atomic_slot.compare_exchange(
            detour as *mut c_void,
            orig,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        let mut dummy = PAGE_PROTECTION_FLAGS(0);
        let _ = VirtualProtect(
            slot_addr.cast(),
            size_of::<*mut c_void>(),
            old_prot,
            &mut dummy,
        );
    }
    SLOT_PTR.store(std::ptr::null_mut(), Ordering::Release);
    Ok(())
}

/// Remote-thread entry point exported by `ferrow_winsearch.dll`.
/// Installs the vtable hook once and returns `0` immediately.
///
/// # Safety
/// `data_dir` must point to a readable, NUL-terminated UTF-16 directory path.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn FerrowAttach(data_dir: *const u16) -> u32 {
    match std::panic::catch_unwind(|| -> Result<()> {
        if data_dir.is_null() {
            return Err(E_POINTER.into());
        }
        let path_str = unsafe { PCWSTR(data_dir).to_string()? };
        let data_dir = PathBuf::from(path_str);
        let result = attach_in_process(data_dir.clone());
        diag(Some(&data_dir), || {
            format!(
                "attach: {result:?} slot={:p} original={:p}",
                SLOT_PTR.load(Ordering::Acquire),
                ORIGINAL.load(Ordering::Acquire)
            )
        });
        result
    }) {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => e.code().0 as u32,
        Err(_) => E_FAIL.0 as u32,
    }
}

/// Optional remote-thread or in-process entry point to restore the vtable slot.
///
/// # Safety
/// Must only be called within a process where `ferrow_winsearch.dll` is loaded.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn FerrowDetach(_reserved: *mut c_void) -> u32 {
    match std::panic::catch_unwind(|| {
        let result = detach_in_process();
        diag(current_data_dir().as_deref(), || {
            format!("detach: {result:?}")
        });
        result
    }) {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => e.code().0 as u32,
        Err(_) => E_FAIL.0 as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::redact_sql;

    #[test]
    fn redact_sql_blanks_every_literal() {
        let sql = "SELECT TOP 30 path FROM SystemIndex WHERE CONTAINS(System.FileName, '\"budget*\" AND \"it''s*\"', 2057) AND System.Kind = 'folder'";
        assert_eq!(
            redact_sql(sql),
            "SELECT TOP 30 path FROM SystemIndex WHERE CONTAINS(System.FileName, '\"xxxxxx*\" AND \"xx''x*\"', 2057) AND System.Kind = 'xxxxxx'"
        );
    }
}
