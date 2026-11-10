//! Colours for the v0 dark theme. Plain functions so views stay short.

use gpui::{rgb, rgba, Hsla, Rgba};

fn h(c: Rgba) -> Hsla {
    c.into()
}

pub fn rail_bg() -> Hsla {
    h(rgb(0x1e1f22))
}
pub fn sidebar_bg() -> Hsla {
    h(rgb(0x2b2d31))
}
pub fn chat_bg() -> Hsla {
    h(rgb(0x313338))
}
pub fn header_border() -> Hsla {
    h(rgb(0x26272b))
}
pub fn input_bg() -> Hsla {
    h(rgb(0x383a40))
}
pub fn input_border() -> Hsla {
    h(rgb(0x1e1f22))
}
pub fn hover_bg() -> Hsla {
    h(rgba(0x4e505860))
}
pub fn selected_bg() -> Hsla {
    h(rgba(0x4e5058a0))
}
pub fn text() -> Hsla {
    h(rgb(0xdbdee1))
}
pub fn text_strong() -> Hsla {
    h(rgb(0xf2f3f5))
}
pub fn text_muted() -> Hsla {
    h(rgb(0x949ba4))
}
pub fn accent() -> Hsla {
    h(rgb(0x5865f2))
}
pub fn link() -> Hsla {
    h(rgb(0x00a8fc))
}
pub fn danger() -> Hsla {
    h(rgb(0xf23f43))
}
pub fn selection() -> Hsla {
    h(rgba(0x5865f260))
}
pub fn code_bg() -> Hsla {
    h(rgb(0x2b2d31))
}
pub fn quote_bar() -> Hsla {
    h(rgb(0x4e5058))
}

// Values from the Fluxer parity spec (default dark theme), used by the newer views.
// The full theme rework replaces the older values above.

/// Embed card background (`--background-primary`).
pub fn embed_bg() -> Hsla {
    h(rgb(0x0c0b0e))
}
/// Embed card border (`--background-modifier-accent`, about 15 % light).
pub fn embed_border() -> Hsla {
    h(rgba(0xcbc8d226))
}
/// Brand colour: BOT tag, default embed edge (`--brand-primary`).
pub fn brand() -> Hsla {
    h(rgb(0x413cdd))
}
/// Mention pill text (`--markup-mention-text`).
pub fn mention_text() -> Hsla {
    h(rgb(0x6ccff9))
}
/// Mention pill fill: mention colour at 20 %.
pub fn mention_fill() -> Hsla {
    h(rgba(0x6ccff933))
}
/// Date separator line (accent at 30 %).
pub fn divider() -> Hsla {
    h(rgba(0xcbc8d20d))
}
/// Date separator text (`--text-tertiary`).
pub fn text_tertiary() -> Hsla {
    h(rgb(0xb1acbb))
}
