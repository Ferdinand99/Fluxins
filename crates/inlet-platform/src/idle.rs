//! Time since the user last touched the keyboard or mouse, for the idle status.
//!
//! - Windows: `GetLastInputInfo`.
//! - macOS: `CGEventSourceSecondsSinceLastEventType`; needs no permission.
//! - Linux on Wayland: GNOME's `org.gnome.Mutter.IdleMonitor`, then
//!   `org.freedesktop.ScreenSaver.GetSessionIdleTime` (KDE). On X11: the
//!   MIT-SCREEN-SAVER extension. Other Wayland compositors give `None`.

use std::time::Duration;

/// Time since the last keyboard or mouse input, or `None` if this system cannot
/// tell. Cheap enough to poll every few seconds.
pub fn idle_time() -> Option<Duration> {
    imp::idle_time()
}

#[cfg(windows)]
mod imp {
    use std::time::Duration;

    use windows::Win32::System::SystemInformation::GetTickCount;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};

    pub(super) fn idle_time() -> Option<Duration> {
        let mut info = LASTINPUTINFO {
            cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };
        // SAFETY: `info` is a valid, sized LASTINPUTINFO.
        if !unsafe { GetLastInputInfo(&mut info) }.as_bool() {
            return None;
        }
        // Both are milliseconds since boot in 32 bits; wrapping_sub handles the
        // 49.7-day wrap.
        // SAFETY: no preconditions.
        let now = unsafe { GetTickCount() };
        Some(Duration::from_millis(u64::from(
            now.wrapping_sub(info.dwTime),
        )))
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::time::Duration;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventSourceSecondsSinceLastEventType(source_state: i32, event_type: u32) -> f64;
    }

    /// kCGEventSourceStateCombinedSessionState
    const COMBINED_SESSION_STATE: i32 = 0;
    /// kCGAnyInputEventType
    const ANY_INPUT_EVENT: u32 = !0;

    pub(super) fn idle_time() -> Option<Duration> {
        // SAFETY: a plain query without pointers.
        let seconds = unsafe {
            CGEventSourceSecondsSinceLastEventType(COMBINED_SESSION_STATE, ANY_INPUT_EVENT)
        };
        (seconds.is_finite() && seconds >= 0.0).then(|| Duration::from_secs_f64(seconds))
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::time::Duration;

    pub(super) fn idle_time() -> Option<Duration> {
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            // XWayland only sees input to X11 windows, so never ask X11 here.
            return wayland();
        }
        if std::env::var_os("DISPLAY").is_some() {
            return x11();
        }
        None
    }

    fn wayland() -> Option<Duration> {
        let bus = crate::linux_bus::session()?;
        let mutter = bus.call_method(
            Some("org.gnome.Mutter.IdleMonitor"),
            "/org/gnome/Mutter/IdleMonitor/Core",
            Some("org.gnome.Mutter.IdleMonitor"),
            "GetIdletime",
            &(),
        );
        if let Ok(reply) = mutter {
            if let Ok(ms) = reply.body().deserialize::<u64>() {
                return Some(Duration::from_millis(ms));
            }
        }
        let screensaver = bus.call_method(
            Some("org.freedesktop.ScreenSaver"),
            "/org/freedesktop/ScreenSaver",
            Some("org.freedesktop.ScreenSaver"),
            "GetSessionIdleTime",
            &(),
        );
        let seconds = screensaver.ok()?.body().deserialize::<u32>().ok()?;
        Some(Duration::from_secs(u64::from(seconds)))
    }

    fn x11() -> Option<Duration> {
        use x11rb::connection::Connection as _;
        use x11rb::protocol::screensaver::ConnectionExt as _;

        let (conn, screen) = x11rb::connect(None).ok()?;
        let root = conn.setup().roots.get(screen)?.root;
        let info = conn.screensaver_query_info(root).ok()?.reply().ok()?;
        Some(Duration::from_millis(u64::from(info.ms_since_user_input)))
    }
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
mod imp {
    pub(super) fn idle_time() -> Option<std::time::Duration> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// On a desktop (and the CI runners) the OS reports a time; it is never absurd.
    #[test]
    fn idle_time_is_plausible() {
        let idle = idle_time();
        if cfg!(any(windows, target_os = "macos")) {
            assert!(
                idle.is_some(),
                "Windows and macOS always report an idle time"
            );
        }
        if let Some(idle) = idle {
            assert!(idle < Duration::from_secs(60 * 60 * 24 * 365), "{idle:?}");
        }
    }
}
