//! Desktop integration for Inlet: tray icon, notifications, taskbar and dock badge,
//! window flashing, autostart, single instance, `inlet://` deep links, idle time and
//! the reduce-motion setting.
//!
//! One module per feature, one backend per operating system. What gpui already
//! provides is reused rather than rebuilt where it works: notifications on macOS
//! and Linux, URLs handed over by macOS, and asking for the user's attention go
//! through gpui (see the `gpui` feature and `gpui_glue`). Windows toasts come from
//! this crate's own thread, because gpui's toast call makes gpui drop frames.
//!
//! Tray and badge calls talk to the shell and must not run while gpui's app is
//! borrowed; with gpui, use the wrappers in `gpui_glue`, which run them in a task.
//!
//! Events from the OS (tray clicks, menu choices, notification clicks, deep links)
//! arrive on one queue, [`events`].

mod args;
pub mod autostart;
mod badge;
pub mod deep_link;
mod error;
mod events;
mod icon;
mod idle;
pub mod jump_list;
#[cfg(target_os = "linux")]
mod linux_bus;
mod motion;
mod notify;
mod single_instance;
#[cfg(windows)]
mod toast;
mod tray;
#[cfg(target_os = "linux")]
mod xdg;

#[cfg(feature = "gpui")]
pub mod gpui_glue;

pub use args::{flag_value, has_flag, split_args};
pub use badge::Badge;
pub use deep_link::DeepLink;
pub use error::{Error, Result};
pub use events::{events, PlatformEvent};
pub use icon::Icon;
pub use idle::idle_time;
pub use motion::reduce_motion;
pub use notify::{
    clean_text, dismiss, escape_markup, notify, Notification, Preview, BODY_MAX_GRAPHEMES,
    TITLE_MAX_GRAPHEMES,
};
pub use single_instance::{single_instance, Instance, SingleInstance};
pub use tray::{Tray, TrayItem, TrayState};

/// Application id: the Wayland app id, the Linux desktop entry name, the Windows
/// AppUserModelID and the macOS LaunchAgent label.
pub const APP_ID: &str = "no.opland.inlet";

/// The name shown to the user by the operating system.
pub const APP_NAME: &str = "Inlet";
