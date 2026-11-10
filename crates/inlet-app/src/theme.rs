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
pub fn mention_bg() -> Hsla {
    h(rgba(0x5865f23d))
}
pub fn quote_bar() -> Hsla {
    h(rgb(0x4e5058))
}
