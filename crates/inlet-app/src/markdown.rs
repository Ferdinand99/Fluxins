// SPDX-License-Identifier: AGPL-3.0-or-later
//! Renders message markdown with Fluxer's own parser (`fluxer_markdown_parser`, from the
//! Fluxer repository, AGPL-3.0-or-later), so a message is split into the same nodes as in
//! Fluxer's clients. v0 draws it simply: styled text runs, code blocks, quotes and lists.
//!
//! Parsing happens on the backend thread ([`parse`] returns plain data that is `Send`);
//! the UI thread only draws.

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;

use fluxer_markdown_parser::ast::{AlertType, EmojiKind, ListItem, MentionKind};
use fluxer_markdown_parser::{EmojiContext, MarkdownParser, Node, ParserFlags};
use gpui::{
    div, prelude::*, px, AnyElement, App, FontStyle, FontWeight, HighlightStyle, InteractiveText,
    SharedString, StrikethroughStyle, StyledText, UnderlineStyle, Window,
};

use crate::model::Names;
use crate::theme;

/// Prefix of the internal link a channel mention carries; clicking it opens the channel.
pub const CHANNEL_LINK: &str = "inlet-channel:";

thread_local! {
    /// Names for the parse running on this thread (set by [`parse_with`]).
    static NAMES: RefCell<Names> = RefCell::new(Names::default());
}

/// One block of a rendered message.
#[derive(Clone, Debug)]
pub enum Block {
    Text(Inline),
    Heading(u8, Inline),
    Subtext(Inline),
    Code { content: SharedString },
    Quote(Vec<Block>),
    Alert(&'static str, Vec<Block>),
    List(Vec<(String, Vec<Block>)>),
}

/// A link in the text.
#[derive(Clone, Debug, PartialEq)]
pub struct Link {
    pub range: Range<usize>,
    pub url: String,
    /// The visible text is not the URL itself: the person cannot see where it goes.
    pub masked: bool,
}

/// Text with styled ranges and clickable links.
#[derive(Clone, Debug, Default)]
pub struct Inline {
    pub text: String,
    pub highlights: Vec<(Range<usize>, Style)>,
    pub links: Vec<Link>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Style {
    Bold,
    Italic,
    Underline,
    Strike,
    Code,
    Spoiler,
    Link,
    Mention,
    /// The target host shown after a masked link.
    LinkTarget,
    /// "(edited)" after the text.
    Edited,
}

/// A URL that may be opened from a message: it parses and is http or https.
pub fn openable(url: &str) -> Option<url::Url> {
    let parsed = url::Url::parse(url).ok()?;
    matches!(parsed.scheme(), "https" | "http").then_some(parsed)
}

/// What to show after a masked link so the target is visible: the host, in its ASCII
/// (punycode) form, so look-alike Unicode domains stand out.
pub fn link_target_label(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(u) => match u.host_str() {
            Some(host) => host.to_string(),
            None => u.scheme().to_string(),
        },
        Err(_) => "invalid link".into(),
    }
}

/// Whether the visible text of a link hides its target.
pub fn is_masked(text: &str, url: &str) -> bool {
    let norm = |s: &str| s.trim().trim_end_matches('/').to_ascii_lowercase();
    norm(text) != norm(url)
}

/// A URL that can fool the reader even when shown in full: a non-ASCII (look-alike)
/// host, or userinfo before the host (`https://fluxer.app@evil.example`).
pub fn is_deceptive(url: &str) -> bool {
    let authority = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let authority = authority.split(['/', '?', '#']).next().unwrap_or("");
    match url::Url::parse(url) {
        Ok(u) => {
            !u.username().is_empty()
                || u.password().is_some()
                || !authority.is_ascii()
                || u.host_str()
                    .is_some_and(|h| h.split('.').any(|label| label.starts_with("xn--")))
        }
        Err(_) => true,
    }
}

pub fn parse(input: &str) -> Vec<Block> {
    parse_with(input, &Names::default())
}

/// Parses `input`, resolving channel, user and role mentions with `names`.
pub fn parse_with(input: &str, names: &Names) -> Vec<Block> {
    NAMES.with(|n| *n.borrow_mut() = names.clone());
    let out = parse_inner(input);
    NAMES.with(|n| *n.borrow_mut() = Names::default());
    out
}

/// Appends a muted "(edited)" to the last text block, or as its own block.
pub fn mark_edited(blocks: &mut Vec<Block>) {
    if let Some(Block::Text(inline)) = blocks.last_mut() {
        push_marked(inline, " (edited)", Style::Edited);
    } else {
        let mut inline = Inline::default();
        push_marked(&mut inline, "(edited)", Style::Edited);
        blocks.push(Block::Text(inline));
    }
}

fn lookup(f: impl Fn(&Names) -> Option<String>) -> Option<String> {
    NAMES.with(|n| f(&n.borrow()))
}

fn parse_inner(input: &str) -> Vec<Block> {
    let flags = ParserFlags::ALL;
    let mut parser = MarkdownParser::new(flags, EmojiContext::default());
    match parser.parse(input) {
        Ok(nodes) => blocks(&nodes),
        Err(err) => {
            log::debug!("markdown parse failed, showing plain text: {err}");
            vec![Block::Text(Inline {
                text: input.to_string(),
                ..Default::default()
            })]
        }
    }
}

fn blocks(nodes: &[Node]) -> Vec<Block> {
    let mut out = Vec::new();
    let mut current = Inline::default();
    let flush = |current: &mut Inline, out: &mut Vec<Block>| {
        let trimmed_len = current.text.trim_end_matches('\n').len();
        current.text.truncate(trimmed_len);
        current.highlights.retain(|(r, _)| r.end <= trimmed_len);
        current.links.retain(|l| l.range.end <= trimmed_len);
        if !current.text.trim().is_empty() {
            out.push(Block::Text(std::mem::take(current)));
        } else {
            *current = Inline::default();
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
            Node::CodeBlock { content, .. } => {
                flush(&mut current, &mut out);
                out.push(Block::Code {
                    content: content.trim_end_matches('\n').to_string().into(),
                });
            }
            Node::Blockquote { children, .. } => {
                flush(&mut current, &mut out);
                out.push(Block::Quote(blocks(children)));
            }
            Node::Alert {
                alert_type,
                children,
            } => {
                flush(&mut current, &mut out);
                let label = match alert_type {
                    AlertType::Note => "Note",
                    AlertType::Tip => "Tip",
                    AlertType::Important => "Important",
                    AlertType::Warning => "Warning",
                    AlertType::Caution => "Caution",
                };
                out.push(Block::Alert(label, blocks(children)));
            }
            Node::List { ordered, items } => {
                flush(&mut current, &mut out);
                out.push(Block::List(list_items(*ordered, items)));
            }
            Node::Table { header, rows, .. } => {
                flush(&mut current, &mut out);
                let mut inline = Inline::default();
                table_row(header, &mut inline);
                for row in rows {
                    inline.text.push('\n');
                    table_row(row, &mut inline);
                }
                out.push(Block::Code {
                    content: inline.text.into(),
                });
            }
            other => push_inline(other, &mut current),
        }
    }
    flush(&mut current, &mut out);
    out
}

fn list_items(ordered: bool, items: &[ListItem]) -> Vec<(String, Vec<Block>)> {
    items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let marker = if ordered {
                format!("{}.", item.ordinal.unwrap_or(i + 1))
            } else {
                "\u{2022}".into()
            };
            (marker, blocks(&item.children))
        })
        .collect()
}

fn table_row(row: &Node, inline: &mut Inline) {
    if let Node::TableRow { cells } = row {
        for (i, cell) in cells.iter().enumerate() {
            if i > 0 {
                inline.text.push_str(" | ");
            }
            if let Node::TableCell { children } = cell {
                for c in children {
                    push_inline(c, inline);
                }
            }
        }
    }
}

fn inline_of(children: &[Node]) -> Inline {
    let mut inline = Inline::default();
    for c in children {
        push_inline(c, &mut inline);
    }
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

fn push_marked(inline: &mut Inline, text: &str, style: Style) {
    let start = inline.text.len();
    inline.text.push_str(text);
    inline.highlights.push((start..inline.text.len(), style));
}

fn push_inline(node: &Node, inline: &mut Inline) {
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
        Node::Mention { kind } => {
            if let MentionKind::Channel { id } = kind {
                let name = lookup(|n| n.channels.get(id).cloned());
                let label = match &name {
                    Some(name) => format!("#{name}"),
                    None => "#unknown-channel".to_string(),
                };
                let start = inline.text.len();
                push_marked(inline, &label, Style::Mention);
                if name.is_some() {
                    inline.links.push(Link {
                        range: start..inline.text.len(),
                        url: format!("{CHANNEL_LINK}{id}"),
                        masked: false,
                    });
                }
                return;
            }
            let label = match kind {
                MentionKind::User { id } => {
                    format!(
                        "@{}",
                        lookup(|n| n.users.get(id).cloned())
                            .unwrap_or_else(|| "unknown-user".into())
                    )
                }
                MentionKind::Channel { .. } => unreachable!("handled above"),
                MentionKind::Role { id } => {
                    format!(
                        "@{}",
                        lookup(|n| n.roles.get(id).cloned())
                            .unwrap_or_else(|| "unknown-role".into())
                    )
                }
                MentionKind::Command { name, .. } => format!("/{name}"),
                MentionKind::GuildNavigation { .. } => "#navigation".into(),
                MentionKind::Everyone => "@everyone".into(),
                MentionKind::Here => "@here".into(),
            };
            push_marked(inline, &label, Style::Mention);
        }
        Node::Timestamp { timestamp, .. } => {
            let text = i64::try_from(*timestamp)
                .ok()
                .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
                .map(|t| {
                    t.with_timezone(&chrono::Local)
                        .format("%Y-%m-%d %H:%M")
                        .to_string()
                })
                .unwrap_or_else(|| timestamp.to_string());
            push_marked(inline, &text, Style::Code);
        }
        Node::Emoji { kind } => match kind {
            EmojiKind::Standard { raw, .. } => inline.text.push_str(raw),
            EmojiKind::Custom { name, .. } => {
                inline.text.push(':');
                inline.text.push_str(name);
                inline.text.push(':');
            }
        },
        // Block nodes inside inline context: fall back to their text.
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
        Node::Table { .. } | Node::TableRow { .. } => {}
    }
}

pub fn mono_font() -> &'static str {
    if cfg!(target_os = "macos") {
        "Menlo"
    } else if cfg!(target_os = "windows") {
        "Consolas"
    } else {
        "DejaVu Sans Mono"
    }
}

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
}

fn highlight(style: Style, revealed: bool) -> HighlightStyle {
    match style {
        Style::Bold => HighlightStyle {
            font_weight: Some(FontWeight::BOLD),
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
            background_color: Some(theme::code_bg()),
            ..Default::default()
        },
        Style::Spoiler if revealed => HighlightStyle {
            background_color: Some(theme::code_bg()),
            ..Default::default()
        },
        Style::Spoiler => HighlightStyle {
            background_color: Some(theme::rail_bg()),
            color: Some(theme::rail_bg()),
            underline: None,
            ..Default::default()
        },
        Style::Link => HighlightStyle {
            color: Some(theme::link()),
            underline: Some(UnderlineStyle {
                thickness: px(1.),
                ..Default::default()
            }),
            ..Default::default()
        },
        Style::Edited => HighlightStyle {
            color: Some(theme::text_muted()),
            ..Default::default()
        },
        Style::LinkTarget => HighlightStyle {
            color: Some(theme::text_muted()),
            ..Default::default()
        },
        Style::Mention => HighlightStyle {
            background_color: Some(theme::mention_fill()),
            color: Some(theme::mention_text()),
            ..Default::default()
        },
    }
}

/// Merges overlapping highlights: gpui wants sorted, non-overlapping ranges. A hidden
/// spoiler wins over everything inside it, so nothing leaks through its colour.
fn flatten(inline: &Inline, revealed: bool) -> Vec<(Range<usize>, HighlightStyle)> {
    if inline.highlights.is_empty() {
        return Vec::new();
    }
    let mut cuts: Vec<usize> = inline
        .highlights
        .iter()
        .flat_map(|(r, _)| [r.start, r.end])
        .collect();
    cuts.sort_unstable();
    cuts.dedup();
    let mut out = Vec::new();
    for w in cuts.windows(2) {
        let (a, b) = (w[0], w[1]);
        let covering: Vec<Style> = inline
            .highlights
            .iter()
            .filter(|(r, _)| r.start <= a && b <= r.end)
            .map(|(_, s)| *s)
            .collect();
        if covering.is_empty() || a >= b {
            continue;
        }
        let style = if !revealed && covering.contains(&Style::Spoiler) {
            highlight(Style::Spoiler, false)
        } else {
            covering.iter().fold(HighlightStyle::default(), |acc, s| {
                acc.highlight(highlight(*s, revealed))
            })
        };
        out.push((a..b, style));
    }
    out
}

fn overlaps(a: &Range<usize>, b: &Range<usize>) -> bool {
    a.start < b.end && b.start < a.end
}

/// What a click on a range of an inline does.
enum Click {
    Link(String, bool),
    Reveal,
}

fn clickables(inline: &Inline, revealed: bool) -> Vec<(Range<usize>, Click)> {
    let spoilers: Vec<&Range<usize>> = inline
        .highlights
        .iter()
        .filter(|(_, s)| *s == Style::Spoiler)
        .map(|(r, _)| r)
        .collect();
    let mut out = Vec::new();
    for link in &inline.links {
        // Links inside a hidden spoiler stay inert until it is revealed.
        if !revealed && spoilers.iter().any(|s| overlaps(s, &link.range)) {
            continue;
        }
        out.push((
            link.range.clone(),
            Click::Link(link.url.clone(), link.masked),
        ));
    }
    if !revealed {
        for s in spoilers {
            out.push((s.clone(), Click::Reveal));
        }
    }
    out.sort_by_key(|(r, _)| r.start);
    out
}

fn inline_element(id: SharedString, inline: &Inline, ctx: &RenderCtx) -> AnyElement {
    let text = StyledText::new(SharedString::from(inline.text.clone()))
        .with_highlights(flatten(inline, ctx.revealed));
    let clicks = clickables(inline, ctx.revealed);
    if clicks.is_empty() {
        return div().child(text).into_any_element();
    }
    let ranges: Vec<Range<usize>> = clicks.iter().map(|(r, _)| r.clone()).collect();
    let actions: Vec<Click> = clicks.into_iter().map(|(_, c)| c).collect();
    let ctx = ctx.clone();
    div()
        .child(
            InteractiveText::new(id, text).on_click(ranges, move |ix, window, cx| {
                match actions.get(ix) {
                    Some(Click::Link(url, masked)) => (ctx.on_link)(url, *masked, window, cx),
                    Some(Click::Reveal) => (ctx.on_reveal)(window, cx),
                    None => {}
                }
            }),
        )
        .into_any_element()
}

/// Draws the blocks of one message. `id` must be unique per message.
pub fn render(id: &str, blocks: &[Block], ctx: &RenderCtx) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .children(
            blocks
                .iter()
                .enumerate()
                .map(|(i, b)| render_block(&format!("{id}-{i}"), b, ctx)),
        )
        .into_any_element()
}

fn render_block(id: &str, block: &Block, ctx: &RenderCtx) -> AnyElement {
    match block {
        Block::Text(inline) => inline_element(id.to_string().into(), inline, ctx),
        Block::Heading(level, inline) => div()
            .font_weight(FontWeight::BOLD)
            .text_size(px(match level {
                1 => 22.,
                2 => 19.,
                _ => 16.,
            }))
            .child(inline_element(id.to_string().into(), inline, ctx))
            .into_any_element(),
        Block::Subtext(inline) => div()
            .text_size(px(12.))
            .text_color(theme::text_muted())
            .child(inline_element(id.to_string().into(), inline, ctx))
            .into_any_element(),
        Block::Code { content } => div()
            .bg(theme::code_bg())
            .border_1()
            .border_color(theme::rail_bg())
            .rounded_md()
            .p_2()
            .font_family(mono_font())
            .text_size(px(13.))
            .child(content.clone())
            .into_any_element(),
        Block::Quote(children) => div()
            .flex()
            .flex_row()
            .gap_2()
            .child(div().w(px(4.)).rounded_sm().bg(theme::quote_bar()))
            .child(div().flex_1().child(render(id, children, ctx)))
            .into_any_element(),
        Block::Alert(label, children) => div()
            .flex()
            .flex_row()
            .gap_2()
            .child(div().w(px(4.)).rounded_sm().bg(theme::accent()))
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .child(div().font_weight(FontWeight::BOLD).child(*label))
                    .child(render(id, children, ctx)),
            )
            .into_any_element(),
        Block::List(items) => div()
            .flex()
            .flex_col()
            .children(items.iter().enumerate().map(|(i, (marker, children))| {
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(div().text_color(theme::text_muted()).child(marker.clone()))
                    .child(
                        div()
                            .flex_1()
                            .child(render(&format!("{id}-l{i}"), children, ctx)),
                    )
            }))
            .into_any_element(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn first_inline(blocks: &[Block]) -> &Inline {
        match &blocks[0] {
            Block::Text(inline) => inline,
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn parses_bold_code_and_link() {
        let blocks = parse("hello **world** `x` https://fluxer.app");
        let inline = first_inline(&blocks);
        assert!(inline.text.starts_with("hello world x"));
        assert!(inline
            .highlights
            .iter()
            .any(|(r, s)| *s == Style::Bold && &inline.text[r.clone()] == "world"));
        assert!(inline
            .highlights
            .iter()
            .any(|(r, s)| *s == Style::Code && &inline.text[r.clone()] == "x"));
        assert_eq!(inline.links.len(), 1);
        assert_eq!(inline.links[0].url, "https://fluxer.app");
        assert!(!inline.links[0].masked);
        assert!(!inline.text.contains("(fluxer.app)"));
    }

    #[test]
    fn masked_link_shows_its_host() {
        let blocks = parse("[Confirm your account](https://fluxer-login.example/x)");
        let inline = first_inline(&blocks);
        assert!(inline.links[0].masked);
        assert_eq!(inline.text, "Confirm your account (fluxer-login.example)");
    }

    #[test]
    fn link_target_is_punycode_for_unicode_hosts() {
        assert_eq!(
            link_target_label("https://flüxer.app/login"),
            "xn--flxer-lva.app"
        );
        assert_eq!(link_target_label("https://fluxer.app"), "fluxer.app");
        assert_eq!(link_target_label("not a url"), "invalid link");
    }

    #[test]
    fn masked_detection() {
        assert!(!is_masked("https://fluxer.app/", "https://fluxer.app"));
        assert!(is_masked("fluxer.app", "https://evil.example"));
    }

    #[test]
    fn lookalike_and_userinfo_links_need_confirmation() {
        assert!(is_deceptive("https://fluxer.\u{430}pp/login"));
        assert!(is_deceptive("https://xn--flxer-lva.app/"));
        assert!(is_deceptive("https://fluxer.app@evil.example/"));
        assert!(!is_deceptive("https://fluxer.app/channels"));
        let blocks = parse("<https://fluxer.app@evil.example>");
        let inline = first_inline(&blocks);
        assert!(inline.links[0].masked);
        assert!(inline.text.contains("(evil.example)"));
    }

    #[test]
    fn mentions_resolve_to_names() {
        let mut names = Names::default();
        names
            .channels
            .insert("1559859109744869376".into(), "general".into());
        names.users.insert("42".into(), "ada".into());
        names.roles.insert("7".into(), "Mods".into());
        let blocks = parse_with(
            "see <#1559859109744869376> <@42> <@!42> <@&7> <#999> <@1>",
            &names,
        );
        let inline = first_inline(&blocks);
        assert_eq!(
            inline.text,
            "see #general @ada @ada @Mods #unknown-channel @unknown-user"
        );
        assert_eq!(inline.links.len(), 1);
        assert_eq!(
            inline.links[0].url,
            format!("{CHANNEL_LINK}1559859109744869376")
        );
        // Names do not leak into the next parse.
        let plain = parse("<#1559859109744869376>");
        assert_eq!(first_inline(&plain).text, "#unknown-channel");
    }

    #[test]
    fn custom_emoji_show_their_name() {
        let blocks = parse("hi <:wave:123456789012345678>");
        assert_eq!(first_inline(&blocks).text, "hi :wave:");
    }

    #[test]
    fn edited_mark_goes_after_the_text() {
        let mut blocks = parse("hello");
        mark_edited(&mut blocks);
        assert_eq!(first_inline(&blocks).text, "hello (edited)");
    }

    #[test]
    fn only_http_links_open() {
        assert!(openable("https://fluxer.app").is_some());
        assert!(openable("http://example.com").is_some());
        assert!(openable("file:///etc/passwd").is_none());
        assert!(openable("fluxer://handoff").is_none());
        assert!(openable("javascript:alert(1)").is_none());
    }

    #[test]
    fn code_block_and_quote() {
        let blocks = parse("before\n```rust\nfn main() {}\n```\n> quoted");
        assert!(blocks
            .iter()
            .any(|b| matches!(b, Block::Code { content } if content.contains("fn main"))));
        assert!(blocks.iter().any(|b| matches!(b, Block::Quote(_))));
    }

    #[test]
    fn overlapping_highlights_are_flattened() {
        let blocks = parse("***both*** plain");
        let flat = flatten(first_inline(&blocks), false);
        for w in flat.windows(2) {
            assert!(w[0].0.end <= w[1].0.start);
        }
    }

    #[test]
    fn links_in_hidden_spoilers_are_inert() {
        let blocks = parse("||[click](https://example.com)||");
        let inline = first_inline(&blocks);
        let hidden = clickables(inline, false);
        assert!(hidden.iter().all(|(_, c)| matches!(c, Click::Reveal)));
        let shown = clickables(inline, true);
        assert!(shown.iter().any(|(_, c)| matches!(c, Click::Link(..))));
    }
}
