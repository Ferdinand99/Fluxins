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
//!   used evicted),
//!   members and presences, users, typing entries (they also expire after
//!   10 s), channels, roles, emoji and settings. Messages never go to disk.
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
//! [`AccountState::from_ready`] on READY, then [`AccountState::apply`] for
//! every dispatch and [`AccountState::tick`] about once a second, and
//! re-renders what the returned [`Changes`] names.

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
pub mod threads;
pub mod typing;
pub mod users;
pub mod voice;

pub use account::AccountState;
pub use ids::{ChannelId, GuildId, MessageId, RoleId, UserId};
pub use messages::{MessageWindow, MessageWindows, MessagesPage};
pub use permissions::Permissions;
pub use read_state::Unread;

use smallvec::SmallVec;

/// The caps that keep the state's memory bounded.
pub mod limits {
    /// Messages kept per channel window.
    pub const MAX_MESSAGES_PER_CHANNEL: usize = 500;
    /// Approximate bytes kept per channel window; the oldest messages go first.
    pub const MAX_WINDOW_BYTES: usize = 4 * 1024 * 1024;
    /// A message larger than this (approximately) is not kept. Fluxer's own
    /// limits keep real messages far below it.
    pub const MAX_MESSAGE_BYTES: usize = 256 * 1024;
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
    /// Custom emoji or stickers per guild.
    pub const MAX_EXPRESSIONS_PER_GUILD: usize = 2_000;
    /// Cached members per guild (the account's own member is kept apart).
    pub const MAX_MEMBERS_PER_GUILD: usize = 2_000;
    /// Cached presences (one per user, across guilds).
    pub const MAX_PRESENCES: usize = 10_000;
    /// Cached users.
    pub const MAX_USERS: usize = 20_000;
    /// Read-state entries.
    pub const MAX_READ_STATES: usize = 20_000;
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
}

/// What an update changed, so the app re-renders only that.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Changes {
    /// Everything may have changed (a new READY): re-render all.
    pub reset: bool,
    /// The guild list, or a guild's properties, roles, emoji or stickers.
    pub guilds: bool,
    /// Channels that were created, changed or deleted.
    pub channels: SmallVec<[ChannelId; 4]>,
    /// Channels whose message window changed.
    pub messages: SmallVec<[ChannelId; 4]>,
    /// Guild members (including the account's own).
    pub members: bool,
    /// Presences.
    pub presence: bool,
    /// Unread or mention counts.
    pub read_state: bool,
    /// The account's permissions in some guild.
    pub permissions: bool,
    /// Channels whose typing list changed.
    pub typing: SmallVec<[ChannelId; 4]>,
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
        for c in other.channels {
            push_unique(&mut self.channels, c);
        }
        for c in other.messages {
            push_unique(&mut self.messages, c);
        }
        for c in other.typing {
            push_unique(&mut self.typing, c);
        }
    }

    pub(crate) fn channel(&mut self, c: ChannelId) {
        push_unique(&mut self.channels, c);
    }

    pub(crate) fn message(&mut self, c: ChannelId) {
        push_unique(&mut self.messages, c);
    }

    pub(crate) fn typing(&mut self, c: ChannelId) {
        push_unique(&mut self.typing, c);
    }
}

fn push_unique(v: &mut SmallVec<[ChannelId; 4]>, c: ChannelId) {
    if !v.contains(&c) {
        v.push(c);
    }
}
