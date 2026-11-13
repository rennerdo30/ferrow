//! In-process OLE DB `IRowset` + `IAccessor` + `IColumnsInfo` + `IGetRow`
//! (`IPropertyStore`) implementation for Windows Search (`SearchHost.exe`).
//!
//! `SearchHost.exe` runs inside a LowBox AppContainer (`S-1-15-2-1`), where
//! `CoCreateInstance(CLSID_CADORecordset)` fails with `E_ACCESSDENIED`.
//! Moreover, after `IRowset::GetNextRows`, `SearchHost.exe` queries `IRowset`
//! for `IGetRow` and calls `IGetRow::GetRowFromHROW(..., &IPropertyStore::IID)`
//! to read item properties directly. This module implements both the OLE DB
//! accessor path and the `IGetRow` -> `IPropertyStore` (`PSCreateMemoryPropertyStore`)
//! path in pure Rust.

#![allow(non_snake_case)]

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::mem::{ManuallyDrop, size_of};
use std::ptr;
use std::sync::Mutex;

use windows::Win32::Foundation::{
    CLASS_E_NOAGGREGATION, E_FAIL, E_INVALIDARG, E_OUTOFMEMORY, E_POINTER, FILETIME, PROPERTYKEY,
};
use windows::Win32::Storage::IndexServer::{DBID, DBID_1, DBKIND_NAME};
use windows::Win32::System::Com::StructuredStorage::{
    CALPWSTR, PROPVARIANT, PROPVARIANT_0_0, PROPVARIANT_0_0_0, PropVariantClear,
};
use windows::Win32::System::Com::{CoTaskMemAlloc, CoTaskMemFree};
use windows::Win32::System::Search::{
    DB_E_BADACCESSORHANDLE, DB_E_BADCOLUMNID, DB_E_BADROWHANDLE, DB_E_CANTCONVERTVALUE,
    DB_S_ENDOFROWSET, DB_S_ERRORSOCCURRED, DBACCESSOR_ROWDATA, DBBINDING, DBCOLUMNINFO,
    DBPART_LENGTH, DBPART_STATUS, DBPART_VALUE, DBROWSTATUS_E_INVALID, DBROWSTATUS_S_OK,
    DBSTATUS_S_TRUNCATED, DBTYPE_VARIANT, HACCESSOR, IAccessor, IAccessor_Impl, IColumnsInfo,
    IColumnsInfo_Impl, IGetRow, IGetRow_Impl, IRowset, IRowset_Impl,
};
use windows::Win32::System::Variant::{
    VARENUM, VARIANT, VARIANT_0_0, VARIANT_0_0_0, VT_BSTR, VT_DATE, VT_FILETIME, VT_I4, VT_LPWSTR,
    VT_NULL, VT_UI4, VT_VECTOR, VariantClear,
};
use windows::Win32::UI::Shell::PropertiesSystem::{
    INamedPropertyStore, IPropertyStore, PSCreateMemoryPropertyStore, PSGetPropertyKeyFromName,
};
use windows::core::{
    BSTR, Error, GUID, HRESULT, IUnknown, Interface, PCWSTR, PWSTR, Ref, Result, implement, w,
};

use crate::hook::diag_attached as diag;
use crate::ipc::SearchHitRow;
use crate::kinds;

/// Difference between Windows `FILETIME` epoch (`1601-01-01`) and Unix epoch
/// (`1970-01-01`) in 100ns ticks.
const UNIX_EPOCH_AS_FILETIME: i64 = 116_444_736_000_000_000;
/// Fallback `FILETIME` ticks (`2026-01-01T00:00:00Z`) when an index does not
/// store date columns.
const DEFAULT_FILETIME_TICKS: u64 = 134_117_856_000_000_000;

/// Default 15-column projection used by Windows 11 Start Menu (`All` tab).
pub const DEFAULT_COLUMNS: [(&str, i32, i32); 15] = [
    ("System.Kind", 12, 24),
    ("System.Search.Rank", 3, 4),
    ("PATH", 202, 260),
    ("System.DateCreated", 7, 8),
    ("System.DateModified", 7, 8),
    ("System.Search.QueryPropertyHits", 12, 24),
    ("System.ItemNameDisplay", 12, 24),
    ("System.FileExtension", 12, 24),
    ("System.ItemType", 12, 24),
    ("System.Title", 12, 24),
    ("System.Music.AlbumTitle", 12, 24),
    ("System.Music.DisplayArtist", 12, 24),
    ("System.DateAccessed", 7, 8),
    ("System.Link.TargetParsingPath", 12, 24),
    ("System.SFGAOFlags", 12, 24),
];

#[derive(Clone)]
struct ColMeta {
    name: String,
    w_type: u16,
    col_size: usize,
    col_flags: u32,
}

#[derive(Clone)]
enum ColVal {
    Empty,
    I4(i32),
    U4(u32),
    Date(f64),
    Str(String),
}

#[derive(Clone)]
struct ColBinding {
    ordinal: usize,
    ob_value: usize,
    ob_length: usize,
    ob_status: usize,
    dw_part: u32,
    cb_max_len: usize,
    w_type: u16,
}

#[derive(Clone)]
struct AccessorEntry {
    flags: u32,
    cols: Vec<ColBinding>,
    /// Caller-declared row buffer size from `CreateAccessor`. Every
    /// `GetData` write is checked against it: binding offsets come
    /// from the consumer and must never address outside its buffer.
    row_size: usize,
    refs: u32,
}

/// Bytes one binding's value part occupies in the caller's buffer.
fn binding_value_width(b: &ColBinding) -> usize {
    let base_type = b.w_type & !0x4000;
    let by_ref = (b.w_type & 0x4000) != 0;
    match base_type {
        12 => {
            if by_ref {
                size_of::<*mut c_void>()
            } else {
                size_of::<VARIANT>()
            }
        }
        130 => {
            if by_ref {
                size_of::<*mut c_void>()
            } else {
                b.cb_max_len
            }
        }
        8 => size_of::<*const u16>(),
        3 | 19 => 4,
        7 => 8,
        _ => 0,
    }
}

fn part_in_bounds(offset: usize, width: usize, row_size: usize) -> bool {
    offset.checked_add(width).is_some_and(|end| end <= row_size)
}

/// True when every requested part of the binding lies inside a buffer
/// of `row_size` bytes. Checked at accessor creation and again in
/// `GetData`, so no accessor can ever direct a write outside the
/// caller's row buffer.
fn binding_in_bounds(b: &ColBinding, row_size: usize) -> bool {
    let has_val = (b.dw_part & (DBPART_VALUE.0 as u32)) != 0;
    let has_len = (b.dw_part & (DBPART_LENGTH.0 as u32)) != 0;
    let has_status = (b.dw_part & (DBPART_STATUS.0 as u32)) != 0;
    (!has_val || part_in_bounds(b.ob_value, binding_value_width(b), row_size))
        && (!has_len || part_in_bounds(b.ob_length, size_of::<usize>(), row_size))
        && (!has_status || part_in_bounds(b.ob_status, size_of::<u32>(), row_size))
}

#[derive(Default)]
struct State {
    cursor: usize,
    rows: BTreeMap<usize, u32>,
    accessors: BTreeMap<usize, AccessorEntry>,
    next_accessor: usize,
}

#[derive(Clone)]
struct RowInfo {
    hit: SearchHitRow,
    file_url: String,
    ext: String,
    item_type_text: String,
    kinds: Vec<&'static str>,
    rank: i32,
    sfgao_flags: u32,
    created_ft: u64,
    modified_ft: u64,
    accessed_ft: u64,
    cols: Vec<ColVal>,
}

#[implement(IRowset, IAccessor, IColumnsInfo, IGetRow)]
struct MemoryRowset {
    schema: Vec<ColMeta>,
    items: Vec<RowInfo>,
    state: Mutex<State>,
}

impl Drop for MemoryRowset {
    fn drop(&mut self) {
        diag(|| {
            let cursor = self.state.get_mut().map_or(0, |s| s.cursor);
            format!(
                "rowset: released after reading {cursor} of {} rows",
                self.items.len()
            )
        });
    }
}

fn information(code: HRESULT) -> Result<()> {
    Err(Error::from_hresult(code))
}

fn unix_ticks_to_filetime(ticks: Option<i64>) -> u64 {
    ticks
        .and_then(|t| t.checked_add(UNIX_EPOCH_AS_FILETIME))
        .and_then(|t| u64::try_from(t).ok())
        .unwrap_or(DEFAULT_FILETIME_TICKS)
}

fn unix_ticks_to_ole_date(ticks: Option<i64>) -> f64 {
    ticks
        .map(|t| 25569.0 + (t as f64) / 864_000_000_000.0)
        .unwrap_or(46000.0)
}

/// `System.SFGAOFlags` native Windows Search reports for a plain file and a
/// folder (file system, can copy/move/rename/delete, property sheet, ...).
const SFGAO_NATIVE_FILE: u32 = 0x4040_0177;
const SFGAO_NATIVE_FOLDER: u32 = 0x7080_017F;

fn classify_file_kind(file_name: &str, is_dir: bool) -> (String, String, Vec<&'static str>, u32) {
    if is_dir {
        return (
            String::new(),
            "Folder".to_owned(),
            vec!["folder"],
            SFGAO_NATIVE_FOLDER,
        );
    }
    let ext = match file_name.rfind('.') {
        Some(pos) if pos > 0 && pos + 1 < file_name.len() => file_name[pos..].to_owned(),
        _ => String::new(),
    };
    let ext_lower = ext.trim_start_matches('.').to_ascii_lowercase();
    // Windows' own kinds (Explorer's KindMap), the same ones Ferrow answers
    // Start's group queries with: every row lands in the group (Documents,
    // Photos, ...) Windows would put it in, and a file Windows gives no
    // kind (for example `.md`) gets none either.
    let kinds: Vec<&'static str> = if ext_lower.is_empty() {
        Vec::new()
    } else {
        kinds::kinds_of_extension(&ext_lower)
            .iter()
            .map(String::as_str)
            // Start hides file rows of kind `program` (it lists programs
            // in Apps itself), so a program file Ferrow finds would never
            // show: report no kind, like Windows does for unknown types.
            .filter(|kind| *kind != "program")
            .collect()
    };
    let type_text = if ext_lower.is_empty() {
        "File".to_owned()
    } else {
        format!("{} File", ext_lower.to_ascii_uppercase())
    };
    (ext, type_text, kinds, SFGAO_NATIVE_FILE)
}

unsafe fn alloc_pwstr(s: &str) -> PWSTR {
    let utf16: Vec<u16> = s.encode_utf16().chain(Some(0)).collect();
    let ptr = unsafe { CoTaskMemAlloc(utf16.len() * 2).cast::<u16>() };
    if !ptr.is_null() {
        unsafe {
            ptr::copy_nonoverlapping(utf16.as_ptr(), ptr, utf16.len());
        }
    }
    PWSTR(ptr)
}

unsafe fn propvar_null() -> PROPVARIANT {
    let mut pv = PROPVARIANT::default();
    pv.Anonymous.Anonymous = ManuallyDrop::new(PROPVARIANT_0_0 {
        vt: VT_NULL,
        ..Default::default()
    });
    pv
}

unsafe fn propvar_i4(v: i32) -> PROPVARIANT {
    let mut pv = PROPVARIANT::default();
    pv.Anonymous.Anonymous = ManuallyDrop::new(PROPVARIANT_0_0 {
        vt: VT_I4,
        Anonymous: PROPVARIANT_0_0_0 { lVal: v },
        ..Default::default()
    });
    pv
}

unsafe fn propvar_u4(v: u32) -> PROPVARIANT {
    let mut pv = PROPVARIANT::default();
    pv.Anonymous.Anonymous = ManuallyDrop::new(PROPVARIANT_0_0 {
        vt: VT_UI4,
        Anonymous: PROPVARIANT_0_0_0 { ulVal: v },
        ..Default::default()
    });
    pv
}

unsafe fn propvar_filetime(ticks: u64) -> PROPVARIANT {
    let mut pv = PROPVARIANT::default();
    pv.Anonymous.Anonymous = ManuallyDrop::new(PROPVARIANT_0_0 {
        vt: VT_FILETIME,
        Anonymous: PROPVARIANT_0_0_0 {
            filetime: FILETIME {
                dwLowDateTime: (ticks & 0xFFFF_FFFF) as u32,
                dwHighDateTime: (ticks >> 32) as u32,
            },
        },
        ..Default::default()
    });
    pv
}

unsafe fn propvar_lpwstr(s: &str) -> PROPVARIANT {
    let mut pv = PROPVARIANT::default();
    pv.Anonymous.Anonymous = ManuallyDrop::new(PROPVARIANT_0_0 {
        vt: VT_LPWSTR,
        Anonymous: PROPVARIANT_0_0_0 {
            pwszVal: unsafe { alloc_pwstr(s) },
        },
        ..Default::default()
    });
    pv
}

unsafe fn propvar_lpwstr_vec(items: &[&str]) -> PROPVARIANT {
    let mut pv = PROPVARIANT::default();
    let elems = unsafe { CoTaskMemAlloc(items.len() * size_of::<PWSTR>()).cast::<PWSTR>() };
    if !elems.is_null() {
        for (i, &s) in items.iter().enumerate() {
            unsafe {
                elems.add(i).write(alloc_pwstr(s));
            }
        }
    }
    pv.Anonymous.Anonymous = ManuallyDrop::new(PROPVARIANT_0_0 {
        vt: VARENUM(VT_VECTOR.0 | VT_LPWSTR.0),
        Anonymous: PROPVARIANT_0_0_0 {
            calpwstr: CALPWSTR {
                cElems: items.len() as u32,
                pElems: elems,
            },
        },
        ..Default::default()
    });
    pv
}

/// `System.Search.QueryPropertyHits` as native Windows Search reports it for
/// a Start menu name match (Windows 11, 2026), so Ferrow rows look like
/// Windows' own to the Start UI.
const QUERY_PROPERTY_HITS: [&str; 6] = [
    "{41CF5AE0-F75A-4806-BD87-59C7D9248EB9},100",
    "{49691C90-7E17-101A-A91C-08002B2ECDA9},9",
    "{B725F130-47EF-101A-A5F1-02608C9EEBAC},10",
    "{E3E0584C-B788-4A5A-BB20-7F5A44C9ACDD},7",
    "{28636AA6-953D-11D2-B5D6-00C04FD918D0},24",
    "{9E5E05AC-1936-4A75-94F7-4704B8B01923},0",
];

unsafe fn build_row_property_store(
    row: &RowInfo,
    row_id: i32,
    riid: *const GUID,
) -> Result<IUnknown> {
    unsafe {
        let mut raw_store = ptr::null_mut();
        PSCreateMemoryPropertyStore(&IPropertyStore::IID, &mut raw_store)?;
        let store = IPropertyStore::from_raw(raw_store);

        let mut props: Vec<(&str, PROPVARIANT)> = vec![
            (
                "System.Kind",
                if row.kinds.is_empty() {
                    propvar_null()
                } else {
                    propvar_lpwstr_vec(&row.kinds)
                },
            ),
            ("System.Search.Rank", propvar_i4(row.rank)),
            ("System.ParsingPath", propvar_lpwstr(&row.file_url)),
            ("System.ItemUrl", propvar_lpwstr(&row.file_url)),
            ("System.ItemPathDisplay", propvar_lpwstr(&row.hit.full_path)),
            ("System.DateCreated", propvar_filetime(row.created_ft)),
            ("System.DateModified", propvar_filetime(row.modified_ft)),
            ("System.DateAccessed", propvar_filetime(row.accessed_ft)),
            (
                "System.Search.QueryPropertyHits",
                propvar_lpwstr_vec(&QUERY_PROPERTY_HITS),
            ),
            ("System.ItemNameDisplay", propvar_lpwstr(&row.hit.file_name)),
            ("System.ItemName", propvar_lpwstr(&row.hit.file_name)),
            ("System.FileName", propvar_lpwstr(&row.hit.file_name)),
            (
                "System.FileExtension",
                if row.hit.is_dir {
                    propvar_null()
                } else {
                    propvar_lpwstr(&row.ext)
                },
            ),
            (
                "System.ItemType",
                propvar_lpwstr(if row.hit.is_dir {
                    "Directory"
                } else {
                    &row.ext
                }),
            ),
            ("System.ItemTypeText", propvar_lpwstr(&row.item_type_text)),
            ("System.Title", propvar_null()),
            ("System.Music.AlbumTitle", propvar_null()),
            ("System.Music.DisplayArtist", propvar_null()),
            ("System.Link.TargetParsingPath", propvar_null()),
            ("System.SFGAOFlags", propvar_u4(row.sfgao_flags)),
            ("System.ItemId", propvar_i4(row_id)),
        ];

        for (name, pv) in &mut props {
            let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
            let mut pkey = PROPERTYKEY::default();
            if PSGetPropertyKeyFromName(PCWSTR(wide.as_ptr()), &mut pkey).is_ok() {
                let _ = store.SetValue(&pkey, pv);
            }
            let _ = PropVariantClear(pv);
        }

        if let Ok(named) = store.cast::<INamedPropertyStore>() {
            let mut pv_path = propvar_lpwstr(&row.file_url);
            let _ = named.SetNamedValue(w!("PATH"), &pv_path);
            let _ = PropVariantClear(&mut pv_path);
        }

        let store: IPropertyStore = if crate::hook::diag_enabled() {
            LoggingPropertyStore {
                inner: store,
                row: row_id,
            }
            .into()
        } else {
            store
        };
        let mut out = ptr::null_mut();
        store.query(riid, &mut out).ok()?;
        Ok(IUnknown::from_raw(out))
    }
}

/// Diagnostics only: forwards to the row's property store and logs which
/// properties `SearchHost.exe` reads (names only, never values).
#[implement(IPropertyStore)]
struct LoggingPropertyStore {
    inner: IPropertyStore,
    row: i32,
}

fn property_key_name(key: *const PROPERTYKEY) -> String {
    if key.is_null() {
        return "null".to_owned();
    }
    unsafe {
        match windows::Win32::UI::Shell::PropertiesSystem::PSGetNameFromPropertyKey(key) {
            Ok(name) => {
                let text = name.to_string().unwrap_or_default();
                CoTaskMemFree(Some(name.0.cast()));
                text
            }
            Err(_) => format!("{{{:?}}}/{}", (*key).fmtid, (*key).pid),
        }
    }
}

impl windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore_Impl
    for LoggingPropertyStore_Impl
{
    fn GetCount(&self) -> Result<u32> {
        let result = unsafe { self.inner.GetCount() };
        diag(|| format!("store row={}: GetCount -> {result:?}", self.row));
        result
    }

    fn GetAt(&self, index: u32, key: *mut PROPERTYKEY) -> Result<()> {
        let result = unsafe { self.inner.GetAt(index, key) };
        diag(|| format!("store row={}: GetAt {index} -> {result:?}", self.row));
        result
    }

    fn GetValue(&self, key: *const PROPERTYKEY) -> Result<PROPVARIANT> {
        let result = unsafe { self.inner.GetValue(key) };
        diag(|| {
            let vt = result
                .as_ref()
                .map(|pv| unsafe { pv.Anonymous.Anonymous.vt.0 });
            format!(
                "store row={}: GetValue {} -> vt {vt:?}",
                self.row,
                property_key_name(key)
            )
        });
        result
    }

    fn SetValue(&self, key: *const PROPERTYKEY, value: *const PROPVARIANT) -> Result<()> {
        diag(|| {
            format!(
                "store row={}: SetValue {}",
                self.row,
                property_key_name(key)
            )
        });
        unsafe { self.inner.SetValue(key, value) }
    }

    fn Commit(&self) -> Result<()> {
        diag(|| format!("store row={}: Commit", self.row));
        unsafe { self.inner.Commit() }
    }
}

impl IGetRow_Impl for MemoryRowset_Impl {
    fn GetRowFromHROW(
        &self,
        punkouter: Ref<IUnknown>,
        hrow: usize,
        riid: *const GUID,
    ) -> Result<IUnknown> {
        if !punkouter.is_null() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        if riid.is_null() {
            return Err(E_INVALIDARG.into());
        }
        let state = self.state.lock().map_err(|_| Error::from_hresult(E_FAIL))?;
        if !state.rows.contains_key(&hrow) || hrow == 0 || hrow > self.items.len() {
            return Err(DB_E_BADROWHANDLE.into());
        }
        let row = self.items[hrow - 1].clone();
        drop(state);
        let result = unsafe { build_row_property_store(&row, hrow as i32, riid) };
        diag(|| {
            format!(
                "rowset: GetRowFromHROW row={hrow} iid={:?} -> {:?}",
                unsafe { *riid },
                result.as_ref().map(|_| ())
            )
        });
        result
    }

    fn GetURLFromHROW(&self, hrow: usize) -> Result<PWSTR> {
        diag(|| format!("rowset: GetURLFromHROW row={hrow}"));
        let state = self.state.lock().map_err(|_| Error::from_hresult(E_FAIL))?;
        if !state.rows.contains_key(&hrow) || hrow == 0 || hrow > self.items.len() {
            return Err(DB_E_BADROWHANDLE.into());
        }
        let url = self.items[hrow - 1].file_url.clone();
        drop(state);
        Ok(unsafe { alloc_pwstr(&url) })
    }
}

fn classify_column(raw_name: &str) -> ColMeta {
    let trimmed = raw_name.trim().trim_matches('"');
    let lower = trimmed.to_ascii_lowercase();
    match lower.as_str() {
        "path" | "system.itemurl" => ColMeta {
            name: "PATH".to_owned(),
            w_type: 130,
            col_size: 260,
            col_flags: 32,
        },
        "system.search.rank" => ColMeta {
            name: "System.Search.Rank".to_owned(),
            w_type: 3,
            col_size: 4,
            col_flags: 48,
        },
        "system.datecreated" | "system.datemodified" | "system.dateaccessed" => ColMeta {
            name: trimmed.to_owned(),
            w_type: 7,
            col_size: 8,
            col_flags: 48,
        },
        "system.search.querypropertyhits" => ColMeta {
            name: trimmed.to_owned(),
            w_type: 12,
            col_size: 24,
            col_flags: 48,
        },
        _ => ColMeta {
            name: trimmed.to_owned(),
            w_type: 12,
            col_size: 24,
            col_flags: 50,
        },
    }
}

fn value_for_column(
    col_name: &str,
    hit: &SearchHitRow,
    ext: &str,
    kinds: &[&str],
    rank: i32,
    sfgao_flags: u32,
) -> ColVal {
    let mod_date = unix_ticks_to_ole_date(hit.modified_ticks);
    let cre_date = unix_ticks_to_ole_date(hit.created_ticks.or(hit.modified_ticks));
    let acc_date = unix_ticks_to_ole_date(hit.accessed_ticks.or(hit.modified_ticks));
    match col_name.to_ascii_lowercase().as_str() {
        "system.kind" => kinds
            .first()
            .map_or(ColVal::Empty, |kind| ColVal::Str((*kind).to_owned())),
        "system.search.rank" => ColVal::I4(rank),
        "path" | "system.itemurl" | "system.itempathdisplay" => ColVal::Str(hit.full_path.clone()),
        "system.datecreated" => ColVal::Date(cre_date),
        "system.datemodified" => ColVal::Date(mod_date),
        "system.dateaccessed" => ColVal::Date(acc_date),
        "system.itemnamedisplay" | "system.itemname" | "system.filename" | "system.title" => {
            ColVal::Str(hit.file_name.clone())
        }
        "system.fileextension" | "system.itemtype" => ColVal::Str(ext.to_owned()),
        "system.sfgaoflags" => ColVal::U4(sfgao_flags),
        _ => ColVal::Empty,
    }
}

fn make_variant(val: &ColVal) -> VARIANT {
    let mut v = VARIANT::default();
    match val {
        ColVal::Empty => {}
        ColVal::I4(n) => {
            v.Anonymous.Anonymous = ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_I4,
                Anonymous: VARIANT_0_0_0 { lVal: *n },
                ..Default::default()
            });
        }
        ColVal::U4(n) => {
            v.Anonymous.Anonymous = ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_UI4,
                Anonymous: VARIANT_0_0_0 { ulVal: *n },
                ..Default::default()
            });
        }
        ColVal::Date(d) => {
            v.Anonymous.Anonymous = ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_DATE,
                Anonymous: VARIANT_0_0_0 { date: *d },
                ..Default::default()
            });
        }
        ColVal::Str(s) => {
            v.Anonymous.Anonymous = ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_BSTR,
                Anonymous: VARIANT_0_0_0 {
                    bstrVal: ManuallyDrop::new(BSTR::from(s.as_str())),
                },
                ..Default::default()
            });
        }
    }
    v
}

impl IRowset_Impl for MemoryRowset_Impl {
    fn GetNextRows(
        &self,
        chapter: usize,
        offset: isize,
        count: isize,
        obtained: *mut usize,
        output: *mut *mut usize,
    ) -> Result<()> {
        if obtained.is_null() || output.is_null() {
            return Err(E_POINTER.into());
        }
        unsafe {
            obtained.write(0);
        }
        if count < 0 {
            return Err(E_INVALIDARG.into());
        }
        if count == 0 {
            return Ok(());
        }
        let mut state = self.state.lock().map_err(|_| Error::from_hresult(E_FAIL))?;
        let _ = chapter;
        let start = if offset >= 0 {
            state
                .cursor
                .saturating_add(offset as usize)
                .min(self.items.len())
        } else {
            state
                .cursor
                .saturating_sub((-offset) as usize)
                .min(self.items.len())
        };
        let take = (count as usize).min(self.items.len().saturating_sub(start));
        diag(|| {
            format!(
                "rowset: GetNextRows chapter={chapter} offset={offset} count={count} start={start} -> {take} of {}",
                self.items.len()
            )
        });
        if take == 0 {
            return information(DB_S_ENDOFROWSET);
        }
        unsafe {
            if (*output).is_null() {
                *output = CoTaskMemAlloc(take * size_of::<usize>()).cast();
                if (*output).is_null() {
                    return Err(E_OUTOFMEMORY.into());
                }
            }
            for n in 0..take {
                let handle = start + n + 1;
                (*output).add(n).write(handle);
                *state.rows.entry(handle).or_default() += 1;
            }
            obtained.write(take);
        }
        state.cursor = start + take;
        if take < count as usize {
            information(DB_S_ENDOFROWSET)
        } else {
            Ok(())
        }
    }

    fn GetData(&self, row: usize, accessor: HACCESSOR, data: *mut c_void) -> Result<()> {
        diag(|| format!("rowset: GetData row={row} accessor={}", accessor.0));
        if data.is_null() {
            return Err(E_POINTER.into());
        }
        let state = self.state.lock().map_err(|_| Error::from_hresult(E_FAIL))?;
        if !state.rows.contains_key(&row) || row == 0 || row > self.items.len() {
            return Err(DB_E_BADROWHANDLE.into());
        }
        let entry = state
            .accessors
            .get(&accessor.0)
            .ok_or_else(|| Error::from_hresult(DB_E_BADACCESSORHANDLE))?;
        let item = &self.items[row - 1].cols;
        let base = data.cast::<u8>();

        unsafe {
            for b in &entry.cols {
                if b.ordinal == 0 || b.ordinal > item.len() {
                    continue;
                }
                // Defense in depth next to CreateAccessor validation:
                // never write outside the declared row buffer.
                if !binding_in_bounds(b, entry.row_size) {
                    continue;
                }
                let val = &item[b.ordinal - 1];
                let has_val = (b.dw_part & (DBPART_VALUE.0 as u32)) != 0;
                let has_len = (b.dw_part & (DBPART_LENGTH.0 as u32)) != 0;
                let has_status = (b.dw_part & (DBPART_STATUS.0 as u32)) != 0;

                if matches!(val, ColVal::Empty) && b.w_type != (DBTYPE_VARIANT.0 as u16) {
                    if has_status {
                        base.add(b.ob_status).cast::<u32>().write_unaligned(3);
                    }
                    if has_len {
                        base.add(b.ob_length).cast::<usize>().write_unaligned(0);
                    }
                    continue;
                }

                let mut status: u32 = 0;
                let mut byte_len: usize = 0;

                let base_type = b.w_type & !0x4000;
                let by_ref = (b.w_type & 0x4000) != 0;

                match base_type {
                    12 => {
                        let v = make_variant(val);
                        byte_len = size_of::<VARIANT>();
                        if has_val {
                            if by_ref {
                                let mem = CoTaskMemAlloc(size_of::<VARIANT>()).cast::<VARIANT>();
                                if !mem.is_null() {
                                    mem.write(v);
                                    base.add(b.ob_value)
                                        .cast::<*mut VARIANT>()
                                        .write_unaligned(mem);
                                }
                            } else {
                                base.add(b.ob_value).cast::<VARIANT>().write_unaligned(v);
                            }
                        } else {
                            let mut drop_v = v;
                            let _ = VariantClear(&mut drop_v);
                        }
                    }
                    130 => {
                        let s = match val {
                            ColVal::Str(s) => s.as_str(),
                            _ => "",
                        };
                        let utf16: Vec<u16> = s.encode_utf16().collect();
                        byte_len = utf16.len() * 2;
                        if has_val {
                            if by_ref {
                                let alloc_bytes = (utf16.len() + 1) * 2;
                                let mem = CoTaskMemAlloc(alloc_bytes).cast::<u16>();
                                if !mem.is_null() {
                                    ptr::copy_nonoverlapping(utf16.as_ptr(), mem, utf16.len());
                                    mem.add(utf16.len()).write(0);
                                    base.add(b.ob_value).cast::<*mut u16>().write_unaligned(mem);
                                }
                            } else if b.cb_max_len >= 2 {
                                let max_units = (b.cb_max_len / 2).saturating_sub(1);
                                let take = utf16.len().min(max_units);
                                let dst = base.add(b.ob_value).cast::<u16>();
                                for (idx, &ch) in utf16[..take].iter().enumerate() {
                                    dst.add(idx).write_unaligned(ch);
                                }
                                dst.add(take).write_unaligned(0);
                                if take < utf16.len() {
                                    status = DBSTATUS_S_TRUNCATED.0 as u32;
                                }
                            }
                        }
                    }
                    8 => {
                        let s = match val {
                            ColVal::Str(s) => s.as_str(),
                            _ => "",
                        };
                        byte_len = size_of::<*const u16>();
                        if has_val {
                            let bstr = BSTR::from(s);
                            base.add(b.ob_value)
                                .cast::<*const u16>()
                                .write_unaligned(bstr.into_raw());
                        }
                    }
                    3 | 19 => {
                        let num: u32 = match val {
                            ColVal::I4(n) => *n as u32,
                            ColVal::U4(n) => *n,
                            _ => 0,
                        };
                        byte_len = 4;
                        if has_val {
                            base.add(b.ob_value).cast::<u32>().write_unaligned(num);
                        }
                    }
                    7 => {
                        let d = match val {
                            ColVal::Date(d) => *d,
                            _ => 46000.0,
                        };
                        byte_len = 8;
                        if has_val {
                            base.add(b.ob_value).cast::<f64>().write_unaligned(d);
                        }
                    }
                    _ => {
                        status = 3;
                    }
                }

                if has_len {
                    base.add(b.ob_length)
                        .cast::<usize>()
                        .write_unaligned(byte_len);
                }
                if has_status {
                    base.add(b.ob_status).cast::<u32>().write_unaligned(status);
                }
            }
        }
        Ok(())
    }

    fn AddRefRows(
        &self,
        count: usize,
        rows: *const usize,
        refs: *mut u32,
        statuses: *mut u32,
    ) -> Result<()> {
        self.change_rows(count, rows, refs, statuses, true)
    }

    fn ReleaseRows(
        &self,
        count: usize,
        rows: *const usize,
        _options: *const u32,
        refs: *mut u32,
        statuses: *mut u32,
    ) -> Result<()> {
        self.change_rows(count, rows, refs, statuses, false)
    }

    fn RestartPosition(&self, _chapter: usize) -> Result<()> {
        diag(|| "rowset: RestartPosition".to_owned());
        self.state
            .lock()
            .map_err(|_| Error::from_hresult(E_FAIL))?
            .cursor = 0;
        Ok(())
    }
}

impl MemoryRowset_Impl {
    fn change_rows(
        &self,
        count: usize,
        rows: *const usize,
        refs: *mut u32,
        statuses: *mut u32,
        add: bool,
    ) -> Result<()> {
        if count == 0 {
            return Ok(());
        }
        if rows.is_null() {
            return Err(E_INVALIDARG.into());
        }
        let mut state = self.state.lock().map_err(|_| Error::from_hresult(E_FAIL))?;
        let mut any_error = false;
        let mut any_ok = false;
        for n in 0..count {
            let row = unsafe { *rows.add(n) };
            let (count_out, status) = match state.rows.get_mut(&row) {
                Some(entry) if add => {
                    *entry += 1;
                    any_ok = true;
                    (*entry, DBROWSTATUS_S_OK.0 as u32)
                }
                Some(entry) => {
                    *entry -= 1;
                    let left = *entry;
                    if left == 0 {
                        state.rows.remove(&row);
                    }
                    any_ok = true;
                    (left, DBROWSTATUS_S_OK.0 as u32)
                }
                None => {
                    any_error = true;
                    (0, DBROWSTATUS_E_INVALID.0 as u32)
                }
            };
            unsafe {
                if !refs.is_null() {
                    refs.add(n).write(count_out);
                }
                if !statuses.is_null() {
                    statuses.add(n).write(status);
                }
            }
        }
        if any_error {
            if any_ok {
                information(DB_S_ERRORSOCCURRED)
            } else {
                Err(DB_E_BADROWHANDLE.into())
            }
        } else {
            Ok(())
        }
    }
}

impl IAccessor_Impl for MemoryRowset_Impl {
    fn AddRefAccessor(&self, accessor: HACCESSOR, refs: *mut u32) -> Result<()> {
        let mut state = self.state.lock().map_err(|_| Error::from_hresult(E_FAIL))?;
        let entry = state
            .accessors
            .get_mut(&accessor.0)
            .ok_or_else(|| Error::from_hresult(DB_E_BADACCESSORHANDLE))?;
        entry.refs += 1;
        if !refs.is_null() {
            unsafe {
                refs.write(entry.refs);
            }
        }
        Ok(())
    }

    fn CreateAccessor(
        &self,
        flags: u32,
        count: usize,
        bindings: *const DBBINDING,
        row_size: usize,
        accessor: *mut HACCESSOR,
        statuses: *mut u32,
    ) -> Result<()> {
        // DBBINDSTATUS values (oledb.h): OK, BADORDINAL, BADBINDINFO.
        const BIND_OK: u32 = 0;
        const BIND_BADORDINAL: u32 = 1;
        const BIND_BADBINDINFO: u32 = 3;

        if accessor.is_null() || (count > 0 && bindings.is_null()) || count > 1024 {
            return Err(E_INVALIDARG.into());
        }
        unsafe {
            accessor.write(HACCESSOR(0));
        }
        if flags == 0 {
            return Err(E_INVALIDARG.into());
        }
        let mut cols = Vec::with_capacity(count);
        let mut any_invalid = false;
        for n in 0..count {
            let b = unsafe { &*bindings.add(n) };
            let binding = ColBinding {
                ordinal: b.iOrdinal,
                ob_value: b.obValue,
                ob_length: b.obLength,
                ob_status: b.obStatus,
                dw_part: b.dwPart,
                cb_max_len: b.cbMaxLen,
                w_type: b.wType,
            };
            let status = if binding.ordinal > self.schema.len() {
                BIND_BADORDINAL
            } else if !binding_in_bounds(&binding, row_size) {
                BIND_BADBINDINFO
            } else {
                BIND_OK
            };
            any_invalid |= status != BIND_OK;
            cols.push(binding);
            if !statuses.is_null() {
                unsafe {
                    statuses.add(n).write(status);
                }
            }
        }
        diag(|| {
            let parts: Vec<String> = cols
                .iter()
                .map(|b| format!("{}:{}:{:#x}", b.ordinal, b.w_type, b.dw_part))
                .collect();
            format!(
                "rowset: CreateAccessor flags={flags:#x} row_size={row_size} invalid={any_invalid} bindings=[{}]",
                parts.join(" ")
            )
        });
        if any_invalid {
            return Err(E_INVALIDARG.into());
        }
        let mut state = self.state.lock().map_err(|_| Error::from_hresult(E_FAIL))?;
        state.next_accessor += 1;
        let id = state.next_accessor;
        state.accessors.insert(
            id,
            AccessorEntry {
                flags,
                cols,
                row_size,
                refs: 1,
            },
        );
        unsafe {
            accessor.write(HACCESSOR(id));
        }
        Ok(())
    }

    fn GetBindings(
        &self,
        accessor: HACCESSOR,
        flags: *mut u32,
        count: *mut usize,
        bindings: *mut *mut DBBINDING,
    ) -> Result<()> {
        if flags.is_null() || count.is_null() || bindings.is_null() {
            return Err(E_INVALIDARG.into());
        }
        let state = self.state.lock().map_err(|_| Error::from_hresult(E_FAIL))?;
        let entry = state
            .accessors
            .get(&accessor.0)
            .ok_or_else(|| Error::from_hresult(DB_E_BADACCESSORHANDLE))?;
        unsafe {
            let buf = CoTaskMemAlloc(entry.cols.len() * size_of::<DBBINDING>()).cast::<DBBINDING>();
            if buf.is_null() && !entry.cols.is_empty() {
                return Err(E_OUTOFMEMORY.into());
            }
            for (i, b) in entry.cols.iter().enumerate() {
                buf.add(i).write(DBBINDING {
                    iOrdinal: b.ordinal,
                    obValue: b.ob_value,
                    obLength: b.ob_length,
                    obStatus: b.ob_status,
                    dwPart: b.dw_part,
                    cbMaxLen: b.cb_max_len,
                    wType: b.w_type,
                    ..Default::default()
                });
            }
            flags.write(entry.flags);
            count.write(entry.cols.len());
            bindings.write(buf);
        }
        Ok(())
    }

    fn ReleaseAccessor(&self, accessor: HACCESSOR, refs: *mut u32) -> Result<()> {
        let mut state = self.state.lock().map_err(|_| Error::from_hresult(E_FAIL))?;
        let entry = state
            .accessors
            .get_mut(&accessor.0)
            .ok_or_else(|| Error::from_hresult(DB_E_BADACCESSORHANDLE))?;
        entry.refs -= 1;
        let left = entry.refs;
        if left == 0 {
            state.accessors.remove(&accessor.0);
        }
        if !refs.is_null() {
            unsafe {
                refs.write(left);
            }
        }
        Ok(())
    }
}

impl IColumnsInfo_Impl for MemoryRowset_Impl {
    fn GetColumnInfo(
        &self,
        count: *mut usize,
        info: *mut *mut DBCOLUMNINFO,
        strings: *mut *mut u16,
    ) -> Result<()> {
        diag(|| "rowset: GetColumnInfo".to_owned());
        if count.is_null() || info.is_null() || strings.is_null() {
            return Err(E_INVALIDARG.into());
        }
        unsafe {
            let encoded: Vec<Vec<u16>> = self
                .schema
                .iter()
                .map(|c| c.name.encode_utf16().chain(Some(0)).collect())
                .collect();
            let total_u16: usize = encoded.iter().map(Vec::len).sum();
            let str_buf = CoTaskMemAlloc(total_u16 * 2).cast::<u16>();
            if str_buf.is_null() && total_u16 > 0 {
                return Err(E_OUTOFMEMORY.into());
            }
            let col_buf = CoTaskMemAlloc(self.schema.len() * size_of::<DBCOLUMNINFO>())
                .cast::<DBCOLUMNINFO>();
            if col_buf.is_null() && !self.schema.is_empty() {
                CoTaskMemFree(Some(str_buf.cast()));
                return Err(E_OUTOFMEMORY.into());
            }

            let mut offset = 0usize;
            for (i, (name_u16, meta)) in encoded.iter().zip(self.schema.iter()).enumerate() {
                let name_ptr = str_buf.add(offset);
                ptr::copy_nonoverlapping(name_u16.as_ptr(), name_ptr, name_u16.len());
                offset += name_u16.len();

                col_buf.add(i).write(DBCOLUMNINFO {
                    pwszName: PWSTR(name_ptr),
                    iOrdinal: i + 1,
                    ulColumnSize: meta.col_size,
                    wType: meta.w_type,
                    dwFlags: meta.col_flags,
                    columnid: DBID {
                        eKind: DBKIND_NAME.0 as u32,
                        uName: DBID_1 {
                            pwszName: PWSTR(name_ptr),
                        },
                        ..Default::default()
                    },
                    ..Default::default()
                });
            }

            count.write(self.schema.len());
            info.write(col_buf);
            strings.write(str_buf);
        }
        Ok(())
    }

    fn MapColumnIDs(&self, count: usize, ids: *const DBID, columns: *mut usize) -> Result<()> {
        if count > 0 && (ids.is_null() || columns.is_null()) {
            return Err(E_INVALIDARG.into());
        }
        let mut any_invalid = false;
        let mut any_valid = false;
        for n in 0..count {
            let id = unsafe { &*ids.add(n) };
            let mut found = usize::MAX;
            if id.eKind == DBKIND_NAME.0 as u32
                && unsafe { !id.uName.pwszName.is_null() }
                && let Ok(req_name) = unsafe { id.uName.pwszName.to_string() }
            {
                for (idx, meta) in self.schema.iter().enumerate() {
                    if req_name.eq_ignore_ascii_case(&meta.name) {
                        found = idx + 1;
                        break;
                    }
                }
            }
            if found == usize::MAX {
                any_invalid = true;
            } else {
                any_valid = true;
            }
            unsafe {
                columns.add(n).write(found);
            }
        }
        diag(|| format!("rowset: MapColumnIDs count={count} invalid={any_invalid}"));
        if any_invalid {
            if any_valid {
                information(DB_S_ERRORSOCCURRED)
            } else {
                Err(DB_E_BADCOLUMNID.into())
            }
        } else {
            Ok(())
        }
    }
}

/// Build an in-memory OLE DB `IRowset` (with `IAccessor`, `IColumnsInfo`, and `IGetRow`)
/// for the specified projection columns and Ferrow search hits.
pub fn build_for_hits(col_names: &[String], hits: Vec<SearchHitRow>) -> Result<IRowset> {
    let _ = DB_E_CANTCONVERTVALUE;
    let schema: Vec<ColMeta> = col_names.iter().map(|c| classify_column(c)).collect();
    let items: Vec<RowInfo> = hits
        .into_iter()
        .enumerate()
        .map(|(idx, hit)| {
            let file_url = format!("file:{}", hit.full_path.replace('\\', "/"));
            let (ext, item_type_text, kinds, sfgao_flags) =
                classify_file_kind(&hit.file_name, hit.is_dir);
            let rank = 1000 - ((idx as i32) * 10).min(900);
            let modified_ft = unix_ticks_to_filetime(hit.modified_ticks);
            let created_ft = unix_ticks_to_filetime(hit.created_ticks.or(hit.modified_ticks));
            let accessed_ft = unix_ticks_to_filetime(hit.accessed_ticks.or(hit.modified_ticks));
            let cols = schema
                .iter()
                .map(|col| value_for_column(&col.name, &hit, &ext, &kinds, rank, sfgao_flags))
                .collect();
            RowInfo {
                hit,
                file_url,
                ext,
                item_type_text,
                kinds,
                rank,
                sfgao_flags,
                created_ft,
                modified_ft,
                accessed_ft,
                cols,
            }
        })
        .collect();
    let rowset: IRowset = MemoryRowset {
        schema,
        items,
        state: Mutex::new(State::default()),
    }
    .into();
    Ok(rowset)
}

/// Verify that `rows` satisfies both the `IGetRow -> IPropertyStore` contract
/// and the `IAccessor -> IRowset::GetData` contract for `expected_name`.
pub fn verify_rowset(rows: &IRowset, expected_name: &str) -> Result<()> {
    unsafe {
        let mut count = 0;
        let mut info = std::ptr::null_mut();
        let mut strings = std::ptr::null_mut();
        rows.cast::<IColumnsInfo>()?
            .GetColumnInfo(&mut count, &mut info, Some(&mut strings))?;
        let columns = std::slice::from_raw_parts(info, count);
        let compatible = DEFAULT_COLUMNS.iter().enumerate().all(|(n, (_, kind, _))| {
            columns.iter().any(|c| {
                c.iOrdinal == n + 1 && c.wType == if *kind == 202 { 130 } else { *kind as u16 }
            })
        });
        CoTaskMemFree(Some(info.cast()));
        CoTaskMemFree(Some(strings.cast()));
        if !compatible {
            return Err(E_FAIL.into());
        }
        let mut count = 0;
        let mut handles = std::ptr::null_mut();
        let status = (rows.vtable().GetNextRows)(rows.as_raw(), 0, 0, 30, &mut count, &mut handles);
        status.ok()?;
        if count != 1 {
            return Err(E_FAIL.into());
        }
        let get_row: IGetRow = rows.cast()?;
        let store_unk = get_row.GetRowFromHROW(None, *handles, &IPropertyStore::IID)?;
        let store: IPropertyStore = store_unk.cast()?;
        if store.GetCount()? == 0 {
            return Err(E_FAIL.into());
        }
        #[repr(C)]
        struct Cell {
            status: u32,
            length: usize,
            value: VARIANT,
        }
        let mut data: [Cell; 15] = std::array::from_fn(|_| Cell {
            status: u32::MAX,
            length: 0,
            value: VARIANT::default(),
        });
        let bindings: Vec<DBBINDING> = (0..15)
            .map(|n| DBBINDING {
                iOrdinal: n + 1,
                obStatus: n * size_of::<Cell>() + std::mem::offset_of!(Cell, status),
                obLength: n * size_of::<Cell>() + std::mem::offset_of!(Cell, length),
                obValue: n * size_of::<Cell>() + std::mem::offset_of!(Cell, value),
                dwPart: (DBPART_VALUE.0 | DBPART_LENGTH.0 | DBPART_STATUS.0) as u32,
                cbMaxLen: size_of::<VARIANT>(),
                wType: DBTYPE_VARIANT.0 as u16,
                ..Default::default()
            })
            .collect();
        let accessors: IAccessor = rows.cast()?;
        let mut handle = HACCESSOR(0);
        accessors.CreateAccessor(
            DBACCESSOR_ROWDATA.0 as u32,
            15,
            bindings.as_ptr(),
            std::mem::size_of_val(&data),
            &mut handle,
            None,
        )?;
        rows.GetData(*handles, handle, data.as_mut_ptr().cast())?;
        let name = &data[6];
        let name_ok = name.status == 0
            && name.value.Anonymous.Anonymous.vt == VT_BSTR
            && name.value.Anonymous.Anonymous.Anonymous.bstrVal.to_string() == expected_name;
        let path_ok = data[2].status == 0
            && data[2].value.Anonymous.Anonymous.vt == VT_BSTR
            && data[2]
                .value
                .Anonymous
                .Anonymous
                .Anonymous
                .bstrVal
                .to_string()
                .ends_with(expected_name);
        for cell in &mut data {
            VariantClear(&mut cell.value)?;
        }
        accessors.ReleaseAccessor(handle, None)?;
        rows.ReleaseRows(
            count,
            handles,
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )?;
        CoTaskMemFree(Some(handles.cast()));
        if !name_ok || !path_ok {
            return Err(E_FAIL.into());
        }
        rows.RestartPosition(0)?;
        Ok(())
    }
}
