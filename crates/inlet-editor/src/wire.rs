// SPDX-License-Identifier: AGPL-3.0-or-later
//! The text Fluxer receives, and what Fluxer reads from it.
//!
//! [`to_wire`] turns the display text and the tokens into wire text. Text the user typed
//! that looks like a mention is escaped, so the server never sees a mention the user did not
//! pick. The rules follow the markdown parser Fluxer uses (`fluxer_markdown_parser`, the same
//! commit as the app). [`server_mentions`] is a port of the server's own mention extraction,
//! and is used to check the result.

use std::collections::{BTreeSet, HashSet};

use fluxer_markdown_parser::ast::MentionKind;
use fluxer_markdown_parser::{EmojiContext, MarkdownParser, Node, ParserFlags};
use linkify::{LinkFinder, LinkKind};

use crate::sanitize::{js_trim, server_strips};
use crate::tokens::{special_at, Id, Special, Token, TokenSpan};

/// The mentions Fluxer finds in a message.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Mentions {
    pub users: BTreeSet<u64>,
    pub roles: BTreeSet<u64>,
    pub channels: BTreeSet<u64>,
    pub everyone: bool,
    pub here: bool,
}

impl Mentions {
    /// The mentions the user picked: what `to_wire` is meant to send, and nothing else.
    /// Ids the server would not read (zero, or above `i64::MAX`) are left out.
    pub fn picked(tokens: &[TokenSpan]) -> Mentions {
        let mut m = Mentions::default();
        let valid = |id: &Id| id.0 != 0 && id.0 <= i64::MAX as u64;
        for t in tokens {
            match &t.token {
                Token::User(id) if valid(id) => {
                    m.users.insert(id.0);
                }
                Token::Role(id) if valid(id) => {
                    m.roles.insert(id.0);
                }
                Token::Channel(id) if valid(id) => {
                    m.channels.insert(id.0);
                }
                Token::Everyone => m.everyone = true,
                Token::Here => m.here = true,
                _ => {}
            }
        }
        m
    }
}

/// What to send as `allowed_mentions`: the mentions the user picked, so the server drops any
/// other mention whatever the text says.
///
/// Send `users` and `roles` as lists and leave `parse` out. Add `parse: ["everyone"]` only
/// when `everyone` is true. The server does not accept `parse` together with the lists
/// (`PARSE_AND_USERS_OR_ROLES_CANNOT_BE_USED_TOGETHER`), so with an everyone mention and
/// picked users or roles at once, send `parse: ["everyone", "users", "roles"]` and rely on
/// the escaping in [`to_wire`] for the rest.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AllowedMentions {
    pub users: Vec<Id>,
    pub roles: Vec<Id>,
    /// An `@everyone` or `@here` token was picked.
    pub everyone: bool,
}

impl AllowedMentions {
    pub(crate) fn from_tokens(tokens: &[TokenSpan]) -> AllowedMentions {
        let mut users = BTreeSet::new();
        let mut roles = BTreeSet::new();
        let mut everyone = false;
        for t in tokens {
            match &t.token {
                Token::User(id) => {
                    users.insert(*id);
                }
                Token::Role(id) => {
                    roles.insert(*id);
                }
                Token::Everyone | Token::Here => everyone = true,
                _ => {}
            }
        }
        AllowedMentions {
            users: users.into_iter().collect(),
            roles: roles.into_iter().collect(),
            everyone,
        }
    }
}

// --- building the wire text ---------------------------------------------------------------

enum Mode<'a> {
    /// Escape every look-alike.
    Escape,
    /// Replace the first character of every look-alike with a marker, to find out which of
    /// them the parser reads as code or as part of a link.
    Probe,
    /// Escape every look-alike except the listed ones.
    Selective(&'a HashSet<usize>),
}

/// The most look-alikes that can be marked. Markers are private-use characters.
const MAX_MARKERS: usize = 0x1800;

fn marker(site: usize) -> char {
    char::from_u32(0xe000 + site as u32).unwrap_or('\u{e000}')
}

fn trailing_backslashes(s: &str) -> usize {
    s.bytes().rev().take_while(|b| *b == b'\\').count()
}

fn push_backslashes(out: &mut String, n: usize) {
    out.extend(std::iter::repeat_n('\\', n));
}

/// Returns the wire text and the number of look-alikes found in the typed text.
fn build(text: &str, tokens: &[TokenSpan], mode: &Mode) -> (String, usize) {
    let mut out = String::with_capacity(text.len() + 16);
    let mut sites = 0;
    let mut pos = 0;
    let mut emit_plain = |out: &mut String, plain: &str| -> usize {
        // Work on the text the server will see: without the characters it removes.
        let cleaned: std::borrow::Cow<str> = if plain.chars().any(server_strips) {
            plain
                .chars()
                .filter(|c| !server_strips(*c))
                .collect::<String>()
                .into()
        } else {
            plain.into()
        };
        for (i, ch) in cleaned.char_indices() {
            if ch == '<' || ch == '@' {
                if let Some(kind) = special_at(&cleaned[i..]) {
                    let site = sites;
                    sites += 1;
                    match mode {
                        Mode::Probe => {
                            out.push(marker(site));
                            continue;
                        }
                        Mode::Selective(skip) if skip.contains(&site) => {}
                        _ => match kind {
                            Special::Angle => {
                                // The backslashes before it are already in `out`. Doubling
                                // them makes an even run; one more escapes the `<`.
                                push_backslashes(out, trailing_backslashes(&cleaned[..i]));
                                out.push('\\');
                            }
                            Special::At => out.push('\\'),
                        },
                    }
                }
            }
            out.push(ch);
        }
        trailing_backslashes(&cleaned)
    };
    for span in tokens {
        let trailing = emit_plain(&mut out, &text[pos..span.range.start]);
        if span.token.wire_starts_with_angle() {
            push_backslashes(&mut out, trailing);
        }
        out.push_str(&span.token.to_wire());
        pos = span.range.end;
    }
    emit_plain(&mut out, &text[pos..]);
    (out, sites)
}

/// The wire text for a buffer.
///
/// Every look-alike in typed text is escaped, except those the parser reads as code (inline
/// code and code blocks) or as part of a link, where a backslash would show. The result is
/// checked with [`server_mentions`]. If the mentions Fluxer would find differ from the
/// picked ones in any way, every look-alike is escaped instead.
pub(crate) fn to_wire(text: &str, tokens: &[TokenSpan]) -> String {
    let (all, sites) = build(text, tokens, &Mode::Escape);
    if sites == 0 || sites > MAX_MARKERS {
        return all;
    }
    // With `@everyone` or `@here` picked together with users or roles, the server can only be
    // told `parse: ["everyone", "users", "roles"]`, and no list limits the mentions. Then
    // only the escapes keep unpicked mentions out, so none may be left out.
    let picked = Mentions::picked(tokens);
    if (picked.everyone || picked.here) && !(picked.users.is_empty() && picked.roles.is_empty()) {
        return all;
    }
    let (probe, _) = build(text, tokens, &Mode::Probe);
    let Some(code_sites) = code_sites(&probe) else {
        return all;
    };
    if code_sites.is_empty() {
        return all;
    }
    let (selective, _) = build(text, tokens, &Mode::Selective(&code_sites));
    if server_mentions(&selective) == picked {
        selective
    } else {
        all
    }
}

fn parse(input: &str) -> Option<Vec<Node>> {
    let mut parser = MarkdownParser::new(ParserFlags::ALL, EmojiContext::default());
    parser.parse(input).ok()
}

/// The look-alikes (by number) that the parser puts into code or a link address.
fn code_sites(probe: &str) -> Option<HashSet<usize>> {
    // The server parses the trimmed content.
    let nodes = parse(js_trim(probe))?;
    let mut found = HashSet::new();
    for node in &nodes {
        visit(node, &mut found);
    }
    Some(found)
}

fn note_markers(s: &str, found: &mut HashSet<usize>) {
    for c in s.chars() {
        let n = c as u32;
        if (0xe000..0xe000 + MAX_MARKERS as u32).contains(&n) {
            found.insert((n - 0xe000) as usize);
        }
    }
}

fn visit(node: &Node, found: &mut HashSet<usize>) {
    match node {
        Node::CodeBlock { content, .. } | Node::InlineCode { content } => {
            note_markers(content, found);
        }
        Node::Link {
            text, url, raw_url, ..
        } => {
            note_markers(url, found);
            note_markers(raw_url, found);
            if let Some(text) = text {
                visit(text, found);
            }
        }
        Node::Blockquote { children, .. }
        | Node::Strong { children }
        | Node::Emphasis { children }
        | Node::Underline { children }
        | Node::Strikethrough { children }
        | Node::Heading { children, .. }
        | Node::Subtext { children }
        | Node::Sequence { children }
        | Node::TableCell { children }
        | Node::Alert { children, .. }
        | Node::Spoiler { children, .. } => children.iter().for_each(|c| visit(c, found)),
        Node::List { items, .. } => items
            .iter()
            .flat_map(|item| item.children.iter())
            .for_each(|c| visit(c, found)),
        Node::Table { header, rows, .. } => {
            visit(header, found);
            rows.iter().for_each(|r| visit(r, found));
        }
        Node::TableRow { cells } => cells.iter().for_each(|c| visit(c, found)),
        Node::Text { .. } | Node::Mention { .. } | Node::Timestamp { .. } | Node::Emoji { .. } => {}
    }
}

// --- what the server reads --------------------------------------------------------------

/// The mentions Fluxer finds in message content. A port of the server's extraction
/// (`fluxer_messages::mention_extractor`) with its normalization of the content first:
/// U+202E and U+000C removed, then trimmed.
pub fn server_mentions(content: &str) -> Mentions {
    let normalized: String = content.chars().filter(|c| !server_strips(*c)).collect();
    let trimmed = js_trim(&normalized);
    let cleaned = blank_raw_urls(trimmed);
    let mut mentions = Mentions::default();
    if let Some(nodes) = parse(&cleaned) {
        for node in &nodes {
            collect(node, &mut mentions);
        }
    }
    mentions
}

fn collect(node: &Node, m: &mut Mentions) {
    match node {
        Node::Mention { kind } => match kind {
            MentionKind::User { id } => insert(id, &mut m.users),
            MentionKind::Role { id } => insert(id, &mut m.roles),
            MentionKind::Channel { id } => insert(id, &mut m.channels),
            MentionKind::Everyone => m.everyone = true,
            MentionKind::Here => m.here = true,
            _ => {}
        },
        Node::Blockquote { children, .. }
        | Node::Strong { children }
        | Node::Emphasis { children }
        | Node::Underline { children }
        | Node::Strikethrough { children }
        | Node::Heading { children, .. }
        | Node::Subtext { children }
        | Node::Sequence { children }
        | Node::TableCell { children }
        | Node::Alert { children, .. }
        | Node::Spoiler { children, .. } => children.iter().for_each(|c| collect(c, m)),
        Node::List { items, .. } => items
            .iter()
            .flat_map(|item| item.children.iter())
            .for_each(|c| collect(c, m)),
        Node::Link { text, .. } => {
            if let Some(text) = text {
                collect(text, m);
            }
        }
        Node::Table { header, rows, .. } => {
            collect(header, m);
            rows.iter().for_each(|r| collect(r, m));
        }
        Node::TableRow { cells } => cells.iter().for_each(|c| collect(c, m)),
        Node::Text { .. }
        | Node::CodeBlock { .. }
        | Node::InlineCode { .. }
        | Node::Timestamp { .. }
        | Node::Emoji { .. } => {}
    }
}

fn insert(id: &str, target: &mut BTreeSet<u64>) {
    if let Ok(id) = id.parse::<i64>() {
        if id != 0 {
            target.insert(id as u64);
        }
    }
}

/// Raw URLs are blanked before the server looks for mentions: from the start of the URL to
/// the next white space.
fn blank_raw_urls(input: &str) -> std::borrow::Cow<'_, str> {
    let mut finder = LinkFinder::new();
    finder.kinds(&[LinkKind::Url]);
    let mut output: Option<String> = None;
    for link in finder.links(input) {
        let end = input[link.end()..]
            .char_indices()
            .find_map(|(offset, ch)| ch.is_whitespace().then_some(link.end() + offset))
            .unwrap_or(input.len());
        let output = output.get_or_insert_with(|| input.to_owned());
        output.replace_range(link.start()..end, &" ".repeat(end - link.start()));
    }
    output.map_or(std::borrow::Cow::Borrowed(input), std::borrow::Cow::Owned)
}
