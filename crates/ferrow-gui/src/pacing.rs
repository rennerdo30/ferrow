//! Frame pacing without vsync (Windows).
//!
//! The buffer swap no longer waits for the display's vertical blank
//! (FR-50: an AMD OpenGL driver never returned from that wait). That wait
//! also capped how often the window redraws during a burst of input, such
//! as moving the mouse or resizing, at the display's refresh rate. This
//! keeps that cap with a short sleep of at most one refresh interval
//! before a frame that follows the previous one too closely. A frame
//! after an idle stretch never waits, and the sleep cannot hang.

use std::cell::Cell;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Refresh rates outside this range are treated as unknown or clamped.
const MIN_HZ: u32 = 30;
const MAX_HZ: u32 = 360;
const FALLBACK_HZ: u32 = 60;

thread_local! {
    static LAST_FRAME: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// One refresh of the primary display, read once.
fn interval() -> Duration {
    static INTERVAL: OnceLock<Duration> = OnceLock::new();
    *INTERVAL.get_or_init(|| interval_for(refresh_hz()))
}

fn interval_for(hz: u32) -> Duration {
    let hz = if hz < MIN_HZ {
        FALLBACK_HZ
    } else {
        hz.min(MAX_HZ)
    };
    Duration::from_secs(1) / hz
}

fn refresh_hz() -> u32 {
    use windows_sys::Win32::Graphics::Gdi::{
        DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW,
    };
    // SAFETY: DEVMODEW is plain data; dmSize tells the call its version.
    let mut mode: DEVMODEW = unsafe { std::mem::zeroed() };
    mode.dmSize = size_of::<DEVMODEW>() as u16;
    // SAFETY: a null device name means the current display; `mode` is live.
    let ok =
        unsafe { EnumDisplaySettingsW(std::ptr::null(), ENUM_CURRENT_SETTINGS, &raw mut mode) };
    if ok == 0 { 0 } else { mode.dmDisplayFrequency }
}

/// How long a frame that starts at `now` waits after one that started at
/// `last`.
fn wait_before(last: Option<Instant>, now: Instant, interval: Duration) -> Duration {
    last.map_or(Duration::ZERO, |last| {
        (last + interval).saturating_duration_since(now)
    })
}

/// Call at the start of every frame, on the UI thread.
pub fn pace_frame() {
    let wait = wait_before(LAST_FRAME.get(), Instant::now(), interval());
    if !wait.is_zero() {
        std::thread::sleep(wait);
    }
    LAST_FRAME.set(Some(Instant::now()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_right_after_another_waits_for_the_rest_of_the_interval() {
        let start = Instant::now();
        let interval = Duration::from_millis(16);
        let wait = wait_before(Some(start), start + Duration::from_millis(4), interval);
        assert_eq!(wait, Duration::from_millis(12));
    }

    #[test]
    fn a_frame_after_an_idle_stretch_or_the_first_frame_never_waits() {
        let start = Instant::now();
        let interval = Duration::from_millis(16);
        assert!(wait_before(Some(start), start + Duration::from_secs(2), interval).is_zero());
        assert!(wait_before(None, start, interval).is_zero());
    }

    #[test]
    fn unknown_or_extreme_refresh_rates_stay_in_range() {
        assert_eq!(interval_for(0), Duration::from_secs(1) / FALLBACK_HZ);
        assert_eq!(interval_for(1), Duration::from_secs(1) / FALLBACK_HZ);
        assert_eq!(interval_for(144), Duration::from_secs(1) / 144);
        assert_eq!(interval_for(1000), Duration::from_secs(1) / MAX_HZ);
    }
}
