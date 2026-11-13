//! OS integration: opening files, revealing them in the file manager,
//! the clipboard, the system accent colour and the window backdrop.

use std::path::{Path, PathBuf};
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

/// Open a web page in the default browser.
pub fn open_url(url: &str) {
    if url.starts_with("https://") {
        open(Path::new(url));
    }
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
    return read_from({
        let mut c = Command::new("wl-paste");
        c.arg("-n");
        c
    })
    .or_else(|| {
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

/// Close the window when setup asks (Windows): setup signals the event
/// `Global\Ferrow.Exit.S<session>` for every session where the app runs,
/// and waits for the app to exit instead of killing it. The waiting
/// thread is blocked in the kernel, so it costs nothing while idle.
pub fn listen_for_setup_exit(on_exit: impl Fn() + Send + 'static) {
    #[cfg(windows)]
    win::listen_for_setup_exit(on_exit);
    #[cfg(not(windows))]
    let _ = on_exit;
}

/// If Ferrow already runs in this session (for example hidden in the
/// tray), ask it to show its window and return true: this copy should
/// exit. Windows only; elsewhere always false.
pub fn activate_running_instance() -> bool {
    #[cfg(windows)]
    return win::activate_running_instance();
    #[cfg(not(windows))]
    false
}

/// Call `on_activate` (from a background thread) whenever Ferrow is
/// started again while this copy runs. Windows only.
pub fn listen_for_activation(on_activate: impl Fn() + Send + 'static) {
    #[cfg(windows)]
    win::listen_for_activation(on_activate);
    #[cfg(not(windows))]
    let _ = on_activate;
}

/// Hide the window to the tray instead of closing. False where there is
/// no tray icon; the caller closes the window as usual.
pub fn hide_to_tray() -> bool {
    #[cfg(windows)]
    return crate::tray::hide_to_tray();
    #[cfg(not(windows))]
    false
}

/// Command-line flag: start hidden in the tray (sign-in autostart).
pub const TRAY_FLAG: &str = "--tray";

/// File left by a copy that was hidden in the tray when setup closed it
/// for an update: the restarted copy goes back to the tray too.
fn tray_marker() -> Option<PathBuf> {
    crate::engine::default_index_path()?
        .parent()
        .map(|dir| dir.join("start-in-tray"))
}

/// Whether this start should stay hidden in the tray: started with
/// [`TRAY_FLAG`] (at sign-in), or restarted by an update while the
/// previous copy was in the tray. Windows only.
pub fn start_hidden() -> bool {
    if !cfg!(windows) {
        return false;
    }
    let after_update = tray_marker().is_some_and(|marker| std::fs::remove_file(marker).is_ok());
    after_update || std::env::args().skip(1).any(|arg| arg == TRAY_FLAG)
}

/// Keep the window hidden in the tray until the user opens it.
pub fn keep_hidden_until_opened() {
    #[cfg(windows)]
    crate::tray::start_hidden();
}

/// Setup is about to close the app for an update: remember a hidden
/// window so the updated copy starts in the tray too.
pub fn remember_tray_for_update() {
    #[cfg(windows)]
    if crate::tray::is_hidden()
        && let Some(marker) = tray_marker()
    {
        let _ = std::fs::write(marker, b"1");
    }
}

/// Start Ferrow (hidden in the tray) when the user signs in, or stop doing
/// so. Only an installed copy registers itself; a development build never
/// does. Windows only.
pub fn sync_start_at_sign_in(enabled: bool) {
    #[cfg(windows)]
    win::sync_start_at_sign_in(enabled);
    #[cfg(not(windows))]
    let _ = enabled;
}

/// Bring the window back from the tray (or to the front).
pub fn show_from_tray() {
    #[cfg(windows)]
    crate::tray::show();
}

/// True once after the window came back from the tray: focus the search box.
pub fn take_tray_focus_request() -> bool {
    #[cfg(windows)]
    return crate::tray::take_focus_request();
    #[cfg(not(windows))]
    false
}

/// Quit right away, even while the window is hidden in the tray (setup
/// asked the app to exit). Returns only where there is no tray.
pub fn quit_from_tray() {
    #[cfg(windows)]
    crate::tray::quit();
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

/// A drive or volume the user can add as an index location.
#[derive(Clone, Debug)]
pub struct DriveInfo {
    /// Root path, e.g. `C:\`.
    pub root: PathBuf,
    /// "Local disk", "Removable", "Network", "CD/DVD", …
    pub kind_label: &'static str,
    /// Volume label, or for a network drive the share it maps to.
    pub detail: String,
}

/// List drives. Volume labels are read only for fixed drives, because
/// querying a disconnected network or empty optical drive can stall;
/// call this off the UI thread anyway.
pub fn list_drives() -> Vec<DriveInfo> {
    #[cfg(windows)]
    return win::list_drives();
    #[cfg(not(windows))]
    return Vec::new();
}

/// Roots of the fixed (internal) drives, from drive types only (no
/// volume queries, so this does not stall on network drives).
pub fn fixed_drive_roots() -> Vec<PathBuf> {
    #[cfg(windows)]
    return win::fixed_drive_roots();
    #[cfg(not(windows))]
    return Vec::new();
}

/// Classify a location for display: drive root, network share, or folder.
pub fn classify_location(path: &Path) -> crate::settings::SourceKind {
    use crate::settings::SourceKind;
    let s = path.as_os_str().to_string_lossy();
    if cfg!(windows) {
        let s = s.replace('/', "\\");
        if s.starts_with("\\\\") && !s.starts_with("\\\\?\\") && !s.starts_with("\\\\.\\") {
            return SourceKind::NetworkFolder;
        }
        let bytes = s.as_bytes();
        let is_root = bytes.len() <= 3
            && bytes.len() >= 2
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':';
        #[cfg(windows)]
        let remote = bytes.len() >= 2 && bytes[1] == b':' && win::is_remote_drive(bytes[0]);
        #[cfg(not(windows))]
        let remote = false;
        return match (is_root, remote) {
            (true, true) => SourceKind::NetworkDrive,
            (true, false) => SourceKind::Drive,
            (false, true) => SourceKind::NetworkFolder,
            (false, false) => SourceKind::Folder,
        };
    }
    if path == Path::new("/") {
        SourceKind::Drive
    } else {
        SourceKind::Folder
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
    const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
    const RUN_VALUE: &str = "Ferrow";

    /// The installed app sits next to its uninstaller; development builds
    /// do not, and must never register themselves for sign-in.
    fn installed_exe() -> Option<std::path::PathBuf> {
        let exe = std::env::current_exe().ok()?;
        exe.with_file_name("uninstall.exe").is_file().then_some(exe)
    }

    pub fn sync_start_at_sign_in(enabled: bool) {
        use std::ffi::OsStr;
        use windows_sys::Win32::System::Registry::{
            HKEY_CURRENT_USER, REG_SZ, RegDeleteKeyValueW, RegSetKeyValueW,
        };
        let Some(exe) = installed_exe() else { return };
        let key = wide(OsStr::new(RUN_KEY));
        let name = wide(OsStr::new(RUN_VALUE));
        if enabled {
            let command = wide(OsStr::new(&format!(
                "\"{}\" {}",
                exe.display(),
                super::TRAY_FLAG
            )));
            // SAFETY: terminated strings; the size covers the value and its NUL.
            unsafe {
                RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    key.as_ptr(),
                    name.as_ptr(),
                    REG_SZ,
                    command.as_ptr().cast(),
                    (command.len() * 2) as u32,
                );
            }
        } else {
            // SAFETY: terminated strings.
            unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr()) };
        }
    }

    /// Auto-reset event the running copy waits on; `Local\` keeps it to
    /// this session.
    const ACTIVATE_EVENT: &str = "Local\\Ferrow.Activate";

    fn activate_event_name() -> Vec<u16> {
        ACTIVATE_EVENT.encode_utf16().chain(Some(0)).collect()
    }

    pub fn activate_running_instance() -> bool {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{EVENT_MODIFY_STATE, OpenEventW, SetEvent};
        use windows_sys::Win32::UI::WindowsAndMessaging::{ASFW_ANY, AllowSetForegroundWindow};
        let name = activate_event_name();
        // SAFETY: a terminated name; the handle is closed below.
        let event = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr()) };
        if event.is_null() {
            return false;
        }
        // This copy was started by the user, so it may hand the right to
        // come to the front to the running copy.
        // SAFETY: plain calls on a live handle.
        unsafe {
            AllowSetForegroundWindow(ASFW_ANY);
            SetEvent(event);
            CloseHandle(event);
        }
        true
    }

    pub fn listen_for_activation(on_activate: impl Fn() + Send + 'static) {
        use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
        use windows_sys::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
        let name = activate_event_name();
        // Auto reset: one signal, one activation. Default security: only
        // this user's processes in this session can open it.
        // SAFETY: a terminated name and default attributes.
        let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, name.as_ptr()) };
        if event.is_null() {
            return;
        }
        let event = event as usize;
        let _ = std::thread::Builder::new()
            .name("ferrow-activate".into())
            .spawn(move || {
                // SAFETY: the event handle stays open for the process lifetime.
                while unsafe { WaitForSingleObject(event as _, INFINITE) } == WAIT_OBJECT_0 {
                    on_activate();
                }
            });
    }

    pub fn listen_for_setup_exit(on_exit: impl Fn() + Send + 'static) {
        use windows_sys::Win32::Foundation::{LocalFree, WAIT_OBJECT_0};
        use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
        use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
        use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
        use windows_sys::Win32::System::Threading::{
            CreateEventW, GetCurrentProcessId, INFINITE, WaitForSingleObject,
        };
        let mut session = 0u32;
        // SAFETY: the current process id and a live output.
        if unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &raw mut session) } == 0 {
            return;
        }
        // The owner (this user) has full access; administrators and SYSTEM
        // may only signal and wait (setup runs elevated).
        let sddl: Vec<u16> = "D:(A;;0x1F0003;;;OW)(A;;0x100002;;;BA)(A;;0x100002;;;SY)"
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut sd = std::ptr::null_mut();
        // SAFETY: terminated input, live output; freed below.
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &raw mut sd,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return;
        }
        let attrs = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd,
            bInheritHandle: 0,
        };
        let name: Vec<u16> = format!("Global\\Ferrow.Exit.S{session}")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        // Manual reset: every instance in the session sees one signal.
        // SAFETY: initialized attributes and a terminated name.
        let event = unsafe { CreateEventW(&attrs, 1, 0, name.as_ptr()) };
        // SAFETY: the descriptor was allocated above; the event copied it.
        unsafe { LocalFree(sd) };
        if event.is_null() {
            return;
        }
        let event = event as usize;
        let _ = std::thread::Builder::new()
            .name("ferrow-setup-exit".into())
            .spawn(move || {
                // SAFETY: the event handle stays open for the process lifetime.
                if unsafe { WaitForSingleObject(event as _, INFINITE) } == WAIT_OBJECT_0 {
                    on_exit();
                }
            });
    }

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

    fn drive_root(letter: u8) -> Vec<u16> {
        [letter as u16, b':' as u16, b'\\' as u16, 0].to_vec()
    }

    pub fn is_remote_drive(letter: u8) -> bool {
        use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;
        use windows_sys::Win32::System::WindowsProgramming::DRIVE_REMOTE;
        let root = drive_root(letter.to_ascii_uppercase());
        // SAFETY: root is a NUL-terminated UTF-16 string.
        unsafe { GetDriveTypeW(root.as_ptr()) == DRIVE_REMOTE }
    }

    pub fn fixed_drive_roots() -> Vec<std::path::PathBuf> {
        use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};
        use windows_sys::Win32::System::WindowsProgramming::DRIVE_FIXED;
        // SAFETY: no preconditions.
        let mask = unsafe { GetLogicalDrives() };
        (0..26u8)
            .filter(|i| mask & (1 << i) != 0)
            .map(|i| b'A' + i)
            .filter(|&l| {
                let root = drive_root(l);
                // SAFETY: root is a NUL-terminated UTF-16 string.
                unsafe { GetDriveTypeW(root.as_ptr()) == DRIVE_FIXED }
            })
            // A `subst` drive is a folder on another drive: scanning it too
            // walks the same files twice and lists every one twice.
            .filter(|&l| !is_subst_drive(l))
            .map(|l| std::path::PathBuf::from(format!("{}:\\", l as char)))
            .collect()
    }

    /// Whether a drive letter is a `subst` alias. Its DOS device target is
    /// a `\??\` path (`\??\E:\folder`); a real volume maps to `\Device\...`.
    pub fn is_subst_drive(letter: u8) -> bool {
        use windows_sys::Win32::Storage::FileSystem::QueryDosDeviceW;
        let name = [letter as u16, b':' as u16, 0];
        let mut target = [0u16; 1024];
        // SAFETY: name is NUL-terminated; the buffer and its length match.
        let n = unsafe { QueryDosDeviceW(name.as_ptr(), target.as_mut_ptr(), target.len() as u32) };
        n >= 4 && target[..4] == [b'\\' as u16, b'?' as u16, b'?' as u16, b'\\' as u16]
    }

    pub fn list_drives() -> Vec<super::DriveInfo> {
        use windows_sys::Win32::NetworkManagement::WNet::WNetGetConnectionW;
        use windows_sys::Win32::Storage::FileSystem::{
            GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
        };
        use windows_sys::Win32::System::WindowsProgramming::{
            DRIVE_CDROM, DRIVE_FIXED, DRIVE_RAMDISK, DRIVE_REMOTE, DRIVE_REMOVABLE,
        };
        // SAFETY: no preconditions.
        let mask = unsafe { GetLogicalDrives() };
        let mut out = Vec::new();
        for i in 0..26u8 {
            if mask & (1 << i) == 0 {
                continue;
            }
            let letter = b'A' + i;
            let root = drive_root(letter);
            // SAFETY: root is a NUL-terminated UTF-16 string.
            let ty = unsafe { GetDriveTypeW(root.as_ptr()) };
            let kind_label = match ty {
                DRIVE_FIXED => "Local disk",
                DRIVE_REMOVABLE => "Removable drive",
                DRIVE_REMOTE => "Network drive",
                DRIVE_CDROM => "CD/DVD drive",
                DRIVE_RAMDISK => "RAM disk",
                _ => continue,
            };
            let mut detail = String::new();
            if ty == DRIVE_FIXED {
                let mut name = [0u16; 261];
                // SAFETY: root is NUL-terminated; the name buffer and its
                // length match; the other out-parameters are optional.
                let ok = unsafe {
                    GetVolumeInformationW(
                        root.as_ptr(),
                        name.as_mut_ptr(),
                        name.len() as u32,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        0,
                    )
                };
                if ok != 0 {
                    let end = name.iter().position(|&c| c == 0).unwrap_or(name.len());
                    detail = String::from_utf16_lossy(&name[..end]);
                }
            } else if ty == DRIVE_REMOTE {
                let local = [letter as u16, b':' as u16, 0];
                let mut remote = [0u16; 1024];
                let mut len = remote.len() as u32;
                // SAFETY: local is NUL-terminated; remote and len describe
                // a writable buffer of that many UTF-16 units.
                let status =
                    unsafe { WNetGetConnectionW(local.as_ptr(), remote.as_mut_ptr(), &mut len) };
                if status == 0 {
                    let end = remote.iter().position(|&c| c == 0).unwrap_or(remote.len());
                    detail = String::from_utf16_lossy(&remote[..end]);
                }
            }
            out.push(super::DriveInfo {
                root: std::path::PathBuf::from(format!("{}:\\", letter as char)),
                kind_label,
                detail,
            });
        }
        out
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

#[cfg(all(test, windows))]
mod tests {
    #[test]
    fn real_volumes_are_not_subst_and_subst_aliases_are_skipped() {
        // The system drive is always a real volume.
        assert!(!super::win::is_subst_drive(b'C'));
        // An unassigned letter is not an alias either.
        let mask = unsafe { windows_sys::Win32::Storage::FileSystem::GetLogicalDrives() };
        if let Some(free) = (b'D'..=b'Z').find(|l| mask & (1 << (l - b'A')) == 0) {
            assert!(!super::win::is_subst_drive(free));
        }
        // Whatever `fixed_drive_roots` returns is never an alias.
        for root in super::win::fixed_drive_roots() {
            let letter = root.to_string_lossy().as_bytes()[0];
            assert!(!super::win::is_subst_drive(letter), "{root:?}");
        }
    }
}
