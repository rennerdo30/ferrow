//! Windows Search (`SearchHost.exe`) live query integration for Ferrow.
//!
//! Provides:
//! - [`sql`]: Windows Search SQL (`SystemIndex`) parser and query extractor.
//! - [`ipc`]: Zero-idle-CPU file-backed query responder (`ReadDirectoryChangesW`)
//!   answering from the app's in-memory index or the `ferrowd` service,
//!   plus the standalone `index.bin` reader for local mode.
//! - `rowset` (Windows): In-process OLE DB `IRowset` + `IAccessor` +
//!   `IColumnsInfo` + `IGetRow` (`IPropertyStore`) implementation compatible
//!   with `SearchHost.exe`'s LowBox AppContainer.
//! - `hook` (Windows): Vtable swap on `tquery.dll`'s `ICommand::Execute`
//!   (`CLSID_CollatorDataSource`) and DLL exports (`FerrowAttach`, `FerrowDetach`).
//! - `controller` (Windows): AppContainer ACL setup, DLL staging, and
//!   `SearchHost.exe` remote attachment.

pub mod ipc;
pub mod kinds;
pub mod sql;

#[cfg(windows)]
pub mod controller;
#[cfg(windows)]
pub mod hook;
#[cfg(windows)]
pub mod rowset;

use std::path::Path;
use std::sync::Arc;

use ferrow_index::Index;

pub use ipc::set_active_index;

/// Synchronize the live Windows Search attachment and responder with the user's
/// setting. On non-Windows platforms this is a no-op.
pub fn sync_state(data_dir: &Path, enabled: bool) -> Result<(), String> {
    #[cfg(windows)]
    {
        controller::sync_state(data_dir, enabled)
    }
    #[cfg(not(windows))]
    {
        let _ = (data_dir, enabled);
        Ok(())
    }
}

/// Share the newly loaded `Arc<Index>` and ensure the Windows Search responder
/// and attachment match `enabled`. The app's own index is now the source.
pub fn on_index_loaded(data_dir: Option<&Path>, index: Arc<Index>, enabled: bool) {
    set_active_index(Some(index));
    if let Some(dir) = data_dir {
        if enabled {
            let _ = sync_state(dir, true);
        }
        ipc::set_source(dir, ipc::IndexSource::Local);
    }
}

/// The `ferrowd` service now answers the app's searches: publish it as the
/// Start menu source (the responder queries it over `ferrow-ipc`) and
/// ensure the attachment matches `enabled`. Any previously shared local
/// index stops answering, so Start results track the service's live index.
pub fn on_service_connected(data_dir: Option<&Path>, enabled: bool) {
    set_active_index(None);
    if let Some(dir) = data_dir {
        if enabled {
            let _ = sync_state(dir, true);
        }
        ipc::set_source(dir, ipc::IndexSource::Service);
    }
}

/// Live state of the integration for the Settings page.
#[derive(Clone, Debug, Default)]
pub struct IntegrationStatus {
    /// `SearchHost.exe` pid the DLL is attached to, if attached.
    pub attached_pid: Option<u32>,
    /// Which index answers Start menu searches, once known.
    pub source: Option<ipc::IndexSource>,
    /// Most recent attachment failure, if attachment has failed.
    pub last_error: Option<String>,
}

/// Current attachment and answering source. Cheap; safe to poll per frame.
pub fn status() -> IntegrationStatus {
    #[cfg(windows)]
    let (attached_pid, last_error) = controller::attach_status();
    #[cfg(not(windows))]
    let (attached_pid, last_error) = (None, None);
    IntegrationStatus {
        attached_pid,
        source: ipc::source(),
        last_error,
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use ferrow_index::{IndexBuilder, flags};
    use windows::Win32::System::Com::{
        COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
    };
    use windows::Win32::System::Search::{DBBINDING, HACCESSOR, IAccessor, ICommandText, IRowset};
    use windows::core::{GUID, Interface, PCWSTR, Result};

    const DIALECT: GUID = GUID::from_u128(0xc8b521fb_5cf3_11ce_ade5_00aa0044773d);

    /// COM tests share process-wide state (the vtable hook, the active
    /// index, pooled test-thread apartments): run them one at a time.
    static COM_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn com_test_lock() -> std::sync::MutexGuard<'static, ()> {
        match COM_TEST_LOCK.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Balanced COM init: uninitialize only when this call actually
    /// initialized (S_OK/S_FALSE), never after RPC_E_CHANGED_MODE.
    struct ComGuard(bool);

    fn com_init() -> ComGuard {
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        ComGuard(hr.is_ok())
    }

    impl Drop for ComGuard {
        fn drop(&mut self) {
            if self.0 {
                unsafe {
                    CoUninitialize();
                }
            }
        }
    }

    fn execute_sql(command: &ICommandText, sql: &str) -> Result<IRowset> {
        unsafe {
            let text: Vec<u16> = sql.encode_utf16().chain(Some(0)).collect();
            command.SetCommandText(&DIALECT, PCWSTR(text.as_ptr()))?;
            let mut rows: Option<IRowset> = None;
            command.Execute(
                None,
                &IRowset::IID,
                None,
                None,
                Some(std::ptr::from_mut(&mut rows).cast()),
            )?;
            rows.ok_or_else(|| {
                windows::core::Error::from_hresult(windows::Win32::Foundation::E_POINTER)
            })
        }
    }

    fn row_count(rows: &IRowset) -> Result<usize> {
        unsafe {
            let mut count = 0;
            let mut handles = std::ptr::null_mut();
            (rows.vtable().GetNextRows)(rows.as_raw(), 0, 0, 30, &mut count, &mut handles).ok()?;
            if count > 0 {
                rows.ReleaseRows(
                    count,
                    handles,
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )?;
            }
            CoTaskMemFree(Some(handles.cast()));
            Ok(count)
        }
    }

    #[test]
    fn vtable_hook_routes_when_enabled_and_restores_when_disabled() -> Result<()> {
        let _lock = com_test_lock();
        let _com = com_init();
        let temp_dir =
            std::env::temp_dir().join(format!("ferrow-winsearch-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(ipc::winsearch_dir(&temp_dir)).unwrap();

        let builder = IndexBuilder::new();
        let root = builder.add_root(temp_dir.to_string_lossy().as_bytes(), flags::DIR);
        builder.add(root, b"ferrow-native-query-proof.txt", 0);
        builder.add(root, b"unrelated.txt", 0);
        let index = Arc::new(builder.freeze());
        set_active_index(Some(index));

        let projection = rowset::DEFAULT_COLUMNS
            .iter()
            .map(|(n, _, _)| format!("\"{n}\""))
            .collect::<Vec<_>>()
            .join(",");
        let query_sql = format!(
            "SELECT TOP 30 {projection} FROM SystemIndex WHERE CONTAINS(System.FileName, '\"ferrow-native-query-proof*\"') ORDER BY System.Search.Rank DESC"
        );
        let empty_sql = "SELECT TOP 1 System.ItemName FROM SystemIndex WHERE System.ItemName = 'ferrow-native-query-poc-82e561d9-never-created.txt'";

        // The native catalog is a host service, not Ferrow code: where it
        // is unavailable (for example CI runners without Windows Search),
        // skip instead of failing. Where it answers, every assertion
        // below stays strict.
        let probe = (|| -> Result<ICommandText> {
            let command = hook::create_native_command()?;
            assert_eq!(row_count(&execute_sql(&command, &query_sql)?)?, 0);
            Ok(command)
        })();
        let Ok(command) = probe else {
            eprintln!("skipping: native Windows Search catalog unavailable here");
            set_active_index(None);
            let _ = std::fs::remove_dir_all(&temp_dir);
            return Ok(());
        };

        // Enable marker and attach hook in-process.
        std::fs::write(ipc::enabled_marker_path(&temp_dir), b"1").unwrap();
        hook::attach_in_process(temp_dir.clone())?;

        // Matching query routes through Ferrow and satisfies both IGetRow and IAccessor.
        let rows = execute_sql(&command, &query_sql)?;
        rowset::verify_rowset(&rows, "ferrow-native-query-proof.txt")?;
        drop(rows);

        // Unsupported query falls back to native tquery.dll.
        assert_eq!(row_count(&execute_sql(&command, empty_sql)?)?, 0);

        // Removing the enabled marker dynamically disables routing without detaching.
        std::fs::remove_file(ipc::enabled_marker_path(&temp_dir)).unwrap();
        assert_eq!(row_count(&execute_sql(&command, &query_sql)?)?, 0);

        // Detach restores original vtable slot.
        hook::detach_in_process()?;
        drop(command);
        set_active_index(None);
        let _ = std::fs::remove_dir_all(&temp_dir);
        Ok(())
    }

    /// The rowset contract (columns, `IGetRow`, `IAccessor`) without the
    /// native catalog, so every Windows test host covers it, including
    /// runners where Windows Search itself never answers.
    #[test]
    fn rowset_contract_without_native_catalog() -> Result<()> {
        let _lock = com_test_lock();
        let _com = com_init();
        let columns: Vec<String> = rowset::DEFAULT_COLUMNS
            .iter()
            .map(|(name, _, _)| (*name).to_owned())
            .collect();
        let hits = vec![ipc::SearchHitRow {
            file_name: "ferrow-native-query-proof.txt".into(),
            full_path: r"C:\Work\ferrow-native-query-proof.txt".into(),
            is_dir: false,
            modified_ticks: Some(17_123_456_000_000_000),
            created_ticks: None,
            accessed_ticks: None,
        }];
        let rows = rowset::build_for_hits(&columns, hits)?;
        rowset::verify_rowset(&rows, "ferrow-native-query-proof.txt")?;

        // Hostile accessors are rejected at creation: GetData must
        // never be able to write outside the declared row buffer.
        let accessor_iface: IAccessor = rows.cast()?;
        for (binding, row_size, expected_status) in [
            // Offset far past the 64-byte row buffer.
            (
                DBBINDING {
                    iOrdinal: 1,
                    obValue: 1_000_000,
                    dwPart: 1,
                    cbMaxLen: 16,
                    wType: 130,
                    ..Default::default()
                },
                64usize,
                3u32,
            ),
            // Offset near usize::MAX: offset + width must not wrap.
            (
                DBBINDING {
                    iOrdinal: 1,
                    obStatus: usize::MAX - 2,
                    dwPart: 4,
                    ..Default::default()
                },
                usize::MAX,
                3u32,
            ),
            // Ordinal beyond the 15-column schema.
            (
                DBBINDING {
                    iOrdinal: 99,
                    obValue: 0,
                    dwPart: 1,
                    cbMaxLen: 16,
                    wType: 130,
                    ..Default::default()
                },
                64usize,
                1u32,
            ),
        ] {
            let mut handle = HACCESSOR(0);
            let mut status = 0u32;
            let created = unsafe {
                accessor_iface.CreateAccessor(
                    2,
                    1,
                    &binding,
                    row_size,
                    &mut handle,
                    Some(&mut status),
                )
            };
            assert!(created.is_err(), "malformed binding must be rejected");
            assert_eq!(handle.0, 0, "rejected accessor must stay null");
            assert_eq!(status, expected_status);
        }

        drop(rows);
        Ok(())
    }
}
