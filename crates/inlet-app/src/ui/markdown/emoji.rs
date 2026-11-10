// SPDX-License-Identifier: AGPL-3.0-or-later
//! Emoji in messages.
//!
//! Fluxer's parser does not look for emoji itself: the client hands it an "emoji
//! context" naming every Unicode emoji in the text (with its byte offset) and every
//! known `:shortcode:`, built from the emoji list (`buildEmojiContext` in Fluxer's
//! `MarkdownParserWasm.ts`). [`context_for`] builds the same from `inlet-emoji`, which
//! carries Fluxer's own list.
//!
//! An emoji takes a placeholder in the text as wide as Fluxer's inline emoji (1.5 em)
//! and its Twemoji drawing is painted over it once the text is laid out ([`overlay`]).
//! A message of only emoji (up to 30) is drawn large, as in Fluxer's `JumboDetector`.
//! Custom (guild) emoji show their `:name:` until custom emoji images arrive.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::{Arc, OnceLock};

use fluxer_markdown_parser::ast::EmojiKind;
use fluxer_markdown_parser::{EmojiContext, Node};
use gpui::{
    canvas, point, prelude::*, size, AnyElement, Bounds, Corners, Image, ImageFormat, Pixels,
    TextLayout,
};

use super::inline::push_marked;
use super::{Emoji, Inline, Style};

/// The text an emoji takes: an em space and an en space (1.5 em, Fluxer's inline
/// emoji width), joined by a word joiner so a line never breaks inside it.
pub(super) const PLACEHOLDER: &str = "\u{2003}\u{2060}\u{2002}";

/// Fluxer shows these as text, not as emoji (`PLAINTEXT_SYMBOLS`).
const PLAIN_SYMBOLS: [&str; 6] = [
    "\u{2122}",
    "\u{2122}\u{fe0f}",
    "\u{a9}",
    "\u{a9}\u{fe0f}",
    "\u{ae}",
    "\u{ae}\u{fe0f}",
];

/// Shortcodes Fluxer keeps as text (`SPECIAL_SHORTCODES`).
const PLAIN_SHORTCODES: [&str; 3] = ["tm", "copyright", "registered"];

/// Fluxer's `MAX_JUMBO_EMOJI_COUNT`.
const MAX_JUMBO: usize = inlet_emoji::MAX_JUMBO_EMOJI;

/// Parsing: appends an emoji.
pub(super) fn push(kind: &EmojiKind, inline: &mut Inline) {
    match kind {
        EmojiKind::Standard { raw, .. } => {
            if !is_known(raw) {
                inline.text.push_str(raw);
                return;
            }
            let start = inline.text.len();
            inline.text.push_str(PLACEHOLDER);
            inline
                .emoji
                .push((start..inline.text.len(), Emoji::Standard(raw.clone())));
        }
        EmojiKind::Custom { name, id, animated } => {
            let start = inline.text.len();
            push_marked(inline, &format!(":{name}:"), Style::Muted);
            inline.emoji.push((
                start..inline.text.len(),
                Emoji::Custom {
                    id: id.clone(),
                    name: name.clone(),
                    animated: *animated,
                },
            ));
        }
    }
}

/// Whether nodes at the top of a message are only emoji and blank text, at most 30
/// emoji (Fluxer's `shouldRenderAsJumboEmojis`).
pub(super) fn is_jumbo(nodes: &[Node]) -> bool {
    let mut count = 0;
    for node in nodes {
        match node {
            Node::Emoji { .. } => {
                count += 1;
                if count > MAX_JUMBO {
                    return false;
                }
            }
            Node::Text { content } if content.trim().is_empty() => {}
            _ => return false,
        }
    }
    count > 0
}

struct Glyphs {
    /// Every known glyph without `U+FE0F`.
    set: HashSet<String>,
    /// The longest, in chars.
    longest: usize,
}

fn glyphs() -> &'static Glyphs {
    static GLYPHS: OnceLock<Glyphs> = OnceLock::new();
    GLYPHS.get_or_init(|| {
        let set: HashSet<String> = inlet_emoji::glyphs()
            .map(|g| g.replace('\u{FE0F}', ""))
            .collect();
        let longest = set.iter().map(|g| g.chars().count()).max().unwrap_or(1);
        Glyphs { set, longest }
    })
}

fn is_known(raw: &str) -> bool {
    glyphs().set.contains(&raw.replace('\u{FE0F}', ""))
}

/// Length in chars of the longest known emoji at the start of `chars`, with a trailing
/// `U+FE0F`.
fn emoji_len(chars: &[(usize, char)]) -> Option<usize> {
    let known = glyphs();
    let mut candidate = String::new();
    let mut counted = 0;
    let mut matched = None;
    for (index, &(_, c)) in chars.iter().enumerate() {
        if c != '\u{FE0F}' {
            counted += 1;
            if counted > known.longest {
                break;
            }
            candidate.push(c);
        }
        if !candidate.is_empty() && known.set.contains(&candidate) {
            matched = Some(index + 1);
        }
    }
    matched
}

/// Only these can start an emoji: anything outside ASCII, and a keycap (`#️⃣`, `1️⃣`).
fn may_start(chars: &[(usize, char)]) -> bool {
    match chars {
        [(_, c), ..] if !c.is_ascii() => true,
        [(_, c), (_, next), ..] => {
            matches!(c, '#' | '*' | '0'..='9') && matches!(next, '\u{FE0F}' | '\u{20E3}')
        }
        _ => false,
    }
}

fn line(out: &mut String, kind: char, fields: &[&str]) {
    out.push(kind);
    for f in fields {
        out.push('\t');
        out.push_str(f);
    }
    out.push('\n');
}

/// The emoji context for parsing `input`: its Unicode emoji (byte offsets), and the
/// glyphs for its `:shortcodes:`, with skin tones.
pub(super) fn context_for(input: &str) -> EmojiContext {
    let mut out = String::new();
    let chars: Vec<(usize, char)> = input.char_indices().collect();
    let mut k = 0;
    while k < chars.len() {
        if !may_start(&chars[k..]) {
            k += 1;
            continue;
        }
        let Some(n) = emoji_len(&chars[k..]) else {
            k += 1;
            continue;
        };
        let start = chars[k].0;
        let end = chars.get(k + n).map_or(input.len(), |(o, _)| *o);
        let raw = &input[start..end];
        k += n;
        if PLAIN_SYMBOLS.contains(&raw) {
            continue;
        }
        let Some(emoji) = inlet_emoji::by_glyph(raw) else {
            continue;
        };
        let (offset, len) = (start.to_string(), (end - start).to_string());
        line(
            &mut out,
            'S',
            &[
                &offset,
                &len,
                raw,
                emoji.name(),
                &inlet_emoji::twemoji_code(raw),
            ],
        );
    }
    let mut seen = HashSet::new();
    for name in shortcodes(input) {
        if PLAIN_SHORTCODES.contains(&name) || !seen.insert(name) {
            continue;
        }
        let Some(emoji) = inlet_emoji::by_shortcode(name) else {
            continue;
        };
        line(
            &mut out,
            'C',
            &[name, emoji.glyph, &inlet_emoji::twemoji_code(emoji.glyph)],
        );
        if emoji.skin_tones {
            for tone in inlet_emoji::SkinTone::TONES {
                let glyph = inlet_emoji::with_skin_tone(emoji, tone);
                let number = tone.number().unwrap_or(0).to_string();
                line(
                    &mut out,
                    'K',
                    &[name, &number, &glyph, &inlet_emoji::twemoji_code(&glyph)],
                );
            }
        }
    }
    EmojiContext::parse(&out)
}

/// The names of `:name:` runs in `input` (letters, digits, `_` and `-`), the way the
/// regular expression in Fluxer's `buildEmojiContext` finds them.
fn shortcodes(input: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = input;
    let mut base = 0;
    while let Some(open) = rest.find(':') {
        let after = &rest[open + 1..];
        let len = after
            .char_indices()
            .find(|(_, c)| !(c.is_alphanumeric() || *c == '_' || *c == '-'))
            .map_or(after.len(), |(i, _)| i);
        if len > 0 && after[len..].starts_with(':') {
            out.push(&input[base + open + 1..base + open + 1 + len]);
            let next = open + 1 + len + 1;
            base += next;
            rest = &rest[next..];
        } else {
            base += open + 1;
            rest = &rest[open + 1..];
        }
    }
    out
}

// ---- Drawing --------------------------------------------------------------------

/// Twemoji drawings are rendered this large, so they stay sharp at 48 px on a 1.5x
/// screen and at the inline size on a 2x one.
const RASTER: u32 = 64;
/// Drawings kept ready (a few kilobytes each); past this the cache starts over.
const MAX_IMAGES: usize = 512;

thread_local! {
    static IMAGES: RefCell<HashMap<String, Option<Arc<Image>>>> = RefCell::new(HashMap::new());
}

/// The bundled Twemoji SVG for `glyph`, sized to [`RASTER`] pixels. Only drawings from
/// `inlet-emoji` reach this; no SVG from the network is ever drawn.
fn sized_svg(svg: &[u8]) -> Vec<u8> {
    // Twemoji drawings have a 36 x 36 view box and no size: give the root a size so
    // gpui renders them at RASTER pixels instead of 36.
    let tag = svg
        .windows(4)
        .position(|w| w[0] == b'<' && &w[1..] == b"svg")
        .map(|p| p + 4);
    let Some(at) = tag else {
        return svg.to_vec();
    };
    let head = &svg[..svg.iter().position(|b| *b == b'>').unwrap_or(svg.len())];
    if head.windows(5).any(|w| w == b"width") {
        return svg.to_vec();
    }
    let attrs = [("width", RASTER), ("height", RASTER)]
        .iter()
        .map(|(name, value)| format!("{name}=\"{value}\""))
        .collect::<Vec<_>>()
        .join(" ");
    let mut out = Vec::with_capacity(svg.len() + attrs.len() + 2);
    out.extend_from_slice(&svg[..at]);
    out.push(b' ');
    out.extend_from_slice(attrs.as_bytes());
    out.extend_from_slice(&svg[at..]);
    out
}

/// The Twemoji image for a Unicode emoji, or `None` when there is no drawing.
fn image(glyph: &str) -> Option<Arc<Image>> {
    IMAGES.with(|images| {
        let mut images = images.borrow_mut();
        if let Some(found) = images.get(glyph) {
            return found.clone();
        }
        if images.len() >= MAX_IMAGES {
            images.clear();
        }
        let made = inlet_emoji::twemoji_svg(glyph)
            .map(|svg| Arc::new(Image::from_bytes(ImageFormat::Svg, sized_svg(&svg))));
        images.insert(glyph.to_string(), made.clone());
        made
    })
}

/// Paints the Twemoji of `emoji` over their placeholders in a laid-out text. `None`
/// when there is nothing to paint. Custom emoji have no image yet: their name shows.
pub(super) fn overlay(
    layout: TextLayout,
    emoji: &[&(Range<usize>, Emoji)],
    side: Pixels,
) -> Option<AnyElement> {
    let slots: Vec<(Range<usize>, Arc<Image>)> = emoji
        .iter()
        .filter_map(|(range, e)| match e {
            Emoji::Standard(glyph) => image(glyph).map(|img| (range.clone(), img)),
            Emoji::Custom { .. } => None,
        })
        .collect();
    if slots.is_empty() {
        return None;
    }
    Some(
        canvas(
            move |_, window, cx| {
                slots
                    .into_iter()
                    .filter_map(|(range, img)| {
                        img.use_render_image(window, cx).map(|ri| (range, ri))
                    })
                    .collect::<Vec<_>>()
            },
            move |_, images, window, _| {
                if images.is_empty() {
                    return;
                }
                let line = layout.line_height();
                let side = if side > line { line } else { side };
                for (range, img) in images {
                    let Some(start) = layout.position_for_index(range.start) else {
                        continue;
                    };
                    let width = match layout.position_for_index(range.end) {
                        Some(end) if end.y == start.y && end.x > start.x => end.x - start.x,
                        _ => side,
                    };
                    let origin = point(start.x + (width - side) / 2., start.y + (line - side) / 2.);
                    let bounds = Bounds::new(origin, size(side, side));
                    let _ = window.paint_image(bounds, bounds, Corners::default(), img, 0, false);
                }
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_0()
        .into_any_element(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcodes_are_found_like_fluxers_regex() {
        assert_eq!(shortcodes(":smile: and :wave:"), vec!["smile", "wave"]);
        assert_eq!(shortcodes("a:b:c:d"), vec!["b"]);
        assert_eq!(shortcodes("::: :x-y_1:"), vec!["x-y_1"]);
        assert!(shortcodes("no colons").is_empty());
    }

    #[test]
    fn svg_gets_a_size_once() {
        let svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 36 36\"><path/></svg>";
        let sized = String::from_utf8(sized_svg(svg)).unwrap();
        assert!(
            sized.starts_with("<svg width=\"64\" height=\"64\" xmlns"),
            "{sized}"
        );
        assert_eq!(sized_svg(sized.as_bytes()), sized.as_bytes());
    }

    #[test]
    fn every_drawable_glyph_has_an_image() {
        assert!(image("\u{1F600}").is_some());
        assert!(image("\u{1F44B}\u{1F3FD}").is_some());
        assert!(image("not an emoji").is_none());
    }

    #[test]
    fn keycaps_and_flags_are_found() {
        let ctx = context_for("1\u{FE0F}\u{20E3} \u{1F1F3}\u{1F1F4} plain 12");
        let mut parser = fluxer_markdown_parser::MarkdownParser::new(
            fluxer_markdown_parser::ParserFlags::ALL,
            ctx,
        );
        let nodes = parser
            .parse("1\u{FE0F}\u{20E3} \u{1F1F3}\u{1F1F4} plain 12")
            .unwrap();
        let count = format!("{nodes:?}").matches("Emoji {").count();
        assert_eq!(count, 2, "{nodes:?}");
    }
}
