// SPDX-License-Identifier: AGPL-3.0-or-later
//! The message list's data: merging REST pages with live messages, grouping, and the
//! messages domain's events.

use gpui::{Context, FollowMode, Window};

use crate::app::Inlet;
use crate::backend::messages::{parse::snowflake_key, Ev};
use crate::model::{self, Message};

pub struct ShownMessage {
    pub message: Message,
    /// Same account as the message above, shortly after: hide the header.
    pub grouped: bool,
    /// Spoilers are shown.
    pub revealed: bool,
}

impl Inlet {
    pub(crate) fn on_messages_event(
        &mut self,
        event: Ev,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            Ev::Guilds(guilds) => self.on_guilds(guilds, cx),
            Ev::Channels { guild_id, channels } => self.on_channels(guild_id, channels, window, cx),
            Ev::Messages {
                channel_id,
                messages,
            } => {
                if Some(&channel_id) == self.selected_channel.as_ref() {
                    self.merge_messages(messages);
                }
            }
            Ev::MessageCreated(message) => {
                if Some(&message.channel_id) == self.selected_channel.as_ref()
                    && !self
                        .chat
                        .messages
                        .iter()
                        .any(|m| m.message.id == message.id)
                {
                    let grouped = self
                        .chat
                        .messages
                        .last()
                        .is_some_and(|m| model::should_group(&m.message, &message));
                    let at = self.chat.messages.len();
                    self.chat.messages.push(ShownMessage {
                        message,
                        grouped,
                        revealed: false,
                    });
                    self.chat.message_list.splice(at..at, 1);
                }
            }
            Ev::MessageUpdated(message) => {
                if let Some(ix) = self
                    .chat
                    .messages
                    .iter()
                    .position(|m| m.message.id == message.id)
                {
                    self.chat.messages[ix].message = message;
                    self.chat.message_list.splice(ix..ix + 1, 1);
                }
            }
            Ev::MessageDeleted {
                channel_id,
                message_id,
            } => {
                if Some(&channel_id) == self.selected_channel.as_ref() {
                    if let Some(ix) = self
                        .chat
                        .messages
                        .iter()
                        .position(|m| m.message.id == message_id)
                    {
                        self.chat.messages.remove(ix);
                        self.regroup();
                        self.chat.message_list.splice(ix..ix + 1, 0);
                    }
                }
            }
        }
    }

    /// Merges a REST page into the view, keeping live messages that arrived meanwhile and
    /// what the person already revealed.
    pub(crate) fn merge_messages(&mut self, loaded: Vec<Message>) {
        let mut by_id: std::collections::HashMap<String, ShownMessage> = self
            .chat
            .messages
            .drain(..)
            .map(|m| (m.message.id.clone(), m))
            .collect();
        let newest_loaded = loaded
            .iter()
            .map(|m| snowflake_key(&m.id))
            .max()
            .map(|(l, s)| (l, s.to_string()));
        let mut merged: Vec<ShownMessage> = loaded
            .into_iter()
            .map(|message| {
                let revealed = by_id.remove(&message.id).is_some_and(|old| old.revealed);
                ShownMessage {
                    message,
                    grouped: false,
                    revealed,
                }
            })
            .collect();
        // Live messages newer than the page stay; older leftovers are what the page replaced.
        for (_, old) in by_id {
            let key = snowflake_key(&old.message.id);
            let newer = newest_loaded
                .as_ref()
                .is_none_or(|(l, s)| (key.0, key.1) > (*l, s.as_str()));
            if newer {
                merged.push(old);
            }
        }
        merged.sort_by(|a, b| snowflake_key(&a.message.id).cmp(&snowflake_key(&b.message.id)));
        self.chat.messages = merged;
        self.regroup();
        self.chat.message_list.reset(self.chat.messages.len());
        self.chat.message_list.set_follow_mode(FollowMode::Tail);
    }

    pub(crate) fn regroup(&mut self) {
        for i in 0..self.chat.messages.len() {
            self.chat.messages[i].grouped = i > 0
                && model::should_group(
                    &self.chat.messages[i - 1].message,
                    &self.chat.messages[i].message,
                )
                && self.chat.messages[i].message.reply.is_none()
                && self.chat.messages[i - 1].message.day() == self.chat.messages[i].message.day();
        }
    }

    pub(crate) fn set_messages(&mut self, messages: Vec<Message>) {
        self.chat.messages = messages
            .into_iter()
            .map(|message| ShownMessage {
                message,
                grouped: false,
                revealed: false,
            })
            .collect();
        self.regroup();
        self.chat.message_list.reset(self.chat.messages.len());
        self.chat.message_list.set_follow_mode(FollowMode::Tail);
    }
}
