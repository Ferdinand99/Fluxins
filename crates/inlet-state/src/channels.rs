//! Channels: guild channels, threads, DMs and group DMs.

use std::collections::{BTreeMap, BTreeSet};

use inlet_protocol::models::channel_type::*;
use inlet_protocol::Channel;

use crate::ids::{ChannelId, GuildId};

/// Fluxer's `GUILD_TEXT_BASED_CHANNEL_TYPES`: guild channels that count
/// towards a guild's unread state (voice channels have a text chat).
pub fn is_guild_text_based(kind: i64) -> bool {
    matches!(kind, GUILD_TEXT | GUILD_VOICE | GUILD_ANNOUNCEMENT)
}

/// Fluxer's `TEXT_BASED_CHANNEL_TYPES` plus thread types: channels that
/// have a read state.
pub fn has_read_state(kind: i64) -> bool {
    is_guild_text_based(kind)
        || matches!(
            kind,
            DM | GROUP_DM
                | PERSONAL_NOTES
                | ANNOUNCEMENT_THREAD
                | PUBLIC_THREAD
                | PRIVATE_THREAD
                | GUILD_FORUM
                | GUILD_MEDIA
        )
}

/// Whether the channel is a DM, group DM or personal notes.
pub fn is_private(kind: i64) -> bool {
    matches!(kind, DM | GROUP_DM | PERSONAL_NOTES)
}

/// Every channel the account knows, by ID.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Channels {
    by_id: BTreeMap<ChannelId, Channel>,
    private: BTreeSet<ChannelId>,
}

impl Channels {
    /// A channel.
    pub fn get(&self, id: ChannelId) -> Option<&Channel> {
        self.by_id.get(&id)
    }

    pub(crate) fn get_mut(&mut self, id: ChannelId) -> Option<&mut Channel> {
        self.by_id.get_mut(&id)
    }

    /// The guild a known channel belongs to.
    pub fn guild_of(&self, id: ChannelId) -> Option<GuildId> {
        GuildId::parse_opt(self.get(id)?.guild_id.as_deref())
    }

    /// The parent (category, or a thread's channel).
    pub fn parent_of(&self, id: ChannelId) -> Option<ChannelId> {
        ChannelId::parse_opt(self.get(id)?.parent_id.as_deref())
    }

    /// Stores a channel. Returns whether anything changed.
    pub(crate) fn insert(&mut self, id: ChannelId, channel: Channel) -> bool {
        if channel.guild_id.is_none() {
            self.private.insert(id);
        } else {
            self.private.remove(&id);
        }
        match self.by_id.get(&id) {
            Some(old) if *old == channel => false,
            _ => {
                self.by_id.insert(id, channel);
                true
            }
        }
    }

    pub(crate) fn remove(&mut self, id: ChannelId) -> Option<Channel> {
        self.private.remove(&id);
        self.by_id.remove(&id)
    }

    /// Direct messages, group DMs and notes, in ID order.
    pub fn private(&self) -> impl Iterator<Item = (ChannelId, &Channel)> {
        self.private
            .iter()
            .filter_map(|id| self.by_id.get(id).map(|c| (*id, c)))
    }

    /// Number of private channels.
    pub fn private_len(&self) -> usize {
        self.private.len()
    }

    /// Number of channels.
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    /// Whether there are no channels.
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }
}
