//! Who is typing where. Entries expire after
//! [`crate::limits::TYPING_EXPIRY_MS`] (Fluxer's rolling expiry), or when the
//! user's message arrives.

use std::collections::BTreeMap;

use crate::ids::{ChannelId, UserId};
use crate::limits::{MAX_TYPING_CHANNELS, MAX_TYPING_PER_CHANNEL, TYPING_EXPIRY_MS};

/// Typing users per channel, with the time each entry expires.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Typing {
    by_channel: BTreeMap<ChannelId, BTreeMap<UserId, i64>>,
}

impl Typing {
    /// Records that `user` started typing at `now_ms`. Returns whether the
    /// list of typing users changed (a refresh of a running entry does not),
    /// and a channel whose entries were dropped to make room.
    pub(crate) fn start(
        &mut self,
        channel: ChannelId,
        user: UserId,
        now_ms: i64,
    ) -> (bool, Option<ChannelId>) {
        let expires = now_ms.saturating_add(TYPING_EXPIRY_MS);
        let mut evicted = None;
        if !self.by_channel.contains_key(&channel) && self.by_channel.len() >= MAX_TYPING_CHANNELS {
            // Make room by dropping the channel whose entries end first.
            let oldest = self
                .by_channel
                .iter()
                .min_by_key(|(id, users)| (users.values().max().copied().unwrap_or(0), **id))
                .map(|(id, _)| *id);
            if let Some(id) = oldest {
                self.by_channel.remove(&id);
                evicted = Some(id);
            }
        }
        let users = self.by_channel.entry(channel).or_default();
        let new = !users.contains_key(&user);
        if new && users.len() >= MAX_TYPING_PER_CHANNEL {
            let first = users
                .iter()
                .min_by_key(|(id, e)| (**e, **id))
                .map(|(id, _)| *id);
            if let Some(id) = first {
                users.remove(&id);
            }
        }
        let e = users.entry(user).or_insert(expires);
        *e = (*e).max(expires);
        (new, evicted)
    }

    /// Removes `user` from `channel` (their message arrived).
    pub(crate) fn stop(&mut self, channel: ChannelId, user: UserId) -> bool {
        let Some(users) = self.by_channel.get_mut(&channel) else {
            return false;
        };
        let removed = users.remove(&user).is_some();
        if users.is_empty() {
            self.by_channel.remove(&channel);
        }
        removed
    }

    /// Drops a channel's entries.
    pub(crate) fn clear_channel(&mut self, channel: ChannelId) -> bool {
        self.by_channel.remove(&channel).is_some()
    }

    /// Removes entries that expired by `now_ms`; returns the channels that changed.
    pub(crate) fn expire(&mut self, now_ms: i64) -> Vec<ChannelId> {
        let mut changed = Vec::new();
        self.by_channel.retain(|ch, users| {
            let before = users.len();
            users.retain(|_, e| *e > now_ms);
            if users.len() != before {
                changed.push(*ch);
            }
            !users.is_empty()
        });
        changed
    }

    /// Users typing in `channel` at `now_ms`, in user ID order.
    pub fn users(&self, channel: ChannelId, now_ms: i64) -> Vec<UserId> {
        self.by_channel.get(&channel).map_or_else(Vec::new, |u| {
            u.iter()
                .filter(|(_, e)| **e > now_ms)
                .map(|(id, _)| *id)
                .collect()
        })
    }

    /// Number of typing entries.
    pub fn len(&self) -> usize {
        self.by_channel.values().map(BTreeMap::len).sum()
    }

    /// Whether nobody is typing.
    pub fn is_empty(&self) -> bool {
        self.by_channel.is_empty()
    }
}
