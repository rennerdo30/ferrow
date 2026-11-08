//! Windows Search (`SearchHost.exe`) live query integration for Ferrow.
//!
//! Provides:
//! - [`sql`]: Windows Search SQL (`SystemIndex`) parser and query extractor.
//! - [`ipc`]: Zero-idle-CPU file-backed query responder (`ReadDirectoryChangesW`)
//!   and standalone `index.bin` reader.
//! - `rowset` (Windows): In-process OLE DB `IRowset` + `IAccessor` +
//!   `IColumnsInfo` + `IGetRow` (`IPropertyStore`) implementation compatible
//!   with `SearchHost.exe`'s LowBox AppContainer.
//! - `hook` (Windows): Vtable swap on `tquery.dll`'s `ICommand::Execute`
//!   (`CLSID_CollatorDataSource`) and DLL exports (`FerrowAttach`, `FerrowDetach`).
//! - `controller` (Windows): AppContainer ACL setup, DLL staging, and
//!   `SearchHost.exe` remote attachment.

pub mod ipc;
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
/// and attachment match `enabled`.
pub fn on_index_loaded(data_dir: Option<&Path>, index: Arc<Index>, enabled: bool) {
    set_active_index(Some(index));
    if enabled && let Some(dir) = data_dir {
        let _ = sync_state(dir, true);
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use ferrow_index::{IndexBuilder, flags};
    use windows::Win32::System::Com::{
        COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
    };
    use windows::Win32::System::Search::{ICommandText, IRowset};
    use windows::core::{GUID, Interface, PCWSTR, Result};

    const DIALECT: GUID = GUID::from_u128(0xc8b521fb_5cf3_11ce_ade5_00aa0044773d);

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
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
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

        let command = hook::create_native_command()?;
        assert_eq!(row_count(&execute_sql(&command, &query_sql)?)?, 0);

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
        unsafe {
            CoUninitialize();
        }
        Ok(())
    }
}
