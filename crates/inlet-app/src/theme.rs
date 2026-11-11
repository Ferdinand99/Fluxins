// SPDX-License-Identifier: AGPL-3.0-or-later
//! Every colour Inlet draws with, in one place. Values follow the Fluxer parity spec
//! (Fluxer's default dark theme, computed to sRGB); views never hard-code colours.
//!
//! Brand, link and mention colours can be overridden per instance (an instance's
//! `branding.theme_color`), the way Fluxer's own client applies instance branding.

use std::sync::atomic::{AtomicU32, Ordering};

use gpui::{rgb, rgba, Hsla, Rgba};

fn h(c: Rgba) -> Hsla {
    c.into()
}

const NONE: u32 = u32::MAX;
static BRAND: AtomicU32 = AtomicU32::new(NONE);
static LINK: AtomicU32 = AtomicU32::new(NONE);

/// Applies an instance accent colour (`0xRRGGBB`), or restores the defaults with `None`.
/// Links and mentions get a light version of the accent, as instance-branded Fluxer
/// shows them.
pub fn set_accent(accent: Option<u32>) {
    match accent {
        Some(c) => {
            BRAND.store(c & 0xFF_FF_FF, Ordering::Relaxed);
            LINK.store(lighten(c & 0xFF_FF_FF), Ordering::Relaxed);
        }
        None => {
            BRAND.store(NONE, Ordering::Relaxed);
            LINK.store(NONE, Ordering::Relaxed);
        }
    }
}

/// Parses `#rrggbb` (the instance's `theme_color`).
pub fn parse_hex(s: &str) -> Option<u32> {
    let hex = s.trim().strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    u32::from_str_radix(hex, 16).ok()
}

/// The accent at 70 % lightness, keeping its hue: readable on the dark background.
fn lighten(c: u32) -> u32 {
    let hsla: Hsla = rgb(c).into();
    let light = Hsla {
        l: 0.70,
        s: hsla.s.max(0.6),
        ..hsla
    };
    let r: Rgba = light.into();
    let to = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u32;
    (to(r.r) << 16) | (to(r.g) << 8) | to(r.b)
}

fn or_default(v: &AtomicU32, default: u32) -> u32 {
    match v.load(Ordering::Relaxed) {
        NONE => default,
        c => c,
    }
}

// ---- Backgrounds -------------------------------------------------------------

/// Shell: rail, channel list, DM list (`--background-secondary`).
pub fn bg_app() -> Hsla {
    h(rgb(0x1a181e))
}
/// Chat area and channel header (`--background-secondary-lighter`).
pub fn bg_chat() -> Hsla {
    h(rgb(0x1e1d23))
}
/// Composer box and inputs (`--background-textarea`).
pub fn bg_textarea() -> Hsla {
    h(rgb(0x242229))
}
/// Guild icons without an image, icon circles (`--guild-list-foreground`).
pub fn guild_item() -> Hsla {
    h(rgb(0x29262f))
}
/// Floating user panel (`--panel-control-bg`).
pub fn panel_bg() -> Hsla {
    h(rgb(0x201d25))
}
/// Embed cards (`--background-primary`).
pub fn embed_bg() -> Hsla {
    h(rgb(0x0c0b0e))
}
/// Code blocks (`--code-block-bg`).
pub fn code_bg() -> Hsla {
    h(rgb(0x211e26))
}
/// Inline code (`--code-inline-bg`).
pub fn code_inline_bg() -> Hsla {
    h(rgb(0x242129))
}
/// Rows and messages under the pointer: white 5 %.
pub fn hover_bg() -> Hsla {
    h(rgba(0xffffff0d))
}
/// Selected channel or DM: white 10 %.
pub fn selected_bg() -> Hsla {
    h(rgba(0xffffff1a))
}

// ---- Text -------------------------------------------------------------------

/// Message text (`--text-chat`).
pub fn text() -> Hsla {
    h(rgb(0xeeedf0))
}
/// Names, titles, selected channel (`--text-primary`).
pub fn text_strong() -> Hsla {
    h(rgb(0xf4f4f6))
}
/// Unread channel names (`--text-secondary`).
pub fn text_secondary() -> Hsla {
    h(rgb(0xe5e4e9))
}
/// Muted: reply line, "(edited)", header icons (`--text-primary-muted`).
pub fn text_muted() -> Hsla {
    h(rgb(0xcecbd4))
}
/// Unselected channel names and icons, category names (`--text-tertiary-muted`).
pub fn text_channel() -> Hsla {
    h(rgb(0x9d97aa))
}
/// Topic, date separators, placeholder (`--text-tertiary`).
pub fn text_tertiary() -> Hsla {
    h(rgb(0xb1acbb))
}
/// Message timestamps: muted at 60 %.
pub fn timestamp() -> Hsla {
    h(rgba(0xcecbd499))
}
/// Code text (`--text-code`).
pub fn code_text() -> Hsla {
    h(rgb(0xe7e2e8))
}

// ---- Brand, links, mentions --------------------------------------------------

/// Brand colour: BOT tag, active guild, default embed edge (`--brand-primary`).
pub fn brand() -> Hsla {
    h(rgb(or_default(&BRAND, 0x413cdd)))
}
/// Links (`--text-link`).
pub fn link() -> Hsla {
    h(rgb(or_default(&LINK, 0x6ccff9)))
}
/// Mention pill text (`--markup-mention-text`).
pub fn mention_text() -> Hsla {
    link()
}
/// Mention pill fill: mention colour at 20 %.
pub fn mention_fill() -> Hsla {
    let mut c = link();
    c.a = 0.2;
    c
}
/// Text selection in inputs (`--text-selection`).
pub fn selection() -> Hsla {
    let mut c = link();
    c.a = 0.35;
    c
}
/// The bar of a message that mentions you.
pub fn mention_me_bar() -> Hsla {
    h(rgb(0xeac532))
}
/// The background of a message that mentions you.
pub fn mention_me_bg() -> Hsla {
    h(rgba(0xeac5321a))
}
/// The background of a message that was jumped to (`rgb(59 130 246 / 0.1)`).
pub fn jumped_bg() -> Hsla {
    h(rgba(0x3b82f61a))
}
/// The bar of a message that was jumped to (`--brand-primary-light`).
pub fn jumped_bar() -> Hsla {
    h(rgb(0xb0adff))
}
/// The reply spine: `--text-chat-muted` 82 % mixed with black.
pub fn reply_spine() -> Hsla {
    h(rgb(0xa9a7ae))
}
/// Text on the brand colour (`--brand-primary-fill`).
pub fn on_brand() -> Hsla {
    h(rgb(0xffffff))
}

// ---- Lines and states -------------------------------------------------------

/// Header and panel dividers: white about 3.5 %.
pub fn divider() -> Hsla {
    h(rgba(0xffffff09))
}
/// Component borders such as the embed frame (`--background-modifier-accent`).
pub fn accent_border() -> Hsla {
    h(rgba(0xc9c5d326))
}
/// Subtle ring around floating surfaces (composer, user panel).
pub fn ring() -> Hsla {
    h(rgba(0xc9c5d30a))
}
/// Input focus ring (`--border-color-focus`).
pub fn focus_ring() -> Hsla {
    h(rgba(0x6ccff973))
}
/// Blockquote bar (`--interactive-muted`).
pub fn quote_bar() -> Hsla {
    h(rgb(0x7170a1))
}
/// Errors, unread counts (`--status-danger`).
pub fn danger() -> Hsla {
    h(rgb(0xe53734))
}
/// Text on the unread count (`white` on `--status-danger`).
pub fn on_danger() -> Hsla {
    h(rgb(0xffffff))
}
/// Date separator line: accent border at 30 %.
pub fn separator_line() -> Hsla {
    h(rgba(0xc9c5d30b))
}

// ---- Markdown ---------------------------------------------------------------

/// Code block border: 90 % `--border-color`, 10 % `--text-code` (`--code-block-border`).
pub fn code_border() -> Hsla {
    h(rgba(0xa19bad47))
}
/// Table borders (`--border-color`, `hsla(258,10%,50%,0.2)`).
pub fn table_border() -> Hsla {
    h(rgba(0x7a738c33))
}
/// Table header row (`--bg-table-header`).
pub fn table_header_bg() -> Hsla {
    h(rgb(0x2a2730))
}
/// Odd table rows (`--bg-table-row-odd`).
pub fn table_row_odd() -> Hsla {
    h(rgb(0x0c0b0e))
}
/// Even table rows (`--bg-table-row-even`).
pub fn table_row_even() -> Hsla {
    h(rgb(0x1a181e))
}
/// A hidden spoiler: the text colour at 16 % (`--markup-spoiler-hidden-bg`).
pub fn spoiler_hidden() -> Hsla {
    let mut c = text();
    c.a = 0.16;
    c
}
/// A revealed spoiler (`--background-modifier-accent`).
pub fn spoiler_revealed() -> Hsla {
    accent_border()
}
/// A mention nobody can resolve: muted text on a muted fill.
pub fn mention_neutral_text() -> Hsla {
    text_muted()
}
/// Fill of a mention nobody can resolve.
pub fn mention_neutral_fill() -> Hsla {
    let mut c = text_muted();
    c.a = 0.16;
    c
}
/// `> [!NOTE]` (`--alert-note-color`, `hsl(198,92%,70%)`).
pub fn alert_note() -> Hsla {
    h(rgb(0x6ccff9))
}
/// `> [!TIP]` (`hsl(152,72%,45%)`).
pub fn alert_tip() -> Hsla {
    h(rgb(0x20c578))
}
/// `> [!IMPORTANT]` (`hsl(278,85%,65%)`).
pub fn alert_important() -> Hsla {
    h(rgb(0xba5af2))
}
/// `> [!WARNING]` (`hsl(45,93%,55%)`).
pub fn alert_warning() -> Hsla {
    h(rgb(0xf7c222))
}
/// `> [!CAUTION]` (`hsl(359,75%,60%)`).
pub fn alert_caution() -> Hsla {
    h(rgb(0xe54c4f))
}

// Code highlighting: the GitHub Dark colours Fluxer's code blocks use.

/// Keywords.
pub fn syntax_keyword() -> Hsla {
    h(rgb(0xff7b72))
}
/// Strings.
pub fn syntax_string() -> Hsla {
    h(rgb(0xa5d6ff))
}
/// Comments.
pub fn syntax_comment() -> Hsla {
    h(rgb(0x8b949e))
}
/// Numbers and other constants, attributes.
pub fn syntax_constant() -> Hsla {
    h(rgb(0x79c0ff))
}
/// Function names.
pub fn syntax_function() -> Hsla {
    h(rgb(0xd2a8ff))
}
/// Type names and parameters.
pub fn syntax_type() -> Hsla {
    h(rgb(0xffa657))
}
/// Markup tags, inserted lines.
pub fn syntax_tag() -> Hsla {
    h(rgb(0x7ee787))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accent_override_and_reset() {
        assert_eq!(parse_hex("#ff7e00"), Some(0xff7e00));
        assert_eq!(parse_hex("ff7e00"), None);
        assert_eq!(parse_hex("#ff7e0"), None);
        set_accent(Some(0xff7e00));
        let brand_now: Rgba = brand().into();
        assert!((brand_now.r - 1.0).abs() < 0.01 && brand_now.b < 0.01);
        let link_now: Rgba = link().into();
        assert!(link_now.r > link_now.b, "the link keeps the accent's hue");
        set_accent(None);
        let brand_default: Rgba = brand().into();
        assert!(
            brand_default.b > brand_default.r,
            "back to the default indigo"
        );
    }
}
