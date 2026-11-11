//! Whether the system asks apps to reduce motion (animations off).
//!
//! - Windows: `SPI_GETCLIENTAREAANIMATION` ("Show animations in Windows" off).
//! - macOS: `NSWorkspace.accessibilityDisplayShouldReduceMotion`.
//! - Linux: the desktop portal's `org.gnome.desktop.interface` `enable-animations`
//!   setting (GNOME, and desktops that mirror it); `None` where the portal does not
//!   have it.

/// `Some(true)` when the system asks for reduced motion, `None` if this system cannot
/// tell. May take a few milliseconds on Linux (a D-Bus call): ask off the UI thread.
pub fn reduce_motion() -> Option<bool> {
    imp::reduce_motion()
}

#[cfg(windows)]
mod imp {
    use windows::core::BOOL;
    use windows::Win32::UI::WindowsAndMessaging::{
        SystemParametersInfoW, SPI_GETCLIENTAREAANIMATION, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    };

    pub(super) fn reduce_motion() -> Option<bool> {
        let mut on = BOOL(1);
        // SAFETY: SPI_GETCLIENTAREAANIMATION writes one BOOL to the pointer given.
        unsafe {
            SystemParametersInfoW(
                SPI_GETCLIENTAREAANIMATION,
                0,
                Some(std::ptr::from_mut(&mut on).cast()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
        }
        .ok()?;
        Some(!on.as_bool())
    }
}

#[cfg(target_os = "macos")]
mod imp {
    pub(super) fn reduce_motion() -> Option<bool> {
        let workspace = objc2_app_kit::NSWorkspace::sharedWorkspace();
        Some(workspace.accessibilityDisplayShouldReduceMotion())
    }
}

#[cfg(target_os = "linux")]
mod imp {
    pub(super) fn reduce_motion() -> Option<bool> {
        let bus = crate::linux_bus::session()?;
        let reply = bus
            .call_method(
                Some("org.freedesktop.portal.Desktop"),
                "/org/freedesktop/portal/desktop",
                Some("org.freedesktop.portal.Settings"),
                "ReadOne",
                &("org.gnome.desktop.interface", "enable-animations"),
            )
            .ok()?;
        let value: zbus::zvariant::OwnedValue = reply.body().deserialize().ok()?;
        let on = bool::try_from(value).ok()?;
        Some(!on)
    }
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
mod imp {
    pub(super) fn reduce_motion() -> Option<bool> {
        None
    }
}
