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
//! The drawings live in a bounded cache ([`EmojiImages`]); an emoji that does not get
//! an image is painted as text.
//! A message of only emoji (up to 30) is drawn large, as in Fluxer's `JumboDetector`.
//! Custom (guild) emoji show their `:name:` until custom emoji images arrive.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use fluxer_markdown_parser::ast::EmojiKind;
use fluxer_markdown_parser::{EmojiContext, Node};
use gpui::{
    canvas, point, prelude::*, size, AnyElement, Bounds, Corners, Image, ImageFormat, Pixels,
    RenderImage, ShapedLine, SharedString, TextAlign, TextLayout, TextRun,
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

// Deviation: the plan (K9) has Twemoji drawn by `inlet-media`, through its bounded
// image cache. That shared cache does not exist yet, so this module keeps its own
// bounded cache ([`EmojiImages`]) and gives the bundled SVG bytes to gpui's
// `Image::from_bytes`. Only drawings from `inlet-emoji` reach gpui; no SVG from the
// network is ever drawn. The seam is `EmojiImages`: moving onto the shared cache
// replaces that type and the release step in [`overlay`].

/// Twemoji drawings are rendered this large, so they stay sharp at 48 px on a 1.5x
/// screen and at the inline size on a 2x one.
const RASTER: u32 = 64;
/// Drawings kept as images. Each is RASTER x RASTER x 4 bytes = 16 KiB decoded, and as
/// much in the GPU atlas: 500 images are 8 MB of each.
pub(super) const MAX_IMAGES: usize = 500;
/// Different emoji drawn as images in one text; the others are painted as text.
pub(super) const MAX_IMAGES_PER_TEXT: usize = 200;
/// An image used this recently may still be on screen, so it is never evicted to make
/// room; the new emoji is painted as text instead.
const IN_USE: Duration = Duration::from_millis(500);

/// One cached drawing.
struct Entry {
    image: Arc<Image>,
    /// What gpui decoded it to, once drawn: dropped from the atlas on eviction.
    render: Option<Arc<RenderImage>>,
    used: Instant,
}

/// A bounded, least-recently-used cache of Twemoji images. Evicted images are queued
/// and released from gpui (its asset cache and the GPU atlas) at the next paint, where
/// the app is at hand.
#[derive(Default)]
pub(super) struct EmojiImages {
    entries: HashMap<String, Entry>,
    /// Glyphs with no drawing (looked up once).
    missing: HashSet<String>,
    released: Vec<(Arc<Image>, Option<Arc<RenderImage>>)>,
}

impl EmojiImages {
    /// The image for `glyph`, or `None` when it has no drawing or every cached image
    /// is in use (it is painted as text then).
    fn reserve(&mut self, glyph: &str, now: Instant) -> Option<Arc<Image>> {
        if let Some(entry) = self.entries.get_mut(glyph) {
            entry.used = now;
            return Some(entry.image.clone());
        }
        if self.missing.contains(glyph) {
            return None;
        }
        if self.entries.len() >= MAX_IMAGES {
            let (oldest, used) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.used)
                .map(|(k, e)| (k.clone(), e.used))?;
            if now.saturating_duration_since(used) < IN_USE {
                return None;
            }
            if let Some(entry) = self.entries.remove(&oldest) {
                self.released.push((entry.image, entry.render));
            }
        }
        let Some(svg) = inlet_emoji::twemoji_svg(glyph) else {
            self.missing.insert(glyph.to_string());
            return None;
        };
        let image = Arc::new(Image::from_bytes(ImageFormat::Svg, sized_svg(&svg)));
        self.entries.insert(
            glyph.to_string(),
            Entry {
                image: image.clone(),
                render: None,
                used: now,
            },
        );
        Some(image)
    }

    /// Notes what gpui decoded `glyph` to, so eviction can drop it from the atlas.
    fn rendered(&mut self, glyph: &str, render: &Arc<RenderImage>) {
        if let Some(entry) = self.entries.get_mut(glyph) {
            entry.render.get_or_insert_with(|| render.clone());
        }
    }

    /// For each emoji of one text: its image, or `None` to paint it as text. At most
    /// [`MAX_IMAGES_PER_TEXT`] different emoji get an image.
    fn plan(&mut self, glyphs: &[&str], now: Instant) -> Vec<Option<Arc<Image>>> {
        let mut seen: HashMap<&str, Option<Arc<Image>>> = HashMap::new();
        glyphs
            .iter()
            .map(|glyph| {
                if let Some(found) = seen.get(glyph) {
                    return found.clone();
                }
                let image = if seen.len() < MAX_IMAGES_PER_TEXT {
                    self.reserve(glyph, now)
                } else {
                    None
                };
                seen.insert(glyph, image.clone());
                image
            })
            .collect()
    }

    fn take_released(&mut self) -> Vec<(Arc<Image>, Option<Arc<RenderImage>>)> {
        std::mem::take(&mut self.released)
    }
}

thread_local! {
    static IMAGES: RefCell<EmojiImages> = RefCell::new(EmojiImages::default());
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

/// How one emoji is painted over its placeholder.
enum Slot {
    Image(String, Arc<Image>),
    /// No image (none left in the cache, or no drawing): the glyph as text.
    Text(String),
}

enum Painted {
    Image(Arc<RenderImage>),
    Text(Box<ShapedLine>),
}

/// Paints the Twemoji of `emoji` over their placeholders in a laid-out text. `None`
/// when there is nothing to paint. Custom emoji have no image yet: their name shows.
pub(super) fn overlay(
    layout: TextLayout,
    emoji: &[&(Range<usize>, Emoji)],
    side: Pixels,
) -> Option<AnyElement> {
    let standard: Vec<(Range<usize>, &str)> = emoji
        .iter()
        .filter_map(|(range, e)| match e {
            Emoji::Standard(glyph) => Some((range.clone(), glyph.as_str())),
            Emoji::Custom { .. } => None,
        })
        .collect();
    if standard.is_empty() {
        return None;
    }
    let glyphs: Vec<&str> = standard.iter().map(|(_, g)| *g).collect();
    let images = IMAGES.with(|c| c.borrow_mut().plan(&glyphs, Instant::now()));
    let slots: Vec<(Range<usize>, Slot)> = standard
        .into_iter()
        .zip(images)
        .map(|((range, glyph), image)| {
            let slot = match image {
                Some(image) => Slot::Image(glyph.to_string(), image),
                None => Slot::Text(glyph.to_string()),
            };
            (range, slot)
        })
        .collect();
    Some(
        canvas(
            move |_, window, cx| {
                // Release what the cache evicted: gpui's copy and the atlas entry.
                for (image, render) in IMAGES.with(|c| c.borrow_mut().take_released()) {
                    image.remove_asset(cx);
                    if let Some(render) = render {
                        cx.drop_image(render, Some(window));
                    }
                }
                let style = window.text_style();
                let font_size = style.font_size.to_pixels(window.rem_size());
                slots
                    .into_iter()
                    .filter_map(|(range, slot)| match slot {
                        Slot::Image(glyph, image) => {
                            let render = image.use_render_image(window, cx)?;
                            IMAGES.with(|c| c.borrow_mut().rendered(&glyph, &render));
                            Some((range, Painted::Image(render)))
                        }
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
                    match item {
                        Painted::Image(img) => {
                            let origin =
                                point(start.x + (width - side) / 2., start.y + (line - side) / 2.);
                            let bounds = Bounds::new(origin, size(side, side));
                            let _ = window.paint_image(
                                bounds,
                                bounds,
                                Corners::default(),
                                img,
                                0,
                                false,
                            );
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
        let mut cache = EmojiImages::default();
        let now = Instant::now();
        assert!(cache.reserve("\u{1F600}", now).is_some());
        assert!(cache.reserve("\u{1F44B}\u{1F3FD}", now).is_some());
        assert!(cache.reserve("not an emoji", now).is_none());
        assert!(cache.missing.contains("not an emoji"));
    }

    /// Decoded bytes of the images in `cache` (what gpui keeps for each, and as much
    /// again in the GPU atlas).
    fn decoded_bytes(cache: &EmojiImages) -> usize {
        cache.entries.len() * (RASTER * RASTER * 4) as usize
    }

    #[test]
    fn emoji_images_are_bounded_and_released() {
        let distinct: Vec<&str> = inlet_emoji::glyphs()
            .filter(|g| inlet_emoji::twemoji_svg(g).is_some())
            .take(2_000)
            .collect();
        assert_eq!(distinct.len(), 2_000);
        let mut cache = EmojiImages::default();
        let start = Instant::now();

        // One message with 2 000 different emoji: 200 images, the rest as text.
        let plan = cache.plan(&distinct, start);
        let images = plan.iter().filter(|i| i.is_some()).count();
        assert_eq!(images, MAX_IMAGES_PER_TEXT);
        assert_eq!(cache.entries.len(), MAX_IMAGES_PER_TEXT);
        assert_eq!(decoded_bytes(&cache), 200 * 16 * 1024);
        let svg_bytes: usize = cache.entries.values().map(|e| e.image.bytes().len()).sum();
        eprintln!(
            "2000 distinct emoji: {images} images, {} KiB decoded (and in the atlas), {} KiB SVG",
            decoded_bytes(&cache) / 1024,
            svg_bytes / 1024
        );
        // The same emoji again reuses the images.
        let again = cache.plan(&distinct, start);
        assert_eq!(again.iter().filter(|i| i.is_some()).count(), images);
        assert_eq!(cache.entries.len(), MAX_IMAGES_PER_TEXT);

        // More texts on the same screen: the cache stops at its cap, nothing in use is
        // evicted, and the rest is painted as text.
        for chunk in distinct[200..1_000].chunks(200) {
            cache.plan(chunk, start);
        }
        assert_eq!(cache.entries.len(), MAX_IMAGES);
        assert!(cache.released.is_empty());
        assert!(decoded_bytes(&cache) <= 8 * 1024 * 1024);

        // Later, other emoji: the oldest images are evicted and queued for release.
        let later = start + Duration::from_secs(5);
        let plan = cache.plan(&distinct[1_000..1_100], later);
        assert!(plan.iter().all(Option::is_some));
        assert_eq!(cache.entries.len(), MAX_IMAGES);
        assert_eq!(cache.take_released().len(), 100);
        assert!(cache.take_released().is_empty());
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
