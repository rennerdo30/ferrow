//! OS integration: opening files, revealing them in the file manager,
//! the clipboard, the system accent colour and the window backdrop.

use std::path::Path;
use std::process::Command;

/// Open a file or folder with its default application.
pub fn open(path: &Path) {
    #[cfg(windows)]
    win::shell_open(path);
    #[cfg(target_os = "macos")]
    spawn(Command::new("open").arg(path));
    #[cfg(all(unix, not(target_os = "macos")))]
    spawn(Command::new("xdg-open").arg(path));
}

/// Show the item selected in the system file manager.
pub fn reveal(path: &Path) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // explorer needs `/select,"path"` as one raw argument.
        let arg = format!("/select,\"{}\"", path.display());
        spawn(Command::new("explorer.exe").raw_arg(arg));
    }
    #[cfg(target_os = "macos")]
    spawn(Command::new("open").arg("-R").arg(path));
    #[cfg(all(unix, not(target_os = "macos")))]
    if let Some(parent) = path.parent() {
        spawn(Command::new("xdg-open").arg(parent));
    }
}

/// Start a helper process without waiting for it. The child is not
/// tracked: the file manager or viewer outlives Ferrow on purpose.
#[allow(dead_code, reason = "unused on platforms with a native open call")]
fn spawn(command: &mut Command) {
    let _ = command.spawn();
}

/// Put text on the system clipboard.
pub fn copy_text(text: &str) {
    #[cfg(windows)]
    win::set_clipboard(text);
    #[cfg(target_os = "macos")]
    pipe_to(Command::new("pbcopy"), text);
    #[cfg(all(unix, not(target_os = "macos")))]
    if !pipe_to(Command::new("wl-copy"), text) {
        let mut c = Command::new("xclip");
        c.args(["-selection", "clipboard"]);
        pipe_to(c, text);
    }
}

/// Read text from the system clipboard.
pub fn paste_text() -> Option<String> {
    #[cfg(windows)]
    return win::get_clipboard();
    #[cfg(target_os = "macos")]
    return read_from(Command::new("pbpaste"));
    #[cfg(all(unix, not(target_os = "macos")))]
    return read_from(Command::new("wl-paste").arg("-n")).or_else(|| {
        let mut c = Command::new("xclip");
        c.args(["-selection", "clipboard", "-o"]);
        read_from(c)
    });
}

#[cfg(unix)]
fn pipe_to(mut command: Command, text: &str) -> bool {
    use std::io::Write;
    use std::process::Stdio;
    let Ok(mut child) = command.stdin(Stdio::piped()).spawn() else {
        return false;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
    }
    child.wait().map(|s| s.success()).unwrap_or(false)
}

#[cfg(unix)]
fn read_from(mut command: Command) -> Option<String> {
    let out = command.output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// True inside a Remote Desktop (RDP) session. Other remote tools
/// (VNC, TeamViewer) mirror the console and cannot be detected; the UI
/// is kept cheap to update everywhere for them.
pub fn is_remote_session() -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_REMOTESESSION};
        // SAFETY: GetSystemMetrics has no preconditions.
        unsafe { GetSystemMetrics(SM_REMOTESESSION) != 0 }
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// The user's accent colour as RGB, where the OS exposes one.
pub fn accent_color() -> Option<[u8; 3]> {
    #[cfg(windows)]
    return win::accent_color();
    #[cfg(not(windows))]
    return None;
}

/// Update the frame for a light/dark switch after `apply_backdrop`.
pub fn set_frame_theme(window: &impl raw_window_handle::HasWindowHandle, dark: bool) {
    #[cfg(windows)]
    crate::frame::set_theme(window, dark);
    #[cfg(not(windows))]
    let _ = (window, dark);
}

/// Install the Windows 11 title-row frame (no-op elsewhere).
pub fn install_frame(
    window: &impl raw_window_handle::HasWindowHandle,
    dark: bool,
    repaint: impl Fn() + Send + Sync + 'static,
) {
    #[cfg(windows)]
    crate::frame::install(window, dark, repaint);
    #[cfg(not(windows))]
    let _ = (window, dark, repaint);
}

/// Whether the app draws its own title row (content extends into the
/// title bar, system caption buttons on top).
pub const CUSTOM_TITLE_ROW: bool = cfg!(windows);

/// Apply the OS translucent backdrop (Mica on Windows 11, vibrancy on
/// macOS). Returns false where unavailable; the UI then paints an
/// opaque background instead.
pub fn apply_backdrop(window: &impl raw_window_handle::HasWindowHandle, dark: bool) -> bool {
    #[cfg(windows)]
    {
        // The custom frame is installed separately (`install_frame`);
        // OpenGL windows cannot show Mica, so the UI paints the solid
        // Windows 11 background.
        let _ = (window, dark);
        false
    }
    #[cfg(target_os = "macos")]
    {
        let _ = dark;
        window_vibrancy::apply_vibrancy(
            window,
            window_vibrancy::NSVisualEffectMaterial::Sidebar,
            None,
            None,
        )
        .is_ok()
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = (window, dark);
        false
    }
}

#[cfg(windows)]
mod win {
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    use windows_sys::Win32::Foundation::{GlobalFree, HANDLE};
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{
        GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock,
    };
    use windows_sys::Win32::System::Ole::CF_UNICODETEXT;
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
        s.encode_wide().chain(Some(0)).collect()
    }

    pub fn shell_open(path: &Path) {
        let file = wide(path.as_os_str());
        let verb = wide("open".as_ref());
        // SAFETY: both strings are NUL-terminated UTF-16 buffers that
        // outlive the call; the other pointers are documented as optional.
        unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                verb.as_ptr(),
                file.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                SW_SHOWNORMAL,
            );
        }
    }

    /// Closes the clipboard when dropped.
    struct Clipboard;

    impl Clipboard {
        fn open() -> Option<Clipboard> {
            // SAFETY: a null owner window is allowed; a successful open is
            // paired with CloseClipboard in Drop.
            (unsafe { OpenClipboard(std::ptr::null_mut()) } != 0).then_some(Clipboard)
        }
    }

    impl Drop for Clipboard {
        fn drop(&mut self) {
            // SAFETY: only constructed after a successful OpenClipboard.
            unsafe { CloseClipboard() };
        }
    }

    pub fn set_clipboard(text: &str) {
        let data: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
        let bytes = data.len() * 2;
        let Some(_clip) = Clipboard::open() else {
            return;
        };
        // SAFETY: the clipboard is open (guard above). The global block is
        // allocated with the required size, locked, filled with exactly
        // `bytes` bytes and unlocked before ownership passes to the system
        // via SetClipboardData. On failure we free the block ourselves.
        unsafe {
            EmptyClipboard();
            let block = GlobalAlloc(GMEM_MOVEABLE, bytes);
            if block.is_null() {
                return;
            }
            let dst = GlobalLock(block) as *mut u16;
            if dst.is_null() {
                GlobalFree(block);
                return;
            }
            std::ptr::copy_nonoverlapping(data.as_ptr(), dst, data.len());
            GlobalUnlock(block);
            if SetClipboardData(CF_UNICODETEXT as u32, block as HANDLE).is_null() {
                GlobalFree(block);
            }
        }
    }

    pub fn get_clipboard() -> Option<String> {
        let _clip = Clipboard::open()?;
        // SAFETY: the clipboard is open. The handle belongs to the system;
        // we only lock it, read up to the terminating NUL and unlock it.
        unsafe {
            let handle = GetClipboardData(CF_UNICODETEXT as u32);
            if handle.is_null() {
                return None;
            }
            let src = GlobalLock(handle) as *const u16;
            if src.is_null() {
                return None;
            }
            let mut len = 0usize;
            while *src.add(len) != 0 {
                len += 1;
            }
            let text = String::from_utf16_lossy(std::slice::from_raw_parts(src, len));
            GlobalUnlock(handle);
            Some(text)
        }
    }

    pub fn accent_color() -> Option<[u8; 3]> {
        let key = wide("Software\\Microsoft\\Windows\\DWM".as_ref());
        let value = wide("AccentColor".as_ref());
        let mut data: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;
        // SAFETY: key and value are NUL-terminated UTF-16; `data` is a
        // u32 and `size` holds its size, matching RRF_RT_REG_DWORD.
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                value.as_ptr(),
                RRF_RT_REG_DWORD,
                std::ptr::null_mut(),
                (&mut data as *mut u32).cast(),
                &mut size,
            )
        };
        // AccentColor is stored as 0xAABBGGRR.
        (status == 0).then_some([data as u8, (data >> 8) as u8, (data >> 16) as u8])
    }
}
