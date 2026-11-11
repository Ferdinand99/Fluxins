// SPDX-License-Identifier: AGPL-3.0-or-later
//! Demo data for the panels over the chat: pinned messages in `#general`, and the
//! demo's answers to the panels' requests (pins, recent mentions, saved messages, the
//! page around a message). The scenes `chat-pins`, `inbox-unreads`, `inbox-mentions`
//! and `inbox-saved` open a panel once `#general` is on screen.

use std::collections::HashMap;

use inlet_protocol as p;
use inlet_state::MessageId;
use serde_json::json;

use super::{snowflake, user_id, MockSession, ME, ROLE_MODERATORS};
use crate::backend::messages as msgs;
use crate::backend::messages::inbox::{
    item, Answer, Cmd, Ev, SavedEntry, AROUND_PAGE, MENTIONS_PAGE, SAVED_PAGE,
};

/// Messages of `#general` that are pinned, by position.
const PINNED: [usize; 3] = [2, 5, 9];

/// Pins a few messages of `#general` (demo IDs, before they become snowflakes).
pub fn add(messages: &mut HashMap<String, Vec<p::Message>>) {
    if let Some(general) = messages.get_mut("c-general") {
        for ix in PINNED {
            if let Some(m) = general.get_mut(ix) {
                m.pinned = true;
            }
        }
    }
}

fn id_of(m: &p::Message) -> Option<MessageId> {
    MessageId::parse(&m.id)
}

impl MockSession {
    /// The demo has no server: the panels' lists come from the demo messages.
    pub fn inbox(&self, cmd: Cmd, answer: Option<Answer>) {
        let names = &self.names;
        match cmd {
            Cmd::AckPins { .. } => {}
            Cmd::DeleteMention { message_id } => {
                // The server's echo, as Fluxer sends it to every session.
                let dispatch = p::Dispatch::parse(
                    "RECENT_MENTION_DELETE",
                    json!({ "message_id": message_id }),
                );
                let _ = self.events.unbounded_send(msgs::Ev::Dispatch {
                    dispatch: Box::new(dispatch),
                    rendered: Vec::new(),
                });
            }
            Cmd::Pins { channel_id, .. } => {
                let Some(answer) = answer else { return };
                let mut list: Vec<p::Message> = self
                    .messages
                    .lock()
                    .unwrap()
                    .get(&channel_id)
                    .map(|l| l.iter().filter(|m| m.pinned).cloned().collect())
                    .unwrap_or_default();
                list.sort_by_key(|m| std::cmp::Reverse(id_of(m)));
                let request = answer.request();
                answer.send(Ev::Pins {
                    channel_id,
                    request,
                    items: list.iter().map(|m| item(m, names)).collect(),
                    next: None,
                });
            }
            Cmd::Mentions {
                before,
                everyone,
                roles,
                ..
            } => {
                let Some(answer) = answer else { return };
                let me = user_id(ME);
                let guild_channels: Vec<String> = self
                    .channels
                    .iter()
                    .filter(|(g, _)| g.is_some())
                    .flat_map(|(_, l)| l.iter().map(|c| c.id.clone()))
                    .collect();
                let before = before.as_deref().and_then(MessageId::parse);
                let mut list: Vec<p::Message> = self
                    .messages
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(ch, _)| guild_channels.contains(ch))
                    .flat_map(|(_, l)| l.iter())
                    .filter(|m| {
                        m.mentions.iter().any(|u| u.id == me)
                            || (everyone && m.mention_everyone())
                            || (roles && m.mention_roles().iter().any(|r| r == ROLE_MODERATORS))
                    })
                    .filter(|m| before.is_none_or(|b| id_of(m).is_some_and(|id| id < b)))
                    .cloned()
                    .collect();
                list.sort_by_key(|m| std::cmp::Reverse(id_of(m)));
                list.truncate(usize::from(MENTIONS_PAGE));
                let request = answer.request();
                answer.send(Ev::Mentions {
                    request,
                    more: false,
                    items: list.iter().map(|m| item(m, names)).collect(),
                });
            }
            Cmd::Saved { before, .. } => {
                let Some(answer) = answer else { return };
                let request = answer.request();
                if before.is_some() {
                    answer.send(Ev::Saved {
                        request,
                        entries: Vec::new(),
                        more: false,
                    });
                    return;
                }
                // Two messages of `#general` and one the account can no longer read.
                let general = snowflake("c-general");
                let mut entries: Vec<SavedEntry> = self
                    .messages
                    .lock()
                    .unwrap()
                    .get(&general)
                    .map(|l| {
                        [3usize, 7]
                            .iter()
                            .filter_map(|ix| l.get(*ix))
                            .map(|m| SavedEntry::Shown(Box::new(item(m, names))))
                            .collect()
                    })
                    .unwrap_or_default();
                entries.reverse();
                entries.push(SavedEntry::Missing {
                    channel_id: snowflake("c-dev"),
                    message_id: "1234567890123456789".into(),
                });
                entries.truncate(usize::from(SAVED_PAGE));
                answer.send(Ev::Saved {
                    request,
                    entries,
                    more: false,
                });
            }
            Cmd::Around {
                channel_id,
                message_id,
                ..
            } => {
                let Some(answer) = answer else { return };
                let Some(target) = MessageId::parse(&message_id) else {
                    return;
                };
                let mut list: Vec<p::Message> = self
                    .messages
                    .lock()
                    .unwrap()
                    .get(&channel_id)
                    .cloned()
                    .unwrap_or_default();
                list.sort_by_key(id_of);
                let half = usize::from(AROUND_PAGE / 2);
                let at = list
                    .iter()
                    .position(|m| id_of(m) >= Some(target))
                    .unwrap_or(list.len());
                let from = at.saturating_sub(half);
                let to = (at + half).min(list.len());
                let mut page: Vec<p::Message> = list[from..to].to_vec();
                page.reverse();
                let rendered = page
                    .iter()
                    .map(|m| crate::backend::messages::parse::to_message(m, names))
                    .collect();
                let request = answer.request();
                answer.send(Ev::Around {
                    channel_id,
                    request,
                    messages: page,
                    rendered,
                });
            }
        }
    }
}
