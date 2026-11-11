// SPDX-License-Identifier: AGPL-3.0-or-later
//! Demo data for the message actions: reactions under a few messages of `#general`, an
//! own message to act on, and which message a scene opens its bar, menu or delete
//! confirmation on (`chat-reactions`, `chat-menu`, `chat-delete`).

use std::collections::HashMap;

use inlet_protocol as p;
use inlet_state::{ChannelId, MessageId};
use serde_json::json;

use super::scenes::Scene;
use super::{at, message_id, msg, snowflake, ME};

/// The own message the scenes act on.
const OWN_SEQ: u64 = 16;
const OWN_TIME: &str = "09:27";
/// The message whose bar the reactions scene shows.
const BAR_SEQ: u64 = 14;
const BAR_TIME: &str = "09:26";

/// Adds reactions and the own message to `#general` (demo IDs, before they become
/// snowflakes).
pub fn add(messages: &mut HashMap<String, Vec<p::Message>>) {
    let Some(general) = messages.get_mut("c-general") else {
        return;
    };
    let react = |m: &mut p::Message, reactions: serde_json::Value| {
        m.extra.insert("reactions".into(), reactions);
    };
    for m in general.iter_mut() {
        if m.id == message_id(at("09:24"), 13) {
            react(
                m,
                json!([
                    {"emoji": {"name": "\u{1F916}"}, "count": 2, "me": false},
                    {"emoji": {"name": "\u{1F44D}"}, "count": 5, "me": true}
                ]),
            );
        } else if m.id == message_id(at(BAR_TIME), BAR_SEQ) {
            react(
                m,
                json!([
                    {"emoji": {"name": "\u{1F440}"}, "count": 1, "me": false},
                    {"emoji": {"id": "123456789012345678", "name": "wave"}, "count": 3, "me": false}
                ]),
            );
            m.pinned = true;
        }
    }
    let mut own = msg(
        OWN_SEQ,
        "c-general",
        ME,
        OWN_TIME,
        "On it \u{2014} the build looks good to me.",
    );
    react(
        &mut own,
        json!([
            {"emoji": {"name": "\u{2764}\u{FE0F}"}, "count": 2, "me": false},
            {"emoji": {"name": "\u{1F389}"}, "count": 1, "me": true}
        ]),
    );
    general.push(own);
}

/// The channel and message a scene opens its overlay on.
pub fn target(scene: Scene) -> Option<(ChannelId, MessageId)> {
    let ch = ChannelId::parse(&snowflake("c-general"))?;
    let (seq, time) = match scene {
        Scene::ChatReactions => (BAR_SEQ, BAR_TIME),
        Scene::ChatMenu | Scene::ChatDelete => (OWN_SEQ, OWN_TIME),
        _ => return None,
    };
    Some((ch, MessageId::parse(&message_id(at(time), seq))?))
}
