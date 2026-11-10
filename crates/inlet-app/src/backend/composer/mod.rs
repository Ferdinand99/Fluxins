// SPDX-License-Identifier: AGPL-3.0-or-later
//! The composer: sending and editing messages, the typing indicator, the client commands
//! that call the server (`/nick`, `/msg`), and the data autocomplete needs. GIFs and
//! uploads get their own submodules.
//!
//! Request budget (all REST goes through the protocol's limiter):
//! - one `GET /users/@me` per session, for the account ID and the message length limit;
//! - one send (`POST`) or edit (`PATCH`) per Enter; `/msg` adds one `POST` for the DM;
//!   `/nick` is one `PATCH`;
//! - typing: at most one request per channel every 8 s (the protocol's debounce), and the
//!   UI asks at most once a second;
//! - autocomplete: `GET` roles, own member and custom emoji, at most once per guild every
//!   ten minutes, and only for a guild whose channel the person opened; member search goes
//!   over the gateway (op 8), which the protocol caps at 10 per 10 s.

mod directory;
mod gif;
mod send;
mod upload;

pub(super) use directory::{members_chunk, State};
pub use directory::{ChannelEntry, CustomEmoji, Directory, Person, RoleEntry};
pub use send::{snowflake_u64, Mentions, Outgoing};

use super::Backend;

pub enum Cmd {
    /// Who is signed in and how long a message may be. Answered with [`Ev::Session`].
    Session,
    /// Send a message.
    Send(Outgoing),
    /// New text for one of the account's messages.
    Edit {
        channel_id: String,
        message_id: String,
        content: String,
        mentions: Mentions,
    },
    /// The person is typing in this channel.
    Typing { channel_id: String },
    /// Roles, channels and custom emoji of a guild, for autocomplete.
    Directory { guild_id: String },
    /// Members whose name starts with `query`. Answered with [`Ev::Members`].
    SearchMembers {
        guild_id: String,
        query: String,
        nonce: String,
    },
    /// `/nick`: a new nickname in the guild, `None` to remove it.
    SetNick {
        guild_id: String,
        nick: Option<String>,
    },
    /// `/msg`: a direct message to `user_id`. `message.channel_id` is ignored.
    DirectMessage { user_id: String, message: Outgoing },
}

pub enum Ev {
    Session {
        user_id: String,
        /// The instance's limit for this account, in UTF-16 code units.
        max_message_length: usize,
    },
    /// The server took the message with this nonce.
    Sent { nonce: String },
    /// The message with this nonce was not sent. It can be sent again with the same nonce.
    SendFailed {
        nonce: String,
        reason: String,
        /// The server asked to wait this long (slowmode or a rate limit).
        retry_after_secs: Option<u64>,
    },
    /// An edit was refused (a saved edit shows up as `MessageUpdated`).
    EditFailed { reason: String },
    Directory {
        guild_id: String,
        directory: Directory,
    },
    Members {
        guild_id: String,
        members: Vec<Person>,
    },
    /// A sentence for the notice bar (a command finished or failed).
    Notice(String),
}

pub(super) fn handle(backend: &mut Backend, cmd: Cmd) {
    match cmd {
        Cmd::Session => backend.composer_session(),
        Cmd::Send(message) => backend.send(message),
        Cmd::Edit {
            channel_id,
            message_id,
            content,
            mentions,
        } => backend.edit(channel_id, message_id, content, mentions),
        Cmd::Typing { channel_id } => backend.typing(channel_id),
        Cmd::Directory { guild_id } => backend.directory(guild_id),
        Cmd::SearchMembers {
            guild_id,
            query,
            nonce,
        } => backend.search_members(guild_id, query, nonce),
        Cmd::SetNick { guild_id, nick } => backend.set_nick(guild_id, nick),
        Cmd::DirectMessage { user_id, message } => backend.direct_message(user_id, message),
    }
}
