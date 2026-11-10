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
