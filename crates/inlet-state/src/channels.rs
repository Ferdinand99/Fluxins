//! Channels: guild channels, threads, DMs and group DMs.

use std::collections::{BTreeMap, BTreeSet};

use inlet_protocol::models::channel_type::*;
use inlet_protocol::models::overwrite_type;
use inlet_protocol::Channel;
use serde_json::Value;

use crate::ids::{ChannelId, GuildId};
use crate::limits::MAX_OVERWRITES;

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

/// One permission overwrite, stored compactly (Fluxer allows 10,000 per channel).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Overwrite {
    /// Role or user ID.
    pub id: u64,
    /// [`inlet_protocol::models::overwrite_type`]: 0 role, 1 member.
    pub kind: u8,
    /// Allowed bits.
    pub allow: u64,
    /// Denied bits.
    pub deny: u64,
}

/// What the stored channel's `extra` holds in place of its overwrites. The
/// protocol crate's calculator reads it as an unreadable list, so code that
/// runs the calculator on a stored channel by mistake hides it (fails
/// closed). [`crate::AccountState::permissions`] uses the compact list.
pub const OVERWRITES_MOVED: bool = false;

/// Reads `permission_overwrites` the way the calculator does, into the
/// compact form: `Some(list)`, or `None` when the list is unreadable or too
/// long (the channel is then hidden from everyone but owner and administrators).
fn parse_overwrites(v: Option<&Value>) -> Option<Vec<Overwrite>> {
    let items = match v {
        None | Some(Value::Null) => return Some(Vec::new()),
        Some(Value::Array(items)) => items,
        Some(_) => return None,
    };
    if items.len() > MAX_OVERWRITES {
        return None;
    }
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        // An entry without a usable target denies everything to everyone.
        let id = match item.get("id") {
            Some(Value::String(s)) => ChannelId::parse(s)?.0,
            Some(Value::Number(n)) => n.as_u64()?,
            _ => return None,
        };
        let kind = match item.get("type").and_then(Value::as_i64) {
            Some(k @ (overwrite_type::ROLE | overwrite_type::MEMBER)) => k as u8,
            _ => return None,
        };
        let num = |key: &str| match item.get(key) {
            Some(Value::String(s)) => s.trim().parse::<u64>().ok(),
            Some(Value::Number(n)) => n.as_u64(),
            _ => None,
        };
        // A mask that does not parse allows nothing and denies everything.
        let (allow, deny) = match (num("allow"), num("deny")) {
            (Some(a), Some(d)) => (a, d),
            _ => (0, u64::MAX),
        };
        out.push(Overwrite {
            id,
            kind,
            allow,
            deny,
        });
    }
    Some(out)
}

/// A stored channel with its guild, parent and overwrites parsed once.
#[derive(Debug, Clone, PartialEq)]
struct Entry {
    channel: Channel,
    guild: Option<GuildId>,
    parent: Option<ChannelId>,
    /// `None`: unreadable or over [`MAX_OVERWRITES`] (fails closed).
    overwrites: Option<Vec<Overwrite>>,
}

/// Every channel the account knows, by ID.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Channels {
    by_id: BTreeMap<ChannelId, Entry>,
    private: BTreeSet<ChannelId>,
}

impl Channels {
    /// A channel.
    pub fn get(&self, id: ChannelId) -> Option<&Channel> {
        self.by_id.get(&id).map(|e| &e.channel)
    }

    /// Changes fields that are not the guild or the parent.
    pub(crate) fn get_mut(&mut self, id: ChannelId) -> Option<&mut Channel> {
        self.by_id.get_mut(&id).map(|e| &mut e.channel)
    }

    /// The guild a known channel belongs to.
    pub fn guild_of(&self, id: ChannelId) -> Option<GuildId> {
        self.by_id.get(&id)?.guild
    }

    /// The parent (category, or a thread's channel).
    pub fn parent_of(&self, id: ChannelId) -> Option<ChannelId> {
        self.by_id.get(&id)?.parent
    }

    /// The channel's overwrites; `None` when they were unreadable or too
    /// many (the channel is hidden).
    pub fn overwrites(&self, id: ChannelId) -> Option<&[Overwrite]> {
        self.by_id.get(&id)?.overwrites.as_deref()
    }

    /// A copy of the channel the protocol crate's calculator can read: the
    /// fields it looks at, with only the overwrites that can apply to a
    /// member with `roles` (the everyone role is `guild`) and user `me`.
    pub(crate) fn permission_view(
        &self,
        id: ChannelId,
        guild: GuildId,
        roles: &BTreeSet<u64>,
        me: u64,
    ) -> Option<Channel> {
        let e = self.by_id.get(&id)?;
        let mut extra = serde_json::Map::new();
        let overwrites = match &e.overwrites {
            None => Value::Bool(OVERWRITES_MOVED),
            Some(list) => Value::Array(
                list.iter()
                    .filter(|o| match o.kind {
                        0 => o.id == guild.0 || roles.contains(&o.id),
                        _ => o.id == me,
                    })
                    .map(|o| {
                        serde_json::json!({"id": o.id.to_string(), "type": o.kind,
                                           "allow": o.allow.to_string(), "deny": o.deny.to_string()})
                    })
                    .collect(),
            ),
        };
        extra.insert("permission_overwrites".into(), overwrites);
        for key in ["thread_metadata", "member"] {
            if let Some(v) = e.channel.extra.get(key) {
                extra.insert(key.into(), v.clone());
            }
        }
        Some(Channel {
            id: e.channel.id.clone(),
            kind: e.channel.kind,
            guild_id: e.channel.guild_id.clone(),
            name: None,
            topic: None,
            position: None,
            parent_id: e.channel.parent_id.clone(),
            last_message_id: None,
            recipients: Vec::new(),
            nsfw: false,
            rate_limit_per_user: None,
            extra,
        })
    }

    /// Stores a channel. Its overwrites move into the compact list; the
    /// stored `extra` keeps an unreadable marker in their place. Returns
    /// whether anything changed.
    pub(crate) fn insert(&mut self, id: ChannelId, mut channel: Channel) -> bool {
        let raw = channel.extra.remove("permission_overwrites");
        let overwrites = parse_overwrites(raw.as_ref());
        if channel.guild_id.is_some() {
            channel.extra.insert(
                "permission_overwrites".into(),
                Value::Bool(OVERWRITES_MOVED),
            );
        }
        let guild = GuildId::parse_opt(channel.guild_id.as_deref());
        if guild.is_none() {
            self.private.insert(id);
        } else {
            self.private.remove(&id);
        }
        match self.by_id.get(&id) {
            Some(old) if old.channel == channel && old.overwrites == overwrites => false,
            _ => {
                let parent = ChannelId::parse_opt(channel.parent_id.as_deref());
                self.by_id.insert(
                    id,
                    Entry {
                        channel,
                        guild,
                        parent,
                        overwrites,
                    },
                );
                true
            }
        }
    }

    pub(crate) fn remove(&mut self, id: ChannelId) -> Option<Channel> {
        self.private.remove(&id);
        self.by_id.remove(&id).map(|e| e.channel)
    }

    /// Direct messages, group DMs and notes, in ID order.
    pub fn private(&self) -> impl Iterator<Item = (ChannelId, &Channel)> {
        self.private
            .iter()
            .filter_map(|id| self.get(*id).map(|c| (*id, c)))
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
