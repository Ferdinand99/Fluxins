// SPDX-License-Identifier: AGPL-3.0-or-later
//! The parse entry, the parser's block nodes as [`Block`]s, and drawing a list of
//! blocks with the spacing of Fluxer's `Markup.module.css`.

use std::cell::RefCell;
use std::rc::Rc;

use fluxer_markdown_parser::{MarkdownParser, Node, ParseError, ParserFlags};
use gpui::{div, prelude::*, px, AnyElement, App, FontWeight, SharedString, Window};

use super::inline::{self, inline_of, push_inline, push_marked, Look};
use super::{alert, code, emoji, table};
use super::{Block, Inline, List, MentionNames, Style};
use crate::model::Names;
use crate::theme;

thread_local! {
    /// Names for the parse running on this thread (set by [`parse_with`]).
    static NAMES: RefCell<Names> = RefCell::new(Names::default());
}

pub fn parse(input: &str) -> Vec<Block> {
    parse_with(input, &Names::default())
}

/// Parses `input`. Mentions keep the names in `names` as a fallback for when the
/// account state does not know them.
pub fn parse_with(input: &str, names: &Names) -> Vec<Block> {
    parse_flags(input, names, ParserFlags::ALL)
}

/// [`parse_with`] for an embed's description or field: no tables, as in Fluxer
/// (`RESTRICTED_EMBED_DESCRIPTION_FLAGS`).
pub fn parse_embed_with(input: &str, names: &Names) -> Vec<Block> {
    parse_flags(input, names, ParserFlags::ALL & !ParserFlags::ALLOW_TABLES)
}

fn parse_flags(input: &str, names: &Names, flags: u32) -> Vec<Block> {
    NAMES.with(|n| *n.borrow_mut() = names.clone());
    let out = parse_inner(input, flags);
    NAMES.with(|n| *n.borrow_mut() = Names::default());
    out
}

/// Appends a muted "(edited)" to the last text block, or as its own block.
pub fn mark_edited(blocks: &mut Vec<Block>) {
    if let Some(Block::Text(inline)) = blocks.last_mut() {
        inline.jumbo = false;
        push_marked(
            inline,
            &format!(" {}", t!("markdown-edited")),
            Style::Edited,
        );
    } else {
        let mut inline = Inline::default();
        push_marked(&mut inline, &t!("markdown-edited"), Style::Edited);
        blocks.push(Block::Text(inline));
    }
}

pub(super) fn lookup(f: impl Fn(&Names) -> Option<String>) -> Option<String> {
    NAMES.with(|n| f(&n.borrow()))
}

/// Fluxer's parser with the flags of a standard message (`STANDARD_FLAGS`, which is
/// every flag) and the emoji found in `input`.
pub(super) fn parse_nodes(input: &str, flags: u32) -> Result<Vec<Node>, ParseError> {
    MarkdownParser::new(flags, emoji::context_for(input)).parse(input)
}

fn parse_inner(input: &str, flags: u32) -> Vec<Block> {
    match parse_nodes(input, flags) {
        Ok(nodes) => {
            let mut out = blocks_of(&nodes);
            if emoji::is_jumbo(&nodes) {
                for b in &mut out {
                    if let Block::Text(inline) = b {
                        inline.jumbo = true;
                    }
                }
            }
            out
        }
        Err(err) => {
            log::debug!("markdown parse failed, showing plain text: {err}");
            vec![Block::Text(Inline {
                text: input.to_string(),
                ..Default::default()
            })]
        }
    }
}

/// The parser's nodes as blocks. Block nodes become their own [`Block`]; everything
/// else gathers into text blocks through [`push_inline`], which handles every kind.
pub(super) fn blocks_of(nodes: &[Node]) -> Vec<Block> {
    let mut out = Vec::new();
    let mut current = Inline::default();
    let flush = |current: &mut Inline, out: &mut Vec<Block>| {
        let mut taken = std::mem::take(current);
        inline::trim(&mut taken);
        if !taken.text.trim().is_empty() {
            out.push(Block::Text(taken));
        }
    };
    for node in nodes {
        match node {
            Node::Heading { level, children } => {
                flush(&mut current, &mut out);
                out.push(Block::Heading(*level, inline_of(children)));
            }
            Node::Subtext { children } => {
                flush(&mut current, &mut out);
                out.push(Block::Subtext(inline_of(children)));
            }
            Node::CodeBlock { language, content } => {
                flush(&mut current, &mut out);
                out.push(Block::Code(code::convert(language.as_deref(), content)));
            }
            Node::Blockquote { children, .. } => {
                flush(&mut current, &mut out);
                out.push(Block::Quote(blocks_of(children)));
            }
            Node::Alert {
                alert_type,
                children,
            } => {
                flush(&mut current, &mut out);
                out.push(Block::Alert(alert::kind(alert_type), blocks_of(children)));
            }
            Node::List { ordered, items } => {
                flush(&mut current, &mut out);
                out.push(Block::List(List {
                    ordered: *ordered,
                    start: items.first().and_then(|i| i.ordinal).unwrap_or(1),
                    items: items.iter().map(|i| blocks_of(&i.children)).collect(),
                }));
            }
            Node::Table {
                header,
                alignments,
                rows,
            } => {
                flush(&mut current, &mut out);
                out.push(Block::Table(Box::new(table::convert(
                    header, alignments, rows,
                ))));
            }
            Node::Spoiler {
                children,
                is_block: Some(true),
            } => {
                flush(&mut current, &mut out);
                out.push(Block::Spoiler(blocks_of(children)));
            }
            other => push_inline(other, &mut current),
        }
    }
    flush(&mut current, &mut out);
    out
}

// ---- Drawing ------------------------------------------------------------------------

/// Called when a link is clicked: the URL and whether its text hides the target.
pub type OnLink = Rc<dyn Fn(&str, bool, &mut Window, &mut App)>;
/// Called when a hidden spoiler is clicked.
pub type OnReveal = Rc<dyn Fn(&mut Window, &mut App)>;

/// Callbacks and state for drawing one message.
#[derive(Clone)]
pub struct RenderCtx {
    /// Spoilers of this message are shown.
    pub revealed: bool,
    /// A link was clicked: the URL and whether its text hides the target.
    pub on_link: OnLink,
    /// A hidden spoiler was clicked.
    pub on_reveal: OnReveal,
    /// Names for the message's mentions, from the account state.
    pub names: Rc<MentionNames>,
    /// Emoji-only text may be drawn large (not in compact mode, as in Fluxer).
    pub jumbo: bool,
}

/// One drawing pass: the context and the time relative timestamps are measured from.
pub(super) struct Draw<'a> {
    pub ctx: &'a RenderCtx,
    pub now_s: i64,
}

/// How deep in lists and quotes a block sits (list markers and quote bars change).
#[derive(Clone, Copy, Default)]
pub(super) struct Depth {
    ordered: usize,
    unordered: usize,
    quote: usize,
}

/// Draws the blocks of one message. `id` must be unique per message.
pub fn render(id: &str, blocks: &[Block], ctx: &RenderCtx) -> AnyElement {
    let d = Draw {
        ctx,
        now_s: chrono::Utc::now().timestamp(),
    };
    render_seq(id, blocks, &d, Depth::default())
}

/// Space above and below a block (Fluxer's margins: headings `1.25rem 0 0.75rem`,
/// code and tables `0.75rem` below, quotes `0.25rem` around, lists and alerts
/// `0.25rem` above).
fn margins(block: &Block, next: Option<&Block>) -> (f32, f32) {
    match block {
        Block::Heading(..) => (20., 12.),
        Block::Code(_) => {
            let flush = matches!(
                next,
                Some(
                    Block::Heading(..)
                        | Block::Quote(_)
                        | Block::Alert(..)
                        | Block::Table(_)
                        | Block::List(_)
                )
            );
            (0., if flush { 0. } else { 12. })
        }
        Block::Table(_) => (0., 12.),
        Block::Quote(_) | Block::Spoiler(_) => (4., 4.),
        Block::Alert(..) | Block::List(_) => (4., 0.),
        Block::Text(_) | Block::Subtext(_) => (0., 0.),
    }
}

/// Draws a list of blocks, one under the other.
pub(super) fn render_seq(id: &str, blocks: &[Block], d: &Draw, depth: Depth) -> AnyElement {
    let mut col = div().flex().flex_col().min_w_0();
    let mut below_previous = 0.;
    for (i, block) in blocks.iter().enumerate() {
        let (top, bottom) = margins(block, blocks.get(i + 1));
        // The first heading sits close to the top (`h1:first-child`).
        let top = if i == 0 && matches!(block, Block::Heading(..)) {
            4.
        } else {
            top
        };
        let gap: f32 = if i == 0 { top } else { top.max(below_previous) };
        below_previous = bottom;
        col = col.child(div().when(gap > 0., |b| b.mt(px(gap))).child(render_block(
            &format!("{id}-{i}"),
            block,
            d,
            depth,
        )));
    }
    col.into_any_element()
}

fn heading_size(level: u8) -> f32 {
    match level {
        1 => 22.,
        2 => 20.,
        3 => 18.,
        4 => 16.,
        5 => 15.,
        _ => 14.,
    }
}

fn render_block(id: &str, block: &Block, d: &Draw, depth: Depth) -> AnyElement {
    let sid = || SharedString::from(id.to_string());
    match block {
        Block::Text(inline) => inline::element(sid(), inline, d, Look::body()),
        Block::Heading(level, inline) => {
            let size = heading_size(*level);
            div()
                .text_size(px(size))
                .line_height(px(size * 1.2))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(if *level >= 6 {
                    theme::text_secondary()
                } else {
                    theme::text_strong()
                })
                .child(inline::element(sid(), inline, d, Look::sized(size)))
                .into_any_element()
        }
        Block::Subtext(inline) => div()
            .text_size(px(13.))
            .line_height(px(18.))
            .text_color(theme::text_muted())
            .child(inline::element(sid(), inline, d, Look::sized(13.)))
            .into_any_element(),
        Block::Code(c) => code::render(id, c, d),
        Block::Quote(children) => div()
            .flex()
            .flex_row()
            .w_full()
            .child(
                div()
                    .w(px(4.))
                    .flex_none()
                    .rounded(px(4.))
                    .bg(theme::quote_bar())
                    .when(depth.quote > 0, |b| b.opacity(0.7)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .ml(px(12.))
                    .py(px(4.))
                    .text_color(theme::text_muted())
                    .child(render_seq(
                        id,
                        children,
                        d,
                        Depth {
                            quote: depth.quote + 1,
                            ..depth
                        },
                    )),
            )
            .into_any_element(),
        Block::Alert(kind, children) => alert::render(id, *kind, children, d, depth),
        Block::List(list) => render_list(id, list, d, depth),
        Block::Table(t) => table::render(id, t, d),
        Block::Spoiler(children) => render_spoiler(id, children, d, depth),
    }
}

fn lower_alpha(mut n: usize) -> String {
    let mut out = Vec::new();
    while n > 0 {
        n -= 1;
        out.push(char::from(b'a' + (n % 26) as u8));
        n /= 26;
    }
    out.iter().rev().collect()
}

fn lower_roman(mut n: usize) -> String {
    const TABLE: [(usize, &str); 13] = [
        (1000, "m"),
        (900, "cm"),
        (500, "d"),
        (400, "cd"),
        (100, "c"),
        (90, "xc"),
        (50, "l"),
        (40, "xl"),
        (10, "x"),
        (9, "ix"),
        (5, "v"),
        (4, "iv"),
        (1, "i"),
    ];
    if n == 0 || n >= 4000 {
        return n.to_string();
    }
    let mut out = String::new();
    for (value, digits) in TABLE {
        while n >= value {
            out.push_str(digits);
            n -= value;
        }
    }
    out
}

/// The marker of item number `n` (or a bullet), by nesting as in Fluxer's CSS: discs,
/// then circles, then squares; numbers, then letters, then roman numerals.
fn marker(ordered: bool, n: usize, depth: Depth) -> String {
    if ordered {
        let number = match depth.ordered {
            0 => n.to_string(),
            1 => lower_alpha(n),
            _ => lower_roman(n),
        };
        format!("{number}.")
    } else {
        match depth.unordered {
            0 => "\u{2022}".into(),
            1 => "\u{25e6}".into(),
            _ => "\u{25aa}".into(),
        }
    }
}

/// Draws a list: markers outside the text, in a column as wide as Fluxer's list margin
/// (`1.25rem`, or `0.65em + 0.6em` per character of the longest number).
fn render_list(id: &str, list: &List, d: &Draw, depth: Depth) -> AnyElement {
    let count = list.items.len();
    let width = if list.ordered {
        let chars = list
            .start
            .saturating_add(count.saturating_sub(1))
            .to_string()
            .len();
        16. * (0.65 + 0.6 * chars as f32)
    } else {
        20.
    };
    let inner = Depth {
        ordered: depth.ordered + usize::from(list.ordered),
        unordered: depth.unordered + usize::from(!list.ordered),
        ..depth
    };
    let mut col = div().flex().flex_col().min_w_0();
    for (i, item) in list.items.iter().enumerate() {
        col = col.child(
            div()
                .flex()
                .flex_row()
                .when(i + 1 < count, |r| r.mb(px(4.)))
                .child(
                    div()
                        .w(px(width))
                        .flex_none()
                        .pr(px(6.))
                        .text_right()
                        .child(marker(list.ordered, list.start.saturating_add(i), depth)),
                )
                .child(div().flex_1().min_w_0().child(render_seq(
                    &format!("{id}-l{i}"),
                    item,
                    d,
                    inner,
                ))),
        );
    }
    col.into_any_element()
}

/// A spoiler around blocks (`.blockSpoiler`): 8 px padding and 6 px corners; hidden, its
/// content is invisible and a click anywhere on it shows the message's spoilers.
fn render_spoiler(id: &str, children: &[Block], d: &Draw, depth: Depth) -> AnyElement {
    let only_code = matches!(children, [Block::Code(_)]);
    let inner = render_seq(id, children, d, depth);
    let frame = div()
        .relative()
        .max_w_full()
        .rounded(px(6.))
        .when(!only_code, |b| b.p(px(8.)));
    let frame = if d.ctx.revealed {
        frame
            .when(!only_code, |b| b.bg(theme::spoiler_revealed()))
            .child(inner)
    } else {
        let on_reveal = d.ctx.on_reveal.clone();
        frame
            .bg(theme::spoiler_hidden())
            .child(div().opacity(0.).child(inner))
            .child(
                div()
                    .id(SharedString::from(format!("{id}-hidden")))
                    .absolute()
                    .inset_0()
                    .rounded(px(6.))
                    .cursor_pointer()
                    .occlude()
                    .hover(|s| s.bg(theme::spoiler_hidden()))
                    .on_click(move |_, window, cx| on_reveal(window, cx)),
            )
    };
    div().flex().flex_row().child(frame).into_any_element()
}

#[cfg(test)]
mod tests {
    use super::super::{Emoji, LiveKind};
    use super::*;

    /// What draws each node kind of Fluxer's AST. There is no `_` arm: a node kind
    /// added to the parser fails the build here (and in `push_inline`) until it has a
    /// renderer.
    fn renderer_of(node: &Node) -> &'static str {
        match node {
            Node::Text { .. } => "text",
            Node::Blockquote { .. } => "quote block",
            Node::Strong { .. } => "bold run",
            Node::Emphasis { .. } => "italic run",
            Node::Underline { .. } => "underline run",
            Node::Strikethrough { .. } => "strike run",
            Node::Spoiler { .. } => "spoiler run or block",
            Node::Heading { .. } => "heading block",
            Node::Subtext { .. } => "subtext block",
            Node::List { .. } => "list block",
            Node::CodeBlock { .. } => "code block",
            Node::InlineCode { .. } => "code run",
            Node::Sequence { .. } => "link text",
            Node::Link { .. } => "link run",
            Node::Mention { .. } => "mention pill",
            Node::Timestamp { .. } => "timestamp",
            Node::Emoji { .. } => "twemoji",
            Node::Table { .. } => "table block",
            Node::TableRow { .. } => "table row",
            Node::TableCell { .. } => "table cell",
            Node::Alert { .. } => "alert block",
        }
    }

    fn children(node: &Node) -> Vec<&Node> {
        match node {
            Node::Text { .. }
            | Node::CodeBlock { .. }
            | Node::InlineCode { .. }
            | Node::Mention { .. }
            | Node::Timestamp { .. }
            | Node::Emoji { .. } => Vec::new(),
            Node::Blockquote { children, .. }
            | Node::Strong { children }
            | Node::Emphasis { children }
            | Node::Underline { children }
            | Node::Strikethrough { children }
            | Node::Spoiler { children, .. }
            | Node::Heading { children, .. }
            | Node::Subtext { children }
            | Node::Sequence { children }
            | Node::TableCell { children }
            | Node::Alert { children, .. } => children.iter().collect(),
            Node::List { items, .. } => items.iter().flat_map(|i| i.children.iter()).collect(),
            Node::Link { text, .. } => text.iter().map(|t| t.as_ref()).collect(),
            Node::Table { header, rows, .. } => std::iter::once(header.as_ref())
                .chain(rows.iter())
                .collect(),
            Node::TableRow { cells } => cells.iter().collect(),
        }
    }

    fn walk(node: &Node, seen: &mut std::collections::BTreeSet<&'static str>) {
        seen.insert(renderer_of(node));
        for c in children(node) {
            walk(c, seen);
        }
    }

    const EVERY_KIND: &str = "# Heading\n\
-# subtext\n\
**bold** *italic* __underline__ ~~strike~~ ||secret|| `code`\n\
[**masked** link](https://fluxer.app/x) and <@42> at <t:1700000000:R> \u{1F44B}\n\
> quoted\n\
\n\
> [!TIP]\n\
> an alert\n\
\n\
- one\n\
- two\n\
\n\
| a | b |\n\
| - | - |\n\
| 1 | 2 |\n\
\n\
```rust\nfn main() {}\n```";

    #[test]
    fn every_node_kind_in_fluxers_ast_has_a_renderer() {
        let nodes = parse_nodes(EVERY_KIND, ParserFlags::ALL).expect("parses");
        let mut seen = std::collections::BTreeSet::new();
        for n in &nodes {
            walk(n, &mut seen);
        }
        assert_eq!(seen.len(), 21, "node kinds seen: {seen:?}\n{nodes:#?}");

        // And each lands in what is drawn.
        let blocks = parse(EVERY_KIND);
        let has = |f: &dyn Fn(&Block) -> bool| blocks.iter().any(f);
        assert!(has(&|b| matches!(b, Block::Heading(1, _))));
        assert!(has(&|b| matches!(b, Block::Subtext(_))));
        assert!(has(&|b| matches!(b, Block::Quote(_))));
        assert!(has(&|b| matches!(
            b,
            Block::Alert(super::super::AlertKind::Tip, _)
        )));
        assert!(has(&|b| matches!(b, Block::List(l) if l.items.len() == 2)));
        assert!(has(&|b| matches!(b, Block::Table(t) if t.rows.len() == 1)));
        assert!(has(&|b| matches!(b, Block::Code(c) if !c.spans.is_empty())));
        let text = blocks
            .iter()
            .filter_map(|b| match b {
                Block::Text(i) => Some(i),
                _ => None,
            })
            .collect::<Vec<_>>();
        let styles: Vec<Style> = text
            .iter()
            .flat_map(|i| i.highlights.iter().map(|(_, s)| *s))
            .collect();
        for s in [
            Style::Bold,
            Style::Italic,
            Style::Underline,
            Style::Strike,
            Style::Spoiler,
            Style::Code,
            Style::Link,
        ] {
            assert!(styles.contains(&s), "{s:?} missing");
        }
        let live: Vec<&LiveKind> = text
            .iter()
            .flat_map(|i| i.live.iter().map(|l| &l.kind))
            .collect();
        assert!(live.iter().any(|k| matches!(k, LiveKind::User { .. })));
        assert!(live.iter().any(|k| matches!(k, LiveKind::Timestamp { .. })));
        assert!(text
            .iter()
            .any(|i| i.emoji.iter().any(|(_, e)| matches!(e, Emoji::Standard(_)))));
        assert!(text.iter().any(|i| i
            .links
            .iter()
            .any(|l| l.masked && i.text[l.range.clone()] == *"masked link")));
    }

    #[test]
    fn list_markers_follow_nesting() {
        let flat = Depth::default();
        assert_eq!(marker(true, 3, flat), "3.");
        assert_eq!(marker(false, 1, flat), "\u{2022}");
        let nested = Depth {
            ordered: 1,
            unordered: 1,
            quote: 0,
        };
        assert_eq!(marker(true, 28, nested), "ab.");
        assert_eq!(marker(false, 1, nested), "\u{25e6}");
        assert_eq!(lower_roman(1994), "mcmxciv");
    }

    #[test]
    fn blocks_are_spaced_like_fluxer() {
        let blocks = parse("```\nx\n```\n# Title\ntext");
        assert_eq!(margins(&blocks[0], blocks.get(1)).1, 0.);
        assert_eq!(margins(&blocks[1], blocks.get(2)), (20., 12.));
    }
}
