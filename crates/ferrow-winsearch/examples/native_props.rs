//! Diagnostics: run a Start-menu-shaped query against native Windows Search
//! and print the properties of the first rows, as `SearchHost.exe` reads them.
//! Usage: `cargo run -p ferrow-winsearch --example native_props -- <term>`

#[cfg(windows)]
fn main() -> windows::core::Result<()> {
    use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
    use windows::Win32::System::Search::{IGetRow, IRowset};
    use windows::Win32::UI::Shell::PropertiesSystem::{IPropertyStore, PSGetPropertyKeyFromName};
    use windows::core::{Interface, PCWSTR};

    let term = std::env::args().nth(1).unwrap_or_else(|| "notepad".into());
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
    let command = ferrow_winsearch::hook::create_native_command()?;
    let sql = format!(
        "SELECT TOP 5 \"System.Kind\",\"System.Search.Rank\",path,\"System.DateCreated\",\"System.DateModified\",\"System.Search.QueryPropertyHits\",\"System.ItemNameDisplay\",\"System.FileExtension\",\"System.ItemType\",\"System.Title\",\"System.Music.AlbumTitle\",\"System.Music.DisplayArtist\",\"System.DateAccessed\",\"System.Link.TargetParsingPath\",\"System.SFGAOFlags\" FROM SystemIndex WHERE WITH (System.ItemNameDisplay) AS #MRProps ((CONTAINS(#MRProps, '\"{term}\"', 1041) RANK BY COERCION(ABSOLUTE, 990)) OR (CONTAINS(#MRProps, '\"{term}*\"', 1041) RANK BY COERCION(ABSOLUTE, 980))) ORDER BY \"System.Search.Rank\" DESC, \"System.DateModified\" DESC"
    );
    const DIALECT: windows::core::GUID =
        windows::core::GUID::from_u128(0xc8b521fb_5cf3_11ce_ade5_00aa0044773d);
    let text: Vec<u16> = sql.encode_utf16().chain(Some(0)).collect();
    unsafe { command.SetCommandText(&DIALECT, PCWSTR(text.as_ptr()))? };
    let mut rowset: Option<IRowset> = None;
    unsafe {
        command.Execute(
            None,
            &IRowset::IID,
            None,
            None,
            Some(std::ptr::from_mut(&mut rowset).cast()),
        )?
    };
    let rowset = rowset.expect("rowset");
    let get_row: IGetRow = rowset.cast()?;
    let mut obtained = 0usize;
    let mut out: *mut usize = std::ptr::null_mut();
    let _ =
        unsafe { (rowset.vtable().GetNextRows)(rowset.as_raw(), 0, 0, 5, &mut obtained, &mut out) };
    let handles: Vec<usize> = unsafe { std::slice::from_raw_parts(out, obtained) }.to_vec();
    println!("rows: {obtained}");
    let names = [
        "System.Kind",
        "System.ParsingPath",
        "System.FileExtension",
        "System.ItemNameDisplay",
        "System.Search.Rank",
        "System.ItemType",
        "System.DateCreated",
        "System.DateModified",
        "System.Search.QueryPropertyHits",
        "System.SFGAOFlags",
        "System.ItemUrl",
        "System.ItemTypeText",
    ];
    for &h in &handles {
        let unk = unsafe { get_row.GetRowFromHROW(None, h, &IPropertyStore::IID)? };
        let store: IPropertyStore = unk.cast()?;
        println!("--- row {h} ({} props)", unsafe { store.GetCount()? });
        for name in names {
            let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
            let mut key = Default::default();
            unsafe { PSGetPropertyKeyFromName(PCWSTR(wide.as_ptr()), &mut key)? };
            let value = unsafe { store.GetValue(&key)? };
            let vt = unsafe { value.Anonymous.Anonymous.vt.0 };
            let text = unsafe { PropVariantToStringAlloc(&value) }
                .map(|p| {
                    let s = unsafe { p.to_string().unwrap_or_default() };
                    unsafe { CoTaskMemFree(Some(p.0.cast())) };
                    s
                })
                .unwrap_or_else(|e| format!("<{e}>"));
            println!("{name} vt={vt} {text}");
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn main() {}
