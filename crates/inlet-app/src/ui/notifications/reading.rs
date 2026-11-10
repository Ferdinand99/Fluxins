// SPDX-License-Identifier: AGPL-3.0-or-later
//! What is read when, and the acknowledgements that follow. The read marks themselves are
//! made in the one account state; this decides when they may be made and sends the
//! acknowledgements to the backend.
//!
//! Rules:
//! - Nothing is read or acknowledged while the chat is out of sight ([`Inlet::chat_is_visible`]).
//! - Reading the open channel (a page that landed, a message that came, the window getting
//!   focus, the list coming back to the bottom) is acknowledged through the gate: at most
//!   once per channel per 3 s, for the newest message only. The message view says when it
//!   may read (`MessageView::may_mark_read`: focus, at the bottom, a complete window).
//! - "Mark as read" (a channel, a server, everything) is acknowledged at once, one
//!   acknowledgement per channel that has something unread; the protocol sends them as
//!   batches of up to 100 channels, one batch per second. Nothing is sent for a channel
//!   that is already read.

use std::time::Instant;

use inlet_state::{AckKind, Changes, ChannelId, GuildId};

use super::unread::{all_unread_targets, category_unread_targets, has_unread, unread_targets};
use crate::app::Inlet;
use crate::backend::notifications as backend;
use gpui::Context;

/// Which channels "mark as read" takes.
enum Which {
    Guild(Option<GuildId>),
    Category(ChannelId),
    Everything,
}

impl Inlet {
    /// The open channel is read up to its newest message: the mark in the state, and the
    /// acknowledgement. `at_once` skips the gate (the person asked for it). Returns the
    /// changes, which the caller passes on to `after_changes`.
    pub(crate) fn read_channel(
        &mut self,
        ch: ChannelId,
        kind: AckKind,
        at_once: bool,
        cx: &mut Context<Self>,
    ) -> Changes {
        if !self.chat_is_visible() {
            return Changes::default();
        }
        let Some(account) = self.account.clone() else {
            return Changes::default();
        };
        let (changes, acked) = account.update(cx, |s, _| {
            if !has_unread(s, ch) {
                return (Changes::default(), None);
            }
            let changes = s.mark_read(ch, None, kind);
            // Nothing changed when a "mark unread" holds: no acknowledgement either.
            let acked = changes
                .read_state
                .then(|| s.read_state(ch).and_then(|e| e.ack_message_id))
                .flatten();
            (changes, acked)
        });
        if let Some(message) = acked {
            let now = Instant::now();
            if at_once {
                self.notifications.gate.clear(ch, now);
                self.backend.send(backend::Cmd::Ack {
                    items: vec![(ch.to_string(), message.to_string())],
                });
            } else {
                self.notifications.gate.note(ch, message, now);
            }
        }
        changes
    }

    /// The open channel is read if it may be: the window has focus, the list is at the
    /// bottom and shows the newest messages, and the chat is on screen. For the moments
    /// that make that true: the window getting focus, the list coming back to the bottom.
    pub(crate) fn read_if_watched(
        &mut self,
        window_active: bool,
        at_bottom: bool,
        cx: &mut Context<Self>,
    ) {
        let (Some(ch), Some(account)) = (self.open_channel_id(), self.account.clone()) else {
            return;
        };
        if !self
            .chat
            .view
            .may_mark_read(account.read(cx), ch, at_bottom, window_active)
        {
            return;
        }
        let changes = self.read_channel(ch, AckKind::Automatic, false, cx);
        if !changes.is_empty() {
            self.after_changes(changes, false, cx);
            cx.notify();
        }
    }

    /// "Mark as read" for the open channel (Escape, the settings window, the banner).
    pub(crate) fn mark_channel_read(&mut self, cx: &mut Context<Self>) {
        if self.chat_is_visible() {
            self.mark_open_channel_read(cx);
        }
    }

    /// "Mark as read" for every channel of the open server, or every direct message.
    pub(crate) fn mark_guild_read(&mut self, cx: &mut Context<Self>) {
        self.mark_read(Which::Guild(self.selected_guild_id()), cx);
    }

    /// "Mark as read" for everything.
    pub(crate) fn mark_all_read(&mut self, cx: &mut Context<Self>) {
        self.mark_read(Which::Everything, cx);
    }

    /// "Mark as read" for the channels of a category of the open server.
    pub(crate) fn mark_category_read(&mut self, category_id: String, cx: &mut Context<Self>) {
        if let Some(cat) = ChannelId::parse(&category_id) {
            self.mark_read(Which::Category(cat), cx);
        }
    }

    fn selected_guild_id(&self) -> Option<GuildId> {
        self.selected_guild.as_deref().and_then(GuildId::parse)
    }

    fn mark_read(&mut self, which: Which, cx: &mut Context<Self>) {
        if !self.chat_is_visible() {
            return;
        }
        let Some(account) = self.account.clone() else {
            return;
        };
        let targets = match which {
            Which::Guild(g) => unread_targets(account.read(cx), g),
            Which::Category(cat) => {
                category_unread_targets(account.read(cx), self.selected_guild_id(), cat)
            }
            Which::Everything => all_unread_targets(account.read(cx)),
        };
        if targets.is_empty() {
            return;
        }
        let mut changes = Changes::default();
        account.update(cx, |s, _| {
            for (ch, message) in &targets {
                changes.merge(s.mark_read(*ch, Some(*message), AckKind::Explicit));
            }
        });
        let now = Instant::now();
        for (ch, _) in &targets {
            self.notifications.gate.clear(*ch, now);
        }
        self.backend.send(backend::Cmd::Ack {
            items: targets
                .iter()
                .map(|(ch, m)| (ch.to_string(), m.to_string()))
                .collect(),
        });
        // The open channel is read too: its "NEW" line goes.
        if self
            .open_channel_id()
            .is_some_and(|open| targets.iter().any(|(ch, _)| *ch == open))
        {
            self.clear_unread_marker(cx);
        }
        self.after_changes(changes, false, cx);
        cx.notify();
    }

    /// About once a second: the acknowledgements that are due, and the settings writes.
    pub(crate) fn tick_notifications(&mut self, cx: &mut Context<Self>) {
        let now = Instant::now();
        let due = self.notifications.gate.due(now);
        if !due.is_empty() {
            self.backend.send(backend::Cmd::Ack {
                items: due
                    .iter()
                    .map(|(ch, m)| (ch.to_string(), m.to_string()))
                    .collect(),
            });
        }
        self.tick_settings_writes(now, cx);
    }
}
