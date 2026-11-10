// SPDX-License-Identifier: AGPL-3.0-or-later
//! Unread state, mention badges, muting and notification levels.
//!
//! A [`ReadTracker`] per session feeds the gateway's events into `inlet-state`
//! (which applies Fluxer's unread and mention rules), tells the UI what changed as
//! small [`BadgeUpdate`]s, sends acknowledgements through the protocol's coalesced
//! `ack` (behind the gate in `messages/ack.rs`), and writes notification settings
//! in batches. The UI only ever sees plain values keyed by the IDs it already has.

mod settings;
mod tracker;

use inlet_state::{ChannelId, GuildId};

use super::{Active, Backend};

pub use settings::{Change, SettingsView, Target};
pub use tracker::{Now, ReadHandle, ReadTracker};

/// What the sidebar and the guild rail show for one channel, category or guild.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Badge {
    /// Unread messages: the white pill and bold name (not for a muted guild).
    pub unread: bool,
    /// Unread mentions: the red count.
    pub mentions: u32,
    /// Muted at some level (the channel, its category or the guild).
    pub muted: bool,
    /// The channel or category is muted itself: its row is drawn dimmed.
    pub dimmed: bool,
}

impl Badge {
    /// Nothing to show: the UI keeps no entry for it.
    pub fn is_default(&self) -> bool {
        *self == Badge::default()
    }
}

/// A change to the badges the UI holds. Entries with a default badge are removals.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BadgeUpdate {
    /// Drop every badge first (a new READY).
    pub reset: bool,
    /// Channel and category ID to its badge.
    pub channels: Vec<(String, Badge)>,
    /// Guild ID to its badge; `None` is the direct messages button.
    pub guilds: Vec<(Option<String>, Badge)>,
}

impl BadgeUpdate {
    pub fn is_empty(&self) -> bool {
        !self.reset && self.channels.is_empty() && self.guilds.is_empty()
    }
}

/// Commands of this domain.
pub enum Cmd {
    /// The channel on screen: open, window in front, and the person at the bottom of it.
    /// `None` when nothing is being read (another window in front, scrolled up, no
    /// channel). Messages that arrive in a watched channel are read at once, and opening
    /// one reads what it holds.
    Watch { channel_id: Option<String> },
    /// "Mark as read" for one channel.
    MarkChannelRead { channel_id: String },
    /// "Mark as read" for every channel of a guild (`None`: every direct message).
    MarkGuildRead { guild_id: Option<String> },
    /// "Mark as read" for everything.
    MarkAllRead,
    /// Changes a mute, a notification level or a suppression.
    Change { target: Target, change: Change },
    /// The settings window is open for this guild and channel: send their settings now
    /// and again whenever they change.
    ShowSettings {
        guild_id: Option<String>,
        channel_id: Option<String>,
    },
    /// The settings window closed.
    HideSettings,
}

/// Events of this domain.
pub enum Ev {
    /// Badges changed.
    Badges(BadgeUpdate),
    /// A channel was opened for reading. `last_read` is the message the account had read
    /// up to just before, for the "new messages" divider.
    Opened {
        channel_id: String,
        last_read: Option<String>,
        mentions: u32,
    },
    /// The settings of the guild and channel the settings window shows.
    Settings(SettingsView),
}

pub(super) fn handle(backend: &mut Backend, cmd: Cmd) {
    let Some(Active::Real(r)) = &backend.active else {
        return;
    };
    let read = r.read.clone();
    let mut tracker = read.lock().unwrap_or_else(|e| e.into_inner());
    let now = Now::current();
    match cmd {
        Cmd::Watch { channel_id } => {
            tracker.watch(channel_id.as_deref().and_then(ChannelId::parse), now)
        }
        Cmd::MarkChannelRead { channel_id } => {
            if let Some(ch) = ChannelId::parse(&channel_id) {
                tracker.mark_channel_read(ch, now);
            }
        }
        Cmd::MarkGuildRead { guild_id } => match guild_id.as_deref() {
            None => tracker.mark_guild_read(None, now),
            Some(g) => {
                if let Some(g) = GuildId::parse(g) {
                    tracker.mark_guild_read(Some(g), now);
                }
            }
        },
        Cmd::MarkAllRead => tracker.mark_all_read(now),
        Cmd::Change { target, change } => tracker.change(target, change, now),
        Cmd::ShowSettings {
            guild_id,
            channel_id,
        } => tracker.show_settings(
            guild_id.as_deref().and_then(GuildId::parse),
            channel_id.as_deref().and_then(ChannelId::parse),
        ),
        Cmd::HideSettings => tracker.hide_settings(),
    }
}
