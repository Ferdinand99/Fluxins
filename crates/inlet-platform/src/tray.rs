//! The tray icon (Windows notification area, macOS menu bar, Linux
//! StatusNotifierItem).
//!
//! gpui has no tray. Windows and macOS use the `tray-icon` crate; Linux has its
//! own small StatusNotifierItem with a dbusmenu over zbus (see `sni`), so neither
//! GTK nor libappindicator is needed.
//!
//! Create the tray on the UI thread. On Windows the tray's hidden window gets its
//! messages from that thread's message loop (gpui's main loop pumps them); on
//! macOS AppKit only allows it on the main thread.
//!
//! With gpui on Windows, shell calls (tray, taskbar) and toasts may dispatch
//! window messages while they run, and gpui's window procedure cannot borrow the
//! app at that moment: it logs "RefCell already borrowed" and skips a frame. The
//! demo therefore keeps them out of the `Application::run` callback and makes the
//! tray and badge calls in a `cx.spawn` task between awaits.

use crate::{Icon, Result};

#[cfg(target_os = "linux")]
mod sni;

/// An entry in the tray menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrayItem {
    /// A clickable entry; choosing it sends [`crate::PlatformEvent::TrayMenu`] with `id`.
    Action {
        id: String,
        label: String,
        enabled: bool,
    },
    /// A separator line.
    Separator,
}

impl TrayItem {
    pub fn action(id: impl Into<String>, label: impl Into<String>) -> Self {
        TrayItem::Action {
            id: id.into(),
            label: label.into(),
            enabled: true,
        }
    }

    pub fn separator() -> Self {
        TrayItem::Separator
    }
}

/// What the tray icon shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TrayState {
    #[default]
    Normal,
    /// Unread messages: a grey dot.
    Unread,
    /// Mentions: a red dot.
    Mention,
}

/// Grey for unread messages without mentions.
// Deviation: Fluxer draws one red dot for any unread badge; Inlet tells unread and
// mentions apart, as TrayState asks.
const UNREAD_GREY: [u8; 3] = [0xb5, 0xba, 0xc1];

pub(crate) fn icon_for(base: &Icon, state: TrayState) -> Icon {
    match state {
        TrayState::Normal => base.clone(),
        TrayState::Unread => base.with_dot(UNREAD_GREY),
        TrayState::Mention => base.with_dot(crate::badge::BADGE_RED),
    }
}

/// A tray icon with a menu. Removed from the tray when dropped.
pub struct Tray {
    base: Icon,
    inner: imp::Tray,
}

impl Tray {
    /// Shows the icon in the tray. On macOS pass a monochrome icon: it is used as a
    /// template image, as Fluxer does, so the menu bar can tint it. On Linux this
    /// fails with [`crate::Error::Unsupported`] when the desktop has no tray host
    /// (GNOME without the AppIndicator extension); the app should then not hide its
    /// window to the tray.
    pub fn new(icon: Icon, menu: Vec<TrayItem>) -> Result<Self> {
        let inner = imp::Tray::new(&icon, &menu)?;
        Ok(Self { base: icon, inner })
    }

    /// Switches the icon between normal, unread and mention.
    pub fn set_state(&self, state: TrayState) {
        if let Err(err) = self.inner.set_icon(&icon_for(&self.base, state)) {
            log::warn!("could not update the tray icon: {err}");
        }
    }

    /// Sets the hover text, for example "Inlet – 3 mentions".
    pub fn set_tooltip(&self, text: &str) {
        let text = crate::clean_text(text, 64);
        if let Err(err) = self.inner.set_tooltip(&text) {
            log::warn!("could not update the tray tooltip: {err}");
        }
    }
}

#[cfg(any(windows, target_os = "macos"))]
mod imp {
    use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
    use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

    use super::TrayItem;
    use crate::events::emit;
    use crate::{Error, Icon, PlatformEvent, Result};

    fn os<E: std::fmt::Display>(err: E) -> Error {
        Error::Os(err.to_string())
    }

    fn to_tray_icon(icon: &Icon) -> Result<tray_icon::Icon> {
        tray_icon::Icon::from_rgba(icon.rgba().to_vec(), icon.width(), icon.height()).map_err(os)
    }

    pub(super) struct Tray {
        icon: TrayIcon,
    }

    impl Tray {
        pub(super) fn new(icon: &Icon, items: &[TrayItem]) -> Result<Self> {
            let menu = Menu::new();
            for item in items {
                match item {
                    TrayItem::Action { id, label, enabled } => {
                        let entry = MenuItem::with_id(MenuId::new(id), label, *enabled, None);
                        menu.append(&entry).map_err(os)?;
                    }
                    TrayItem::Separator => {
                        menu.append(&PredefinedMenuItem::separator()).map_err(os)?
                    }
                }
            }
            TrayIconEvent::set_event_handler(Some(|event: TrayIconEvent| {
                if let TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } = event
                {
                    emit(PlatformEvent::TrayClicked);
                }
            }));
            MenuEvent::set_event_handler(Some(|event: MenuEvent| {
                emit(PlatformEvent::TrayMenu(event.id.0));
            }));
            let builder = TrayIconBuilder::new()
                .with_tooltip(crate::APP_NAME)
                .with_menu(Box::new(menu));
            // As in Fluxer: a left click toggles the window on Windows and opens
            // the menu on macOS, where the icon is a template image.
            #[cfg(windows)]
            let builder = builder
                .with_icon(to_tray_icon(icon)?)
                .with_menu_on_left_click(false);
            #[cfg(target_os = "macos")]
            let builder = builder.with_icon_templated(to_tray_icon(icon)?);
            let icon = builder.build().map_err(os)?;
            Ok(Self { icon })
        }

        pub(super) fn set_icon(&self, icon: &Icon) -> Result<()> {
            #[cfg(windows)]
            self.icon.set_icon(Some(to_tray_icon(icon)?)).map_err(os)?;
            #[cfg(target_os = "macos")]
            self.icon
                .set_icon_templated(Some(to_tray_icon(icon)?))
                .map_err(os)?;
            Ok(())
        }

        pub(super) fn set_tooltip(&self, text: &str) -> Result<()> {
            self.icon.set_tooltip(Some(text)).map_err(os)
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    pub(super) use super::sni::Tray;
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
mod imp {
    use super::TrayItem;
    use crate::{Error, Icon, Result};

    pub(super) struct Tray;

    impl Tray {
        pub(super) fn new(_icon: &Icon, _items: &[TrayItem]) -> Result<Self> {
            Err(Error::Unsupported("no tray on this OS"))
        }

        pub(super) fn set_icon(&self, _icon: &Icon) -> Result<()> {
            Ok(())
        }

        pub(super) fn set_tooltip(&self, _text: &str) -> Result<()> {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_draw_different_icons() {
        let base = Icon::from_rgba([40, 40, 40, 255].repeat(32 * 32), 32, 32).unwrap();
        let normal = icon_for(&base, TrayState::Normal);
        let unread = icon_for(&base, TrayState::Unread);
        let mention = icon_for(&base, TrayState::Mention);
        assert_eq!(normal, base);
        assert_ne!(unread, base);
        assert_ne!(mention, unread);
    }

    /// Creates a real tray icon. Windows only: macOS allows it only on the main
    /// thread, which the test harness does not use, so macOS is covered by the
    /// tray_demo example in CI.
    #[cfg(windows)]
    #[test]
    #[ignore = "puts an icon in the real notification area; run in CI with --include-ignored"]
    fn windows_tray_icon_can_be_created_and_updated() {
        let base = Icon::from_rgba([40, 90, 200, 255].repeat(32 * 32), 32, 32).unwrap();
        let tray = Tray::new(
            base,
            vec![TrayItem::action("open", "Open"), TrayItem::separator()],
        )
        .unwrap();
        tray.set_state(TrayState::Mention);
        tray.set_tooltip("Inlet \u{2013} 2 mentions");
        drop(tray);
    }
}
