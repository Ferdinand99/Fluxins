// SPDX-License-Identifier: AGPL-3.0-or-later
//! Message markdown, parsed with Fluxer's own parser (`fluxer_markdown_parser`, from the
//! Fluxer repository, AGPL-3.0-or-later), so a message is split into the same nodes as in
//! Fluxer's clients, and drawn the way Fluxer's `Markup.module.css` draws them.
//!
//! Parsing happens once, on the backend thread ([`parse`] returns plain data that is
//! `Send`); the converted message keeps the result. The UI thread only draws. What can
//! change after parsing is kept apart from the text and worked out when drawn
//! ([`Live`]): mention names (from the account state) and relative timestamps.
//!
//! - `blocks`: the parse entry, nodes to [`Block`]s, and drawing a list of blocks
//! - `inline`: inline nodes to an [`Inline`], and drawing one
//! - `mention`: mention pills and their names
//! - `timestamp`: `<t:…>` timestamps
//! - `emoji`: Unicode emoji as Twemoji, the parser's emoji context, jumbo emoji
//! - `code`: code blocks with highlighting
//! - `table`, `alert`: tables and alerts (`> [!NOTE]`)

mod alert;
mod blocks;
mod code;
mod emoji;
mod inline;
mod mention;
mod table;
mod timestamp;

use std::collections::HashSet;
use std::ops::Range;

use fluxer_markdown_parser::ast::TimestampStyle;
use gpui::SharedString;

pub use blocks::{
    mark_edited, parse, parse_bio_with, parse_embed_with, parse_with, render, RenderCtx,
};
pub use code::{with_message_budget, OffThread};
pub use mention::MentionNames;
pub use timestamp::refresh_due;

pub fn mono_font() -> &'static str {
    crate::assets::mono_font()
}

/// The URLs of the links a message's text hides in spoilers: inside an inline `||…||`
/// (the rule that makes them inert, [`inline::link_in_spoiler`]) or anywhere in a
/// spoiler around whole blocks. The embeds of these links are covered until the
/// message's spoilers are shown.
pub fn spoilered_links(blocks: &[Block]) -> Vec<String> {
    fn all_links(inline: &Inline, out: &mut Vec<String>) {
        out.extend(inline.links.iter().map(|l| l.url.clone()));
    }
    fn hidden_links(inline: &Inline, out: &mut Vec<String>) {
        out.extend(inline::spoilered_links(inline).map(|l| l.url.clone()));
    }
    fn walk(blocks: &[Block], hidden: bool, out: &mut Vec<String>) {
        let inline = |i: &Inline, out: &mut Vec<String>| {
            if hidden {
                all_links(i, out)
            } else {
                hidden_links(i, out)
            }
        };
        for b in blocks {
            match b {
                Block::Text(i) | Block::Heading(_, i) | Block::Subtext(i) => inline(i, out),
                Block::Quote(inner) | Block::Alert(_, inner) => walk(inner, hidden, out),
                Block::List(list) => list.items.iter().for_each(|item| walk(item, hidden, out)),
                Block::Table(t) => t
                    .header
                    .iter()
                    .chain(t.rows.iter().flatten())
                    .for_each(|i| inline(i, out)),
                Block::Spoiler(inner) => walk(inner, true, out),
                Block::Code(_) => {}
            }
        }
    }
    let mut out = Vec::new();
    walk(blocks, false, &mut out);
    out
}

/// Prefix of the internal link a channel mention carries; clicking it opens the channel.
pub const CHANNEL_LINK: &str = "inlet-channel:";

/// One block of a rendered message.
#[derive(Clone, Debug)]
pub enum Block {
    Text(Inline),
    /// `#`, `##` or `###`.
    Heading(u8, Inline),
    /// `-# small text`.
    Subtext(Inline),
    Code(Code),
    Quote(Vec<Block>),
    Alert(AlertKind, Vec<Block>),
    List(List),
    Table(Box<Table>),
    /// A spoiler around whole blocks (`||` over several lines).
    Spoiler(Vec<Block>),
}

#[derive(Clone, Debug)]
pub struct List {
    pub ordered: bool,
    /// The number of the first item of an ordered list.
    pub start: usize,
    pub items: Vec<Vec<Block>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlertKind {
    Note,
    Tip,
    Important,
    Warning,
    Caution,
}

/// A fenced code block.
#[derive(Clone, Debug)]
pub struct Code {
    pub content: SharedString,
    /// The language after the fence, normalised and capped; `None` when there is none
    /// or it is not a plausible name. Fluxer does not show it; kept for the tests and
    /// for code block actions.
    #[allow(dead_code)]
    pub language: Option<String>,
    /// A KaTeX block (`latex`, `tex`, `katex`), shown as code with a label.
    pub math: bool,
    /// The colours, worked out off the UI thread when the block is first drawn; `None`
    /// for a block that stays plain (no known language, or past the size limits).
    pub highlight: Option<std::sync::Arc<code::Highlight>>,
}

/// Highlighting classes, coloured from `theme.rs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodeColor {
    Keyword,
    String,
    Comment,
    Constant,
    Function,
    Type,
    Tag,
}

#[derive(Clone, Debug, Default)]
pub struct Table {
    pub header: Vec<Inline>,
    pub align: Vec<Align>,
    pub rows: Vec<Vec<Inline>>,
    /// Rows or columns past the limits were left out.
    pub cut: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

/// A link in the text.
#[derive(Clone, Debug, PartialEq)]
pub struct Link {
    pub range: Range<usize>,
    pub url: String,
    /// The visible text is not the URL itself: the person cannot see where it goes.
    pub masked: bool,
}

/// Text with styled ranges, clickable links, emoji and pieces worked out when drawn.
#[derive(Clone, Debug, Default)]
pub struct Inline {
    pub text: String,
    pub highlights: Vec<(Range<usize>, Style)>,
    pub links: Vec<Link>,
    /// Mentions and timestamps, sorted and apart. `text` holds a label from parse time
    /// at each range; drawing replaces it with the current one.
    pub live: Vec<Live>,
    /// Emoji at their placeholders in `text`, sorted.
    pub emoji: Vec<(Range<usize>, Emoji)>,
    /// Only emoji and spaces (up to 30): drawn large, as Fluxer's jumbo emoji.
    pub jumbo: bool,
}

/// A piece of text whose label depends on when and where it is drawn.
#[derive(Clone, Debug, PartialEq)]
pub struct Live {
    pub range: Range<usize>,
    pub kind: LiveKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum LiveKind {
    /// `<@id>`; `fallback` is the name the backend knew when parsing.
    User {
        id: String,
        fallback: Option<String>,
    },
    /// `<@&id>`. Only the roles of the message's own server are named.
    Role { id: String },
    /// `<#id>`. Only channels the account can see are named.
    Channel { id: String },
    /// `<t:unix:R>`: a relative time. The other styles do not change and are formatted
    /// once, when parsing.
    Timestamp { unix: i64, style: TimestampStyle },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Emoji {
    /// A Unicode emoji, drawn with Twemoji.
    Standard(String),
    /// A guild emoji (`<:name:id>`, `<a:name:id>` when animated), drawn with its
    /// picture from the media proxy.
    Custom {
        id: String,
        /// Kept for the hover card and copying, which come with the message actions.
        #[allow(dead_code)]
        name: String,
        animated: bool,
    },
}

/// The custom emoji `(id, animated)` in `blocks`, in order, each once. The views fetch
/// their pictures before drawing the text.
pub fn custom_emoji(blocks: &[Block]) -> Vec<(&str, bool)> {
    fn inline<'a>(i: &'a Inline, seen: &mut HashSet<&'a str>, out: &mut Vec<(&'a str, bool)>) {
        for (_, e) in &i.emoji {
            if let Emoji::Custom { id, animated, .. } = e {
                if seen.insert(id.as_str()) {
                    out.push((id.as_str(), *animated));
                }
            }
        }
    }
    fn walk<'a>(blocks: &'a [Block], seen: &mut HashSet<&'a str>, out: &mut Vec<(&'a str, bool)>) {
        for b in blocks {
            match b {
                Block::Text(i) | Block::Heading(_, i) | Block::Subtext(i) => inline(i, seen, out),
                Block::Quote(inner) | Block::Alert(_, inner) | Block::Spoiler(inner) => {
                    walk(inner, seen, out)
                }
                Block::List(list) => list.items.iter().for_each(|item| walk(item, seen, out)),
                Block::Table(t) => t
                    .header
                    .iter()
                    .chain(t.rows.iter().flatten())
                    .for_each(|i| inline(i, seen, out)),
                Block::Code(_) => {}
            }
        }
    }
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    walk(blocks, &mut seen, &mut out);
    out
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
    /// `@everyone`, `@here`, commands: a mention pill with fixed text.
    Mention,
    /// A timestamp that is not a valid date: its number on the timestamp background.
    Timestamp,
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
        Err(_) => t!("markdown-invalid-link"),
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

#[cfg(test)]
mod tests {
    use super::inline::{self, clickables, compose, Click};
    use super::*;
    use crate::model::Names;

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
    fn user_mentions_keep_the_backends_names_as_fallback() {
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
        assert_eq!(inline.live.len(), 6);
        // Drawn without the account state, only user names fall back to the backend's
        // (one table for all servers): roles and channels need the account state,
        // which knows their server.
        let c = compose(inline, &MentionNames::default(), 0);
        assert_eq!(
            c.text,
            "see #unknown-channel @ada @ada @Unknown\u{a0}role #unknown-channel @unknown-user"
        );
        assert!(c.links.is_empty());
        // Names do not leak into the next parse.
        let plain = parse("<@42>");
        assert_eq!(first_inline(&plain).text, "@unknown-user");
    }

    #[test]
    fn known_channels_open_on_click() {
        let blocks = parse("see <#9>");
        let mut now = MentionNames::default();
        now.channels.insert(
            "9".into(),
            mention::ChannelName {
                name: "general".into(),
                pill: true,
            },
        );
        let c = compose(first_inline(&blocks), &now, 0);
        assert_eq!(c.links.len(), 1);
        assert_eq!(c.links[0].url, format!("{CHANNEL_LINK}9"));
    }

    #[test]
    fn names_from_the_account_state_win_over_the_fallback() {
        let mut names = Names::default();
        names.users.insert("42".into(), "old name".into());
        let blocks = parse_with("hi <@42>, see <#9>", &names);
        let mut now = MentionNames::default();
        now.users.insert("42".into(), "Ada Lovelace".into());
        now.channels.insert(
            "9".into(),
            mention::ChannelName {
                name: "announcements".into(),
                pill: true,
            },
        );
        let c = compose(first_inline(&blocks), &now, 0);
        assert_eq!(c.text, "hi @Ada\u{a0}Lovelace, see #announcements");
    }

    #[test]
    fn custom_emoji_take_an_emoji_placeholder() {
        let blocks = parse("hi <:wave:123456789012345678> <a:party:223456789012345678>");
        let inline = first_inline(&blocks);
        // The same room as a Unicode emoji; the picture is painted over it.
        assert_eq!(
            inline.text,
            format!("hi {} {}", emoji::PLACEHOLDER, emoji::PLACEHOLDER)
        );
        assert!(
            matches!(&inline.emoji[0].1, Emoji::Custom { name, animated: false, .. } if name == "wave")
        );
        assert!(matches!(
            &inline.emoji[1].1,
            Emoji::Custom { animated: true, .. }
        ));
        // Only custom emoji are listed for fetching, each once, also in quotes, lists,
        // tables and spoilers; none from code.
        let id = |n: u8| format!("10000000000000000{n}");
        let blocks = parse(&format!(
            "<:wave:{}> \u{1F600}\n> <:wave:{}> <a:party:{}>\n- ||<:x:{}>||\n\n| <:t:{}> |\n| - |\n| a |\n\n```\n<:code:{}>\n```",
            id(1),
            id(1),
            id(2),
            id(3),
            id(4),
            id(5)
        ));
        assert_eq!(
            custom_emoji(&blocks),
            vec![
                ("100000000000000001", false),
                ("100000000000000002", true),
                ("100000000000000003", false),
                ("100000000000000004", false)
            ]
        );
        // An emoji-only message of custom emoji is drawn large.
        assert!(
            first_inline(&parse(
                "<:wave:100000000000000001> <a:party:100000000000000002>"
            ))
            .jumbo
        );
    }

    #[test]
    fn unicode_emoji_and_shortcodes_become_twemoji() {
        let blocks = parse("hi \u{1F44B} and :smile: and :wave::skin-tone-3:");
        let inline = first_inline(&blocks);
        let glyphs: Vec<&str> = inline
            .emoji
            .iter()
            .filter_map(|(_, e)| match e {
                Emoji::Standard(g) => Some(g.as_str()),
                Emoji::Custom { .. } => None,
            })
            .collect();
        assert_eq!(glyphs.len(), 3, "{inline:?}");
        assert_eq!(glyphs[0], "\u{1F44B}");
        assert_eq!(glyphs[1], "\u{1F604}");
        assert_eq!(glyphs[2], "\u{1F44B}\u{1F3FD}");
        assert!(!inline.jumbo);
        // Copyright and trademark signs stay text, as in Fluxer.
        let plain = parse("\u{a9} 2026");
        assert!(first_inline(&plain).emoji.is_empty());
    }

    #[test]
    fn emoji_only_messages_are_jumbo() {
        assert!(first_inline(&parse("\u{1F600} \u{1F680}")).jumbo);
        assert!(first_inline(&parse(":smile:")).jumbo);
        assert!(!first_inline(&parse("\u{1F600} ok")).jumbo);
        let many = "\u{1F600}".repeat(31);
        assert!(!first_inline(&parse(&many)).jumbo);
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
        assert!(blocks.iter().any(|b| matches!(b, Block::Code(c)
            if c.content.contains("fn main") && c.language.as_deref() == Some("rust"))));
        assert!(blocks.iter().any(|b| matches!(b, Block::Quote(_))));
    }

    #[test]
    fn overlapping_highlights_are_flattened() {
        let blocks = parse("***both*** plain");
        let c = compose(first_inline(&blocks), &MentionNames::default(), 0);
        let flat = c.flatten(false);
        for w in flat.windows(2) {
            assert!(w[0].0.end <= w[1].0.start);
        }
    }

    #[test]
    fn links_in_hidden_spoilers_are_inert() {
        let blocks = parse("||[click](https://example.com)||");
        let c = compose(first_inline(&blocks), &MentionNames::default(), 0);
        let hidden = clickables(&c, false);
        assert!(hidden.iter().all(|(_, c)| matches!(c, Click::Reveal)));
        let shown = clickables(&c, true);
        assert!(shown.iter().any(|(_, c)| matches!(c, Click::Link(..))));
    }

    #[test]
    fn hidden_spoiler_text_is_covered() {
        // gpui blends a highlight colour over the text colour: the hidden text must be
        // painted in an opaque colour that equals its fill, or it shows through.
        let hidden = inline::highlight(inline::Paint::Style(Style::Spoiler), false);
        let color = hidden.color.expect("a text colour");
        assert_eq!(color.a, 1.0);
        assert_eq!(hidden.background_color, Some(color));
        assert_eq!(crate::theme::text().blend(color), color);
    }

    #[test]
    fn block_spoilers_alerts_tables_and_lists() {
        let blocks = parse(
            "||line one\nline two||\n> [!WARNING]\n> careful\n\n| a | b |\n| - | :-: |\n| 1 | 2 |\n\n3. three\n4. four",
        );
        assert!(
            blocks.iter().any(|b| matches!(b, Block::Spoiler(_))),
            "{blocks:?}"
        );
        assert!(blocks
            .iter()
            .any(|b| matches!(b, Block::Alert(AlertKind::Warning, _))));
        let table = blocks.iter().find_map(|b| match b {
            Block::Table(t) => Some(t),
            _ => None,
        });
        let table = table.expect("a table");
        assert_eq!(table.header.len(), 2);
        assert_eq!(table.align, vec![Align::Left, Align::Center]);
        assert_eq!(table.rows[0][1].text, "2");
        assert!(blocks.iter().any(
            |b| matches!(b, Block::List(l) if l.ordered && l.start == 3 && l.items.len() == 2)
        ));
    }
}
