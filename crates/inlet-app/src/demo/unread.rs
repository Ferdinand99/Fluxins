// SPDX-License-Identifier: AGPL-3.0-or-later
//! Demo unread state for the default scene: unread channels, mentions, a muted channel, a
//! muted category and a muted server. It goes into the demo's READY (read states and
//! notification settings), so the app works it out like it does for a real account.

use serde_json::{json, Value};

use super::{at, message_id, snowflake};

/// A demo channel with something unread, and what the account has read of it.
struct Entry {
    channel: &'static str,
    /// Mentions that have not been read.
    mentions: u32,
    /// The channel has been read before (its ack is older than its newest message).
    acked: bool,
}

const ENTRIES: [Entry; 9] = [
    Entry {
        channel: "c-dev",
        mentions: 0,
        acked: false,
    },
    Entry {
        channel: "c-ann",
        mentions: 0,
        acked: false,
    },
    Entry {
        channel: "c-rules",
        mentions: 12,
        acked: true,
    },
    Entry {
        channel: "c-help",
        mentions: 3,
        acked: true,
    },
    // Muted below: unread, drawn dimmed.
    Entry {
        channel: "c-memes",
        mentions: 0,
        acked: false,
    },
    Entry {
        channel: "dm-ada",
        mentions: 1,
        acked: true,
    },
    Entry {
        channel: "dm-group",
        mentions: 0,
        acked: false,
    },
    Entry {
        channel: "r-general",
        mentions: 0,
        acked: false,
    },
    // In a muted server: no pill on the rail, the mentions still count.
    Entry {
        channel: "s-general",
        mentions: 2,
        acked: true,
    },
];

/// The newest message of a demo channel that has no messages of its own, so it can be
/// unread.
pub fn newest(channel_id: &str) -> Option<String> {
    ENTRIES
        .iter()
        .any(|e| snowflake(e.channel) == channel_id)
        .then(|| message_id(at("10:30"), 1))
}

/// What READY says the account has read.
pub fn read_states() -> Vec<Value> {
    ENTRIES
        .iter()
        .filter(|e| e.acked)
        .map(|e| {
            json!({
                "id": snowflake(e.channel),
                "mention_count": e.mentions,
                "last_message_id": message_id(at("09:00"), 1),
            })
        })
        .collect()
}

/// The notification settings: a muted server, and in the first server a muted channel and
/// a muted category.
pub fn guild_settings() -> Vec<Value> {
    vec![
        json!({"guild_id": snowflake("g3"), "muted": true}),
        json!({"guild_id": snowflake("g1"), "channel_overrides": [
            {"channel_id": snowflake("c-memes"), "muted": true, "message_notifications": 3},
            {"channel_id": snowflake("c-voice"), "muted": true, "message_notifications": 3},
        ]}),
    ]
}
