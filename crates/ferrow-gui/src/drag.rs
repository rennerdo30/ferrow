//! Dragging a result out of Ferrow into Explorer, the desktop, a mail or
//! chat window, an editor (Windows).
//!
//! The drag carries a plain file list (`CF_HDROP`) in the shell's own data
//! object, and the shell's default drag behaviour runs it. Nothing here
//! resolves the path or asks a shell extension anything, so an unreachable
//! network drive cannot stall the window when a drag starts.

use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::System::Com::{
    DVASPECT_CONTENT, FORMATETC, IDataObject, STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL,
};
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GMEM_ZEROINIT, GlobalAlloc, GlobalLock, GlobalUnlock,
};
use windows::Win32::System::Ole::{
    CF_HDROP, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE, OleInitialize,
};
use windows::Win32::UI::Shell::{DROPFILES, SHCreateDataObject, SHDoDragDrop};
use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, PostMessageW, WM_LBUTTONUP};

/// A `CF_HDROP` medium listing `path`.
fn file_list(path: &Path) -> Option<STGMEDIUM> {
    let mut name: Vec<u16> = path.as_os_str().encode_wide().collect();
    // Each name ends with NUL, and the list with one more.
    name.extend([0, 0]);
    let header = size_of::<DROPFILES>();
    let bytes = header + name.len() * 2;
    // SAFETY: a fresh movable block of `bytes` bytes, locked while written
    // and unlocked before it is handed to the data object.
    unsafe {
        let block = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes).ok()?;
        let at = GlobalLock(block) as *mut u8;
        if at.is_null() {
            return None;
        }
        let files = DROPFILES {
            pFiles: header as u32,
            fWide: true.into(),
            ..Default::default()
        };
        std::ptr::write_unaligned(at as *mut DROPFILES, files);
        std::ptr::copy_nonoverlapping(name.as_ptr() as *const u8, at.add(header), name.len() * 2);
        let _ = GlobalUnlock(block);
        Some(STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 { hGlobal: block },
            pUnkForRelease: std::mem::ManuallyDrop::new(None),
        })
    }
}

fn hdrop_format() -> FORMATETC {
    FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    }
}

/// The shell's data object carrying `path` as a file list.
fn data_object(path: &Path) -> Option<IDataObject> {
    // SAFETY: COM/OLE is initialized on this thread by the caller; the
    // data object takes ownership of the medium (fRelease = true).
    unsafe {
        let data =
            SHCreateDataObject::<Option<&IDataObject>, IDataObject>(None, None, None).ok()?;
        let medium = file_list(path)?;
        data.SetData(&hdrop_format(), &medium, true).ok()?;
        Some(data)
    }
}

/// Run a drag of `path` from the window `hwnd` until the user drops it.
/// Call on the UI thread while the left button is held. Returns false
/// when the drag could not start.
pub fn drag_out(hwnd: HWND, path: &Path) -> bool {
    // SAFETY: OLE on the UI thread (already initialized by the window
    // library; a second call only adds a reference). The data object and
    // the medium it takes ownership of live until SHDoDragDrop returns.
    let started = unsafe {
        let _ = OleInitialize(None);
        let Some(data) = data_object(path) else {
            return false;
        };
        SHDoDragDrop(
            Some(hwnd),
            &data,
            None,
            DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK,
        )
        .is_ok()
    };
    // The drag loop took the button release; give it back to the window,
    // or the app would still think the button is held.
    let mut at = POINT::default();
    // SAFETY: our own window; plain message to it.
    unsafe {
        let _ = GetCursorPos(&mut at);
        let _ = windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut at);
        let pos = ((at.y as u16 as u32) << 16) | (at.x as u16 as u32);
        let _ = PostMessageW(Some(hwnd), WM_LBUTTONUP, WPARAM(0), LPARAM(pos as isize));
    }
    started
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};

    /// The drag carries exactly the path, readable the way drop targets
    /// read it, including a non-ASCII name.
    #[test]
    fn data_object_carries_the_path_as_a_file_list() {
        // SAFETY: test thread; paired with OleUninitialize.
        unsafe { OleInitialize(None) }.unwrap();
        let path = std::env::temp_dir().join("Ferrow drag ü 文書.txt");
        let data = data_object(&path).expect("data object");
        // SAFETY: reading back the medium the object holds.
        unsafe {
            let mut medium = data.GetData(&hdrop_format()).expect("CF_HDROP");
            let drop = HDROP(medium.u.hGlobal.0);
            assert_eq!(DragQueryFileW(drop, u32::MAX, None), 1);
            let mut buf = [0u16; 512];
            let len = DragQueryFileW(drop, 0, Some(&mut buf)) as usize;
            assert_eq!(
                String::from_utf16_lossy(&buf[..len]),
                path.display().to_string()
            );
            windows::Win32::System::Ole::ReleaseStgMedium(&mut medium);
            windows::Win32::System::Ole::OleUninitialize();
        }
    }
}
