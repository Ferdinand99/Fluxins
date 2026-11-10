//! Read state: how far the account has read each channel, and its mentions.
//!
//! Follows Fluxer's web client (`fluxer_app/src/features/read_state`):
//!
//! - A channel is unread when its newest message is newer than the ack. With
//!   no ack, the floor is the time the account joined the guild, or the
//!   channel's creation (`ReadStateEntry.ackTimestamp`).
//! - A new message from the account acks the channel locally. A message
//!   that the ack already covers changes nothing. Otherwise it counts as a
//!   mention when it mentions the account, `@everyone`/`@here` (unless
//!   suppressed, or in a thread the account has not joined), one of the
//!   account's roles (unless suppressed), or arrives in an unmuted DM
//!   (`ReadStateMentionMachine`).
//! - MESSAGE_ACK from the server: a manual ack ("mark unread") always
//!   applies; an older ack is ignored; the current ack updates the mention
//!   count; a newer ack moves the ack (`ReadStateServerAckMachine`).
//! - Visited guilds have no read state (`supportsUnreadTracking`).

use std::collections::BTreeMap;

use inlet_protocol::models::parse_timestamp_ms;
use inlet_protocol::ReadState;

use crate::ids::{ChannelId, GuildId, MessageId};
use crate::limits::{MAX_COUNTED_MENTIONS, MAX_READ_STATES};

/// What the sidebar shows for a channel or guild.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Unread {
    /// There are unread messages (show the channel bold / the guild pill).
    pub unread: bool,
    /// Unread mentions (the red badge).
    pub mentions: u32,
    /// The channel or guild is muted: show it dimmed, without the unread pill.
    pub muted: bool,
}

/// One channel's read state.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReadStateEntry {
    /// The channel's guild, `None` for DMs.
    pub guild: Option<GuildId>,
    /// The channel's newest message.
    pub last_message_id: Option<MessageId>,
    /// The last message the account has read.
    pub ack_message_id: Option<MessageId>,
    /// Unread mentions.
    pub mention_count: u32,
    /// The ack was set by hand ("mark unread").
    pub manual: bool,
    /// Newest pin time (Unix ms), 0 when unknown.
    pub last_pin_ms: i64,
    /// Last acknowledged pin time (Unix ms).
    pub ack_pin_ms: i64,
    /// Messages counted as mentions since the last ack, so a repeated
    /// MESSAGE_CREATE is not counted twice.
    counted: Vec<MessageId>,
}

impl ReadStateEntry {
    /// Fluxer's `isUnread`: the newest message is newer than the ack, or, with
    /// no ack, newer than `ack_floor_ms`.
    pub fn has_unread(&self, ack_floor_ms: i64) -> bool {
        let Some(last) = self.last_message_id else {
            return false;
        };
        match self.ack_message_id {
            Some(ack) => ack < last,
            None => ack_floor_ms < last.timestamp_ms(),
        }
    }

    /// Whether the pins changed since they were last looked at.
    pub fn has_unread_pins(&self) -> bool {
        self.last_pin_ms > self.ack_pin_ms
    }

    fn set_last(&mut self, id: Option<MessageId>) -> bool {
        if id.is_some() && id > self.last_message_id {
            self.last_message_id = id;
            return true;
        }
        false
    }

    fn reset_mentions(&mut self, count: u32) {
        self.mention_count = count;
        self.counted.clear();
    }
}

fn clamp_count(n: u64) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Every channel's read state.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReadStates {
    by_channel: BTreeMap<ChannelId, ReadStateEntry>,
}

impl ReadStates {
    /// READY's `read_states`.
    pub(crate) fn from_ready(rows: &[ReadState]) -> Self {
        let mut s = ReadStates::default();
        for row in rows {
            let Some(ch) = ChannelId::parse(&row.id) else {
                continue;
            };
            let Some(e) = s.ensure(ch, None) else { break };
            e.ack_message_id = MessageId::parse_opt(row.last_message_id.as_deref());
            e.reset_mentions(clamp_count(row.mention_count));
            e.ack_pin_ms = row
                .last_pin_timestamp
                .as_deref()
                .and_then(parse_timestamp_ms)
                .unwrap_or(0);
        }
        s
    }

    /// A channel's entry.
    pub fn get(&self, ch: ChannelId) -> Option<&ReadStateEntry> {
        self.by_channel.get(&ch)
    }

    /// The entry, created if there is room.
    pub(crate) fn ensure(
        &mut self,
        ch: ChannelId,
        guild: Option<GuildId>,
    ) -> Option<&mut ReadStateEntry> {
        if !self.by_channel.contains_key(&ch) && self.by_channel.len() >= MAX_READ_STATES {
            return None;
        }
        let e = self.by_channel.entry(ch).or_default();
        if guild.is_some() {
            e.guild = guild;
        }
        Some(e)
    }

    /// Fluxer's `seedGuildChannel` / READY channel pass: the channel's newest
    /// message and pin time.
    pub(crate) fn seed(
        &mut self,
        ch: ChannelId,
        guild: Option<GuildId>,
        last: Option<MessageId>,
        last_pin_ms: i64,
    ) -> bool {
        let Some(e) = self.ensure(ch, guild) else {
            return false;
        };
        let before = e.clone();
        // Deviation: Fluxer overwrites last_message_id here; keeping the newer
        // one makes a late GUILD_CREATE harmless after live messages.
        e.set_last(last);
        e.last_pin_ms = e.last_pin_ms.max(last_pin_ms);
        *e != before
    }

    /// A newer `last_message_id` (CHANNEL_UPDATE, PASSIVE_UPDATES, pages).
    pub(crate) fn bump_last(&mut self, ch: ChannelId, last: MessageId) -> bool {
        self.by_channel
            .get_mut(&ch)
            .is_some_and(|e| e.set_last(Some(last)))
    }

    /// A new message (`handleIncomingMessage`). `from_me` acks locally;
    /// `mention` is the result of the mention rules.
    pub(crate) fn incoming(
        &mut self,
        ch: ChannelId,
        id: MessageId,
        from_me: bool,
        mention: bool,
    ) -> bool {
        let Some(e) = self.by_channel.get_mut(&ch) else {
            return false;
        };
        let before = e.clone();
        e.set_last(Some(id));
        if from_me {
            if e.ack_message_id < Some(id) {
                e.ack_message_id = Some(id);
                e.manual = false;
                e.reset_mentions(0);
            }
            return *e != before;
        }
        if e.ack_message_id.is_some_and(|ack| id <= ack) {
            return *e != before;
        }
        // Deviation: Fluxer counts a repeated MESSAGE_CREATE again; it is
        // counted once here.
        if mention && !e.counted.contains(&id) {
            if e.counted.len() >= MAX_COUNTED_MENTIONS {
                e.counted.remove(0);
            }
            e.counted.push(id);
            e.mention_count = e.mention_count.saturating_add(1);
        }
        *e != before
    }

    /// MESSAGE_ACK from the server (`ReadStateServerAckMachine`, without the
    /// version check: the protocol crate does not pass `version` on).
    pub(crate) fn server_ack(
        &mut self,
        ch: ChannelId,
        id: MessageId,
        mentions: u64,
        manual: bool,
    ) -> bool {
        let Some(e) = self.by_channel.get_mut(&ch) else {
            return false;
        };
        let before = e.clone();
        let mentions = clamp_count(mentions);
        if manual {
            e.ack_message_id = Some(id);
            e.manual = true;
            e.reset_mentions(mentions);
        } else if e.ack_message_id.is_some_and(|ack| id < ack) {
            // Older than the current ack: ignored.
        } else if e.ack_message_id == Some(id) {
            e.mention_count = mentions;
        } else {
            e.ack_message_id = Some(id);
            e.manual = false;
            e.reset_mentions(mentions);
        }
        *e != before
    }

    /// The account read the channel up to `id` (after the app sent the ack).
    /// Never moves the ack back. `mentions_after` are the mentions that stay
    /// unread after `id`.
    pub(crate) fn local_ack(&mut self, ch: ChannelId, id: MessageId, mentions_after: u32) -> bool {
        let Some(e) = self.by_channel.get_mut(&ch) else {
            return false;
        };
        if e.ack_message_id.is_some_and(|ack| id < ack) {
            return false;
        }
        let before = e.clone();
        e.set_last(Some(id));
        e.ack_message_id = Some(id);
        e.manual = false;
        e.counted.retain(|m| *m > id);
        e.mention_count = mentions_after;
        *e != before
    }

    /// CHANNEL_PINS_UPDATE.
    pub(crate) fn pins_updated(&mut self, ch: ChannelId, pin_ms: i64) -> bool {
        match self.by_channel.get_mut(&ch) {
            Some(e) if e.last_pin_ms != pin_ms => {
                e.last_pin_ms = pin_ms;
                true
            }
            _ => false,
        }
    }

    /// The pins were looked at.
    pub(crate) fn ack_pins(&mut self, ch: ChannelId) -> bool {
        match self.by_channel.get_mut(&ch) {
            Some(e) if e.ack_pin_ms < e.last_pin_ms => {
                e.ack_pin_ms = e.last_pin_ms;
                true
            }
            _ => false,
        }
    }

    pub(crate) fn remove(&mut self, ch: ChannelId) -> bool {
        self.by_channel.remove(&ch).is_some()
    }

    pub(crate) fn retain(&mut self, mut keep: impl FnMut(ChannelId, &ReadStateEntry) -> bool) {
        self.by_channel.retain(|k, v| keep(*k, v));
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.by_channel.len()
    }

    /// Whether there are no entries.
    pub fn is_empty(&self) -> bool {
        self.by_channel.is_empty()
    }
}
