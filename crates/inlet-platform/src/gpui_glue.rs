//! Glue between this crate and a gpui application.
//!
//! gpui already has system notifications with click responses on Windows, macOS
//! and Linux, receives the URLs macOS hands an app bundle, and can ask for the
//! user's attention. These are used as they are; this module only connects them to
//! [`crate::events`] and applies the checks of this crate.

use gpui::{App, Application, SharedString, SystemNotification, Window};

use crate::events::emit;
use crate::{deep_link, Notification, PlatformEvent};

/// Call once, before the app runs. Links that macOS hands to the bundle
/// (`CFBundleURLTypes`) arrive as [`PlatformEvent::DeepLink`]; anything that does not
/// parse is dropped. On Windows and Linux links come through the command line and
/// [`crate::single_instance`] instead.
pub fn install_on_application(app: &Application) {
    app.on_open_urls(|urls| {
        for url in urls.iter().take(8) {
            match deep_link::parse(url) {
                Some(link) => emit(PlatformEvent::DeepLink(link)),
                None => log::warn!("ignored a link the OS opened with Inlet"),
            }
        }
    });
}

/// Call once at start-up, inside `Application::run`. Sets the app identity
/// (Windows toasts need the AppUserModelID) and routes notification clicks to
/// [`PlatformEvent::NotificationClicked`]. Action buttons are not used: a click
/// only opens the conversation.
pub fn install(cx: &mut App) {
    cx.set_app_identity(crate::APP_ID, crate::APP_NAME);
    cx.on_system_notification_response(|response, _cx| {
        if response.action_id.is_none() {
            emit(PlatformEvent::NotificationClicked(response.tag.to_string()));
        }
    });
}

/// Shows a notification. On macOS this only works inside an app bundle; gpui
/// logs and skips it otherwise.
pub fn notify(cx: &App, notification: &Notification) {
    cx.show_system_notification(SystemNotification {
        tag: SharedString::from(notification.tag().to_string()),
        title: SharedString::from(notification.title().to_string()),
        body: SharedString::from(notification.body().to_string()),
        actions: Vec::new(),
    });
}

/// Removes a notification, for example when the channel has been read elsewhere.
pub fn dismiss(cx: &App, tag: &str) {
    cx.dismiss_system_notification(tag);
}

/// Draws attention to the window until it is focused, as Fluxer's `flashFrame`
/// does: on Windows the taskbar button flashes until the window comes to the
/// front; on macOS the dock icon bounces once; on X11 the window manager is told
/// it demands attention. Wayland has no equivalent yet (gpui does nothing there).
pub fn flash_window(window: &Window) {
    if window.is_window_active() {
        return;
    }
    #[cfg(windows)]
    if flash_windows(window) {
        return;
    }
    window.request_attention();
}

#[cfg(windows)]
fn flash_windows(window: &Window) -> bool {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        FlashWindowEx, FLASHWINFO, FLASHW_TIMERNOFG, FLASHW_TRAY,
    };

    // gpui's own `Window::window_handle` returns its handle type; this is the trait's.
    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return false;
    };
    let RawWindowHandle::Win32(win32) = handle.as_raw() else {
        return false;
    };
    let info = FLASHWINFO {
        cbSize: std::mem::size_of::<FLASHWINFO>() as u32,
        hwnd: HWND(win32.hwnd.get() as *mut std::ffi::c_void),
        dwFlags: FLASHW_TRAY | FLASHW_TIMERNOFG,
        uCount: 0,
        dwTimeout: 0,
    };
    // SAFETY: the handle is a live window of this process.
    let _ = unsafe { FlashWindowEx(&info) };
    true
}
