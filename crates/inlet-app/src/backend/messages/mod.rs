// SPDX-License-Identifier: AGPL-3.0-or-later
//! Messages and the channels they live in: loading guild channel lists and the DM list,
//! loading messages, and turning protocol data into the UI model. One submodule per
//! part, so the tasks that add pins, the inbox, threads, actions and read state each
//! work in their own file.

mod ack;
mod actions;
mod fetch;
mod inbox;
pub mod parse;
mod pins;
mod threads;

use super::Backend;
use crate::model::{Channel, Guild, Message};

pub enum Cmd {
    /// Load channels for a guild, or DMs when `guild_id` is `None`.
    OpenGuild {
        guild_id: Option<String>,
    },
    OpenChannel {
        channel_id: String,
    },
}

pub enum Ev {
    Guilds(Vec<Guild>),
    Channels {
        guild_id: Option<String>,
        channels: Vec<Channel>,
    },
    Messages {
        channel_id: String,
        messages: Vec<Message>,
    },
    MessageCreated(Message),
    MessageUpdated(Message),
    MessageDeleted {
        channel_id: String,
        message_id: String,
    },
}

pub(super) fn handle(backend: &mut Backend, cmd: Cmd) {
    match cmd {
        Cmd::OpenGuild { guild_id } => backend.open_guild(guild_id),
        Cmd::OpenChannel { channel_id } => backend.open_channel(channel_id),
    }
}
