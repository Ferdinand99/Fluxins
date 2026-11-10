//! Renders message markdown with Fluxer's own parser (`fluxer_markdown_parser`, from the
//! Fluxer repository, AGPL-3.0-or-later), so a message is split into the same nodes as in
//! Fluxer's clients. v0 draws it simply: styled text runs, code blocks, quotes and lists.

use std::ops::Range;

use fluxer_markdown_parser::ast::{AlertType, EmojiKind, ListItem, MentionKind};
use fluxer_markdown_parser::{EmojiContext, MarkdownParser, Node, ParserFlags};
use gpui::{
    AnyElement, FontStyle, FontWeight, HighlightStyle, InteractiveText, SharedString,
    StrikethroughStyle, StyledText, UnderlineStyle, div, prelude::*, px,
};

use crate::theme;

/// One block of a rendered message.
#[derive(Clone, Debug)]
pub enum Block {
    Text(Inline),
    Heading(u8, Inline),
    Subtext(Inline),
    Code { language: Option<String>, content: SharedString },
    Quote(Vec<Block>),
    Alert(&'static str, Vec<Block>),
    List(Vec<(String, Vec<Block>)>),
}

/// Text with styled ranges and clickable links.
#[derive(Clone, Debug, Default)]
pub struct Inline {
    pub text: String,
    pub highlights: Vec<(Range<usize>, Style)>,
    pub links: Vec<(Range<usize>, String)>,
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
}

pub fn parse(input: &str) -> Vec<Block> {
    let flags = ParserFlags::ALL;
    let mut parser = MarkdownParser::new(flags, EmojiContext::default());
    match parser.parse(input) {
        Ok(nodes) => blocks(&nodes),
        Err(err) => {
            log::debug!("markdown parse failed, showing plain text: {err}");
            vec![Block::Text(Inline { text: input.to_string(), ..Default::default() })]
        }
    }
}

fn blocks(nodes: &[Node]) -> Vec<Block> {
    let mut out = Vec::new();
    let mut current = Inline::default();
    let flush = |current: &mut Inline, out: &mut Vec<Block>| {
        let trimmed_len = current.text.trim_end_matches('\n').len();
        current.text.truncate(trimmed_len);
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
            Node::CodeBlock { language, content } => {
                flush(&mut current, &mut out);
                out.push(Block::Code {
                    language: language.clone(),
                    content: content.trim_end_matches('\n').to_string().into(),
                });
            }
            Node::Blockquote { children, .. } => {
                flush(&mut current, &mut out);
                out.push(Block::Quote(blocks(children)));
            }
            Node::Alert { alert_type, children } => {
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
                out.push(Block::Code { language: None, content: inline.text.into() });
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
            let marker = if ordered { format!("{}.", item.ordinal.unwrap_or(i + 1)) } else { "\u{2022}".into() };
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

fn push_inline(node: &Node, inline: &mut Inline) {
    match node {
        Node::Text { content } => inline.text.push_str(content),
        Node::Strong { children } => styled(children, Style::Bold, inline),
        Node::Emphasis { children } => styled(children, Style::Italic, inline),
        Node::Underline { children } => styled(children, Style::Underline, inline),
        Node::Strikethrough { children } => styled(children, Style::Strike, inline),
        Node::Spoiler { children, .. } => styled(children, Style::Spoiler, inline),
        Node::Sequence { children } => children.iter().for_each(|c| push_inline(c, inline)),
        Node::InlineCode { content } => {
            let start = inline.text.len();
            inline.text.push_str(content);
            inline.highlights.push((start..inline.text.len(), Style::Code));
        }
        Node::Link { text, url, .. } => {
            let start = inline.text.len();
            match text {
                Some(t) => push_inline(t, inline),
                None => inline.text.push_str(url),
            }
            let range = start..inline.text.len();
            inline.highlights.push((range.clone(), Style::Link));
            inline.links.push((range, url.clone()));
        }
        Node::Mention { kind } => {
            let label = match kind {
                MentionKind::User { id } => format!("@{}", short_id(id)),
                MentionKind::Channel { id } => format!("#{}", short_id(id)),
                MentionKind::Role { id } => format!("@role-{}", short_id(id)),
                MentionKind::Command { name, .. } => format!("/{name}"),
                MentionKind::GuildNavigation { .. } => "#navigation".into(),
                MentionKind::Everyone => "@everyone".into(),
                MentionKind::Here => "@here".into(),
            };
            let start = inline.text.len();
            inline.text.push_str(&label);
            inline.highlights.push((start..inline.text.len(), Style::Mention));
        }
        Node::Timestamp { timestamp, .. } => {
            let text = chrono::DateTime::from_timestamp(*timestamp as i64, 0)
                .map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_else(|| timestamp.to_string());
            let start = inline.text.len();
            inline.text.push_str(&text);
            inline.highlights.push((start..inline.text.len(), Style::Code));
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
        Node::CodeBlock { content, .. } => {
            let start = inline.text.len();
            inline.text.push_str(content);
            inline.highlights.push((start..inline.text.len(), Style::Code));
        }
        Node::List { items, .. } => {
            for item in items {
                inline.text.push_str("\n\u{2022} ");
                item.children.iter().for_each(|c| push_inline(c, inline));
            }
        }
        Node::Table { .. } | Node::TableRow { .. } => {}
    }
}

fn short_id(id: &str) -> &str {
    id
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

fn highlight(style: Style) -> HighlightStyle {
    match style {
        Style::Bold => HighlightStyle { font_weight: Some(FontWeight::BOLD), ..Default::default() },
        Style::Italic => HighlightStyle { font_style: Some(FontStyle::Italic), ..Default::default() },
        Style::Underline => HighlightStyle {
            underline: Some(UnderlineStyle { thickness: px(1.), ..Default::default() }),
            ..Default::default()
        },
        Style::Strike => HighlightStyle {
            strikethrough: Some(StrikethroughStyle { thickness: px(1.), ..Default::default() }),
            ..Default::default()
        },
        Style::Code => HighlightStyle { background_color: Some(theme::code_bg()), ..Default::default() },
        Style::Spoiler => HighlightStyle {
            background_color: Some(theme::rail_bg()),
            color: Some(theme::rail_bg()),
            ..Default::default()
        },
        Style::Link => HighlightStyle {
            color: Some(theme::link()),
            underline: Some(UnderlineStyle { thickness: px(1.), ..Default::default() }),
            ..Default::default()
        },
        Style::Mention => HighlightStyle {
            background_color: Some(theme::mention_bg()),
            color: Some(theme::text_strong()),
            ..Default::default()
        },
    }
}

/// Merges overlapping highlights: gpui wants sorted, non-overlapping ranges.
fn flatten(inline: &Inline) -> Vec<(Range<usize>, HighlightStyle)> {
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
        let mut style = HighlightStyle::default();
        let mut any = false;
        for (r, s) in &inline.highlights {
            if r.start <= a && b <= r.end {
                style = style.highlight(highlight(*s));
                any = true;
            }
        }
        if any && a < b {
            out.push((a..b, style));
        }
    }
    out
}

fn inline_element(id: SharedString, inline: &Inline) -> AnyElement {
    let text = StyledText::new(SharedString::from(inline.text.clone())).with_highlights(flatten(inline));
    if inline.links.is_empty() {
        return div().child(text).into_any_element();
    }
    let ranges: Vec<Range<usize>> = inline.links.iter().map(|(r, _)| r.clone()).collect();
    let urls: Vec<String> = inline.links.iter().map(|(_, u)| u.clone()).collect();
    div()
        .child(InteractiveText::new(id, text).on_click(ranges, move |ix, _window, cx| {
            if let Some(url) = urls.get(ix) {
                if url.starts_with("https://") || url.starts_with("http://") {
                    cx.open_url(url);
                }
            }
        }))
        .into_any_element()
}

/// Draws the blocks of one message. `id` must be unique per message.
pub fn render(id: &str, blocks: &[Block]) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .children(blocks.iter().enumerate().map(|(i, b)| render_block(&format!("{id}-{i}"), b)))
        .into_any_element()
}

fn render_block(id: &str, block: &Block) -> AnyElement {
    match block {
        Block::Text(inline) => inline_element(id.to_string().into(), inline),
        Block::Heading(level, inline) => div()
            .font_weight(FontWeight::BOLD)
            .text_size(px(match level {
                1 => 22.,
                2 => 19.,
                _ => 16.,
            }))
            .child(inline_element(id.to_string().into(), inline))
            .into_any_element(),
        Block::Subtext(inline) => div()
            .text_size(px(12.))
            .text_color(theme::text_muted())
            .child(inline_element(id.to_string().into(), inline))
            .into_any_element(),
        Block::Code { content, .. } => div()
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
            .child(div().flex_1().child(render(id, children)))
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
                    .child(render(id, children)),
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
                    .child(div().flex_1().child(render(&format!("{id}-l{i}"), children)))
            }))
            .into_any_element(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bold_code_and_link() {
        let blocks = parse("hello **world** `x` https://fluxer.app");
        let Block::Text(inline) = &blocks[0] else { panic!("expected text, got {blocks:?}") };
        assert!(inline.text.starts_with("hello world x"));
        assert!(inline.highlights.iter().any(|(r, s)| *s == Style::Bold && &inline.text[r.clone()] == "world"));
        assert!(inline.highlights.iter().any(|(r, s)| *s == Style::Code && &inline.text[r.clone()] == "x"));
        assert_eq!(inline.links.len(), 1);
        assert_eq!(inline.links[0].1, "https://fluxer.app");
    }

    #[test]
    fn code_block_and_quote() {
        let blocks = parse("before\n```rust\nfn main() {}\n```\n> quoted");
        assert!(blocks.iter().any(|b| matches!(b, Block::Code { content, .. } if content.contains("fn main"))));
        assert!(blocks.iter().any(|b| matches!(b, Block::Quote(_))));
    }

    #[test]
    fn overlapping_highlights_are_flattened() {
        let blocks = parse("***both*** plain");
        let Block::Text(inline) = &blocks[0] else { panic!() };
        let flat = flatten(inline);
        for w in flat.windows(2) {
            assert!(w[0].0.end <= w[1].0.start);
        }
    }
}
