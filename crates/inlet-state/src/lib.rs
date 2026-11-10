//! Per-account client state for Inlet.
//!
//! One [`AccountState`] holds what one signed-in account knows: guilds,
//! channels, users, members, roles, the account's permissions, presences,
//! read state with unread and mention counts, typing, notification settings
//! and a window of recent messages for a few channels. It is fed with the
//! protocol crate's typed gateway events ([`AccountState::apply`]) and REST
//! results ([`AccountState::apply_messages`]), and the UI reads from it.
//!
//! # Rules
//!
//! - **Pure.** No network, no files, no threads, no clock of its own: the
//!   app passes the time with [`AccountState::tick`]. The same events in the
//!   same order always give the same state.
//! - **Robust.** Unknown events, events for guilds or channels it does not
//!   know, duplicates and events out of order change nothing or are applied
//!   idempotently. Nothing panics.
//! - **Bounded.** Every collection has a cap ([`limits`]): message windows
//!   (500 messages and about 4 MiB per channel, 30 channels, least recently
//!   used evicted), members (per guild and per account) and presences, users,
//!   typing entries (they also expire after 10 s), channels, roles, emoji and
//!   settings. Every stored object is also trimmed to a byte budget for its
//!   kind. Messages never go to disk.
//! - **Fails closed.** Permissions are 0 until a guild's roles are loaded,
//!   and are recomputed on every role, overwrite, member and guild change
//!   that can affect them ([`AccountState::permissions`]).
//! - **Fluxer's read-state rules.** Unread and mention counting follow
//!   Fluxer's web client (`fluxer_app` `read_state` and `GuildReadState`);
//!   deviations are marked `// Deviation:` in the code.
//!
//! # Use from the app
//!
//! The app keeps one `AccountState` per account in a gpui entity, calls
//! [`AccountState::from_ready_at`] on READY, then [`AccountState::apply`] for
//! every dispatch and [`AccountState::tick`] about once a second, and
//! re-renders what the returned [`Changes`] names. Before it asks for the
//! newest page of a channel it calls [`AccountState::begin_load`], so live
//! messages that arrive while the request is in flight are kept.

mod account;
mod bounded;
pub mod channels;
pub mod guild_settings;
pub mod guilds;
mod ids;
pub mod members;
pub mod messages;
pub mod permissions;
pub mod presence;
pub mod read_state;
pub mod relationships;
pub mod roles;
pub mod settings;
mod size;
pub mod threads;
pub mod typing;
pub mod users;
pub mod voice;

pub use account::AccountState;
pub use ids::{ChannelId, GuildId, MessageId, RoleId, UserId};
pub use messages::{MessageWindow, MessageWindows, MessagesPage};
pub use permissions::Permissions;
pub use read_state::{AckKind, Unread};

use smallvec::SmallVec;

/// The caps that keep the state's memory bounded.
pub mod limits {
    /// Messages kept per channel window.
    pub const MAX_MESSAGES_PER_CHANNEL: usize = 500;
    /// Approximate bytes kept per channel window; the oldest messages go
    /// first. The estimate undercounts `serde_json`'s real heap use by about
    /// half, so the real cap is somewhat higher.
    pub const MAX_WINDOW_BYTES: usize = 4 * 1024 * 1024;
    /// A message larger than this (approximately) is kept as a stub without
    /// content. Fluxer's own limits keep real messages far below it.
    pub const MAX_MESSAGE_BYTES: usize = 256 * 1024;
    /// Live messages buffered while a window waits for its newest page.
    pub const MAX_PENDING_LIVE: usize = 100;
    /// Channels with a message window; the least recently used is evicted.
    pub const MAX_MESSAGE_WINDOWS: usize = 30;
    /// Deleted message IDs remembered per window, so a page fetched before
    /// the delete does not bring the message back.
    pub const MAX_RECENTLY_DELETED: usize = 128;
    /// Reactions (distinct emoji) kept per message.
    pub const MAX_REACTIONS_PER_MESSAGE: usize = 100;
    /// Guilds.
    pub const MAX_GUILDS: usize = 1_000;
    /// Channels per guild (Fluxer allows 500).
    pub const MAX_CHANNELS_PER_GUILD: usize = 2_000;
    /// Direct and group direct message channels.
    pub const MAX_PRIVATE_CHANNELS: usize = 5_000;
    /// Roles per guild (Fluxer allows 250).
    pub const MAX_ROLES_PER_GUILD: usize = 1_000;
    /// Role IDs kept per cached member (the account's own member keeps all).
    pub const MAX_ROLES_PER_MEMBER: usize = 256;
    /// Permission overwrites per channel; more makes the channel hidden.
    pub const MAX_OVERWRITES: usize = 1_000;
    /// Custom emoji or stickers per guild.
    pub const MAX_EXPRESSIONS_PER_GUILD: usize = 2_000;
    /// Cached members per guild (the account's own member is kept apart).
    pub const MAX_MEMBERS_PER_GUILD: usize = 2_000;
    /// Cached members across every guild of the account.
    pub const MAX_MEMBERS_TOTAL: usize = 50_000;
    /// Cached presences (one per user, across guilds).
    pub const MAX_PRESENCES: usize = 10_000;
    /// Cached users.
    pub const MAX_USERS: usize = 20_000;
    /// Read-state entries of known channels.
    pub const MAX_READ_STATES: usize = 100_000;
    /// Read states kept for channels that are not known right now (READY
    /// rows for guilds still loading, channels whose access was lost).
    pub const MAX_ARCHIVED_READ_STATES: usize = 20_000;
    /// Message IDs counted as mentions since the last ack, per channel.
    pub const MAX_COUNTED_MENTIONS: usize = 256;
    /// Typing users per channel.
    pub const MAX_TYPING_PER_CHANNEL: usize = 32;
    /// Channels with someone typing.
    pub const MAX_TYPING_CHANNELS: usize = 256;
    /// How long a typing indicator lasts (Fluxer's `TYPING_ROLLING_EXPIRY_MS`).
    pub const TYPING_EXPIRY_MS: i64 = 10_000;
    /// Guild notification settings entries.
    pub const MAX_GUILD_SETTINGS: usize = 1_100;
    /// Channel overrides per guild settings entry.
    pub const MAX_CHANNEL_OVERRIDES: usize = 2_000;
    /// Raw `extra` bytes kept per user.
    pub const MAX_USER_EXTRA_BYTES: usize = 16 * 1024;
    /// Raw `extra` bytes kept per member.
    pub const MAX_MEMBER_EXTRA_BYTES: usize = 16 * 1024;
    /// Raw `extra` bytes kept per role.
    pub const MAX_ROLE_EXTRA_BYTES: usize = 8 * 1024;
    /// Raw `extra` bytes kept per channel.
    pub const MAX_CHANNEL_EXTRA_BYTES: usize = 64 * 1024;
    /// Raw `extra` bytes kept per guild record.
    pub const MAX_GUILD_EXTRA_BYTES: usize = 256 * 1024;
    /// Items listed in one [`crate::ChangeSet`] before it says "all".
    pub const MAX_CHANGE_ITEMS: usize = 64;
}

/// A bounded list of changed items. Past [`limits::MAX_CHANGE_ITEMS`] it
/// switches to "all", so the app re-renders everything of that kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeSet<T> {
    items: SmallVec<[T; 4]>,
    all: bool,
}

impl<T> Default for ChangeSet<T> {
    fn default() -> Self {
        ChangeSet {
            items: SmallVec::new(),
            all: false,
        }
    }
}

impl<T: Copy + PartialEq> ChangeSet<T> {
    /// Whether nothing changed.
    pub fn is_empty(&self) -> bool {
        !self.all && self.items.is_empty()
    }

    /// Whether too much changed to list: treat every item as changed.
    pub fn is_all(&self) -> bool {
        self.all
    }

    /// Whether `item` changed (always true when [`ChangeSet::is_all`]).
    pub fn contains(&self, item: &T) -> bool {
        self.all || self.items.contains(item)
    }

    /// The listed items (empty when [`ChangeSet::is_all`]).
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.items.iter()
    }

    pub(crate) fn add(&mut self, item: T) {
        if self.all || self.items.contains(&item) {
            return;
        }
        if self.items.len() >= limits::MAX_CHANGE_ITEMS {
            self.set_all();
        } else {
            self.items.push(item);
        }
    }

    pub(crate) fn set_all(&mut self) {
        self.all = true;
        self.items.clear();
    }

    fn merge(&mut self, other: ChangeSet<T>) {
        if other.all {
            self.set_all();
        }
        for i in other.items {
            self.add(i);
        }
    }
}

/// What an update changed, so the app re-renders only that.
///
/// The `bool` fields say "something of this kind changed"; the change sets
/// say where.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Changes {
    /// Everything may have changed (a new READY): re-render all.
    pub reset: bool,
    /// The guild list, or a guild's properties, roles, emoji or stickers.
    pub guilds: bool,
    /// Channels that were created, changed or deleted.
    pub channels: ChangeSet<ChannelId>,
    /// Channels whose message window changed or was evicted.
    pub messages: ChangeSet<ChannelId>,
    /// Some guild's members changed.
    pub members: bool,
    /// Guilds whose cached members changed (the account's own included).
    pub member_guilds: ChangeSet<GuildId>,
    /// Presences.
    pub presence: bool,
    /// Some unread or mention count changed.
    pub read_state: bool,
    /// Channels whose read state changed.
    pub read_state_channels: ChangeSet<ChannelId>,
    /// Guild pills to refresh; `None` is the direct messages button.
    pub read_state_guilds: ChangeSet<Option<GuildId>>,
    /// The account's permissions changed in some guild.
    pub permissions: bool,
    /// Guilds where the account's permissions changed.
    pub permission_guilds: ChangeSet<GuildId>,
    /// Channels whose typing list changed.
    pub typing: ChangeSet<ChannelId>,
    /// The signed-in account or other users.
    pub users: bool,
    /// Notification settings (mutes, levels).
    pub settings: bool,
}

impl Changes {
    /// Whether nothing changed.
    pub fn is_empty(&self) -> bool {
        *self == Changes::default()
    }

    /// Adds the changes of `other`.
    pub fn merge(&mut self, other: Changes) {
        self.reset |= other.reset;
        self.guilds |= other.guilds;
        self.members |= other.members;
        self.presence |= other.presence;
        self.read_state |= other.read_state;
        self.permissions |= other.permissions;
        self.users |= other.users;
        self.settings |= other.settings;
        self.channels.merge(other.channels);
        self.messages.merge(other.messages);
        self.member_guilds.merge(other.member_guilds);
        self.read_state_channels.merge(other.read_state_channels);
        self.read_state_guilds.merge(other.read_state_guilds);
        self.permission_guilds.merge(other.permission_guilds);
        self.typing.merge(other.typing);
    }

    pub(crate) fn channel(&mut self, c: ChannelId) {
        self.channels.add(c);
    }

    pub(crate) fn message(&mut self, c: ChannelId) {
        self.messages.add(c);
    }

    pub(crate) fn typing(&mut self, c: ChannelId) {
        self.typing.add(c);
    }

    /// A channel's read state changed (`guild` `None` for a DM).
    pub(crate) fn read(&mut self, c: ChannelId, guild: Option<GuildId>) {
        self.read_state = true;
        self.read_state_channels.add(c);
        self.read_state_guilds.add(guild);
    }

    /// Every read state may have changed (mutes, settings).
    pub(crate) fn read_all(&mut self) {
        self.read_state = true;
        self.read_state_channels.set_all();
        self.read_state_guilds.set_all();
    }

    pub(crate) fn perms(&mut self, g: GuildId) {
        self.permissions = true;
        self.permission_guilds.add(g);
        // Hidden channels drop out of the guild pill.
        self.read_state = true;
        self.read_state_guilds.add(Some(g));
    }

    pub(crate) fn members_of(&mut self, g: GuildId) {
        self.members = true;
        self.member_guilds.add(g);
    }
}
