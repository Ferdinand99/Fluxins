// SPDX-License-Identifier: AGPL-3.0-or-later
//! Turns protocol channels, messages and embeds into the UI model, parsing markdown on
//! the backend thread. Also keeps the names that mentions resolve to.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use inlet_protocol as p;
use inlet_protocol::models::channel_type;

use crate::model::{
    self, Attachment, Channel, ChannelKind, Embed, EmbedField, Message, Names, Reply,
};

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

pub fn remember_user(n: &mut Names, u: &p::User) {
    n.users
        .insert(u.id.clone(), model::clean_name(u.display_name()));
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
    }
}

/// Converts and parses a message. Runs on the backend thread, so markdown parsing never
/// blocks the UI.
pub fn to_message(m: &p::Message, names: &SharedNames) -> Message {
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
        timestamp_ms,
        timestamp,
        content: m.content.clone(),
        blocks: Vec::new(),
        truncated: false,
        attachments: m
            .attachments
            .iter()
            .map(|a| Attachment {
                filename: model::clean_name(&a.filename),
                url: a
                    .url
                    .clone()
                    .or_else(|| a.proxy_url.clone())
                    .unwrap_or_default(),
            })
            .collect(),
        embeds: m
            .embeds
            .iter()
            .filter_map(|e| to_embed(e, &snapshot))
            .take(10)
            .collect(),
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
        model::snippet(content, 90)
    };
    Some(Reply {
        author: model::clean_name(name),
        snippet,
    })
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

/// Converts an embed object. Images, thumbnails and videos become links.
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
            .map(|d| crate::ui::markdown::parse_with(capped(d), names))
            .unwrap_or_default(),
        fields: Vec::new(),
        footer: str_at(v, &["footer", "text"]).map(|t| model::snippet(t, 256)),
        media: Vec::new(),
    };
    if let Some(fields) = v.get("fields").and_then(|f| f.as_array()) {
        for f in fields.iter().take(25) {
            let name = str_at(f, &["name"])
                .map(|n| model::snippet(n, 256))
                .unwrap_or_default();
            let value = str_at(f, &["value"])
                .map(|t| crate::ui::markdown::parse_with(capped(t), names))
                .unwrap_or_default();
            embed.fields.push(EmbedField { name, value });
        }
    }
    for (key, label) in [
        ("image", t!("embed-image")),
        ("thumbnail", t!("embed-thumbnail")),
        ("video", t!("embed-video")),
    ] {
        if let Some(url) = str_at(v, &[key, "url"]) {
            embed.media.push(Attachment {
                filename: label,
                url: url.to_string(),
            });
        }
    }
    let empty = embed.title.is_none()
        && embed.author.is_none()
        && embed.description.is_empty()
        && embed.fields.is_empty()
        && embed.footer.is_none()
        && embed.media.is_empty();
    (!empty).then_some(embed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embeds_become_cards() {
        let v = serde_json::json!({
            "title": "Welcome to IWGamin's Pack!",
            "description": "React in <#100> to unlock the server.",
            "color": 0xF1C40F,
            "author": {"name": "Sylo"},
            "fields": [{"name": "Rules", "value": "**Be kind**", "inline": false}],
            "footer": {"text": "Sylo"},
            "image": {"url": "https://example.com/banner.png"}
        });
        let mut names = Names::default();
        names.channels.insert("100".into(), "verify".into());
        let e = to_embed(&v, &names).expect("embed");
        assert_eq!(e.title.as_deref(), Some("Welcome to IWGamin's Pack!"));
        assert_eq!(e.color, Some(0xF1C40F));
        assert_eq!(e.fields.len(), 1);
        assert_eq!(e.media.len(), 1);
        match &e.description[0] {
            crate::ui::markdown::Block::Text(i) => assert!(i.text.contains("#verify")),
            other => panic!("{other:?}"),
        }
        assert!(to_embed(&serde_json::json!({}), &names).is_none());
        assert!(to_embed(&serde_json::json!("x"), &names).is_none());
    }

    #[test]
    fn reply_preview() {
        let v = serde_json::json!({"author": {"username": "ferdinand", "global_name": "Ferdinand99"}, "content": "Hi! I can't see the #events channel anymore"});
        let r = to_reply(&v).unwrap();
        assert_eq!(r.author, "Ferdinand99");
        assert!(r.snippet.starts_with("Hi! I can't see"));
    }
}
