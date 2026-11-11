//! Whether the system asks for quiet right now: the user's do-not-disturb, a
//! presentation or a full-screen game. Toasts follow these modes on their own (the
//! system holds them back); an app that plays its own notification sound must ask
//! here first, because Inlet's toasts are silent and the sound is the app's.
//!
//! - Windows: `SHQueryUserNotificationState`. Quiet while the user is away or the
//!   screen is locked, a full-screen app or Direct3D game runs, presentation mode is
//!   on, or in the quiet time after an update. Windows 11's Do Not Disturb (Focus
//!   Assist) is not part of that documented state; on versions that do not report it
//!   through this call, the toast is held back by Windows but the sound still plays.
//! - Linux: the notification server's `Inhibited` property (KDE Plasma and other
//!   servers that implement it), then GNOME's "show banners" switch (its Do Not
//!   Disturb) read through the settings portal.
//! - macOS: not detected. Focus has no public API for an ordinary app, so the sound
//!   plays while a Focus is on (the notification itself follows Focus).
//!
//! The calls can block (D-Bus on Linux): call [`quiet_mode`] off the UI thread.

/// Whether a notification sound should stay silent because the system is in a quiet
/// mode. Errors and unknown states count as not quiet.
pub fn quiet_mode() -> bool {
    #[cfg(windows)]
    {
        windows_quiet()
    }
    #[cfg(target_os = "linux")]
    {
        linux_quiet()
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        false
    }
}

/// `QUERY_USER_NOTIFICATION_STATE` values that mean quiet: not present (away, screen
/// saver, locked), busy (a full-screen app), a full-screen Direct3D app, presentation
/// mode and quiet time. Accepting notifications (5) and a Store app in full screen
/// (7) are not quiet: the second shows toasts as banners itself.
#[cfg_attr(not(windows), allow(dead_code))]
fn quiet_from_windows_state(state: i32) -> bool {
    matches!(state, 1 | 2 | 3 | 4 | 6)
}

#[cfg(windows)]
fn windows_quiet() -> bool {
    use windows::Win32::UI::Shell::SHQueryUserNotificationState;
    // SAFETY: no arguments; the call only reads the session's state.
    match unsafe { SHQueryUserNotificationState() } {
        Ok(state) => quiet_from_windows_state(state.0),
        Err(err) => {
            log::info!("could not read the notification state: {err}");
            false
        }
    }
}

#[cfg(target_os = "linux")]
fn linux_quiet() -> bool {
    let Some(bus) = crate::linux_bus::session() else {
        return false;
    };
    let inhibited = bus
        .call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &("org.freedesktop.Notifications", "Inhibited"),
        )
        .ok()
        .and_then(|reply| {
            reply
                .body()
                .deserialize::<zbus::zvariant::OwnedValue>()
                .ok()
        })
        .and_then(|v| as_bool(&v));
    if inhibited == Some(true) {
        return true;
    }
    gnome_banners_off(bus)
}

/// GNOME's Do Not Disturb is `org.gnome.desktop.notifications show-banners = false`,
/// read through the settings portal (also inside a Flatpak). `ReadOne` is the current
/// method; `Read` (deprecated, the value wrapped twice) is the fallback.
#[cfg(target_os = "linux")]
fn gnome_banners_off(bus: &zbus::blocking::Connection) -> bool {
    let key = ("org.gnome.desktop.notifications", "show-banners");
    let read = |method: &str| {
        bus.call_method(
            Some("org.freedesktop.portal.Desktop"),
            "/org/freedesktop/portal/desktop",
            Some("org.freedesktop.portal.Settings"),
            method,
            &key,
        )
        .ok()
        .and_then(|reply| {
            reply
                .body()
                .deserialize::<zbus::zvariant::OwnedValue>()
                .ok()
        })
        .and_then(|v| as_bool(&v))
    };
    read("ReadOne").or_else(|| read("Read")) == Some(false)
}

/// A boolean, also when a variant wraps it (once or more).
#[cfg(target_os = "linux")]
fn as_bool(v: &zbus::zvariant::Value<'_>) -> Option<bool> {
    match v {
        zbus::zvariant::Value::Bool(b) => Some(*b),
        zbus::zvariant::Value::Value(inner) => as_bool(inner),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_states_that_mean_quiet() {
        // Away or locked, busy, Direct3D full screen, presentation, quiet time.
        for state in [1, 2, 3, 4, 6] {
            assert!(quiet_from_windows_state(state), "{state}");
        }
        // Accepts notifications, a Store app in full screen, anything unknown.
        for state in [0, 5, 7, 8, -1] {
            assert!(!quiet_from_windows_state(state), "{state}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn wrapped_booleans_are_read() {
        use zbus::zvariant::Value;
        assert_eq!(as_bool(&Value::Bool(false)), Some(false));
        let twice = Value::Value(Box::new(Value::Value(Box::new(Value::Bool(true)))));
        assert_eq!(as_bool(&twice), Some(true));
        assert_eq!(as_bool(&Value::U32(1)), None);
    }

    /// Without a session bus (CI containers) or on an unknown system the answer is
    /// "not quiet", and asking does not hang.
    #[test]
    fn asking_answers() {
        let started = std::time::Instant::now();
        let _ = quiet_mode();
        assert!(started.elapsed() < std::time::Duration::from_secs(30));
    }
}
