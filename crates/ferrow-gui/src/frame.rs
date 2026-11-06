//! Windows 11 frame: the content extends into the title bar, the way
//! current Windows apps (Explorer, Settings, Terminal) look.
//!
//! The app draws the caption buttons itself with the system's own
//! Segoe Fluent Icons glyphs. The maximise button reports itself to
//! Windows as `HTMAXBUTTON`, so hovering it opens Snap Layouts exactly as
//! on a standard window. Everything else in the title row that is not a
//! control drags the window.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
use windows_sys::Win32::Graphics::Gdi::ScreenToClient;
use windows_sys::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    TME_LEAVE, TME_NONCLIENT, TRACKMOUSEEVENT, TrackMouseEvent,
};
use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetClientRect, HTCAPTION, HTCLIENT, HTMAXBUTTON, HTTOP, HTTOPLEFT, HTTOPRIGHT, IsZoomed,
    NCCALCSIZE_PARAMS, SM_CXPADDEDBORDER, SM_CYFRAME, SW_MAXIMIZE, SW_RESTORE, SWP_FRAMECHANGED,
    SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SetWindowPos, ShowWindow, WM_NCCALCSIZE, WM_NCHITTEST,
    WM_NCLBUTTONDBLCLK, WM_NCLBUTTONDOWN, WM_NCLBUTTONUP, WM_NCMOUSELEAVE, WM_NCMOUSEMOVE,
};

const DWMWA_USE_IMMERSIVE_DARK_MODE: u32 = 20;

/// Title-row geometry in physical client pixels, set by the UI.
static CAPTION_HEIGHT: AtomicI32 = AtomicI32::new(0);
/// The search box: clicks there go to the app.
static CONTROL_LEFT: AtomicI32 = AtomicI32::new(0);
static CONTROL_RIGHT: AtomicI32 = AtomicI32::new(0);
/// Left edge of the caption buttons, and the maximise button's span.
static BUTTONS_LEFT: AtomicI32 = AtomicI32::new(i32::MAX);
static MAX_LEFT: AtomicI32 = AtomicI32::new(0);
static MAX_RIGHT: AtomicI32 = AtomicI32::new(0);

static MAX_HOVERED: AtomicBool = AtomicBool::new(false);
static MAX_PRESSED: AtomicBool = AtomicBool::new(false);
static REPAINT: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

/// Geometry of the title row, all in physical client pixels.
pub struct Caption {
    pub height: f32,
    pub control: (f32, f32),
    pub buttons_left: f32,
    pub max_button: (f32, f32),
}

pub fn set_caption(c: &Caption) {
    let s = |a: &AtomicI32, v: f32| a.store(v.round() as i32, Ordering::Relaxed);
    s(&CAPTION_HEIGHT, c.height);
    s(&CONTROL_LEFT, c.control.0);
    s(&CONTROL_RIGHT, c.control.1);
    s(&BUTTONS_LEFT, c.buttons_left);
    s(&MAX_LEFT, c.max_button.0);
    s(&MAX_RIGHT, c.max_button.1);
}

/// Hover and press state of the maximise button (it receives
/// non-client mouse messages, so the app cannot see them itself).
pub fn max_button_state() -> (bool, bool) {
    (
        MAX_HOVERED.load(Ordering::Relaxed),
        MAX_PRESSED.load(Ordering::Relaxed),
    )
}

fn hwnd_of(window: &impl raw_window_handle::HasWindowHandle) -> Option<HWND> {
    match window.window_handle().ok()?.as_raw() {
        raw_window_handle::RawWindowHandle::Win32(h) => Some(h.hwnd.get() as HWND),
        _ => None,
    }
}

/// Light or dark system frame (border and Snap Layouts flyout colours).
pub fn set_theme(window: &impl raw_window_handle::HasWindowHandle, dark: bool) {
    let Some(hwnd) = hwnd_of(window) else { return };
    let dark: i32 = dark.into();
    // SAFETY: hwnd is our live top-level window; the value is a 4-byte
    // integer and the size passed matches it.
    unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            (&raw const dark).cast(),
            4,
        );
    }
}

/// Remove the caption strip and install the hit-test hook. `repaint`
/// asks the UI to redraw when the maximise button's state changes.
pub fn install(
    window: &impl raw_window_handle::HasWindowHandle,
    dark: bool,
    repaint: impl Fn() + Send + Sync + 'static,
) -> bool {
    let Some(hwnd) = hwnd_of(window) else {
        return false;
    };
    let _ = REPAINT.set(Box::new(repaint));
    set_theme(window, dark);
    // SAFETY: hwnd is our live window, owned by this (the UI) thread, so
    // subclassing it here is allowed. `subclass_proc` matches SUBCLASSPROC
    // and lives for the whole process. SetWindowPos with SWP_FRAMECHANGED
    // only makes Windows recompute the frame.
    unsafe {
        SetWindowSubclass(hwnd, Some(subclass_proc), 1, 0);
        SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER,
        );
    }
    true
}

fn repaint() {
    if let Some(f) = REPAINT.get() {
        f();
    }
}

fn set_flag(flag: &AtomicBool, value: bool) {
    if flag.swap(value, Ordering::Relaxed) != value {
        repaint();
    }
}

/// Resize border thickness for this window's DPI.
///
/// # Safety
/// `hwnd` must be a live window.
unsafe fn frame_thickness(hwnd: HWND) -> i32 {
    // SAFETY: guaranteed by the caller.
    unsafe {
        let dpi = GetDpiForWindow(hwnd);
        GetSystemMetricsForDpi(SM_CYFRAME, dpi) + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi)
    }
}

/// Classify a client point in the title row.
fn caption_hit(x: i32, y: i32) -> u32 {
    if y >= CAPTION_HEIGHT.load(Ordering::Relaxed) {
        return HTCLIENT;
    }
    if x >= BUTTONS_LEFT.load(Ordering::Relaxed) {
        let (l, r) = (
            MAX_LEFT.load(Ordering::Relaxed),
            MAX_RIGHT.load(Ordering::Relaxed),
        );
        return if x >= l && x < r {
            HTMAXBUTTON
        } else {
            HTCLIENT
        };
    }
    let (l, r) = (
        CONTROL_LEFT.load(Ordering::Relaxed),
        CONTROL_RIGHT.load(Ordering::Relaxed),
    );
    if x >= l && x < r { HTCLIENT } else { HTCAPTION }
}

unsafe extern "system" fn subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    // SAFETY: Windows calls this on the window's thread with valid message
    // arguments. For WM_NCCALCSIZE with wparam != 0, lparam points to a
    // NCCALCSIZE_PARAMS owned by the caller for the duration of the call.
    unsafe {
        match msg {
            WM_NCCALCSIZE if wparam != 0 => {
                let params = lparam as *mut NCCALCSIZE_PARAMS;
                let original_top = (*params).rgrc[0].top;
                let r = DefSubclassProc(hwnd, msg, wparam, lparam);
                if r != 0 {
                    return r;
                }
                // Drop the caption strip, keep the side and bottom borders.
                let rect: &mut RECT = &mut (*params).rgrc[0];
                rect.top = original_top;
                if IsZoomed(hwnd) != 0 {
                    // A maximised window hangs over the screen edge by the
                    // frame thickness; keep the content on screen.
                    rect.top += frame_thickness(hwnd);
                }
                0
            }
            WM_NCHITTEST => {
                let hit = DefSubclassProc(hwnd, msg, wparam, lparam);
                if hit != HTCLIENT as LRESULT {
                    return hit;
                }
                let mut pt = POINT {
                    x: (lparam & 0xFFFF) as i16 as i32,
                    y: ((lparam >> 16) & 0xFFFF) as i16 as i32,
                };
                ScreenToClient(hwnd, &mut pt);
                let border = frame_thickness(hwnd);
                if IsZoomed(hwnd) == 0 && pt.y < border {
                    let mut rc = RECT::default();
                    GetClientRect(hwnd, &mut rc);
                    return if pt.x < border * 2 {
                        HTTOPLEFT as LRESULT
                    } else if pt.x > rc.right - border * 2 {
                        HTTOPRIGHT as LRESULT
                    } else {
                        HTTOP as LRESULT
                    };
                }
                caption_hit(pt.x, pt.y) as LRESULT
            }
            WM_NCMOUSEMOVE => {
                let over = wparam == HTMAXBUTTON as WPARAM;
                if over && !MAX_HOVERED.load(Ordering::Relaxed) {
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE | TME_NONCLIENT,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    TrackMouseEvent(&mut tme);
                }
                set_flag(&MAX_HOVERED, over);
                if !over {
                    set_flag(&MAX_PRESSED, false);
                }
                DefSubclassProc(hwnd, msg, wparam, lparam)
            }
            WM_NCMOUSELEAVE => {
                set_flag(&MAX_HOVERED, false);
                set_flag(&MAX_PRESSED, false);
                DefSubclassProc(hwnd, msg, wparam, lparam)
            }
            // Handle the maximise button ourselves; the default handler
            // would draw a legacy button.
            WM_NCLBUTTONDOWN | WM_NCLBUTTONDBLCLK if wparam == HTMAXBUTTON as WPARAM => {
                set_flag(&MAX_PRESSED, true);
                0
            }
            WM_NCLBUTTONUP if wparam == HTMAXBUTTON as WPARAM => {
                if MAX_PRESSED.swap(false, Ordering::Relaxed) {
                    let cmd = if IsZoomed(hwnd) != 0 {
                        SW_RESTORE
                    } else {
                        SW_MAXIMIZE
                    };
                    ShowWindow(hwnd, cmd);
                }
                repaint();
                0
            }
            _ => DefSubclassProc(hwnd, msg, wparam, lparam),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_row_hit_regions() {
        set_caption(&Caption {
            height: 48.0,
            control: (200.0, 600.0),
            buttons_left: 900.0,
            max_button: (946.0, 992.0),
        });
        assert_eq!(caption_hit(50, 10), HTCAPTION);
        assert_eq!(caption_hit(300, 10), HTCLIENT);
        assert_eq!(caption_hit(910, 10), HTCLIENT); // minimise
        assert_eq!(caption_hit(950, 10), HTMAXBUTTON);
        assert_eq!(caption_hit(1000, 10), HTCLIENT); // close
        assert_eq!(caption_hit(50, 60), HTCLIENT); // below the title row
    }
}
