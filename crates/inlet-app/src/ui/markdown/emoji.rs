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
//! and its picture is painted over it once the text is laid out ([`overlay`]): the
//! Twemoji drawing of a Unicode emoji (an emoji that does not get an image is painted as
//! text), or the picture of a custom (guild) emoji. The pictures and their bounds live
//! in `ui::media::emoji`.
//! A message of only emoji (up to 30) is drawn large, as in Fluxer's `JumboDetector`.

use std::collections::HashSet;
use std::ops::Range;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};

use fluxer_markdown_parser::ast::EmojiKind;
use fluxer_markdown_parser::{EmojiContext, Node};
use gpui::{
    canvas, point, prelude::*, size, AnyElement, Bounds, Corners, Image, Pixels, RenderImage,
    ShapedLine, SharedString, TextAlign, TextLayout, TextRun,
};

use super::{Emoji, Inline};
use crate::ui::media::emoji::{self as media_emoji, EmojiPictures, Picture};

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
            inline.text.push_str(PLACEHOLDER);
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

/// How one emoji is painted over its placeholder.
enum Slot {
    /// A Twemoji drawing.
    Image(String, Arc<Image>),
    /// No Twemoji image (none left in the cache, or no drawing): the glyph as text.
    Text(String),
    /// A custom emoji's picture (see `ui::media::emoji`).
    Custom(Picture),
}

enum Painted {
    Image(Arc<RenderImage>),
    Text(Box<ShapedLine>),
    Custom(Picture),
}

/// Paints the emoji of a laid-out text over their placeholders: Twemoji, and custom
/// emoji from `pictures` (the message's, see `RenderCtx`). `None` when there is nothing
/// to paint.
pub(super) fn overlay(
    layout: TextLayout,
    emoji: &[&(Range<usize>, Emoji)],
    side: Pixels,
    pictures: Option<Rc<EmojiPictures>>,
) -> Option<AnyElement> {
    if emoji.is_empty() {
        return None;
    }
    let glyphs: Vec<&str> = emoji
        .iter()
        .filter_map(|(_, e)| match e {
            Emoji::Standard(glyph) => Some(glyph.as_str()),
            Emoji::Custom { .. } => None,
        })
        .collect();
    let mut images = media_emoji::twemoji_plan(&glyphs).into_iter();
    let slots: Vec<(Range<usize>, Slot)> = emoji
        .iter()
        .map(|(range, e)| {
            let slot = match e {
                Emoji::Standard(glyph) => match images.next().flatten() {
                    Some(image) => Slot::Image(glyph.clone(), image),
                    None => Slot::Text(glyph.clone()),
                },
                Emoji::Custom { id, .. } => {
                    Slot::Custom(pictures.as_ref().map_or(Picture::Missing, |p| p.get(id)))
                }
            };
            (range.clone(), slot)
        })
        .collect();
    let budget = pictures.map(|p| p.budget.clone());
    Some(
        canvas(
            move |_, window, cx| {
                let style = window.text_style();
                let font_size = style.font_size.to_pixels(window.rem_size());
                slots
                    .into_iter()
                    .filter_map(|(range, slot)| match slot {
                        Slot::Image(glyph, image) => {
                            let render = image.use_render_image(window, cx)?;
                            media_emoji::twemoji_rendered(&glyph, &render);
                            Some((range, Painted::Image(render)))
                        }
                        Slot::Custom(picture) => Some((range, Painted::Custom(picture))),
                        Slot::Text(glyph) => {
                            let run = TextRun {
                                len: glyph.len(),
                                font: style.font(),
                                color: style.color,
                                background_color: None,
                                underline: None,
                                strikethrough: None,
                            };
                            let line = window.text_system().shape_line(
                                SharedString::from(glyph),
                                font_size,
                                &[run],
                                None,
                            );
                            Some((range, Painted::Text(Box::new(line))))
                        }
                    })
                    .collect::<Vec<_>>()
            },
            move |_, painted, window, cx| {
                let line = layout.line_height();
                let side = if side > line { line } else { side };
                for (range, item) in painted {
                    let Some(start) = layout.position_for_index(range.start) else {
                        continue;
                    };
                    let width = match layout.position_for_index(range.end) {
                        Some(end) if end.y == start.y && end.x > start.x => end.x - start.x,
                        _ => side,
                    };
                    let origin = point(start.x + (width - side) / 2., start.y + (line - side) / 2.);
                    let bounds = Bounds::new(origin, size(side, side));
                    match item {
                        Painted::Image(img) => {
                            let _ = window.paint_image(
                                bounds,
                                bounds,
                                Corners::default(),
                                img,
                                0,
                                false,
                            );
                        }
                        Painted::Custom(picture) => {
                            if let Some(budget) = &budget {
                                media_emoji::paint_emoji(&picture, bounds, budget, window, cx);
                            }
                        }
                        Painted::Text(text) => {
                            let x = start.x + (width - text.width()).max(Pixels::ZERO) / 2.;
                            let _ = text.paint(
                                point(x, start.y),
                                line,
                                TextAlign::Left,
                                None,
                                window,
                                cx,
                            );
                        }
                    }
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
