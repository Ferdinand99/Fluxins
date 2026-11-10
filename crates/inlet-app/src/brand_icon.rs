// SPDX-License-Identifier: AGPL-3.0-or-later
//! The app icon at runtime.
//!
//! - Windows: `build.rs` embeds `assets/brand/windows/inlet.ico` as icon resource 1,
//!   which gpui loads for every window. Nothing to do here.
//! - Linux and FreeBSD on X11: gpui takes the window icon from `WindowOptions::icon`,
//!   so the 128 px PNG is built in and decoded here. Wayland ignores it: compositors
//!   take the icon from the `.desktop` file that matches the app id
//!   (`no.opland.inlet`), which comes with packaging.
//! - macOS: the icon comes from the app bundle (`assets/brand/macos/inlet.icns`),
//!   which also comes with packaging. A bare binary shows the generic icon.

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
const WINDOW_ICON_PNG: &[u8] =
    include_bytes!("../../../assets/brand/linux/hicolor/128x128/apps/no.opland.inlet.png");

/// The window icon for X11, or `None` if the built-in PNG cannot be decoded.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub fn window_icon() -> Option<std::sync::Arc<image::RgbaImage>> {
    match image::load_from_memory_with_format(WINDOW_ICON_PNG, image::ImageFormat::Png) {
        Ok(img) => Some(std::sync::Arc::new(img.to_rgba8())),
        Err(err) => {
            log::warn!("window icon: {err}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    /// Sizes (width, height) of the frames in an .ico file, from its directory.
    fn ico_sizes(bytes: &[u8]) -> Vec<(u32, u32)> {
        let le16 = |i: usize| u16::from_le_bytes([bytes[i], bytes[i + 1]]);
        assert_eq!(le16(0), 0, "reserved field");
        assert_eq!(le16(2), 1, "type 1 = icon");
        let side = |b: u8| if b == 0 { 256 } else { u32::from(b) };
        (0..usize::from(le16(4)))
            .map(|n| {
                let e = 6 + 16 * n;
                (side(bytes[e]), side(bytes[e + 1]))
            })
            .collect()
    }

    #[test]
    fn windows_icon_has_every_size() {
        let ico = include_bytes!("../../../assets/brand/windows/inlet.ico");
        let sizes: Vec<u32> = ico_sizes(ico)
            .into_iter()
            .map(|(w, h)| {
                assert_eq!(w, h, "square frames");
                w
            })
            .collect();
        assert_eq!(sizes, [16, 20, 24, 32, 40, 48, 64, 128, 256]);
    }

    #[test]
    fn favicon_has_small_sizes() {
        let ico = include_bytes!("../../../assets/brand/favicon/favicon.ico");
        assert_eq!(ico_sizes(ico), [(16, 16), (32, 32), (48, 48)]);
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    #[test]
    fn window_icon_decodes() {
        let icon = super::window_icon().expect("built-in icon decodes");
        assert_eq!(icon.dimensions(), (128, 128));
        // Opaque in the middle, transparent in the rounded corner.
        assert_eq!(icon.get_pixel(64, 64).0[3], 255);
        assert_eq!(icon.get_pixel(0, 0).0[3], 0);
    }
}
