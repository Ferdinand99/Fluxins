// SPDX-License-Identifier: AGPL-3.0-or-later
//! Demo scenes: a fixed state the app opens in, picked with `INLET_DEMO_SCENE` (demo
//! builds only). Every scene has the same guilds, channels and messages; a scene sets
//! what differs (read state, messages on their way, compact mode). Which channel opens
//! is set with `INLET_DEMO_GUILD` and `INLET_DEMO_CHANNEL`.
//!
//! - `default`: three guilds, long and non-Latin channel names, messages of every kind,
//!   embeds and two DMs.
//! - `chat-cozy`: as `default`, plus an unread line ("NEW") in `#general` and a message
//!   being sent and one that failed.
//! - `chat-compact`: `chat-cozy` in compact mode.
//! - `chat-system`: for `#system-messages`, every system message type.
//! - `chat-replies`: for `#replies`, replies with loaded, deleted and missing originals.
//! - `markdown-inline`, `markdown-blocks`, `markdown-alerts`, `markdown-code`: for
//!   `#development`, every kind of markdown node (see `markdown.rs`).

use std::collections::HashMap;

use inlet_protocol as p;
use serde_json::{json, Value};

use super::{
    guilds, markdown, messages, snowflake, social, unread, user, user_id, ME, ROLE_CONTRIBUTORS,
    ROLE_MODERATORS,
};
use crate::backend::messages::Pending;
use crate::model::{Channel, ChannelKind, Guild};

/// A demo scene.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scene {
    Default,
    ChatCozy,
    ChatCompact,
    ChatSystem,
    ChatReplies,
    MarkdownInline,
    MarkdownBlocks,
    MarkdownAlerts,
    MarkdownCode,
}

impl Scene {
    pub fn from_name(name: &str) -> Option<Scene> {
        Some(match name {
            "default" => Scene::Default,
            "chat-cozy" => Scene::ChatCozy,
            "chat-compact" => Scene::ChatCompact,
            "chat-system" => Scene::ChatSystem,
            "chat-replies" => Scene::ChatReplies,
            "markdown-inline" => Scene::MarkdownInline,
            "markdown-blocks" => Scene::MarkdownBlocks,
            "markdown-alerts" => Scene::MarkdownAlerts,
            "markdown-code" => Scene::MarkdownCode,
            _ => return None,
        })
    }
}

/// The scene named in `INLET_DEMO_SCENE`; `default` when it is not set or unknown.
pub fn current() -> Scene {
    match std::env::var("INLET_DEMO_SCENE") {
        Ok(name) => Scene::from_name(name.trim()).unwrap_or_else(|| {
            log::warn!("unknown demo scene {name:?}, showing the default scene");
            Scene::Default
        }),
        Err(_) => Scene::Default,
    }
}

/// What a scene shows. All IDs are snowflakes.
pub struct Data {
    pub guilds: Vec<Guild>,
    /// Channel lists per guild (`None` = direct messages).
    pub channels: HashMap<Option<String>, Vec<Channel>>,
    /// Messages per channel.
    pub messages: HashMap<String, Vec<p::Message>>,
    /// The READY the demo gateway sends first.
    pub ready: p::Ready,
    /// Messages of the demo account on their way, or failed.
    pub outgoing: Vec<Pending>,
}

fn channel_type(kind: ChannelKind) -> i64 {
    match kind {
        ChannelKind::Text | ChannelKind::Other => 0,
        ChannelKind::Dm => 1,
        ChannelKind::Voice => 2,
        ChannelKind::GroupDm => 3,
        ChannelKind::Category => 4,
        ChannelKind::Announcement => 5,
    }
}

fn newest_id(messages: &HashMap<String, Vec<p::Message>>, channel: &str) -> Option<String> {
    messages
        .get(channel)?
        .iter()
        .filter_map(|m| inlet_state::MessageId::parse(&m.id))
        .max()
        .map(|id| id.to_string())
}

fn channel_json(
    c: &Channel,
    guild: Option<&str>,
    messages: &HashMap<String, Vec<p::Message>>,
    unread_demo: bool,
) -> Value {
    let mut v = json!({
        "id": c.id,
        "type": channel_type(c.kind),
        "guild_id": guild,
        "position": c.position,
        "parent_id": c.parent_id,
        "last_message_id": newest_id(messages, &c.id).or_else(|| unread_demo.then(|| unread::newest(&c.id)).flatten()),
    });
    if guild.is_some() {
        v["name"] = json!(c.name);
        v["topic"] = json!(c.topic);
    } else {
        let recipients: Vec<Value> = c.name.split(", ").map(user).collect();
        v["recipients"] = Value::Array(recipients);
    }
    v
}

fn ready(
    guilds: &[Guild],
    channels: &HashMap<Option<String>, Vec<Channel>>,
    messages: &HashMap<String, Vec<p::Message>>,
    read_states: Vec<Value>,
    guild_settings: Vec<Value>,
    unread_demo: bool,
) -> p::Ready {
    let guilds: Vec<Value> = guilds
        .iter()
        .map(|g| {
            let list: Vec<Value> = channels
                .get(&Some(g.id.clone()))
                .map(|l| {
                    l.iter()
                        .map(|c| channel_json(c, Some(&g.id), messages, unread_demo))
                        .collect()
                })
                .unwrap_or_default();
            json!({
                "id": g.id,
                "properties": {"id": g.id, "name": g.name, "owner_id": user_id(ME)},
                "channels": list,
                "roles": [
                    {"id": g.id, "name": "@everyone", "permissions": "2251799813685247", "position": 0},
                    {"id": ROLE_MODERATORS, "name": "Moderators", "color": 0xE6_7E_22, "position": 2},
                    {"id": ROLE_CONTRIBUTORS, "name": "Contributors", "color": 0x1A_BC_9C, "position": 1}
                ],
                "members": [{
                    "user": user(ME),
                    "roles": [ROLE_MODERATORS],
                    "joined_at": "2026-01-01T00:00:00+00:00"
                }],
                "member_count": 5
            })
        })
        .collect();
    let private: Vec<Value> = channels
        .get(&None)
        .map(|l| {
            l.iter()
                .map(|c| channel_json(c, None, messages, unread_demo))
                .collect()
        })
        .unwrap_or_default();
    serde_json::from_value(json!({
        "session_id": "demo",
        "user": user(ME),
        "guilds": guilds,
        "private_channels": private,
        "users": [],
        "read_states": read_states,
        "user_guild_settings": guild_settings,
    }))
    .expect("the demo READY is valid")
}

pub fn data(scene: Scene) -> Data {
    let mut channels = HashMap::new();
    guilds::channels(&mut channels);
    social::dm_channels(&mut channels);
    let mut by_name = HashMap::new();
    messages::messages(&mut by_name);
    social::dm_messages(&mut by_name);
    match scene {
        Scene::MarkdownInline => {
            by_name.insert(markdown::SCENE_CHANNEL.into(), markdown::inline());
        }
        Scene::MarkdownBlocks => {
            by_name.insert(markdown::SCENE_CHANNEL.into(), markdown::blocks());
        }
        Scene::MarkdownAlerts => {
            by_name.insert(markdown::SCENE_CHANNEL.into(), markdown::alerts());
        }
        Scene::MarkdownCode => {
            by_name.insert(markdown::SCENE_CHANNEL.into(), markdown::code());
        }
        _ => {}
    }

    // Demo IDs become snowflakes everywhere.
    let guilds: Vec<Guild> = guilds::guilds()
        .into_iter()
        .map(|g| Guild {
            id: snowflake(&g.id),
            ..g
        })
        .collect();
    let channels: HashMap<Option<String>, Vec<Channel>> = channels
        .into_iter()
        .map(|(guild, list)| {
            let list = list
                .into_iter()
                .map(|c| Channel {
                    id: snowflake(&c.id),
                    parent_id: c.parent_id.as_deref().map(snowflake),
                    ..c
                })
                .collect();
            (guild.as_deref().map(snowflake), list)
        })
        .collect();
    let messages: HashMap<String, Vec<p::Message>> = by_name
        .into_iter()
        .map(|(name, list)| (snowflake(&name), list))
        .collect();

    let general = snowflake("c-general");
    let mut read_states = Vec::new();
    let mut outgoing = Vec::new();
    if matches!(scene, Scene::ChatCozy | Scene::ChatCompact) {
        // Read up to the fourth-newest message: the last three are new.
        let mut ids: Vec<inlet_state::MessageId> = messages
            .get(&general)
            .map(|l| {
                l.iter()
                    .filter_map(|m| inlet_state::MessageId::parse(&m.id))
                    .collect()
            })
            .unwrap_or_default();
        ids.sort();
        if let Some(ack) = ids.iter().rev().nth(3) {
            read_states.push(json!({"id": general, "last_message_id": ack.to_string()}));
        }
        outgoing.push(Pending {
            channel_id: general.clone(),
            nonce: "demo-sending".into(),
            text: "On its way \u{2026}".into(),
            failed: None,
        });
        outgoing.push(Pending {
            channel_id: general.clone(),
            nonce: "demo-failed".into(),
            text: "This one did not go through.".into(),
            failed: Some("the network is unreachable".into()),
        });
    }
    let mut guild_settings = Vec::new();
    let unread_demo = scene == Scene::Default;
    if unread_demo {
        // Unread messages, mentions and mutes in the sidebar and the rail.
        read_states.extend(unread::read_states());
        guild_settings = unread::guild_settings();
    }
    let ready = ready(
        &guilds,
        &channels,
        &messages,
        read_states,
        guild_settings,
        unread_demo,
    );
    Data {
        guilds,
        channels,
        messages,
        ready,
        outgoing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_scene_builds_and_the_ready_knows_its_channels() {
        for name in [
            "default",
            "chat-cozy",
            "chat-compact",
            "chat-system",
            "chat-replies",
            "markdown-inline",
            "markdown-blocks",
            "markdown-alerts",
            "markdown-code",
        ] {
            let scene = Scene::from_name(name).expect(name);
            let data = data(scene);
            assert_eq!(data.guilds.len(), 3);
            assert!(data.channels.contains_key(&None));
            let state = inlet_state::AccountState::from_ready_at(&data.ready, 1);
            for id in ["c-general", "c-system", "c-replies", "c-joins", "dm-ada"] {
                let ch = inlet_state::ChannelId::parse(&snowflake(id)).expect(id);
                assert!(state.channel(ch).is_some(), "{name}: {id}");
                assert!(data.messages.contains_key(&snowflake(id)), "{name}: {id}");
            }
            assert_eq!(
                !data.outgoing.is_empty(),
                matches!(scene, Scene::ChatCozy | Scene::ChatCompact)
            );
        }
        assert_eq!(Scene::from_name("nope"), None);
    }
}
