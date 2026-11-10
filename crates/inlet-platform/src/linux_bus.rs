//! One shared D-Bus session connection for the badge and idle-time queries.

use std::sync::OnceLock;

/// The session bus, connected on first use. `None` if there is no session bus (a
/// bare CI container, for example); that is remembered and not retried.
pub(crate) fn session() -> Option<&'static zbus::blocking::Connection> {
    static BUS: OnceLock<Option<zbus::blocking::Connection>> = OnceLock::new();
    BUS.get_or_init(|| match zbus::blocking::Connection::session() {
        Ok(conn) => Some(conn),
        Err(err) => {
            log::info!("no D-Bus session bus: {err}");
            None
        }
    })
    .as_ref()
}

/// 0 = not asked yet or asking, 1 = the server reads markup in the body, 2 = it
/// shows the body as plain text.
static BODY_MARKUP: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Asks the notification server once, on a background thread, whether it reads
/// markup in the body (`GetCapabilities` contains `body-markup`).
#[cfg_attr(not(feature = "gpui"), allow(dead_code))]
pub(crate) fn query_body_markup() {
    static ASKED: std::sync::Once = std::sync::Once::new();
    ASKED.call_once(|| {
        let _ = std::thread::Builder::new()
            .name("inlet-notify-caps".into())
            .spawn(|| {
                let Some(bus) = session() else { return };
                let caps = bus
                    .call_method(
                        Some("org.freedesktop.Notifications"),
                        "/org/freedesktop/Notifications",
                        Some("org.freedesktop.Notifications"),
                        "GetCapabilities",
                        &(),
                    )
                    .and_then(|reply| reply.body().deserialize::<Vec<String>>());
                if let Ok(caps) = caps {
                    let markup = caps.iter().any(|c| c == "body-markup");
                    BODY_MARKUP.store(
                        if markup { 1 } else { 2 },
                        std::sync::atomic::Ordering::Relaxed,
                    );
                }
            });
    });
}

/// Whether to escape notification bodies. Until the server has answered (or when
/// it cannot be asked) the answer is yes: every common server reads markup, and
/// escaping too much only shows an entity, while too little can show a link.
#[cfg_attr(not(feature = "gpui"), allow(dead_code))]
pub(crate) fn body_markup() -> bool {
    query_body_markup();
    BODY_MARKUP.load(std::sync::atomic::Ordering::Relaxed) != 2
}
