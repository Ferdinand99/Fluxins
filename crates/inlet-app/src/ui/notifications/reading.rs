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
//! - The once-a-second tick checks the same rules ([`should_read_now`]). That is what
//!   makes reading at the bottom reliable: the list's own events miss some ways of
//!   getting there (dragging the scroll bar, "Jump to present", the list taking up
//!   following the newest messages only at its next layout, closing About).
//! - "Mark as read" (a channel, a server, everything) is acknowledged at once, one
//!   acknowledgement per channel that has something unread; the protocol sends them as
//!   batches of up to 100 channels, one batch per second. Nothing is sent for a channel
//!   that is already read.

use std::time::Instant;

use inlet_state::{AccountState, AckKind, Changes, ChannelId, GuildId, MessageId};

use super::unread::{all_unread_targets, category_unread_targets, has_unread, unread_targets};
use crate::app::Inlet;
use crate::backend::notifications as backend;
use crate::ui::chat::messages::MessageView;
use gpui::Context;

/// What is known about the screen at a moment the open channel might be read.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Sight {
    /// The chat is on screen ([`Inlet::chat_is_visible`]).
    pub chat_visible: bool,
    /// The window has focus.
    pub window_active: bool,
    /// The list is at the bottom, following the newest messages.
    pub at_bottom: bool,
}

/// Whether the open channel `ch` is to be read now: the chat is on screen, the window
/// has focus, the list is at the bottom of a window that holds the newest messages
/// (`MessageView::may_mark_read`), and there is something to read.
pub(crate) fn should_read_now(
    view: &MessageView,
    state: &AccountState,
    ch: ChannelId,
    sight: Sight,
) -> bool {
    sight.chat_visible
        && view.may_mark_read(state, ch, sight.at_bottom, sight.window_active)
        && has_unread(state, ch)
}

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

    /// The open channel is read if it may be ([`should_read_now`]). Called by the tick
    /// once a second, and at once when the window gets focus or the list reports the
    /// bottom. Never call it from inside the list's scroll handler: it can redraw rows.
    pub(crate) fn read_if_watched(
        &mut self,
        window_active: bool,
        at_bottom: bool,
        cx: &mut Context<Self>,
    ) {
        let (Some(ch), Some(account)) = (self.open_channel_id(), self.account.clone()) else {
            return;
        };
        let sight = Sight {
            chat_visible: self.chat_is_visible(),
            window_active,
            at_bottom,
        };
        if !should_read_now(&self.chat.view, account.read(cx), ch, sight) {
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
        self.send_read_marks(targets, cx);
    }

    /// "Mark as read" on channels the inbox lists, each up to the message given. The
    /// person clicked it in the panel that covers the chat, so it is not held back by
    /// [`Inlet::chat_is_visible`]; only channels with something unread are sent.
    pub(crate) fn mark_listed_read(
        &mut self,
        targets: Vec<(ChannelId, MessageId)>,
        cx: &mut Context<Self>,
    ) {
        let Some(account) = self.account.clone() else {
            return;
        };
        let targets: Vec<(ChannelId, MessageId)> = {
            let state = account.read(cx);
            targets
                .into_iter()
                .filter(|(ch, _)| has_unread(state, *ch))
                .collect()
        };
        self.send_read_marks(targets, cx);
    }

    /// Marks `targets` read in the state and sends one acknowledgement for them.
    fn send_read_marks(&mut self, targets: Vec<(ChannelId, MessageId)>, cx: &mut Context<Self>) {
        let Some(account) = self.account.clone() else {
            return;
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

    /// About once a second: reading the open channel if it is watched, the
    /// acknowledgements that are due, and the settings writes.
    pub(crate) fn tick_notifications(&mut self, cx: &mut Context<Self>) {
        let at_bottom = self.chat.message_list.is_following_tail();
        self.read_if_watched(self.notifications.window_active, at_bottom, cx);
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

#[cfg(test)]
mod tests {
    use inlet_protocol as p;
    use inlet_protocol::models::FLUXER_EPOCH_MS;
    use inlet_protocol::permissions::{READ_MESSAGE_HISTORY, SEND_MESSAGES, VIEW_CHANNEL};
    use inlet_state::{AccountState, AckKind, ChannelId, MessagesPage};
    use serde_json::json;

    use super::{should_read_now, Sight};
    use crate::ui::chat::messages::MessageView;

    const CH: ChannelId = ChannelId(301);
    /// 2026-01-01T00:00:00Z, when the account joined the guild.
    const JOINED_MS: u64 = 1_767_225_600_000;

    /// A message ID `secs` after the join time.
    fn mid(secs: u64) -> u64 {
        ((JOINED_MS + secs * 1000 - FLUXER_EPOCH_MS) << 22) + 1
    }

    fn message(id: u64) -> p::Message {
        serde_json::from_value(json!({
            "id": id.to_string(), "channel_id": "301", "type": 0,
            "author": {"id": "7", "username": "u7"},
            "content": "hi", "timestamp": "2026-01-01T01:00:00Z"
        }))
        .unwrap()
    }

    /// A guild with one text channel whose newest message is `mid(109)`, read up to
    /// `mid(104)`.
    fn state() -> AccountState {
        let ready: p::Ready = serde_json::from_value(json!({
            "session_id": "s",
            "user": {"id": "10", "username": "me", "mfa_enabled": false},
            "guilds": [{
                "id": "20",
                "properties": {"id": "20", "name": "G", "owner_id": "99", "mfa_level": 0,
                               "default_message_notifications": 0},
                "roles": [{"id": "20", "name": "@everyone", "position": 0,
                           "permissions": (VIEW_CHANNEL | SEND_MESSAGES | READ_MESSAGE_HISTORY).to_string()}],
                "channels": [{"id": "301", "type": 0, "position": 0, "name": "general",
                              "last_message_id": mid(109).to_string()}],
                "members": [{"user": {"id": "10", "username": "me"}, "roles": [],
                             "joined_at": "2026-01-01T00:00:00Z"}],
                "member_count": 2,
            }],
            "read_states": [{"id": "301", "mention_count": 0,
                             "last_message_id": mid(104).to_string()}],
            "user_guild_settings": [],
            "presences": [],
        }))
        .unwrap();
        AccountState::from_ready_at(&ready, JOINED_MS as i64 + 3_600_000)
    }

    /// The newest page lands: the window is complete.
    fn newest_page(state: &mut AccountState) {
        let msgs: Vec<p::Message> = (100..=109).map(|s| message(mid(s))).collect();
        let _ = state.begin_load(CH);
        let _ = state.apply_messages(CH, MessagesPage::latest(msgs, 50));
    }

    const SEEN: Sight = Sight {
        chat_visible: true,
        window_active: true,
        at_bottom: true,
    };

    #[test]
    fn the_tick_reads_only_a_watched_complete_unread_channel() {
        let mut s = state();
        let view = MessageView::loaded_for_tests();
        // The newest page has not landed: the window is not complete.
        let _ = s.begin_load(CH);
        assert!(!should_read_now(&view, &s, CH, SEEN), "window not complete");
        newest_page(&mut s);
        assert!(s.unread(CH).unread);

        // The true case: on screen, focused, at the bottom of a complete window, unread.
        assert!(should_read_now(&view, &s, CH, SEEN));

        let hidden = Sight {
            chat_visible: false,
            ..SEEN
        };
        assert!(!should_read_now(&view, &s, CH, hidden), "chat hidden");
        let unfocused = Sight {
            window_active: false,
            ..SEEN
        };
        assert!(
            !should_read_now(&view, &s, CH, unfocused),
            "window unfocused"
        );
        let scrolled_up = Sight {
            at_bottom: false,
            ..SEEN
        };
        assert!(!should_read_now(&view, &s, CH, scrolled_up), "scrolled up");
        // The view has not shown the channel's page yet.
        assert!(!should_read_now(&MessageView::default(), &s, CH, SEEN));

        // Already read: nothing to do, so nothing is acknowledged again.
        let _ = s.mark_read(CH, None, AckKind::Explicit);
        assert!(!should_read_now(&view, &s, CH, SEEN), "already read");
    }

    #[test]
    fn a_detached_window_is_not_read_by_the_tick() {
        let mut s = state();
        newest_page(&mut s);
        let view = MessageView::loaded_for_tests();
        assert!(should_read_now(&view, &s, CH, SEEN));
        // Older pages push the newest messages out of the window: it is detached.
        for _ in 0..11 {
            let oldest = s.messages(CH).and_then(|w| w.oldest_id()).unwrap();
            let msgs: Vec<p::Message> = (0..50).map(|i| message(oldest.0 - 1 - i)).collect();
            let _ = s.apply_messages(CH, MessagesPage::before(oldest, msgs, 50));
        }
        assert!(!s.messages(CH).unwrap().has_newest());
        assert!(!should_read_now(&view, &s, CH, SEEN), "detached");
    }

    #[test]
    fn mark_unread_holds_against_the_tick_and_a_waiting_ack() {
        use crate::backend::messages::ack::AckGate;
        use inlet_state::MessageId;
        use std::time::{Duration, Instant};

        let mut s = state();
        newest_page(&mut s);
        let view = MessageView::loaded_for_tests();
        // The channel was read while watched: an acknowledgement waits in the gate.
        let t0 = Instant::now();
        let mut gate = AckGate::default();
        let _ = s.mark_read(CH, None, AckKind::Automatic);
        gate.note(CH, MessageId(mid(109)), t0);
        assert!(!s.unread(CH).unread);

        // "Mark as unread" from mid(106): the mark goes to the message before it, and
        // the waiting acknowledgement is dropped (what `drop_pending_ack` does).
        let mark = crate::ui::chat::unread_mark(&s, CH, MessageId(mid(106))).unwrap();
        assert_eq!(mark.after, MessageId(mid(105)));
        let _ = s.apply(&mark.ack);
        gate.clear(CH, t0);
        assert!(s.unread(CH).unread);

        // The tick: watched, at the bottom, unread, so it tries an automatic read. The
        // state ignores it while the mark holds, so nothing is acknowledged either.
        assert!(should_read_now(&view, &s, CH, SEEN));
        let changes = s.mark_read(CH, None, AckKind::Automatic);
        assert!(!changes.read_state, "an automatic read does not undo it");
        assert!(s.unread(CH).unread);
        assert!(gate.due(t0 + Duration::from_secs(4)).is_empty());

        // Reading on purpose (Escape, the banner, opening the channel again) clears it.
        let _ = s.mark_read(CH, None, AckKind::Explicit);
        assert!(!s.unread(CH).unread);
    }
}
