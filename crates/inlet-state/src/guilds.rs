//! Guilds: properties, roles, members, expressions and the account's
//! cached permissions.

use std::collections::{BTreeMap, BTreeSet};

use inlet_protocol::models::parse_timestamp_ms;
use inlet_protocol::{Guild, GuildEmoji, GuildSticker};

use crate::ids::{ChannelId, GuildId};
use crate::members::Members;
use crate::roles::Roles;

/// What the account knows about one guild.
#[derive(Debug, Clone, PartialEq)]
pub struct GuildState {
    /// Guild ID.
    pub id: GuildId,
    /// The guild record; `None` while the guild is unavailable and was never loaded.
    pub properties: Option<Guild>,
    /// Temporarily unavailable (outage); the rest is the last known state.
    pub unavailable: bool,
    /// The account visits the guild without being a member.
    pub visiting: bool,
    /// Roles.
    pub roles: Roles,
    /// Members.
    pub members: Members,
    /// The guild's channels (threads included).
    pub channels: BTreeSet<ChannelId>,
    /// Total members, if known.
    pub member_count: Option<u64>,
    /// Members online, if known.
    pub online_count: Option<u64>,
    /// Custom emoji.
    pub emojis: Vec<GuildEmoji>,
    /// Stickers.
    pub stickers: Vec<GuildSticker>,
    /// The account was removed from the guild (kicked or left); everything is denied.
    pub left: bool,
    /// Permission bits per channel, recomputed on every change that can affect them.
    pub(crate) perms: BTreeMap<ChannelId, u64>,
    /// Guild-level permission bits.
    pub(crate) guild_perms: u64,
    /// Whether the account's timeout was running at the last recompute.
    pub(crate) timed_out: bool,
}

impl GuildState {
    pub(crate) fn new(id: GuildId) -> Self {
        GuildState {
            id,
            properties: None,
            unavailable: false,
            visiting: false,
            roles: Roles::default(),
            members: Members::default(),
            channels: BTreeSet::new(),
            member_count: None,
            online_count: None,
            emojis: Vec::new(),
            stickers: Vec::new(),
            left: false,
            perms: BTreeMap::new(),
            guild_perms: 0,
            timed_out: false,
        }
    }

    /// The guild's name, or an empty string while unknown.
    pub fn name(&self) -> &str {
        self.properties.as_ref().map_or("", |p| p.name.as_str())
    }

    /// The owner's user ID.
    pub fn owner_id(&self) -> Option<&str> {
        self.properties.as_ref().and_then(|p| p.owner_id.as_deref())
    }

    /// The guild's default notification level (`default_message_notifications`).
    pub fn default_notifications(&self) -> i64 {
        self.properties
            .as_ref()
            .and_then(|p| p.extra.get("default_message_notifications"))
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(crate::guild_settings::notifications::ALL_MESSAGES)
    }

    /// When the account joined, in Unix milliseconds: the account's member
    /// `joined_at`, or the guild's `joined_at`.
    pub fn joined_at_ms(&self) -> Option<i64> {
        self.members
            .me()
            .and_then(|m| m.joined_at.as_deref())
            .or_else(|| {
                self.properties
                    .as_ref()
                    .and_then(|p| p.extra.get("joined_at"))
                    .and_then(serde_json::Value::as_str)
            })
            .and_then(parse_timestamp_ms)
    }

    /// Whether the roles are loaded (permissions fail closed until they are).
    pub fn roles_loaded(&self) -> bool {
        self.roles.loaded(self.id)
    }
}
