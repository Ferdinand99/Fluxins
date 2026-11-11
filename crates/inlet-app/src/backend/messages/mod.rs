// SPDX-License-Identifier: AGPL-3.0-or-later
//! Messages and the channels they live in: loading guild channel lists and the DM list,
//! loading pages of messages, and turning protocol data into the UI model. One submodule
//! per part, so the tasks that add pins, the inbox, threads, actions and read state each
//! work in their own file.
//!
//! The account's messages live in [`inlet_state::AccountState`] on the UI thread. The
//! backend hands it the gateway's events and the REST pages unchanged, together with the
//! messages they carry already converted and parsed ([`Message`]), so markdown is never
//! parsed on the UI thread.

pub(crate) mod ack;
pub mod actions;
mod fetch;
mod inbox;
pub(crate) mod outbox;
pub mod parse;
mod pins;
mod threads;

pub(super) use fetch::OlderPages;

use inlet_protocol as p;
use inlet_state::MessagesPage;

use super::Backend;
use crate::model::{Channel, Guild, Message};

/// Messages asked for per page (the API allows 1-100).
pub const PAGE_SIZE: u8 = 50;

pub enum Cmd {
    /// Load channels for a guild, or DMs when `guild_id` is `None`.
    OpenGuild { guild_id: Option<String> },
    /// Load the newest page of a channel. The UI opens the channel's window with
    /// `AccountState::begin_load` first. Exactly one `Ev::Page` or `Ev::PageFailed` with
    /// the same `request` answers it, also when the load is aborted or panics.
    OpenChannel { channel_id: String, request: u64 },
    /// Load the page before `before` (the person scrolled to the top). At most one runs
    /// at a time, and the backend drops requests that come within a second of the last.
    /// Answered like `OpenChannel`.
    LoadOlder {
        channel_id: String,
        before: String,
        request: u64,
    },
    /// Send a failed message again with the nonce it had, so the server can drop a
    /// copy that did arrive the first time.
    Resend {
        channel_id: String,
        nonce: String,
        text: String,
    },
    /// A message action: a reaction, delete, pin and the rest (see [`actions`]).
    Action(actions::Cmd),
}

/// A message being sent, or one that could not be sent (a row with "Retry").
#[derive(Clone, Debug)]
pub struct Pending {
    pub channel_id: String,
    /// The nonce sent with the message; the gateway's MESSAGE_CREATE carries it back.
    pub nonce: String,
    pub text: String,
    /// The message it replies to.
    pub reply_to: Option<String>,
    /// The names of its attachments.
    pub files: Vec<String>,
    /// `None` while sending; the reason once sending failed.
    pub failed: Option<String>,
}

pub enum Ev {
    Guilds(Vec<Guild>),
    Channels {
        guild_id: Option<String>,
        channels: Vec<Channel>,
    },
    /// A gateway event for the account state, with the messages it carries converted.
    Dispatch {
        dispatch: Box<p::Dispatch>,
        rendered: Vec<Message>,
    },
    /// A page of messages from REST, with its messages converted.
    Page {
        channel_id: String,
        request: u64,
        page: MessagesPage,
        rendered: Vec<Message>,
    },
    /// Loading a page failed or was given up (aborted, panicked, no session). A failure
    /// is also reported as an error.
    PageFailed {
        channel_id: String,
        request: u64,
        older: bool,
    },
    /// A message is being sent, or could not be sent.
    Pending(Pending),
    /// The server took the message with this nonce: its pending row goes.
    Sent {
        nonce: String,
    },
    /// The answer to a message action.
    Action(actions::Ev),
}

pub(super) fn handle(backend: &mut Backend, cmd: Cmd) {
    match cmd {
        Cmd::OpenGuild { guild_id } => backend.open_guild(guild_id),
        Cmd::OpenChannel {
            channel_id,
            request,
        } => backend.open_channel(channel_id, request),
        Cmd::LoadOlder {
            channel_id,
            before,
            request,
        } => backend.load_older(channel_id, before, request),
        Cmd::Resend {
            channel_id,
            nonce,
            text,
        } => backend.resend(channel_id, nonce, text),
        Cmd::Action(cmd) => backend.message_action(cmd),
    }
}
