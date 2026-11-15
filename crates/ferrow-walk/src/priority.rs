//! Background priority for scans that run while an index already answers
//! searches (service rescans). Such a scan must never compete with
//! searches or with the user's own programs for CPU or disk.

use std::cell::Cell;

thread_local! {
    static BACKGROUND: Cell<bool> = const { Cell::new(false) };
}

/// Mark the calling thread as background work: lowest normal CPU priority,
/// and raw volume reads it starts use low I/O priority. Call it on every
/// thread of a background scan pool (rayon `start_handler`) and on the
/// thread that runs the scan.
pub fn enter_background() {
    BACKGROUND.set(true);
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_LOWEST,
        };
        // SAFETY: pseudo-handle of the calling thread; no memory involved.
        // Failure leaves normal priority, which is only slower to yield.
        unsafe {
            SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_LOWEST);
        }
    }
}

/// Undo [`enter_background`] on the calling thread.
pub fn leave_background() {
    BACKGROUND.set(false);
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_NORMAL,
        };
        // SAFETY: as in `enter_background`.
        unsafe {
            SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_NORMAL);
        }
    }
}

/// Whether [`enter_background`] was called on this thread.
pub fn is_background() -> bool {
    BACKGROUND.get()
}
