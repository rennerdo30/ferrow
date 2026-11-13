//! Notification-area (tray) icon on Windows.
//!
//! Closing the window hides it instead of quitting, so the index, live
//! updates and the Start menu integration keep running. Clicking the icon
//! brings the window back; its menu offers Open and Quit. The icon talks to
//! the main window itself (its callback messages arrive in the frame's
//! window subclass), so no extra window or thread is needed.

use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_SETVERSION,
    NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIcon, CreatePopupMenu, DestroyMenu, HICON, IsIconic, IsWindowVisible,
    MF_SEPARATOR, MF_STRING, PostMessageW, RegisterWindowMessageW, SM_CXSMICON, SW_HIDE,
    SW_RESTORE, SW_SHOW, SWP_SHOWWINDOW, SetForegroundWindow, SetMenuDefaultItem, ShowWindow,
    TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, WINDOWPOS, WM_APP, WM_CLOSE,
    WM_CONTEXTMENU, WM_DESTROY, WM_NULL, WM_SHOWWINDOW, WM_WINDOWPOSCHANGED, WM_WINDOWPOSCHANGING,
};

/// Callback message the icon sends to the main window.
const TRAY_MESSAGE: u32 = WM_APP + 0x46;
/// Posted to hide the window again while Ferrow starts in the tray.
const HIDE_MESSAGE: u32 = WM_APP + 0x47;
const ICON_ID: u32 = 1;
/// `NIN_SELECT` / `NIN_KEYSELECT`: the icon was clicked or chosen with the
/// keyboard (`NOTIFYICON_VERSION_4`).
const NIN_SELECT: u32 = 0x0400;
const NIN_KEYSELECT: u32 = 0x0401;
const MENU_OPEN: usize = 1;
const MENU_QUIT: usize = 2;

static WINDOW: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());
/// Explorer broadcasts this after it restarts; the icon must be re-added.
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
static INSTALLED: AtomicBool = AtomicBool::new(false);
static FOCUS_SEARCH: AtomicBool = AtomicBool::new(false);
/// Set when Ferrow starts in the tray: the window must not appear until
/// the user opens it (eframe shows its window after the first frame).
static KEEP_HIDDEN: AtomicBool = AtomicBool::new(false);

/// Start hidden in the tray: block the window from showing until [`show`].
pub fn start_hidden() {
    KEEP_HIDDEN.store(true, Ordering::Release);
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

/// Add the icon for `hwnd`, the app's main window. Call on the UI thread.
pub fn install(hwnd: HWND) {
    WINDOW.store(hwnd, Ordering::Release);
    // SAFETY: a terminated UTF-16 string.
    let message = unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) };
    TASKBAR_CREATED.store(message, Ordering::Relaxed);
    add_icon(hwnd);
}

fn icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
    // SAFETY: NOTIFYICONDATAW is plain data; all-zero is a valid start.
    let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    data.hWnd = hwnd;
    data.uID = ICON_ID;
    data
}

fn add_icon(hwnd: HWND) {
    let mut data = icon_data(hwnd);
    data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
    data.uCallbackMessage = TRAY_MESSAGE;
    data.hIcon = create_icon(hwnd);
    let tip = wide("Ferrow");
    data.szTip[..tip.len()].copy_from_slice(&tip);
    // SAFETY: `data` is fully initialized and describes our own window.
    let added = unsafe { Shell_NotifyIconW(NIM_ADD, &data) } != 0;
    if added {
        data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        // SAFETY: as above.
        unsafe { Shell_NotifyIconW(NIM_SETVERSION, &data) };
    }
    INSTALLED.store(added, Ordering::Release);
}

fn remove_icon() {
    let hwnd = WINDOW.load(Ordering::Acquire);
    if hwnd.is_null() || !INSTALLED.swap(false, Ordering::AcqRel) {
        return;
    }
    let data = icon_data(hwnd);
    // SAFETY: removes the icon this process added for its own window.
    unsafe { Shell_NotifyIconW(NIM_DELETE, &data) };
}

/// The Ferrow logo as an icon at the small-icon size for the window's DPI.
fn create_icon(hwnd: HWND) -> HICON {
    // SAFETY: hwnd is our live window.
    let size = unsafe { GetSystemMetricsForDpi(SM_CXSMICON, GetDpiForWindow(hwnd)) }.max(16) as u32;
    let Some((width, height, rgba)) = crate::logo::rgba_at_least(size) else {
        return std::ptr::null_mut();
    };
    // 32-bit colour plane in BGRA order with alpha; the AND mask is unused
    // for alpha icons but must exist (one bit per pixel, rows WORD-aligned).
    let bgra: Vec<u8> = rgba
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|[r, g, b, a]| [*b, *g, *r, *a])
        .collect();
    let mask_row = width.div_ceil(16) as usize * 2;
    let mask = vec![0u8; mask_row * height as usize];
    // SAFETY: both planes match `width` x `height` at the declared depths.
    unsafe {
        CreateIcon(
            std::ptr::null_mut(),
            width as i32,
            height as i32,
            1,
            32,
            mask.as_ptr(),
            bgra.as_ptr(),
        )
    }
}

/// Bring the window back (from the tray, or when Ferrow is started again)
/// and put the cursor in the search box.
pub fn show() {
    let hwnd = WINDOW.load(Ordering::Acquire);
    if hwnd.is_null() {
        return;
    }
    FOCUS_SEARCH.store(true, Ordering::Release);
    KEEP_HIDDEN.store(false, Ordering::Release);
    // SAFETY: hwnd is our live window; both calls are safe from any thread.
    unsafe {
        ShowWindow(
            hwnd,
            if IsIconic(hwnd) != 0 {
                SW_RESTORE
            } else {
                SW_SHOW
            },
        );
        SetForegroundWindow(hwnd);
    }
    crate::frame::repaint();
}

/// Hide the window to the tray. False when there is no tray icon (the
/// caller then closes the app as usual).
pub fn hide_to_tray() -> bool {
    let hwnd = WINDOW.load(Ordering::Acquire);
    if hwnd.is_null() || !INSTALLED.load(Ordering::Acquire) {
        return false;
    }
    // SAFETY: hwnd is our live window.
    unsafe { ShowWindow(hwnd, SW_HIDE) };
    true
}

/// True while the window is hidden in the tray.
pub fn is_hidden() -> bool {
    let hwnd = WINDOW.load(Ordering::Acquire);
    // SAFETY: hwnd is our live window.
    !hwnd.is_null() && INSTALLED.load(Ordering::Acquire) && unsafe { IsWindowVisible(hwnd) } == 0
}

/// True once after [`show`]: the UI should focus the search box.
pub fn take_focus_request() -> bool {
    FOCUS_SEARCH.swap(false, Ordering::AcqRel)
}

/// Quit the app for real (tray menu, setup). Settings are saved as they
/// change, so nothing is lost by ending the process here.
pub fn quit() -> ! {
    remove_icon();
    std::process::exit(0)
}

fn menu(hwnd: HWND, x: i32, y: i32) {
    // SAFETY: the menu is created, shown and destroyed on this thread; the
    // strings outlive the calls that copy them.
    unsafe {
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return;
        }
        let open = wide("Open Ferrow");
        let quit_text = wide("Quit Ferrow");
        AppendMenuW(menu, MF_STRING, MENU_OPEN, open.as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
        AppendMenuW(menu, MF_STRING, MENU_QUIT, quit_text.as_ptr());
        SetMenuDefaultItem(menu, MENU_OPEN as u32, 0);
        // Without this the menu would not close when clicking elsewhere.
        SetForegroundWindow(hwnd);
        let chosen = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY,
            x,
            y,
            0,
            hwnd,
            std::ptr::null(),
        ) as usize;
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
        match chosen {
            MENU_OPEN => show(),
            MENU_QUIT => quit(),
            _ => {}
        }
    }
}

/// Tray handling inside the main window's subclass procedure. `Some` when
/// the message was handled here.
pub fn handle(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    match msg {
        TRAY_MESSAGE => {
            match (lparam & 0xFFFF) as u32 {
                NIN_SELECT | NIN_KEYSELECT => show(),
                WM_CONTEXTMENU => {
                    let x = (wparam & 0xFFFF) as i16 as i32;
                    let y = ((wparam >> 16) & 0xFFFF) as i16 as i32;
                    menu(hwnd, x, y);
                }
                _ => {}
            }
            Some(0)
        }
        // The ✕ button, Alt+F4 and the taskbar's Close window: keep running.
        WM_CLOSE if hide_to_tray() => Some(0),
        // Started in the tray: drop any attempt to show the window until
        // the user opens it.
        WM_WINDOWPOSCHANGING if KEEP_HIDDEN.load(Ordering::Acquire) => {
            // SAFETY: for this message lparam points to the caller's WINDOWPOS.
            unsafe { (*(lparam as *mut WINDOWPOS)).flags &= !SWP_SHOWWINDOW };
            None
        }
        // Some show paths (ShowWindow) make the window visible regardless;
        // hide it again before it paints.
        WM_SHOWWINDOW | WM_WINDOWPOSCHANGED if KEEP_HIDDEN.load(Ordering::Acquire) => {
            // SAFETY: hwnd is our live window, on its own thread.
            unsafe {
                if IsWindowVisible(hwnd) != 0 {
                    PostMessageW(hwnd, HIDE_MESSAGE, 0, 0);
                }
            }
            None
        }
        HIDE_MESSAGE => {
            if KEEP_HIDDEN.load(Ordering::Acquire) {
                // SAFETY: hwnd is our live window.
                unsafe { ShowWindow(hwnd, SW_HIDE) };
            }
            Some(0)
        }
        WM_DESTROY => {
            remove_icon();
            None
        }
        _ if msg != 0 && msg == TASKBAR_CREATED.load(Ordering::Relaxed) => {
            add_icon(hwnd);
            None
        }
        _ => None,
    }
}
