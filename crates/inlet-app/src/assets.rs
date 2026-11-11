// SPDX-License-Identifier: AGPL-3.0-or-later
//! Icons and fonts built into the binary.
//!
//! Icons are Phosphor Icons (MIT, `assets/icons/LICENSE-PHOSPHOR.txt`), mostly the
//! `fill` weight. Fonts are IBM Plex Sans and IBM Plex Mono (SIL Open Font License 1.1,
//! `assets/fonts/LICENSE-IBM-PLEX.txt`), used on macOS and Linux. Windows uses the
//! system font (Segoe UI Variable Text, then Segoe UI), as Fluxer does there.

use gpui::{prelude::*, svg, App, Hsla, Pixels, Svg};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    /// Text channel (`hash`, bold).
    Hash,
    /// Voice channel (`speaker-high`).
    Speaker,
    /// Announcement channel (`megaphone`).
    Megaphone,
    /// Forum and other channels (`chats-circle`).
    Chats,
    /// Direct message (`at`, bold).
    At,
    /// Direct messages home (`chat-circle`).
    ChatCircle,
    CaretDown,
    CaretRight,
    SignOut,
    Info,
    /// Notification settings (`bell`).
    Bell,
    /// Muted (`bell-slash`).
    BellSlash,
    /// A ticked box (`check-square`).
    CheckSquare,
    /// Settings (`gear`).
    Gear,
}

impl Icon {
    fn data(self) -> &'static [u8] {
        match self {
            Icon::Hash => include_bytes!("../assets/icons/hash.svg"),
            Icon::Speaker => include_bytes!("../assets/icons/speaker-high.svg"),
            Icon::Megaphone => include_bytes!("../assets/icons/megaphone.svg"),
            Icon::Chats => include_bytes!("../assets/icons/chats-circle.svg"),
            Icon::At => include_bytes!("../assets/icons/at.svg"),
            Icon::ChatCircle => include_bytes!("../assets/icons/chat-circle.svg"),
            Icon::CaretDown => include_bytes!("../assets/icons/caret-down.svg"),
            Icon::CaretRight => include_bytes!("../assets/icons/caret-right.svg"),
            Icon::SignOut => include_bytes!("../assets/icons/sign-out.svg"),
            Icon::Info => include_bytes!("../assets/icons/info.svg"),
            Icon::Bell => include_bytes!("../assets/icons/bell.svg"),
            Icon::BellSlash => include_bytes!("../assets/icons/bell-slash.svg"),
            Icon::CheckSquare => include_bytes!("../assets/icons/check-square.svg"),
            Icon::Gear => include_bytes!("../assets/icons/gear.svg"),
        }
    }

    /// The icon as an element, `size` square, drawn in `color`.
    pub fn el(self, size: Pixels, color: Hsla) -> Svg {
        svg()
            .data(self.data())
            .size(size)
            .flex_none()
            .text_color(color)
    }
}

// The four shapes of Fluxer's `StatusIndicator` (a disc, a moon, a disc with a bar, a
// ring), drawn as masks in our own SVG; the caller fills them with the status colour.
const ONLINE_SVG: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" width="12" height="12" viewBox="0 0 12 12"><circle cx="6" cy="6" r="6"/></svg>"##;
const IDLE_SVG: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" width="12" height="12" viewBox="0 0 12 12"><mask id="m"><circle cx="6" cy="6" r="6" fill="#fff"/><circle cx="3" cy="3" r="4.5" fill="#000"/></mask><rect width="12" height="12" mask="url(#m)"/></svg>"##;
const DND_SVG: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" width="12" height="12" viewBox="0 0 12 12"><mask id="m"><circle cx="6" cy="6" r="6" fill="#fff"/><rect x="1.5" y="4.5" width="9" height="3" rx="1.5" fill="#000"/></mask><rect width="12" height="12" mask="url(#m)"/></svg>"##;
const OFFLINE_SVG: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" width="12" height="12" viewBox="0 0 12 12"><mask id="m"><circle cx="6" cy="6" r="6" fill="#fff"/><circle cx="6" cy="6" r="3" fill="#000"/></mask><rect width="12" height="12" mask="url(#m)"/></svg>"##;

/// The drawing of a status dot: `online`, `idle`, `dnd`, else offline (a ring).
pub fn status_svg(status: &str) -> &'static [u8] {
    match status {
        "online" => ONLINE_SVG,
        "idle" => IDLE_SVG,
        "dnd" => DND_SVG,
        _ => OFFLINE_SVG,
    }
}

/// Gives a bundled SVG without a size (Twemoji: a 36 x 36 view box) a size of `px`,
/// so gpui rasterises it sharp at that size. One with a width stays as it is.
pub fn sized_svg(svg: &[u8], px: u32) -> Vec<u8> {
    let Some(at) = svg.windows(4).position(|w| w == b"<svg").map(|p| p + 4) else {
        return svg.to_vec();
    };
    let head_end = svg[at..]
        .iter()
        .position(|b| *b == b'>')
        .map_or(svg.len(), |p| p + at);
    if svg[at..head_end].windows(5).any(|w| w == b"width") {
        return svg.to_vec();
    }
    let attrs = format!(" width=\"{px}\" height=\"{px}\"");
    let mut out = Vec::with_capacity(svg.len() + attrs.len());
    out.extend_from_slice(&svg[..at]);
    out.extend_from_slice(attrs.as_bytes());
    out.extend_from_slice(&svg[at..]);
    out
}

static FONTS: std::sync::OnceLock<(&'static str, &'static str)> = std::sync::OnceLock::new();

/// The UI font family chosen at start-up.
pub fn ui_font() -> &'static str {
    FONTS.get().map(|f| f.0).unwrap_or("IBM Plex Sans")
}

/// The monospace font family chosen at start-up.
pub fn mono_font() -> &'static str {
    FONTS.get().map(|f| f.1).unwrap_or("IBM Plex Mono")
}

/// Registers the bundled fonts (not on Windows) and picks the UI and mono families.
pub fn load_fonts(cx: &App) {
    let _ = FONTS.set(choose_fonts(cx));
}

#[cfg(target_os = "windows")]
fn choose_fonts(cx: &App) -> (&'static str, &'static str) {
    let names = cx.text_system().all_font_names();
    let ui = if names.iter().any(|n| n == "Segoe UI Variable Text") {
        "Segoe UI Variable Text"
    } else {
        "Segoe UI"
    };
    (ui, "Consolas")
}

#[cfg(not(target_os = "windows"))]
fn choose_fonts(cx: &App) -> (&'static str, &'static str) {
    use std::borrow::Cow;
    let fonts: Vec<Cow<'static, [u8]>> = vec![
        Cow::Borrowed(include_bytes!("../assets/fonts/IBMPlexSans-Regular.ttf")),
        Cow::Borrowed(include_bytes!("../assets/fonts/IBMPlexSans-Italic.ttf")),
        Cow::Borrowed(include_bytes!("../assets/fonts/IBMPlexSans-Medium.ttf")),
        Cow::Borrowed(include_bytes!("../assets/fonts/IBMPlexSans-SemiBold.ttf")),
        Cow::Borrowed(include_bytes!("../assets/fonts/IBMPlexSans-Bold.ttf")),
        Cow::Borrowed(include_bytes!("../assets/fonts/IBMPlexMono-Regular.ttf")),
        Cow::Borrowed(include_bytes!("../assets/fonts/IBMPlexMono-Medium.ttf")),
    ];
    match cx.text_system().add_fonts(fonts) {
        Ok(()) => ("IBM Plex Sans", "IBM Plex Mono"),
        Err(err) => {
            log::warn!("could not load the bundled fonts: {err:#}");
            if cfg!(target_os = "macos") {
                (".SystemUIFont", "Menlo")
            } else {
                ("DejaVu Sans", "DejaVu Sans Mono")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn svg_drawings_get_a_size_once() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 36 36"><path/></svg>"#;
        let sized = String::from_utf8(sized_svg(svg, 64)).unwrap();
        assert!(
            sized.starts_with(r#"<svg width="64" height="64" xmlns="#),
            "{sized}"
        );
        assert_eq!(sized_svg(&sized_svg(svg, 64), 64), sized_svg(svg, 64));
        assert_eq!(status_svg("busy"), status_svg("offline"));
    }
}
