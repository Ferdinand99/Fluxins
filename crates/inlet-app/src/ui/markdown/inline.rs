// SPDX-License-Identifier: AGPL-3.0-or-later
//! Inline nodes: text, styles, links, mentions, timestamps and emoji.

use fluxer_markdown_parser::ast::{EmojiKind, MentionKind};
use fluxer_markdown_parser::Node;

use super::parse::lookup;
use super::{is_deceptive, is_masked, link_target_label, Inline, Link, Style, CHANNEL_LINK};

pub(super) fn inline_of(children: &[Node]) -> Inline {
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

pub(super) fn push_marked(inline: &mut Inline, text: &str, style: Style) {
    let start = inline.text.len();
    inline.text.push_str(text);
    inline.highlights.push((start..inline.text.len(), style));
}

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
        Node::Mention { kind } => {
            if let MentionKind::Channel { id } = kind {
                let name = lookup(|n| n.channels.get(id).cloned());
                let label = match &name {
                    Some(name) => format!("#{name}"),
                    None => t!("markdown-unknown-channel"),
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
                            .unwrap_or_else(|| t!("markdown-unknown-user"))
                    )
                }
                MentionKind::Channel { .. } => unreachable!("handled above"),
                MentionKind::Role { id } => {
                    format!(
                        "@{}",
                        lookup(|n| n.roles.get(id).cloned())
                            .unwrap_or_else(|| t!("markdown-unknown-role"))
                    )
                }
                MentionKind::Command { name, .. } => format!("/{name}"),
                MentionKind::GuildNavigation { .. } => t!("markdown-guild-navigation"),
                MentionKind::Everyone => t!("markdown-everyone"),
                MentionKind::Here => t!("markdown-here"),
            };
            push_marked(inline, &label, Style::Mention);
        }
        Node::Timestamp { timestamp, .. } => {
            let text = i64::try_from(*timestamp)
                .ok()
                .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
                .map(|t| {
                    t.with_timezone(&chrono::Local)
                        .format(&t!("timestamp-format"))
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
