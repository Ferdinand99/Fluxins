// SPDX-License-Identifier: AGPL-3.0-or-later
//! What the UI shows. The backend converts protocol types into these, so the views do
//! not depend on the wire format.

use std::collections::HashMap;

use crate::ui::markdown::Block;

/// Longest name shown anywhere (guild, channel, author, file).
pub const MAX_NAME_CHARS: usize = 100;
/// Message text drawn before "Show more".
pub const DISPLAY_CHARS: usize = 4_000;
/// Message text kept at all; the rest is dropped before parsing.
pub const MAX_CONTENT_BYTES: usize = 16 * 1024;
/// Consecutive messages from the same author within this window share one header.
pub const GROUP_WINDOW_MS: i64 = 7 * 60 * 1000;

#[derive(Clone, Debug)]
pub struct Guild {
    pub id: String,
    pub name: String,
}

impl Guild {
    /// Up to three initials for the guild rail ("Inlet Dev" -> "ID").
    pub fn initials(&self) -> String {
        let s: String = self
            .name
            .split_whitespace()
            .filter_map(|w| w.chars().next())
            .take(3)
            .flat_map(char::to_uppercase)
            .collect();
        if s.is_empty() {
            "?".into()
        } else {
            s
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelKind {
    Text,
    Announcement,
    Voice,
    Category,
    Dm,
    GroupDm,
    Other,
}

#[derive(Clone, Debug)]
pub struct Channel {
    pub id: String,
    pub name: String,
    pub kind: ChannelKind,
    pub parent_id: Option<String>,
    pub position: i64,
    /// Channel topic, cleaned; shown after the name in the header.
    pub topic: Option<String>,
    /// Slowmode: seconds between two messages of one member (0 = off).
    pub slowmode_secs: u32,
}

/// Names for resolving mentions (`<#id>`, `<@id>`, `<@&id>`) while parsing.
#[derive(Clone, Debug, Default)]
pub struct Names {
    pub channels: HashMap<String, String>,
    pub users: HashMap<String, String>,
    pub roles: HashMap<String, String>,
    /// User IDs in the order they were first seen, so the oldest go first past the cap
    /// (see `backend::messages::parse::remember_user`).
    pub user_order: std::collections::VecDeque<String>,
}

/// What kind of embed it is (`type`), which decides how it is drawn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EmbedKind {
    /// Sent by a bot or webhook; also anything Inlet does not know.
    #[default]
    Rich,
    /// A link preview.
    Link,
    /// An article preview, with a large image.
    Article,
    /// A link that is an image: drawn as the image alone.
    Image,
    /// A link that is a looping video (GIF-like): drawn as the media alone.
    Gifv,
    /// A video page (YouTube and the like): a thumbnail with a play button.
    Video,
    /// A Bluesky post.
    Bluesky,
}

/// An image, thumbnail or video of an embed. URLs are kept as sent; the media view
/// only loads `proxy_url` after checking it is on the instance's media proxy.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EmbedMedia {
    /// The original (third-party) URL. Never fetched; opened in the browser on click.
    pub url: String,
    /// The same media through the instance's media proxy.
    pub proxy_url: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub animated: bool,
    /// Marked as explicit by the server's media inspection.
    pub explicit: bool,
}

/// An embed (bots post these, and link previews), drawn as a card.
#[derive(Clone, Debug, Default)]
pub struct Embed {
    pub kind: EmbedKind,
    /// Colour of the left bar, `0xRRGGBB`.
    pub color: Option<u32>,
    pub author: Option<String>,
    /// Link of the author name (opened on click, http/https only).
    pub author_url: Option<String>,
    /// Author icon through the media proxy (checked before loading).
    pub author_icon: Option<String>,
    /// The site a link preview is from.
    pub provider: Option<String>,
    pub title: Option<String>,
    /// Link of the title (http/https only when opened).
    pub url: Option<String>,
    pub description: Vec<Block>,
    pub fields: Vec<EmbedField>,
    pub footer: Option<String>,
    /// Footer icon through the media proxy (checked before loading).
    pub footer_icon: Option<String>,
    /// The embed's time, milliseconds since the Unix epoch.
    pub timestamp_ms: Option<i64>,
    pub thumbnail: Option<EmbedMedia>,
    pub image: Option<EmbedMedia>,
    pub video: Option<EmbedMedia>,
    /// The embed is marked as explicit.
    pub nsfw: bool,
}

impl Embed {
    /// Whether the embed shows an image or video below its text.
    pub fn has_media(&self) -> bool {
        self.image.is_some() || self.video.is_some()
    }
}

#[derive(Clone, Debug)]
pub struct EmbedField {
    pub name: String,
    pub value: Vec<Block>,
    /// Shares a row with the inline fields next to it.
    pub inline: bool,
}

/// The message a reply points to, for the line above the reply.
#[derive(Clone, Debug)]
pub struct Reply {
    pub author: String,
    /// The first words, cleaned and shortened.
    pub snippet: String,
}

impl Channel {
    pub fn is_messageable(&self) -> bool {
        matches!(
            self.kind,
            ChannelKind::Text | ChannelKind::Announcement | ChannelKind::Dm | ChannelKind::GroupDm
        )
    }
}

/// What an attachment is, by its content type.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AttachmentKind {
    Image,
    Video,
    Audio,
    #[default]
    File,
}

/// A file attached to a message. URLs are kept as sent; the media view only loads or
/// saves them after checking that they are on the instance's media proxy.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Attachment {
    pub id: String,
    /// File name, cleaned.
    pub filename: String,
    /// The signed URL of the file; it expires (`expires_at_ms`).
    pub url: Option<String>,
    /// The file through the media proxy.
    pub proxy_url: Option<String>,
    pub content_type: Option<String>,
    /// Size in bytes.
    pub size: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub kind: AttachmentKind,
    pub spoiler: bool,
    /// Marked as explicit by the server's media inspection.
    pub explicit: bool,
    pub animated: bool,
    /// Length of audio or video, in seconds.
    pub duration_secs: Option<u64>,
    /// Alt text, cleaned.
    pub alt: Option<String>,
    /// When the signed URLs stop working, milliseconds since the Unix epoch.
    pub expires_at_ms: Option<i64>,
    /// The server says the URLs no longer work.
    pub expired: bool,
}

/// A user's avatar: the user and the image hash (`a_` = animated).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserAvatar {
    pub user_id: String,
    pub hash: String,
}

#[derive(Clone, Debug)]
pub struct Message {
    pub id: String,
    pub channel_id: String,
    /// Display name, cleaned.
    pub author: String,
    /// Account name, cleaned; shown next to the display name when they differ.
    pub author_username: String,
    /// The author is a bot: show a BOT tag.
    pub author_bot: bool,
    /// The author's avatar, when they have one (otherwise their initials show). Boxed:
    /// most messages are converted and kept, and this keeps them small.
    pub author_avatar: Option<Box<UserAvatar>>,
    /// Milliseconds since the Unix epoch.
    pub timestamp_ms: i64,
    /// Already formatted for display, in local time.
    pub timestamp: String,
    pub content: String,
    /// The first [`DISPLAY_CHARS`] of `content`, parsed. Parsed on the backend thread.
    pub blocks: Vec<Block>,
    /// `content` is longer than what `blocks` shows.
    pub truncated: bool,
    /// The URLs the whole text hides in spoilers (see `markdown::spoilered_links`);
    /// the embeds of these links are covered until the spoilers are shown. Only worked
    /// out for a message with embeds, on the backend thread, so drawing never parses.
    pub spoiler_links: Vec<String>,
    pub attachments: Vec<Attachment>,
    pub embeds: Vec<Embed>,
    pub reply: Option<Reply>,
    pub edited: bool,
}

impl Message {
    /// Whether there is anything to show under the header.
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty() && self.embeds.is_empty() && self.attachments.is_empty()
    }

    /// Fills `blocks` and `truncated` from `content`, without mention names.
    #[cfg(test)]
    pub fn parse(&mut self) {
        self.parse_with(&Names::default());
    }

    /// Fills `blocks` and `truncated` from `content`, resolving mentions with `names`.
    /// Caps the stored content first.
    pub fn parse_with(&mut self, names: &Names) {
        if self.content.len() > MAX_CONTENT_BYTES {
            let cut = floor_char_boundary(&self.content, MAX_CONTENT_BYTES);
            self.content.truncate(cut);
        }
        let shown = match self.content.char_indices().nth(DISPLAY_CHARS) {
            Some((ix, _)) => {
                self.truncated = true;
                &self.content[..ix]
            }
            None => {
                self.truncated = false;
                &self.content[..]
            }
        };
        self.blocks = crate::ui::markdown::parse_with(shown, names);
        if self.edited {
            crate::ui::markdown::mark_edited(&mut self.blocks);
        }
        // A cut text is read whole for this, so a spoiler past the cut still counts.
        self.spoiler_links = if self.embeds.is_empty() {
            Vec::new()
        } else if self.truncated {
            let whole = crate::ui::markdown::parse_with(&self.content, names);
            crate::ui::markdown::spoilered_links(&whole)
        } else {
            crate::ui::markdown::spoilered_links(&self.blocks)
        };
    }
}

fn floor_char_boundary(s: &str, mut ix: usize) -> usize {
    while ix > 0 && !s.is_char_boundary(ix) {
        ix -= 1;
    }
    ix
}

/// Removes characters that can fake or break a name: control characters, line breaks,
/// bidi overrides and isolates, zero-width characters. Caps the length.
pub fn clean_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| !is_hidden_or_control(*c))
        .take(MAX_NAME_CHARS)
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        "(no name)".into()
    } else {
        trimmed.to_string()
    }
}

fn is_hidden_or_control(c: char) -> bool {
    c.is_control()
        || matches!(c,
            '\u{200B}'..='\u{200F}'   // zero-width space/joiners, LRM, RLM
            | '\u{202A}'..='\u{202E}' // embeddings and overrides
            | '\u{2066}'..='\u{2069}' // isolates
            | '\u{2028}' | '\u{2029}' // line and paragraph separators
            | '\u{FEFF}'              // zero-width no-break space
            | '\u{061C}'              // Arabic letter mark
        )
}

/// Orders channels the way the sidebar shows them: uncategorised first, then each
/// category followed by its children, all by position.
pub fn sidebar_order(channels: &[Channel]) -> Vec<Channel> {
    let mut by_pos = channels.to_vec();
    by_pos.sort_by_key(|c| (c.position, c.id.clone()));
    let mut out = Vec::with_capacity(by_pos.len());
    for c in by_pos
        .iter()
        .filter(|c| c.parent_id.is_none() && c.kind != ChannelKind::Category)
    {
        out.push(c.clone());
    }
    for cat in by_pos.iter().filter(|c| c.kind == ChannelKind::Category) {
        out.push(cat.clone());
        for c in by_pos
            .iter()
            .filter(|c| c.parent_id.as_deref() == Some(cat.id.as_str()))
        {
            out.push(c.clone());
        }
    }
    // Children whose category is unknown still show up.
    for c in by_pos.iter() {
        if c.kind != ChannelKind::Category && !out.iter().any(|o| o.id == c.id) {
            out.push(c.clone());
        }
    }
    out
}

/// "Thursday, 1 October 2026" for a date separator.
pub fn day_label(day: chrono::NaiveDate) -> String {
    day.format("%A, %-d %B %Y").to_string()
}

/// The first `max` characters of `text` on one line, with "…" when cut.
pub fn snippet(text: &str, max: usize) -> String {
    let one_line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let cleaned = clean_name(&one_line);
    match cleaned.char_indices().nth(max) {
        Some((ix, _)) => format!("{}\u{2026}", &cleaned[..ix]),
        None => cleaned,
    }
}

/// Parses an RFC 3339 timestamp into (milliseconds since epoch, local display text in
/// the system locale's format).
pub fn timestamp(rfc3339: &str) -> (i64, String) {
    match chrono::DateTime::parse_from_rfc3339(rfc3339) {
        Ok(t) => {
            let ms = t.timestamp_millis();
            (ms, crate::ui::chat::time::full(ms))
        }
        Err(_) => (0, clean_name(rfc3339)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(id: &str, kind: ChannelKind, parent: Option<&str>, pos: i64) -> Channel {
        Channel {
            id: id.into(),
            name: id.into(),
            kind,
            parent_id: parent.map(Into::into),
            position: pos,
            topic: None,
            slowmode_secs: 0,
        }
    }

    fn msg(id: &str, author: &str, t: i64) -> Message {
        Message {
            id: id.into(),
            channel_id: "c".into(),
            author: author.into(),
            author_username: author.into(),
            author_bot: false,
            author_avatar: None,
            timestamp_ms: t,
            timestamp: String::new(),
            content: String::new(),
            blocks: vec![],
            truncated: false,
            spoiler_links: vec![],
            attachments: vec![],
            embeds: vec![],
            reply: None,
            edited: false,
        }
    }

    #[test]
    fn day_labels_and_snippets() {
        let day = chrono::NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        assert_eq!(day_label(day), "Thursday, 1 October 2026");
        assert_eq!(snippet("Hi!  I can't\nsee", 100), "Hi! I can't see");
        assert_eq!(snippet("abcdef", 3), "abc\u{2026}");
    }

    #[test]
    fn categories_group_their_children() {
        let list = vec![
            ch("b", ChannelKind::Text, Some("cat"), 1),
            ch("cat", ChannelKind::Category, None, 0),
            ch("a", ChannelKind::Text, None, 5),
            ch("orphan", ChannelKind::Text, Some("gone"), 0),
        ];
        let ids: Vec<_> = sidebar_order(&list).into_iter().map(|c| c.id).collect();
        assert_eq!(ids, ["a", "cat", "b", "orphan"]);
    }

    #[test]
    fn initials() {
        assert_eq!(
            Guild {
                id: "1".into(),
                name: "Inlet Dev Hub".into()
            }
            .initials(),
            "IDH"
        );
        assert_eq!(
            Guild {
                id: "1".into(),
                name: "".into()
            }
            .initials(),
            "?"
        );
    }

    #[test]
    fn clean_name_strips_bidi_control_and_zero_width() {
        assert_eq!(clean_name("ad\u{202E}a"), "ada");
        assert_eq!(clean_name("a\u{200B}d\u{2066}a\u{2069}"), "ada");
        assert_eq!(clean_name("line\nbreak\t"), "linebreak");
        assert_eq!(clean_name("\u{202E}\u{200F}"), "(no name)");
        assert_eq!(clean_name("Åse 😀"), "Åse 😀");
        assert_eq!(clean_name(&"x".repeat(500)).chars().count(), MAX_NAME_CHARS);
    }

    #[test]
    fn long_content_is_truncated_for_display() {
        let mut m = msg("1", "a", 0);
        m.content = "ø".repeat(DISPLAY_CHARS + 10);
        m.parse();
        assert!(m.truncated);
        let mut big = msg("2", "a", 0);
        big.content = "x".repeat(MAX_CONTENT_BYTES * 2);
        big.parse();
        assert_eq!(big.content.len(), MAX_CONTENT_BYTES);
    }

    #[test]
    fn spoiler_links_of_a_cut_text_are_worked_out_when_it_is_parsed() {
        let hidden = "https://x.example/a.png";
        let mut m = msg("3", "a", 0);
        m.embeds.push(Embed::default());
        m.content = format!("{}\n\n||{hidden}||", "ø".repeat(DISPLAY_CHARS + 10));
        m.parse();
        assert!(m.truncated);
        // The spoiler lies past the cut: `blocks` does not have it, the list does.
        assert_eq!(m.spoiler_links, vec![hidden.to_string()]);

        // A message that is not cut reads its own blocks.
        let mut short = msg("4", "a", 0);
        short.embeds.push(Embed::default());
        short.content = format!("see ||{hidden}|| and https://open.example/");
        short.parse();
        assert!(!short.truncated);
        assert_eq!(short.spoiler_links, vec![hidden.to_string()]);

        // Without embeds there is nothing to cover, so nothing is worked out.
        let mut plain = msg("5", "a", 0);
        plain.content = format!("||{hidden}||");
        plain.parse();
        assert!(plain.spoiler_links.is_empty());
    }
}
