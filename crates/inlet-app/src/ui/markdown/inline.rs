// SPDX-License-Identifier: AGPL-3.0-or-later
//! Inline nodes: text, styles, links, mentions, timestamps and emoji. Parsing turns
//! them into an [`Inline`]; drawing composes the current labels of its [`Live`] pieces
//! into it ([`compose`]) and draws the result as one styled text, with Twemoji painted
//! over their placeholders.

use std::ops::Range;

use fluxer_markdown_parser::ast::TimestampStyle;
use fluxer_markdown_parser::Node;
use gpui::{
    div, prelude::*, px, AnyElement, FontStyle, FontWeight, HighlightStyle, Hsla, InteractiveText,
    Pixels, SharedString, StrikethroughStyle, StyledText, UnderlineStyle,
};

use super::blocks::Draw;
use super::mention::{self, MentionNames};
use super::{emoji, timestamp};
use super::{
    is_deceptive, is_masked, link_target_label, Emoji, Inline, Link, Live, LiveKind, Style,
};
use crate::theme;

// ---- Parsing ------------------------------------------------------------------

pub(super) fn inline_of(children: &[Node]) -> Inline {
    let mut inline = Inline::default();
    for c in children {
        push_inline(c, &mut inline);
    }
    trim(&mut inline);
    inline
}

fn styled(children: &[Node], style: Style, inline: &mut Inline) {
    let start = inline.text.len();
    for c in children {
        push_inline(c, inline);
    }
    let end = inline.text.len();
    if end > start {
        inline.highlights.push((start..end, style));
    }
}

pub(super) fn push_marked(inline: &mut Inline, text: &str, style: Style) {
    let start = inline.text.len();
    inline.text.push_str(text);
    inline.highlights.push((start..inline.text.len(), style));
}

/// Appends a piece whose label is worked out again when drawn.
pub(super) fn push_live(inline: &mut Inline, label: &str, kind: LiveKind) {
    let start = inline.text.len();
    inline.text.push_str(label);
    inline.live.push(Live {
        range: start..inline.text.len(),
        kind,
    });
}

/// Every node kind of Fluxer's AST has a place here: the match has no catch-all arm,
/// so a new kind in the parser fails the build until it is handled.
pub(super) fn push_inline(node: &Node, inline: &mut Inline) {
    match node {
        Node::Text { content } => inline.text.push_str(content),
        Node::Strong { children } => styled(children, Style::Bold, inline),
        Node::Emphasis { children } => styled(children, Style::Italic, inline),
        Node::Underline { children } => styled(children, Style::Underline, inline),
        Node::Strikethrough { children } => styled(children, Style::Strike, inline),
        Node::Spoiler { children, .. } => styled(children, Style::Spoiler, inline),
        Node::Sequence { children } => children.iter().for_each(|c| push_inline(c, inline)),
        Node::InlineCode { content } => push_marked(inline, content, Style::Code),
        Node::Link { text, url, .. } => {
            let start = inline.text.len();
            match text {
                Some(t) => push_inline(t, inline),
                None => inline.text.push_str(url),
            }
            let range = start..inline.text.len();
            let masked = is_masked(&inline.text[range.clone()], url) || is_deceptive(url);
            inline.highlights.push((range.clone(), Style::Link));
            inline.links.push(Link {
                range,
                url: url.clone(),
                masked,
            });
            if masked {
                push_marked(
                    inline,
                    &format!(" ({})", link_target_label(url)),
                    Style::LinkTarget,
                );
            }
        }
        Node::Mention { kind } => mention::push(kind, inline),
        Node::Timestamp { timestamp, style } => timestamp::push(*timestamp, *style, inline),
        Node::Emoji { kind } => emoji::push(kind, inline),
        // Block nodes inside inline context (a table cell, a link text): their text.
        Node::Heading { children, .. }
        | Node::Subtext { children }
        | Node::Blockquote { children, .. }
        | Node::Alert { children, .. }
        | Node::TableCell { children } => children.iter().for_each(|c| push_inline(c, inline)),
        Node::CodeBlock { content, .. } => push_marked(inline, content, Style::Code),
        Node::List { items, .. } => {
            for item in items {
                inline.text.push_str("\n\u{2022} ");
                item.children.iter().for_each(|c| push_inline(c, inline));
            }
        }
        Node::TableRow { cells } => {
            for (i, cell) in cells.iter().enumerate() {
                if i > 0 {
                    inline.text.push_str(" | ");
                }
                push_inline(cell, inline);
            }
        }
        Node::Table { header, rows, .. } => {
            push_inline(header, inline);
            for row in rows {
                inline.text.push('\n');
                push_inline(row, inline);
            }
        }
    }
}

/// Drops line breaks at both ends, moving every range with the text.
pub(super) fn trim(inline: &mut Inline) {
    let end = inline.text.trim_end_matches('\n').len();
    let start = end - inline.text[..end].trim_start_matches('\n').len();
    if start == 0 && end == inline.text.len() {
        return;
    }
    let cut = |r: &Range<usize>| -> Option<Range<usize>> {
        let (s, e) = (r.start.clamp(start, end), r.end.clamp(start, end));
        (s < e).then(|| s - start..e - start)
    };
    inline.text = inline.text[start..end].to_string();
    inline.highlights = inline
        .highlights
        .drain(..)
        .filter_map(|(r, s)| cut(&r).map(|r| (r, s)))
        .collect();
    inline.links = inline
        .links
        .drain(..)
        .filter_map(|l| {
            cut(&l.range).map(|range| Link {
                range,
                url: l.url,
                masked: l.masked,
            })
        })
        .collect();
    // Live pieces and emoji are atomic: kept whole or dropped.
    let whole = |r: &Range<usize>| r.start >= start && r.end <= end;
    inline.live = inline
        .live
        .drain(..)
        .filter(|l| whole(&l.range))
        .map(|l| Live {
            range: l.range.start - start..l.range.end - start,
            kind: l.kind,
        })
        .collect();
    inline.emoji = inline
        .emoji
        .drain(..)
        .filter(|(r, _)| whole(r))
        .map(|(r, e)| (r.start - start..r.end - start, e))
        .collect();
}

// ---- Composing for drawing ----------------------------------------------------------

/// How a range of composed text is painted.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Paint {
    Style(Style),
    /// A mention pill in its own colours (role colours, or the neutral pill).
    Pill {
        text: Hsla,
        fill: Hsla,
    },
}

/// An inline with the current labels in place, ready to draw.
#[derive(Debug, Default)]
pub(super) struct Composed {
    pub text: String,
    pub paints: Vec<(Range<usize>, Paint)>,
    pub links: Vec<Link>,
    pub emoji: Vec<(Range<usize>, Emoji)>,
}

/// Puts the current label of every live piece into the text (names from `names`,
/// relative times against `now_s`, Unix seconds) and moves every range with it. The
/// parsed inline is not changed, so nothing is parsed again.
pub(super) fn compose(inline: &Inline, names: &MentionNames, now_s: i64) -> Composed {
    if inline.live.is_empty() {
        return Composed {
            text: inline.text.clone(),
            paints: inline
                .highlights
                .iter()
                .map(|(r, s)| (r.clone(), Paint::Style(*s)))
                .collect(),
            links: inline.links.clone(),
            emoji: inline.emoji.clone(),
        };
    }
    let mut text = String::with_capacity(inline.text.len() + 16);
    let mut paints = Vec::with_capacity(inline.highlights.len() + inline.live.len());
    let mut links = Vec::with_capacity(inline.links.len());
    // (old start, old end, new start, new end) of every live piece.
    let mut moves: Vec<(usize, usize, usize, usize)> = Vec::with_capacity(inline.live.len());
    let mut at = 0;
    for live in &inline.live {
        let r = &live.range;
        if r.start < at || r.end > inline.text.len() {
            continue;
        }
        text.push_str(&inline.text[at..r.start]);
        let shown = mention::resolve(&live.kind, names, now_s);
        let start = text.len();
        text.push_str(&shown.text);
        let end = text.len();
        moves.push((r.start, r.end, start, end));
        if let Some(paint) = shown.paint {
            paints.push((start..end, paint));
        }
        if let Some(url) = shown.link {
            links.push(Link {
                range: start..end,
                url,
                masked: false,
            });
        }
        at = r.end;
    }
    text.push_str(&inline.text[at..]);
    let map = |o: usize| -> usize {
        let mut shift: isize = 0;
        for &(os, oe, _, ne) in &moves {
            if o >= oe {
                shift = ne as isize - oe as isize;
            } else if o > os {
                // Inside a replaced piece: its end.
                return ne;
            } else {
                break;
            }
        }
        (o as isize + shift) as usize
    };
    let map_range = |r: &Range<usize>| map(r.start)..map(r.end);
    paints.extend(
        inline
            .highlights
            .iter()
            .map(|(r, s)| (map_range(r), Paint::Style(*s))),
    );
    links.extend(inline.links.iter().map(|l| Link {
        range: map_range(&l.range),
        url: l.url.clone(),
        masked: l.masked,
    }));
    links.sort_by_key(|l| l.range.start);
    let emoji = inline
        .emoji
        .iter()
        .map(|(r, e)| (map_range(r), e.clone()))
        .collect();
    Composed {
        text,
        paints,
        links,
        emoji,
    }
}

fn highlight(paint: Paint, revealed: bool) -> HighlightStyle {
    let style = match paint {
        Paint::Pill { text, fill } => {
            return HighlightStyle {
                color: Some(text),
                background_color: Some(fill),
                font_weight: Some(FontWeight::MEDIUM),
                ..Default::default()
            }
        }
        Paint::Style(style) => style,
    };
    match style {
        Style::Bold => HighlightStyle {
            font_weight: Some(FontWeight::SEMIBOLD),
            ..Default::default()
        },
        Style::Italic => HighlightStyle {
            font_style: Some(FontStyle::Italic),
            ..Default::default()
        },
        Style::Underline => HighlightStyle {
            underline: Some(UnderlineStyle {
                thickness: px(1.),
                ..Default::default()
            }),
            ..Default::default()
        },
        Style::Strike => HighlightStyle {
            strikethrough: Some(StrikethroughStyle {
                thickness: px(1.),
                ..Default::default()
            }),
            ..Default::default()
        },
        Style::Code => HighlightStyle {
            background_color: Some(theme::code_inline_bg()),
            color: Some(theme::code_text()),
            ..Default::default()
        },
        Style::Spoiler if revealed => HighlightStyle {
            background_color: Some(theme::spoiler_revealed()),
            ..Default::default()
        },
        Style::Spoiler => HighlightStyle {
            background_color: Some(theme::spoiler_hidden()),
            color: Some(gpui::transparent_black()),
            ..Default::default()
        },
        // Links have no underline at rest, as in Fluxer.
        Style::Link => HighlightStyle {
            color: Some(theme::link()),
            ..Default::default()
        },
        Style::Mention => HighlightStyle {
            background_color: Some(theme::mention_fill()),
            color: Some(theme::mention_text()),
            font_weight: Some(FontWeight::MEDIUM),
            ..Default::default()
        },
        Style::Timestamp => HighlightStyle {
            background_color: Some(theme::hover_bg()),
            ..Default::default()
        },
        Style::Edited | Style::LinkTarget | Style::Muted => HighlightStyle {
            color: Some(theme::text_muted()),
            ..Default::default()
        },
    }
}

fn is_spoiler(p: &Paint) -> bool {
    matches!(p, Paint::Style(Style::Spoiler))
}

impl Composed {
    /// Merges overlapping paints: gpui wants sorted, non-overlapping ranges. A hidden
    /// spoiler wins over everything inside it, so nothing leaks through its colour.
    pub(super) fn flatten(&self, revealed: bool) -> Vec<(Range<usize>, HighlightStyle)> {
        if self.paints.is_empty() {
            return Vec::new();
        }
        let mut cuts: Vec<usize> = self
            .paints
            .iter()
            .flat_map(|(r, _)| [r.start, r.end])
            .collect();
        cuts.sort_unstable();
        cuts.dedup();
        let mut out = Vec::new();
        for w in cuts.windows(2) {
            let (a, b) = (w[0], w[1]);
            if a >= b {
                continue;
            }
            let covering: Vec<Paint> = self
                .paints
                .iter()
                .filter(|(r, _)| r.start <= a && b <= r.end)
                .map(|(_, p)| *p)
                .collect();
            if covering.is_empty() {
                continue;
            }
            let style = if !revealed && covering.iter().any(is_spoiler) {
                highlight(Paint::Style(Style::Spoiler), false)
            } else {
                covering.iter().fold(HighlightStyle::default(), |acc, p| {
                    acc.highlight(highlight(*p, revealed))
                })
            };
            out.push((a..b, style));
        }
        out
    }

    fn hidden_spoilers(&self, revealed: bool) -> Vec<Range<usize>> {
        if revealed {
            return Vec::new();
        }
        self.paints
            .iter()
            .filter(|(_, p)| is_spoiler(p))
            .map(|(r, _)| r.clone())
            .collect()
    }

    /// Inline code ranges, drawn in the monospace font. gpui wants them sorted and
    /// apart.
    fn code_ranges(&self) -> Vec<(Range<usize>, SharedString)> {
        let mono = SharedString::from(super::mono_font());
        let mut ranges: Vec<Range<usize>> = self
            .paints
            .iter()
            .filter(|(r, p)| *p == Paint::Style(Style::Code) && r.start < r.end)
            .map(|(r, _)| r.clone())
            .collect();
        ranges.sort_by_key(|r| r.start);
        let mut out: Vec<(Range<usize>, SharedString)> = Vec::with_capacity(ranges.len());
        for r in ranges {
            if out.last().is_some_and(|(last, _)| r.start < last.end) {
                continue;
            }
            out.push((r, mono.clone()));
        }
        out
    }
}

fn overlaps(a: &Range<usize>, b: &Range<usize>) -> bool {
    a.start < b.end && b.start < a.end
}

/// What a click on a range of an inline does.
pub(super) enum Click {
    Link(String, bool),
    Reveal,
}

pub(super) fn clickables(c: &Composed, revealed: bool) -> Vec<(Range<usize>, Click)> {
    let spoilers = c.hidden_spoilers(revealed);
    let mut out = Vec::new();
    for link in &c.links {
        // Links inside a hidden spoiler stay inert until it is revealed.
        if spoilers.iter().any(|s| overlaps(s, &link.range)) {
            continue;
        }
        out.push((
            link.range.clone(),
            Click::Link(link.url.clone(), link.masked),
        ));
    }
    for s in spoilers {
        out.push((s, Click::Reveal));
    }
    out.sort_by_key(|(r, _)| r.start);
    out
}

/// The emoji size for an inline. Fluxer draws inline emoji 1.375 times the text size
/// (22 px in a message), centred on the line.
#[derive(Clone, Copy)]
pub(super) struct Look {
    pub emoji: Pixels,
}

impl Look {
    pub fn body() -> Self {
        Look { emoji: px(22.) }
    }

    pub fn sized(font: f32) -> Self {
        Look {
            emoji: px(font * 1.375),
        }
    }
}

/// Jumbo emoji are 48 px (`--emoji-size-jumbo-emoji: 3rem`); the text is sized so the
/// placeholder is as wide.
const JUMBO_FONT: f32 = 32.;
const JUMBO_EMOJI: f32 = 48.;
const JUMBO_LINE: f32 = 54.;

/// Draws one inline. `id` must be unique per message.
pub(super) fn element(id: SharedString, inline: &Inline, d: &Draw, look: Look) -> AnyElement {
    let composed = compose(inline, &d.ctx.names, d.now_s);
    for l in &inline.live {
        if let LiveKind::Timestamp {
            unix,
            style: TimestampStyle::RelativeTime,
        } = l.kind
        {
            timestamp::note_drawn(unix, d.now_s);
        }
    }
    let revealed = d.ctx.revealed;
    let jumbo = inline.jumbo && d.ctx.jumbo;
    let size = if jumbo { px(JUMBO_EMOJI) } else { look.emoji };
    let hidden = composed.hidden_spoilers(revealed);
    let emoji: Vec<&(Range<usize>, Emoji)> = composed
        .emoji
        .iter()
        .filter(|(r, _)| !hidden.iter().any(|s| overlaps(s, r)))
        .collect();
    let text = StyledText::new(SharedString::from(composed.text.clone()))
        .with_highlights(composed.flatten(revealed))
        .with_font_family_overrides(composed.code_ranges());
    let layout = text.layout().clone();
    let overlay = emoji::overlay(layout, &emoji, size);
    let clicks = clickables(&composed, revealed);
    let text = if clicks.is_empty() {
        text.into_any_element()
    } else {
        let ranges: Vec<Range<usize>> = clicks.iter().map(|(r, _)| r.clone()).collect();
        let actions: Vec<Click> = clicks.into_iter().map(|(_, c)| c).collect();
        let ctx = d.ctx.clone();
        InteractiveText::new(id, text)
            .on_click(ranges, move |ix, window, cx| match actions.get(ix) {
                Some(Click::Link(url, masked)) => (ctx.on_link)(url, *masked, window, cx),
                Some(Click::Reveal) => (ctx.on_reveal)(window, cx),
                None => {}
            })
            .into_any_element()
    };
    div()
        .when(jumbo, |d| {
            d.text_size(px(JUMBO_FONT)).line_height(px(JUMBO_LINE))
        })
        .when(overlay.is_some(), |d| d.relative())
        .child(text)
        .children(overlay)
        .into_any_element()
}
