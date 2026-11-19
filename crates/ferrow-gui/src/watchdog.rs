//! Restart the app when its UI thread hangs for good (Windows).
//!
//! A graphics driver can block the UI thread forever: an AMD OpenGL
//! driver never returned from `SwapBuffers` (FR-50). Nothing inside the
//! process can free that thread, and while it is stuck the window, the
//! tray icon and the "start Ferrow again" activation all wait on it.
//!
//! A small thread looks at the window every few seconds without touching
//! the UI thread: `IsHungAppWindow` only reads whether the thread still
//! takes input. When the window stays hung, the watchdog starts a fresh
//! copy and ends this one. The fresh copy waits for this one to exit, so it
//! takes over the single-instance event cleanly. A limit on restarts in a
//! row keeps a driver that hangs every time from turning this into a loop.

use std::time::Duration;

use windows_sys::Win32::Foundation::{CloseHandle, HWND};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, PROCESS_SYNCHRONIZE, TerminateProcess, WaitForSingleObject,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{IsHungAppWindow, IsWindowVisible};

/// How often the window is looked at. One wake-up every few seconds on an
/// otherwise sleeping thread; it never wakes the UI thread.
const POLL: Duration = Duration::from_secs(5);
/// Hung readings in a row before restarting. Windows reports a window as
/// hung after 5 s without taking input, so this restarts after about 20 s.
const HUNG_POLLS: u32 = 3;
/// Restarts in a row before the watchdog gives up on this machine state.
const MAX_RESTARTS: u32 = 3;
/// Responsive readings after which earlier restarts no longer count
/// (10 minutes): a hang days later gets its restart again.
const HEALTHY_POLLS: u32 = 120;
/// Restarts in a row so far, handed to each fresh copy (not persistent).
const RESTARTS_ENV: &str = "FERROW_HANG_RESTARTS";
/// `--replaces=<pid>`: wait for that hung copy to exit before starting.
const REPLACES_FLAG: &str = "--replaces=";
/// Upper bound on that wait; the hung copy is ended right after the spawn.
const REPLACED_EXIT_WAIT_MS: u32 = 10_000;

/// The watchdog's decision, kept apart from the Windows calls so it can
/// be tested.
#[derive(Debug)]
struct Watch {
    hung: u32,
    healthy: u32,
    restarts: u32,
}

impl Watch {
    fn new(restarts: u32) -> Self {
        Self {
            hung: 0,
            healthy: 0,
            restarts,
        }
    }

    /// Record one reading; true when the app should restart now.
    fn observe(&mut self, hung: bool) -> bool {
        if !hung {
            self.hung = 0;
            self.healthy = self.healthy.saturating_add(1);
            if self.healthy >= HEALTHY_POLLS {
                self.restarts = 0;
            }
            return false;
        }
        self.healthy = 0;
        self.hung = self.hung.saturating_add(1);
        self.hung >= HUNG_POLLS && self.restarts < MAX_RESTARTS
    }
}

fn restarts_so_far() -> u32 {
    std::env::var(RESTARTS_ENV)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

/// Started as the replacement of a hung copy: wait (bounded) until that
/// copy has exited, so this one does not hand its start to the hung one.
/// Call first thing in `main`.
pub fn wait_for_replaced_copy() {
    let Some(pid) = std::env::args()
        .skip(1)
        .find_map(|arg| arg.strip_prefix(REPLACES_FLAG)?.parse::<u32>().ok())
    else {
        return;
    };
    // SAFETY: a plain open for waiting; the handle is closed below.
    let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if process.is_null() {
        // Already gone.
        return;
    }
    // SAFETY: a live handle, closed right after the bounded wait.
    unsafe {
        WaitForSingleObject(process, REPLACED_EXIT_WAIT_MS);
        CloseHandle(process);
    }
}

/// Watch `hwnd`, the app's main window, from a background thread.
pub fn start(hwnd: HWND) {
    let hwnd = hwnd as usize;
    let _ = std::thread::Builder::new()
        .name("ferrow-watchdog".into())
        .spawn(move || {
            let hwnd = hwnd as HWND;
            let mut watch = Watch::new(restarts_so_far());
            loop {
                std::thread::sleep(POLL);
                // SAFETY: only reads the window's state; allowed from any
                // thread, and a window that is gone reads as not hung.
                let hung = unsafe { IsHungAppWindow(hwnd) } != 0;
                if watch.observe(hung) && restart(hwnd, watch.restarts + 1) {
                    return;
                }
            }
        });
}

/// Start a fresh copy and end this one. False (and this copy keeps
/// running) when the fresh copy could not be started.
fn restart(hwnd: HWND, restarts: u32) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let mut command = std::process::Command::new(exe);
    command
        .arg(format!("{REPLACES_FLAG}{}", std::process::id()))
        .env(RESTARTS_ENV, restarts.to_string());
    // Come back the way the user left it: in the tray, unless the window
    // was showing or the user was just trying to open it.
    // SAFETY: reads the window's style; allowed from any thread.
    let visible = unsafe { IsWindowVisible(hwnd) } != 0;
    if !visible && !crate::tray::show_pending() {
        command.arg(crate::platform::TRAY_FLAG);
    }
    if command.spawn().is_err() {
        return false;
    }
    crate::tray::remove_icon();
    // SAFETY: ends this process at once. Its UI thread is stuck, so no
    // orderly exit could finish; settings are saved as they change.
    unsafe { TerminateProcess(GetCurrentProcess(), 1) };
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restarts_only_after_the_window_stays_hung() {
        let mut watch = Watch::new(0);
        for _ in 1..HUNG_POLLS {
            assert!(!watch.observe(true));
        }
        assert!(watch.observe(true));
    }

    #[test]
    fn a_responsive_reading_starts_the_count_again() {
        let mut watch = Watch::new(0);
        for _ in 1..HUNG_POLLS {
            assert!(!watch.observe(true));
        }
        assert!(!watch.observe(false));
        for _ in 1..HUNG_POLLS {
            assert!(!watch.observe(true));
        }
        assert!(watch.observe(true));
    }

    #[test]
    fn gives_up_after_the_restart_limit() {
        let mut watch = Watch::new(MAX_RESTARTS);
        for _ in 0..HUNG_POLLS * 4 {
            assert!(!watch.observe(true));
        }
    }

    #[test]
    fn a_long_healthy_run_forgets_earlier_restarts() {
        let mut watch = Watch::new(MAX_RESTARTS);
        for _ in 0..HEALTHY_POLLS {
            watch.observe(false);
        }
        for _ in 1..HUNG_POLLS {
            assert!(!watch.observe(true));
        }
        assert!(watch.observe(true));
    }
}
