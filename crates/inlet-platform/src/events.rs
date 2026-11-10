use std::sync::OnceLock;

use crate::DeepLink;

/// Something the operating system told the app.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlatformEvent {
    /// The tray icon was clicked (left click on Windows and Linux; on macOS a click
    /// opens the menu, as in Fluxer).
    TrayClicked,
    /// A tray menu entry was chosen; carries the entry's id.
    TrayMenu(String),
    /// A notification was clicked; carries its tag.
    NotificationClicked(String),
    /// An `inlet://` link arrived, already validated. It only says where to go: the
    /// app shows the target and asks before doing anything.
    DeepLink(DeepLink),
}

/// Bounded so that a flood of clicks cannot grow memory without limit. Events beyond
/// this are dropped with a warning.
const QUEUE: usize = 256;

type Bus = (
    async_channel::Sender<PlatformEvent>,
    async_channel::Receiver<PlatformEvent>,
);

fn bus() -> &'static Bus {
    static BUS: OnceLock<Bus> = OnceLock::new();
    BUS.get_or_init(|| async_channel::bounded(QUEUE))
}

/// The queue of platform events. Read it from one place in the app: every clone
/// takes from the same queue, so two readers would each miss events.
pub fn events() -> async_channel::Receiver<PlatformEvent> {
    bus().1.clone()
}

pub(crate) fn emit(event: PlatformEvent) {
    if let Err(err) = bus().0.try_send(event) {
        log::warn!("platform event dropped: {}", err.into_inner().kind());
    }
}

impl PlatformEvent {
    /// A short name for logs. Never includes ids, tags or link contents.
    pub fn kind(&self) -> &'static str {
        match self {
            PlatformEvent::TrayClicked => "tray-clicked",
            PlatformEvent::TrayMenu(_) => "tray-menu",
            PlatformEvent::NotificationClicked(_) => "notification-clicked",
            PlatformEvent::DeepLink(_) => "deep-link",
        }
    }
}
