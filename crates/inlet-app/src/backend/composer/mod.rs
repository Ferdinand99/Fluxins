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
//! - a Retry more than five minutes after an unclear failure first reads up to 100 of the
//!   channel's messages since the first send (one `GET`) and sends only if none of them
//!   is an own message with the same text (lists carry no nonce);
//! - autocomplete reads roles, channels, emoji and rights from the account state (no
//!   request); member search goes over the gateway (op 8), which the protocol caps at 10
//!   per 10 s.

mod directory;
mod gif;
mod send;
mod upload;

pub(super) use directory::{members_chunk, State};
pub use directory::{ChannelEntry, CustomEmoji, Directory, Person, Rights, RoleEntry};
pub use send::{snowflake_u64, Mentions, Outgoing};

use super::Backend;

pub enum Cmd {
    /// Who is signed in and how long a message may be. Answered with [`Ev::Session`]
    /// carrying the same `token`.
    Session { token: u64 },
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
        /// The `token` of the request.
        token: u64,
        user_id: String,
        /// The instance's limit for this account, in UTF-16 code units.
        max_message_length: usize,
    },
    /// A send was refused with a wait (slowmode or a rate limit); the countdown takes it.
    /// The message list shows the failed message itself.
    SlowmodeWait { channel_id: String, secs: u64 },
    /// A `/msg` was not delivered.
    DirectMessageFailed { nonce: String, reason: String },
    /// The server took an edit (the gateway brings the new text to the list).
    Edited { message_id: String },
    /// An edit was refused.
    EditFailed { message_id: String, reason: String },
    Members {
        guild_id: String,
        members: Vec<Person>,
    },
    /// A sentence for the notice bar (a command finished or failed).
    Notice(String),
}

pub(super) fn handle(backend: &mut Backend, cmd: Cmd) {
    match cmd {
        Cmd::Session { token } => backend.composer_session(token),
        Cmd::Send(message) => backend.send(message),
        Cmd::Edit {
            channel_id,
            message_id,
            content,
            mentions,
        } => backend.edit(channel_id, message_id, content, mentions),
        Cmd::Typing { channel_id } => backend.typing(channel_id),
        Cmd::SearchMembers {
            guild_id,
            query,
            nonce,
        } => backend.search_members(guild_id, query, nonce),
        Cmd::SetNick { guild_id, nick } => backend.set_nick(guild_id, nick),
        Cmd::DirectMessage { user_id, message } => backend.direct_message(user_id, message),
    }
}
