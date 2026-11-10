// SPDX-License-Identifier: AGPL-3.0-or-later
//! Message markdown, parsed with Fluxer's own parser (`fluxer_markdown_parser`, from the
//! Fluxer repository, AGPL-3.0-or-later), so a message is split into the same nodes as in
//! Fluxer's clients. v0 draws it simply: styled text runs, code blocks, quotes and lists.
//!
//! Parsing happens on the backend thread ([`parse`] returns plain data that is `Send`);
//! the UI thread only draws.
//!
//! - `parse`: nodes to [`Block`]s
//! - `inline`: inline nodes (text, styles, links, mentions, emoji) to an [`Inline`]
//! - `render`: [`Block`]s to gpui elements

mod inline;
mod parse;
mod render;

use std::ops::Range;

use gpui::SharedString;

pub use parse::{mark_edited, parse, parse_with};
pub use render::{mono_font, render, RenderCtx};

/// Prefix of the internal link a channel mention carries; clicking it opens the channel.
pub const CHANNEL_LINK: &str = "inlet-channel:";

/// One block of a rendered message.
#[derive(Clone, Debug)]
pub enum Block {
    Text(Inline),
    Heading(u8, Inline),
    Subtext(Inline),
    Code { content: SharedString },
    Quote(Vec<Block>),
    Alert(String, Vec<Block>),
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
    use super::render::{clickables, flatten, Click};
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
