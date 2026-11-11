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
//! Call [`quiet_mode`] off the UI thread. On Linux every D-Bus call has a 500 ms limit
//! (a server that never answers counts as "not quiet"), and the answer is kept for one
//! second, so a burst of sounds asks once.

/// Whether a notification sound should stay silent because the system is in a quiet
/// mode. Errors and unknown states count as not quiet.
pub fn quiet_mode() -> bool {
    #[cfg(windows)]
    {
        windows_quiet()
    }
    #[cfg(target_os = "linux")]
    {
        static LAST: Answer = std::sync::Mutex::new(None);
        cached(&LAST, std::time::Instant::now(), || {
            quiet_bus().is_some_and(linux_quiet_on)
        })
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

/// How long a D-Bus call for the quiet state may take.
#[cfg(target_os = "linux")]
const CALL_LIMIT: std::time::Duration = std::time::Duration::from_millis(500);
/// How long an answer is kept.
#[cfg(target_os = "linux")]
const CACHE_FOR: std::time::Duration = std::time::Duration::from_secs(1);

/// The last answer and when it was taken.
#[cfg(target_os = "linux")]
type Answer = std::sync::Mutex<Option<(std::time::Instant, bool)>>;

/// The answer in `last` while it is younger than [`CACHE_FOR`], else a fresh one from
/// `ask`, kept in `last`.
#[cfg(target_os = "linux")]
fn cached(last: &Answer, now: std::time::Instant, ask: impl FnOnce() -> bool) -> bool {
    let mut last = last.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, quiet)) = *last {
        if now.saturating_duration_since(at) < CACHE_FOR {
            return quiet;
        }
    }
    let quiet = ask();
    *last = Some((now, quiet));
    quiet
}

/// A session-bus connection of its own whose calls give up after [`CALL_LIMIT`]
/// (the shared one waits as long as the bus does). Made once; `None` without a bus.
#[cfg(target_os = "linux")]
fn quiet_bus() -> Option<&'static zbus::blocking::Connection> {
    static BUS: std::sync::OnceLock<Option<zbus::blocking::Connection>> =
        std::sync::OnceLock::new();
    BUS.get_or_init(|| {
        bounded_session()
            .map_err(|e| log::info!("no D-Bus for the quiet state: {e}"))
            .ok()
    })
    .as_ref()
}

#[cfg(target_os = "linux")]
fn bounded_session() -> zbus::Result<zbus::blocking::Connection> {
    zbus::blocking::connection::Builder::session()?
        .method_timeout(CALL_LIMIT)
        .build()
}

#[cfg(target_os = "linux")]
fn linux_quiet_on(bus: &zbus::blocking::Connection) -> bool {
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
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_answer_is_kept_for_a_second() {
        use std::time::{Duration, Instant};
        let last: Answer = std::sync::Mutex::new(None);
        let t0 = Instant::now();
        assert!(cached(&last, t0, || true));
        assert!(cached(&last, t0 + Duration::from_millis(900), || false));
        assert!(!cached(&last, t0 + Duration::from_millis(1100), || false));
    }

    /// A stand-in notification server whose `Inhibited` answers after `delay`.
    #[cfg(target_os = "linux")]
    struct Server {
        /// How long `Inhibited` takes to answer, in milliseconds.
        delay_ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
    }

    #[cfg(target_os = "linux")]
    #[zbus::interface(name = "org.freedesktop.Notifications")]
    impl Server {
        #[zbus(property)]
        fn inhibited(&self) -> bool {
            let ms = self.delay_ms.load(std::sync::atomic::Ordering::Relaxed);
            std::thread::sleep(std::time::Duration::from_millis(ms));
            true
        }
    }

    /// Puts a stand-in server on the session bus; `None` (skipped) without a session
    /// bus or when a real notification server holds the name.
    #[cfg(target_os = "linux")]
    fn serve(
        delay_ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) -> Option<zbus::blocking::Connection> {
        if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
            eprintln!("skipped: no D-Bus session bus (run under dbus-run-session)");
            return None;
        }
        let conn = zbus::blocking::Connection::session().ok()?;
        conn.object_server()
            .at("/org/freedesktop/Notifications", Server { delay_ms })
            .ok()?;
        match conn.request_name("org.freedesktop.Notifications") {
            Ok(_) => Some(conn),
            Err(err) => {
                eprintln!("skipped: the notification name is taken ({err})");
                None
            }
        }
    }

    /// An inhibited server means quiet; the same server answering too late means "not
    /// quiet", within the limit and not after the bus's own timeout.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_server_that_does_not_answer_is_not_quiet() {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::Arc;
        use std::time::{Duration, Instant};
        let delay = Arc::new(AtomicU64::new(0));
        let Some(_server) = serve(delay.clone()) else {
            return;
        };
        let client = bounded_session().expect("a bounded connection");
        assert!(linux_quiet_on(&client));

        delay.store(3_000, Ordering::Relaxed);
        let started = Instant::now();
        assert!(!linux_quiet_on(&client));
        // The property call gives up after 500 ms; the portal calls fail at once (no
        // portal on the test bus) or after 500 ms each.
        assert!(
            started.elapsed() < Duration::from_millis(2500),
            "{:?}",
            started.elapsed()
        );
    }
}
