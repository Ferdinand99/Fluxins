//! The unread badge on the dock icon (macOS), the taskbar button (Windows, as an
//! overlay icon) and the launcher (Linux, Unity LauncherEntry API, which KDE Plasma
//! and the GNOME docks read).

use crate::Icon;

/// What the badge shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Badge {
    /// No badge.
    None,
    /// Unread messages without a count (Fluxer's `UNREAD_BADGE_DOT`).
    Dot,
    /// A count, normally of mentions.
    Count(u32),
}

impl From<Option<u32>> for Badge {
    fn from(count: Option<u32>) -> Self {
        match count {
            None | Some(0) => Badge::None,
            Some(n) => Badge::Count(n),
        }
    }
}

impl Badge {
    /// The text on the macOS dock tile: the count, or a bullet for the dot.
    pub fn dock_label(self) -> Option<String> {
        match self {
            Badge::None => None,
            Badge::Dot => Some("\u{2022}".to_string()),
            Badge::Count(n) => Some(n.to_string()),
        }
    }

    /// The text in the Windows overlay icon: 1 to 9, then "9+", as Fluxer's frames.
    pub fn overlay_label(self) -> Option<String> {
        match self {
            Badge::None => None,
            Badge::Dot => Some(String::new()),
            Badge::Count(n) if n > 9 => Some("9+".to_string()),
            Badge::Count(n) => Some(n.to_string()),
        }
    }
}

/// Fluxer's indicator red.
pub(crate) const BADGE_RED: [u8; 3] = [0xdd, 0x00, 0x00];

/// 5x7 glyphs for the overlay: digits and "+". One row per byte, high bit left.
fn glyph(c: char) -> Option<[u8; 7]> {
    Some(match c {
        '0' => [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E],
        '1' => [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E],
        '2' => [0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F],
        '3' => [0x1F, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0E],
        '4' => [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02],
        '5' => [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E],
        '6' => [0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E],
        '7' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
        '8' => [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E],
        '9' => [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C],
        '+' => [0x00, 0x04, 0x04, 0x1F, 0x04, 0x04, 0x00],
        _ => return None,
    })
}

/// Draws the Windows overlay: a red disc with the label in white, `side` pixels
/// square. Fluxer ships these as pre-drawn icon files; Inlet draws them.
pub fn render_overlay(badge: Badge, side: u32) -> Option<Icon> {
    let label = badge.overlay_label()?;
    let s = side.clamp(8, 256) as usize;
    let mut rgba = vec![0u8; s * s * 4];
    let r = s as f32 / 2.0;
    for y in 0..s {
        for x in 0..s {
            let cov = crate::icon::coverage(x, y, r, r, r);
            let i = (y * s + x) * 4;
            rgba[i..i + 3].copy_from_slice(&BADGE_RED);
            rgba[i + 3] = (cov * 255.0).round() as u8;
        }
    }
    // Scale the 5x7 glyphs (with one column between them) to fit 60 % of the side.
    let glyphs: Vec<[u8; 7]> = label.chars().filter_map(glyph).collect();
    if !glyphs.is_empty() {
        let text_w = glyphs.len() * 6 - 1;
        let scale = ((s * 6 / 10) / text_w.max(7)).max(1);
        let (w, h) = (text_w * scale, 7 * scale);
        let (x0, y0) = ((s - w.min(s)) / 2, (s - h.min(s)) / 2);
        for (n, rows) in glyphs.iter().enumerate() {
            for (row, bits) in rows.iter().enumerate() {
                for col in 0..5 {
                    if bits & (0x10 >> col) == 0 {
                        continue;
                    }
                    for dy in 0..scale {
                        for dx in 0..scale {
                            let x = x0 + (n * 6 + col) * scale + dx;
                            let y = y0 + row * scale + dy;
                            if x < s && y < s {
                                let i = (y * s + x) * 4;
                                rgba[i..i + 4].copy_from_slice(&[255, 255, 255, 255]);
                            }
                        }
                    }
                }
            }
        }
    }
    Icon::from_rgba(rgba, s as u32, s as u32).ok()
}

/// Sets the badge. `set_badge(Some(3))`, `set_badge(None)` and
/// `set_badge(Badge::Dot)` all work. Call it on the UI thread: macOS only allows
/// the dock tile there, and on Windows the taskbar list is a COM object of that
/// thread. Failures are logged, not returned: a badge is never worth an error
/// dialog.
pub fn set_badge(badge: impl Into<Badge>) {
    imp::set(badge.into());
}

#[cfg(target_os = "macos")]
mod imp {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSApplication;
    use objc2_foundation::NSString;

    use super::Badge;

    pub(super) fn set(badge: Badge) {
        let Some(mtm) = MainThreadMarker::new() else {
            log::warn!("set_badge called off the main thread; ignored");
            return;
        };
        let tile = NSApplication::sharedApplication(mtm).dockTile();
        let label = badge.dock_label().map(|l| NSString::from_str(&l));
        tile.setBadgeLabel(label.as_deref());
    }
}

#[cfg(windows)]
mod imp {
    use windows::core::{BOOL, HSTRING, PCWSTR};
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows::Win32::Graphics::Gdi::{
        CreateBitmap, CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
        DIB_RGB_COLORS, HGDIOBJ,
    };
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
    use windows::Win32::System::Threading::GetCurrentProcessId;
    use windows::Win32::UI::Shell::{ITaskbarList3, TaskbarList};
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateIconIndirect, DestroyIcon, EnumWindows, GetWindow, GetWindowThreadProcessId,
        IsWindowVisible, GW_OWNER, HICON, ICONINFO,
    };

    use super::{render_overlay, Badge};
    use crate::{Icon, Result};

    pub(super) fn set(badge: Badge) {
        if let Err(err) = try_set(badge) {
            log::warn!("could not set the taskbar badge: {err}");
        }
    }

    fn try_set(badge: Badge) -> Result<()> {
        let windows = own_top_level_windows();
        if windows.is_empty() {
            return Ok(());
        }
        // SAFETY: plain COM creation; gpui has initialised COM on the UI thread.
        let taskbar: ITaskbarList3 =
            unsafe { CoCreateInstance(&TaskbarList, None, CLSCTX_INPROC_SERVER)? };
        unsafe { taskbar.HrInit()? };
        let icon = match render_overlay(badge, 32) {
            Some(icon) => Some(make_hicon(&icon)?),
            None => None,
        };
        let description = HSTRING::from(badge.overlay_label().unwrap_or_default());
        for hwnd in windows {
            // SAFETY: the window belongs to this process; the taskbar copies the icon.
            unsafe {
                taskbar.SetOverlayIcon(
                    hwnd,
                    icon.unwrap_or_default(),
                    PCWSTR(description.as_ptr()),
                )?;
            }
        }
        if let Some(icon) = icon {
            // SAFETY: created above and no longer used.
            unsafe { DestroyIcon(icon)? };
        }
        Ok(())
    }

    /// Visible, unowned top-level windows of this process: the app's main windows,
    /// not the hidden window of the tray icon.
    fn own_top_level_windows() -> Vec<HWND> {
        unsafe extern "system" fn collect(hwnd: HWND, list: LPARAM) -> BOOL {
            // SAFETY: `list` is the Vec passed to EnumWindows below, alive for the call.
            let list = unsafe { &mut *(list.0 as *mut Vec<HWND>) };
            let mut pid = 0u32;
            unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid as *mut u32)) };
            let owned = unsafe { GetWindow(hwnd, GW_OWNER) }.is_ok_and(|o| !o.is_invalid());
            if pid == unsafe { GetCurrentProcessId() }
                && unsafe { IsWindowVisible(hwnd) }.as_bool()
                && !owned
            {
                list.push(hwnd);
            }
            true.into()
        }
        let mut list: Vec<HWND> = Vec::new();
        // SAFETY: the callback only touches `list`, which outlives the call.
        let _ = unsafe { EnumWindows(Some(collect), LPARAM(&mut list as *mut Vec<HWND> as isize)) };
        list
    }

    pub(crate) fn make_hicon(icon: &Icon) -> Result<HICON> {
        let (w, h) = (icon.width() as i32, icon.height() as i32);
        let header = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // top-down rows
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        };
        let info = BITMAPINFO {
            bmiHeader: header,
            ..Default::default()
        };
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        // SAFETY: standard 32-bit DIB creation; `bits` receives the pixel buffer.
        let color = unsafe { CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0)? };
        let pixels = icon.to_bgra_premultiplied();
        // SAFETY: the DIB holds exactly w*h*4 bytes, the same as `pixels`.
        unsafe { std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits.cast::<u8>(), pixels.len()) };
        let mask_bytes = vec![0u8; ((w + 15) / 16 * 2 * h) as usize];
        // SAFETY: a monochrome mask of the same size; all zero, alpha decides.
        let mask = unsafe { CreateBitmap(w, h, 1, 1, Some(mask_bytes.as_ptr().cast())) };
        let icon_info = ICONINFO {
            fIcon: true.into(),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color,
        };
        // SAFETY: both bitmaps are valid; the icon copies them, so they are freed after.
        let hicon = unsafe { CreateIconIndirect(&icon_info) };
        unsafe {
            let _ = DeleteObject(HGDIOBJ(color.0));
            let _ = DeleteObject(HGDIOBJ(mask.0));
        }
        Ok(hicon?)
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::collections::HashMap;

    use zbus::zvariant::Value;

    use super::Badge;

    pub(super) fn set(badge: Badge) {
        let Some(bus) = crate::linux_bus::session() else {
            return;
        };
        let (count, visible) = match badge {
            Badge::None | Badge::Dot => (0i64, false),
            Badge::Count(n) => (i64::from(n), true),
        };
        let app_uri = format!("application://{}.desktop", crate::APP_ID);
        let mut props: HashMap<&str, Value<'_>> = HashMap::new();
        props.insert("count", Value::from(count));
        props.insert("count-visible", Value::from(visible));
        if let Err(err) = bus.emit_signal(
            None::<&str>,
            "/no/opland/inlet/launcher",
            "com.canonical.Unity.LauncherEntry",
            "Update",
            &(app_uri, props),
        ) {
            log::info!("could not send the launcher badge: {err}");
        }
    }
}

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
mod imp {
    pub(super) fn set(_badge: super::Badge) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn badge_from_count() {
        assert_eq!(Badge::from(None), Badge::None);
        assert_eq!(Badge::from(Some(0)), Badge::None);
        assert_eq!(Badge::from(Some(4)), Badge::Count(4));
    }

    #[test]
    fn labels() {
        assert_eq!(Badge::None.dock_label(), None);
        assert_eq!(Badge::Dot.dock_label().as_deref(), Some("\u{2022}"));
        assert_eq!(Badge::Count(120).dock_label().as_deref(), Some("120"));
        assert_eq!(Badge::Count(9).overlay_label().as_deref(), Some("9"));
        assert_eq!(Badge::Count(10).overlay_label().as_deref(), Some("9+"));
        assert_eq!(Badge::Dot.overlay_label().as_deref(), Some(""));
    }

    #[test]
    fn overlay_is_a_red_disc_with_white_text() {
        assert!(render_overlay(Badge::None, 32).is_none());
        let icon = render_overlay(Badge::Count(3), 32).unwrap();
        assert_eq!((icon.width(), icon.height()), (32, 32));
        let px = |x: usize, y: usize| -> [u8; 4] {
            let i = (y * 32 + x) * 4;
            icon.rgba()[i..i + 4].try_into().unwrap()
        };
        // Corners are outside the disc.
        assert_eq!(px(0, 0)[3], 0);
        assert_eq!(px(31, 31)[3], 0);
        // Some white pixels (the digit) and some red ones (the disc).
        let white = icon
            .rgba()
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| **p == [255, 255, 255, 255])
            .count();
        let red = icon
            .rgba()
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| **p == [0xdd, 0, 0, 255])
            .count();
        assert!(white > 20, "{white}");
        assert!(red > 300, "{red}");
        // The dot has no text.
        let dot = render_overlay(Badge::Dot, 32).unwrap();
        assert!(!dot
            .rgba()
            .as_chunks::<4>()
            .0
            .contains(&[255, 255, 255, 255]));
    }
}
