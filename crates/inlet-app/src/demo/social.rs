// SPDX-License-Identifier: AGPL-3.0-or-later
//! Demo direct messages (the DM list and its messages), presences, the account's own
//! status, and people typing.

use std::collections::HashMap;
use std::time::Duration;

use inlet_protocol as p;
use serde_json::{json, Value};

use super::scenes::Scene;
use super::{channel, msg, snowflake, user_id};
use crate::backend::{messages as msgs, Ui};
use crate::model::{Channel, ChannelKind};

/// Adds the DM list (the `None` guild).
pub fn dm_channels(channels: &mut HashMap<Option<String>, Vec<Channel>>) {
    channels.insert(
        None,
        vec![
            channel("dm-ada", "ada", ChannelKind::Dm, None, 0),
            channel("dm-linus", "linus", ChannelKind::Dm, None, 1),
            channel(
                "dm-group",
                "ada, linus, grace",
                ChannelKind::GroupDm,
                None,
                2,
            ),
        ],
    );
}

/// Adds the messages of the DMs.
pub fn dm_messages(messages: &mut HashMap<String, Vec<p::Message>>) {
    messages.insert(
        "dm-ada".into(),
        vec![msg(
            20,
            "dm-ada",
            "ada",
            "10:01",
            "Hi! This is a direct message.",
        )],
    );
}

/// The presences in READY: Ada online, Linus idle, Grace on do not disturb.
pub fn presences() -> Value {
    json!([
        {"user": {"id": user_id("ada")}, "status": "online"},
        {"user": {"id": user_id("linus")}, "status": "idle"},
        {"user": {"id": user_id("grace")}, "status": "dnd",
         "custom_status": {"text": "Compiling", "emoji_name": "\u{1F4BB}"}},
    ])
}

/// The demo account's own status: online, or in the status picker scene do not disturb
/// with a custom status.
pub fn user_settings(scene: Scene) -> Value {
    if scene == Scene::StatusPicker {
        json!({
            "status": "dnd",
            "custom_status": {"text": "Reviewing pull requests", "emoji_name": "\u{1F3A7}"}
        })
    } else {
        json!({"status": "online", "custom_status": null})
    }
}

/// In the status picker scene, Ada and Linus keep typing in `#general` (a TYPING_START
/// every 5 s; a client sends one every 8 s and the line lasts 10 s).
pub fn start_typing(events: &Ui) {
    if super::scenes::current() != Scene::StatusPicker {
        return;
    }
    let events = events.clone();
    tokio::spawn(async move {
        loop {
            for who in ["ada", "linus"] {
                let typing: p::models::TypingStart = serde_json::from_value(json!({
                    "channel_id": snowflake("c-general"),
                    "user_id": user_id(who),
                    "guild_id": snowflake("g1"),
                    "timestamp": chrono::Utc::now().timestamp(),
                }))
                .expect("a valid TYPING_START");
                let sent = events.unbounded_send(msgs::Ev::Dispatch {
                    dispatch: Box::new(p::Dispatch::TypingStart(typing)),
                    rendered: Vec::new(),
                });
                if sent.is_err() {
                    // The UI is gone.
                    return;
                }
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
}
