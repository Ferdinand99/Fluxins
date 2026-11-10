// SPDX-License-Identifier: AGPL-3.0-or-later
//! The composer's demo data: who "you" are, autocomplete data with the demo's snowflake
//! IDs (pills need real snowflakes), and edits of the demo's own messages.

use inlet_protocol as p;

use super::{snowflake, user_id, MockSession, ME};
use crate::backend::composer::{ChannelEntry, CustomEmoji, Directory, Person, Rights, RoleEntry};
use crate::backend::messages::{self as msgs, parse};

/// Text channels of the demo guilds that autocomplete offers.
const CHANNELS: [(&str, &str); 4] = [
    ("c-welcome", "welcome"),
    ("c-general", "general"),
    ("c-dev", "development"),
    ("c-replies", "replies"),
];

/// The demo account's ID: the author ID of the demo's own messages.
pub fn demo_user_id() -> String {
    user_id(ME)
}

/// Roles, channels and emoji of every demo guild.
pub fn directory(_guild_id: &str) -> Directory {
    Directory {
        roles: vec![
            RoleEntry {
                id: 3001,
                name: "Moderators".into(),
                mentionable: true,
            },
            RoleEntry {
                id: 3002,
                name: "Helpers".into(),
                mentionable: false,
            },
        ],
        channels: CHANNELS
            .iter()
            .filter_map(|(id, name)| {
                Some(ChannelEntry {
                    id: snowflake(id).parse().ok()?,
                    name: (*name).into(),
                })
            })
            .collect(),
        emojis: vec![CustomEmoji {
            id: 4001,
            name: "inlet_wave".into(),
            animated: false,
        }],
        // The demo account may mention everyone in its text channels.
        rights: CHANNELS
            .iter()
            .map(|(id, _)| {
                (
                    snowflake(id),
                    Rights {
                        mention_everyone: true,
                        bypass_slowmode: false,
                    },
                )
            })
            .collect(),
    }
}

/// Members whose name or account name starts with `query`.
pub fn members(query: &str) -> Vec<Person> {
    let all = [
        ("ada", "ada", "ada"),
        ("linus", "linus", "linus"),
        ("grace", "Grace Hopper", "grace"),
        ("aase", "\u{c5}se \u{d8}deg\u{e5}rd", "aase"),
    ];
    let q = query.to_lowercase();
    all.iter()
        .filter(|(_, name, user)| {
            name.to_lowercase().starts_with(&q) || user.to_lowercase().starts_with(&q)
        })
        .filter_map(|(key, name, user)| {
            Some(Person {
                id: user_id(key).parse().ok()?,
                name: (*name).into(),
                username: (*user).into(),
            })
        })
        .collect()
}

impl MockSession {
    /// New text for one of the demo's own messages, as the gateway would report it.
    pub fn edit(&mut self, channel_id: &str, message_id: &str, content: &str) {
        let updated = {
            let mut messages = self.messages.lock().unwrap();
            let Some(m) = messages
                .get_mut(channel_id)
                .and_then(|list| list.iter_mut().find(|m| m.id == message_id))
            else {
                return;
            };
            if m.author.id != demo_user_id() {
                return;
            }
            m.content = content.into();
            m.edited_timestamp = Some(chrono::Utc::now().to_rfc3339());
            m.clone()
        };
        let rendered = vec![parse::to_message(&updated, &self.names)];
        let _ = self.events.unbounded_send(msgs::Ev::Dispatch {
            dispatch: Box::new(p::Dispatch::MessageUpdate(Box::new(updated))),
            rendered,
        });
    }
}
