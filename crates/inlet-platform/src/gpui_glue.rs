//! Glue between this crate and a gpui application.
//!
//! gpui already has notifications with click responses on macOS and Linux,
//! receives the URLs macOS hands an app bundle, and can ask for the user's
//! attention. These are used as they are. Windows toasts come from this crate's
//! own thread instead (see [`notify`]).
//!
//! Tray and badge calls talk to the shell, which on Windows can dispatch window
//! messages during the call; if gpui's app is borrowed at that moment (inside
//! `cx.update`, a callback or a render), gpui drops a frame. The functions here run
//! those calls in a `cx.spawn` task, outside any borrow, so they are safe to call
//! from anywhere in the app. The raw calls are not public.

use std::rc::Rc;

use gpui::{App, Application, Task, Window};
#[cfg(not(windows))]
use gpui::{SharedString, SystemNotification};

use crate::events::emit;
use crate::{
    deep_link, Badge, Icon, Notification, PlatformEvent, Result, Tray, TrayItem, TrayState,
};

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

/// Call once at start-up, inside `Application::run`. Sets the app identity (the
/// Windows AppUserModelID) and routes notification clicks on macOS and Linux to
/// [`PlatformEvent::NotificationClicked`]. Action buttons are not used: a click
/// only opens the conversation.
pub fn install(cx: &mut App) {
    cx.set_app_identity(crate::APP_ID, crate::APP_NAME);
    cx.on_system_notification_response(|response, _cx| {
        if response.action_id.is_none() {
            emit(PlatformEvent::NotificationClicked(response.tag.to_string()));
        }
    });
    #[cfg(target_os = "linux")]
    crate::linux_bus::query_body_markup();
}

/// Shows a notification.
///
/// - Windows: [`crate::notify`], from this crate's notification thread; `cx` is
///   not touched, so gpui never drops a frame for it.
/// - macOS: gpui, only inside an app bundle (gpui logs and skips it otherwise).
/// - Linux: gpui; the body is escaped first when the notification server reads
///   markup, so a message can never become a link or formatting.
pub fn notify(cx: &App, notification: &Notification) -> Result<()> {
    #[cfg(windows)]
    {
        let _ = cx;
        crate::notify(notification)
    }
    #[cfg(not(windows))]
    {
        cx.show_system_notification(SystemNotification {
            tag: SharedString::from(notification.tag().to_string()),
            title: SharedString::from(notification.title().to_string()),
            body: SharedString::from(body_for_server(notification)),
            actions: Vec::new(),
        });
        Ok(())
    }
}

/// The body as the notification server should get it.
#[cfg(not(windows))]
fn body_for_server(notification: &Notification) -> String {
    #[cfg(target_os = "linux")]
    if crate::linux_bus::body_markup() {
        return crate::escape_markup(notification.body());
    }
    notification.body().to_string()
}

/// Removes a notification, for example when the channel has been read elsewhere.
pub fn dismiss(cx: &App, tag: &str) -> Result<()> {
    #[cfg(windows)]
    {
        let _ = cx;
        crate::dismiss(tag)
    }
    #[cfg(not(windows))]
    {
        cx.dismiss_system_notification(tag);
        Ok(())
    }
}

/// Sets the dock, taskbar or launcher badge, from a task outside any borrow.
/// `set_badge(cx, Some(3))`, `set_badge(cx, None)` and `set_badge(cx, Badge::Dot)`
/// all work.
pub fn set_badge(cx: &App, badge: impl Into<Badge>) {
    let badge = badge.into();
    cx.spawn(async move |_| crate::badge::set_badge(badge))
        .detach();
}

/// Creates the tray icon from a task outside any borrow. Await the task (from
/// another task) to get the tray; keep the `Rc` for as long as the icon should
/// stay. On Linux it fails with [`crate::Error::Unsupported`] when the desktop has
/// no tray host; the app should then not hide its window to the tray.
pub fn create_tray(cx: &App, icon: Icon, menu: Vec<TrayItem>) -> Task<Result<Rc<Tray>>> {
    cx.spawn(async move |_| Tray::new(icon, menu).map(Rc::new))
}

/// Switches the tray icon between normal, unread and mention, from a task.
pub fn set_tray_state(cx: &App, tray: &Rc<Tray>, state: TrayState) {
    let tray = tray.clone();
    cx.spawn(async move |_| tray.set_state(state)).detach();
}

/// Sets the tray hover text, from a task.
pub fn set_tray_tooltip(cx: &App, tray: &Rc<Tray>, text: impl Into<String>) {
    let (tray, text) = (tray.clone(), text.into());
    cx.spawn(async move |_| tray.set_tooltip(&text)).detach();
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
