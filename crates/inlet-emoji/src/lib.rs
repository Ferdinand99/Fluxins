// SPDX-License-Identifier: AGPL-3.0-or-later
//! Emoji for Inlet, with the same shortcodes, categories, skin tones and graphics as Fluxer.
//!
//! * [`by_shortcode`], [`categories`] and [`with_skin_tone`] come from Fluxer's emoji list
//!   (`data/emojis.json`, AGPL-3.0-or-later, see `SOURCES.md`).
//! * [`search`] ranks matches the way Fluxer's emoji picker does.
//! * [`twemoji_svg`] returns the Twemoji drawing of a glyph (CC-BY-4.0, see
//!   `LICENSE-TWEMOJI`). The drawings are kept compressed in the binary and unpacked one at
//!   a time. Callers draw them with the image code in `inlet-media`; SVG files from the
//!   network are never drawn.
//! * [`emoji_only`] tells whether a message is only emoji, which Fluxer shows larger.

mod data;
mod search;
mod text;
mod twemoji;

pub use data::{
    all, by_glyph, by_shortcode, by_shortcut, categories, glyphs, shortcode_glyph, with_skin_tone,
    Category, Emoji, SkinTone,
};
pub use search::search;
pub use text::{emoji_only, MAX_JUMBO_EMOJI};
pub use twemoji::{twemoji_code, twemoji_svg};
