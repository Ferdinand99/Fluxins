// SPDX-License-Identifier: AGPL-3.0-or-later
//! Turns protocol channels, messages and embeds into the UI model, parsing markdown on
//! the backend thread. Also keeps the names that mentions resolve to.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use inlet_protocol as p;
use inlet_protocol::models::channel_type;

use crate::model::{self, Channel, ChannelKind, Embed, EmbedField, Message, Names, Reply};

/// Channel lists per guild (`None` = direct messages), shared by REST tasks and the
/// gateway forwarder.
pub type ChannelCache = Arc<Mutex<HashMap<Option<String>, Vec<Channel>>>>;

/// Channel, user and role names seen so far, for resolving mentions. Shared by the
/// gateway forwarder and REST tasks of one session.
pub type SharedNames = Arc<Mutex<Names>>;

pub fn remember_channels(names: &SharedNames, list: &[Channel]) {
    let mut n = names.lock().unwrap();
    for c in list {
        if c.kind != ChannelKind::Dm && c.kind != ChannelKind::GroupDm {
            n.channels.insert(c.id.clone(), c.name.clone());
        }
    }
}

/// User names kept for mentions; past this the first seen are forgotten (a mention of
/// a forgotten user shows as an unknown mention until they post again).
pub const MAX_USER_NAMES: usize = 5_000;

pub fn remember_user(n: &mut Names, u: &p::User) {
    let name = model::clean_name(u.display_name());
    if n.users.insert(u.id.clone(), name).is_none() {
        n.user_order.push_back(u.id.clone());
        while n.users.len() > MAX_USER_NAMES {
            let Some(old) = n.user_order.pop_front() else {
                break;
            };
            n.users.remove(&old);
        }
    }
}

/// Roles from a READY or GUILD_CREATE guild (raw JSON objects with `id` and `name`).
pub fn remember_roles(names: &SharedNames, roles: &[serde_json::Value]) {
    let mut n = names.lock().unwrap();
    for r in roles {
        if let (Some(id), Some(name)) = (
            r.get("id").and_then(|v| v.as_str()),
            r.get("name").and_then(|v| v.as_str()),
        ) {
            n.roles.insert(id.to_string(), model::clean_name(name));
        }
    }
}

fn channel_kind(kind: i64) -> ChannelKind {
    match kind {
        channel_type::GUILD_TEXT | channel_type::PERSONAL_NOTES => ChannelKind::Text,
        channel_type::GUILD_ANNOUNCEMENT => ChannelKind::Announcement,
        channel_type::GUILD_VOICE => ChannelKind::Voice,
        channel_type::GUILD_CATEGORY => ChannelKind::Category,
        channel_type::DM => ChannelKind::Dm,
        channel_type::GROUP_DM => ChannelKind::GroupDm,
        channel_type::GUILD_FORUM => ChannelKind::Forum,
        channel_type::GUILD_MEDIA => ChannelKind::Media,
        _ => ChannelKind::Other,
    }
}

pub fn to_channel(c: &p::Channel) -> Channel {
    let name = match (&c.name, c.recipients.is_empty()) {
        (Some(n), _) if !n.is_empty() => n.clone(),
        (_, false) => c
            .recipients
            .iter()
            .map(|u| u.display_name().to_string())
            .collect::<Vec<_>>()
            .join(", "),
        _ if c.kind == channel_type::PERSONAL_NOTES => t!("channel-personal-notes"),
        _ => t!("channel-unnamed", id = c.id.as_str()),
    };
    Channel {
        id: c.id.clone(),
        name: model::clean_name(&name),
        kind: channel_kind(c.kind),
        parent_id: c.parent_id.clone(),
        position: c.position.unwrap_or(0),
        topic: c
            .topic
            .as_deref()
            .filter(|t| !t.trim().is_empty())
            .map(|t| model::snippet(t, 160)),
        // Fluxer allows up to six hours; anything else is not a slowmode.
        slowmode_secs: c
            .rate_limit_per_user
            .filter(|s| (1..=21_600).contains(s))
            .map_or(0, |s| s as u32),
    }
}

/// The stickers of a message: at most [`model::MAX_STICKERS`], only with a snowflake ID
/// (the picture's URL is built from it), names cleaned.
fn to_stickers(m: &p::Message) -> Vec<model::Sticker> {
    m.stickers()
        .into_iter()
        .filter(|s| {
            !s.id.is_empty() && s.id.len() <= 20 && s.id.bytes().all(|b| b.is_ascii_digit())
        })
        .take(model::MAX_STICKERS)
        .map(|s| model::Sticker {
            id: s.id,
            name: model::clean_name(&s.name),
            animated: s.animated,
        })
        .collect()
}

/// Converts and parses a message. Runs on the backend thread, so markdown parsing never
/// blocks the UI. The text and all embeds share one code highlighting budget.
pub fn to_message(m: &p::Message, names: &SharedNames) -> Message {
    crate::ui::markdown::with_message_budget(|| convert_message(m, names))
}

fn convert_message(m: &p::Message, names: &SharedNames) -> Message {
    let (timestamp_ms, timestamp) = model::timestamp(&m.timestamp);
    let reply = m.extra.get("referenced_message").and_then(to_reply);
    let snapshot = {
        let mut n = names.lock().unwrap();
        remember_user(&mut n, &m.author);
        for u in &m.mentions {
            remember_user(&mut n, u);
        }
        n.clone()
    };
    let mut out = Message {
        id: m.id.clone(),
        channel_id: m.channel_id.clone(),
        author: model::clean_name(m.author.display_name()),
        author_username: model::clean_name(&m.author.username),
        author_bot: m.author.bot,
        author_avatar: crate::backend::media::user_avatar(&m.author).map(Box::new),
        timestamp_ms,
        timestamp,
        content: m.content.clone(),
        blocks: Vec::new(),
        truncated: false,
        spoiler_links: Vec::new(),
        attachments: m
            .attachments
            .iter()
            .take(crate::backend::media::MAX_ATTACHMENTS)
            .map(crate::backend::media::to_attachment)
            .collect(),
        embeds: m
            .embeds
            .iter()
            .filter_map(|e| to_embed(e, &snapshot))
            .take(10)
            .collect(),
        stickers: to_stickers(m),
        reply,
        edited: m.edited_timestamp.is_some(),
    };
    out.parse_with(&snapshot);
    out
}

fn to_reply(v: &serde_json::Value) -> Option<Reply> {
    let author = v.get("author")?;
    let name = author
        .get("global_name")
        .and_then(|n| n.as_str())
        .filter(|n| !n.is_empty())
        .or_else(|| author.get("username").and_then(|n| n.as_str()))?;
    let content = v.get("content").and_then(|c| c.as_str()).unwrap_or("");
    let snippet = if content.trim().is_empty() {
        t!("reply-no-text")
    } else {
        model::snippet(&hide_spoilers(content), 90)
    };
    Some(Reply {
        author: model::clean_name(name),
        snippet,
    })
}

/// What a spoiler's letters show as in a reply preview.
const SPOILER_COVER: char = '\u{2588}';

/// The preview above a reply is plain text: spoilers (`||text||`) are covered there,
/// letter by letter, so their text cannot be read before it is revealed.
fn hide_spoilers(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut rest = content;
    while let Some(open) = rest.find("||") {
        let after = &rest[open + 2..];
        let Some(close) = after.find("||") else {
            break;
        };
        out.push_str(&rest[..open]);
        out.extend(after[..close].chars().map(
            |c| {
                if c.is_whitespace() {
                    c
                } else {
                    SPOILER_COVER
                }
            },
        ));
        rest = &after[close + 2..];
    }
    out.push_str(rest);
    out
}

fn str_at<'a>(v: &'a serde_json::Value, path: &[&str]) -> Option<&'a str> {
    let mut cur = v;
    for key in path {
        cur = cur.get(key)?;
    }
    cur.as_str().filter(|s| !s.trim().is_empty())
}

/// Longest embed text kept before parsing (Fluxer allows 4 096 for a description).
const MAX_EMBED_TEXT: usize = 4_096;

fn capped(s: &str) -> &str {
    match s.char_indices().nth(MAX_EMBED_TEXT) {
        Some((ix, _)) => &s[..ix],
        None => s,
    }
}

/// Converts an embed object. Its media (images, thumbnails, videos, icons) is filled
/// by the media domain.
pub fn to_embed(v: &serde_json::Value, names: &Names) -> Option<Embed> {
    if !v.is_object() {
        return None;
    }
    let mut embed = Embed {
        color: v
            .get("color")
            .and_then(|c| c.as_u64())
            .map(|c| (c & 0xFF_FF_FF) as u32),
        author: str_at(v, &["author", "name"]).map(model::clean_name),
        title: str_at(v, &["title"]).map(|t| model::snippet(t, 256)),
        url: str_at(v, &["url"]).map(str::to_string),
        description: str_at(v, &["description"])
            .map(|d| crate::ui::markdown::parse_embed_with(capped(d), names))
            .unwrap_or_default(),
        fields: Vec::new(),
        footer: str_at(v, &["footer", "text"]).map(|t| model::snippet(t, 256)),
        ..Default::default()
    };
    if let Some(fields) = v.get("fields").and_then(|f| f.as_array()) {
        for f in fields.iter().take(25) {
            let name = str_at(f, &["name"])
                .map(|n| model::snippet(n, 256))
                .unwrap_or_default();
            let value = str_at(f, &["value"])
                .map(|t| crate::ui::markdown::parse_embed_with(capped(t), names))
                .unwrap_or_default();
            let inline = f.get("inline").and_then(|i| i.as_bool()).unwrap_or(false);
            embed.fields.push(EmbedField {
                name,
                value,
                inline,
            });
        }
    }
    crate::backend::media::fill_embed(v, &mut embed);
    let empty = embed.title.is_none()
        && embed.author.is_none()
        && embed.description.is_empty()
        && embed.fields.is_empty()
        && embed.footer.is_none()
        && embed.thumbnail.is_none()
        && !embed.has_media();
    (!empty).then_some(embed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embeds_become_cards() {
        let v = serde_json::json!({
            "title": "Welcome to IWGamin's Pack!",
            "description": "Ask <@100> to unlock the server.",
            "color": 0xF1C40F,
            "author": {"name": "Sylo"},
            "fields": [{"name": "Rules", "value": "**Be kind**", "inline": false}],
            "footer": {"text": "Sylo"},
            "image": {"url": "https://example.com/banner.png"}
        });
        let mut names = Names::default();
        names.users.insert("100".into(), "ada".into());
        let e = to_embed(&v, &names).expect("embed");
        assert_eq!(e.title.as_deref(), Some("Welcome to IWGamin's Pack!"));
        assert_eq!(e.color, Some(0xF1C40F));
        assert_eq!(e.fields.len(), 1);
        assert!(e.image.is_some());
        match &e.description[0] {
            crate::ui::markdown::Block::Text(i) => assert!(i.text.contains("@ada")),
            other => panic!("{other:?}"),
        }
        assert!(to_embed(&serde_json::json!({}), &names).is_none());
        assert!(to_embed(&serde_json::json!("x"), &names).is_none());
    }

    #[test]
    fn user_names_are_capped_oldest_first() {
        let mut n = Names::default();
        for i in 0..(MAX_USER_NAMES as u64 + 1_000) {
            let u: p::User =
                serde_json::from_value(serde_json::json!({"id": i.to_string(), "username": "u"}))
                    .unwrap();
            remember_user(&mut n, &u);
        }
        assert_eq!(n.users.len(), MAX_USER_NAMES);
        assert!(!n.users.contains_key("0"));
        assert!(n
            .users
            .contains_key(&(MAX_USER_NAMES as u64 + 999).to_string()));
    }

    #[test]
    fn reply_preview() {
        let v = serde_json::json!({"author": {"username": "ferdinand", "global_name": "Ferdinand99"}, "content": "Hi! I can't see the #events channel anymore"});
        let r = to_reply(&v).unwrap();
        assert_eq!(r.author, "Ferdinand99");
        assert!(r.snippet.starts_with("Hi! I can't see"));
    }

    #[test]
    fn reply_preview_covers_spoilers() {
        let v = serde_json::json!({"author": {"username": "ana"},
                                   "content": "the end: ||Bruce is dead|| ok ||x"});
        let r = to_reply(&v).unwrap();
        assert_eq!(
            r.snippet,
            "the end: \u{2588}\u{2588}\u{2588}\u{2588}\u{2588} \u{2588}\u{2588} \u{2588}\u{2588}\u{2588}\u{2588} ok ||x"
        );
        assert!(!r.snippet.contains("dead"));
    }
}
