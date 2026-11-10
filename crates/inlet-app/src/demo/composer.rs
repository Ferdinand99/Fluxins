// SPDX-License-Identifier: AGPL-3.0-or-later
//! The composer's demo data: who "you" are, autocomplete data with numeric stand-in IDs
//! (pills need real snowflakes), and edits of the demo's own messages.

use super::MockSession;
use crate::backend::composer::{ChannelEntry, CustomEmoji, Directory, Person, RoleEntry};
use crate::backend::messages as msgs;

/// The demo account's ID: the author ID of the demo's own messages.
pub const DEMO_USER_ID: &str = "user-demo";

/// Roles, channels and emoji of every demo guild.
pub fn directory(_guild_id: &str) -> Directory {
    Directory {
        roles: vec![
            RoleEntry {
                id: 3001,
                name: "Moderators".into(),
            },
            RoleEntry {
                id: 3002,
                name: "Helpers".into(),
            },
        ],
        channels: vec![
            ChannelEntry {
                id: 1001,
                name: "verify".into(),
            },
            ChannelEntry {
                id: 1002,
                name: "general".into(),
            },
        ],
        emojis: vec![CustomEmoji {
            id: 4001,
            name: "inlet_wave".into(),
            animated: false,
        }],
        mention_everyone: true,
        bypass_slowmode: false,
    }
}

/// Members whose name or account name starts with `query`.
pub fn members(query: &str) -> Vec<Person> {
    let all = [
        (2001, "ada", "ada"),
        (2002, "linus", "linus"),
        (2003, "Grace Hopper", "grace"),
        (2004, "Åse Ødegård", "aase"),
    ];
    let q = query.to_lowercase();
    all.iter()
        .filter(|(_, name, user)| {
            name.to_lowercase().starts_with(&q) || user.to_lowercase().starts_with(&q)
        })
        .map(|(id, name, user)| Person {
            id: *id,
            name: (*name).into(),
            username: (*user).into(),
        })
        .collect()
}

impl MockSession {
    /// New text for one of the demo's own messages.
    pub fn edit(&mut self, channel_id: &str, message_id: &str, content: &str) {
        let updated = {
            let mut messages = self.messages.lock().unwrap();
            let Some(m) = messages
                .get_mut(channel_id)
                .and_then(|list| list.iter_mut().find(|m| m.id == message_id))
            else {
                return;
            };
            if m.author_id != DEMO_USER_ID {
                return;
            }
            m.content = content.into();
            m.edited = true;
            m.parse_with(&super::demo_names());
            m.clone()
        };
        let _ = self
            .events
            .unbounded_send(msgs::Ev::MessageUpdated(updated));
    }
}
