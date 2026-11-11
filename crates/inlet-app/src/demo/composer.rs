// SPDX-License-Identifier: AGPL-3.0-or-later
//! The composer's demo data: who "you" are, members for `@` search (with the demo's
//! snowflake IDs), and edits of the demo's own messages. Roles, channels and emoji come
//! from the demo's READY through the account state.

use inlet_protocol as p;

use super::{user_id, MockSession, ME};
use crate::backend::composer::Person;
use crate::backend::messages::{self as msgs, parse};

/// The demo account's ID: the author ID of the demo's own messages.
pub fn demo_user_id() -> String {
    user_id(ME)
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
