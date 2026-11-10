//! Desktop integration for Inlet: tray icon, notifications, taskbar and dock badge,
//! window flashing, autostart, single instance, `inlet://` deep links and idle time.
//!
//! One module per feature, one backend per operating system. What gpui already
//! provides is reused rather than rebuilt: system notifications and their click
//! responses, URLs handed over by macOS, and asking for the user's attention all go
//! through gpui (see the `gpui` feature and [`gpui_glue`]).
//!
//! Events from the OS (tray clicks, menu choices, notification clicks, deep links)
//! arrive on one queue, [`events`].

pub mod autostart;
mod badge;
pub mod deep_link;
mod error;
mod events;
mod icon;
mod idle;
#[cfg(target_os = "linux")]
mod linux_bus;
mod notify;
mod single_instance;
mod tray;
#[cfg(target_os = "linux")]
mod xdg;

#[cfg(feature = "gpui")]
pub mod gpui_glue;

pub use badge::{render_overlay, set_badge, Badge};
pub use deep_link::DeepLink;
pub use error::{Error, Result};
pub use events::{events, PlatformEvent};
pub use icon::Icon;
pub use idle::idle_time;
pub use notify::{clean_text, Notification, Preview, BODY_MAX_GRAPHEMES, TITLE_MAX_GRAPHEMES};
pub use single_instance::{single_instance, Instance, SingleInstance};
pub use tray::{Tray, TrayItem, TrayState};

/// Application id: the Wayland app id, the Linux desktop entry name, the Windows
/// AppUserModelID and the macOS LaunchAgent label.
pub const APP_ID: &str = "no.opland.inlet";

/// The name shown to the user by the operating system.
pub const APP_NAME: &str = "Inlet";
