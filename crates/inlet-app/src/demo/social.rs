// SPDX-License-Identifier: AGPL-3.0-or-later
//! Demo direct messages: the DM list and its messages.

use std::collections::HashMap;

use super::{channel, msg};
use crate::model::{Channel, ChannelKind, Message};

/// Adds the DM list (the `None` guild).
pub fn dm_channels(channels: &mut HashMap<Option<String>, Vec<Channel>>) {
    channels.insert(
        None,
        vec![
            channel("dm-ada", "ada", ChannelKind::Dm, None, 0),
            channel(
                "dm-group",
                "ada, linus, grace",
                ChannelKind::GroupDm,
                None,
                1,
            ),
        ],
    );
}

/// Adds the messages of the DMs.
pub fn dm_messages(messages: &mut HashMap<String, Vec<Message>>) {
    messages.insert(
        "dm-ada".into(),
        vec![msg(
            20,
            "dm-ada",
            "ada",
            "10:01",
            "Hi! This is a direct message.",
        )],
    );
}
