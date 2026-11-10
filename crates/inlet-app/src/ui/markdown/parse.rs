// SPDX-License-Identifier: AGPL-3.0-or-later
//! Turns the parser's nodes into [`Block`]s.

use std::cell::RefCell;

use fluxer_markdown_parser::ast::{AlertType, ListItem};
use fluxer_markdown_parser::{EmojiContext, MarkdownParser, Node, ParserFlags};

use super::inline::{inline_of, push_inline, push_marked};
use super::{Block, Inline, Style};
use crate::model::Names;

thread_local! {
    /// Names for the parse running on this thread (set by [`parse_with`]).
    static NAMES: RefCell<Names> = RefCell::new(Names::default());
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
                    AlertType::Note => t!("markdown-alert-note"),
                    AlertType::Tip => t!("markdown-alert-tip"),
                    AlertType::Important => t!("markdown-alert-important"),
                    AlertType::Warning => t!("markdown-alert-warning"),
                    AlertType::Caution => t!("markdown-alert-caution"),
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
