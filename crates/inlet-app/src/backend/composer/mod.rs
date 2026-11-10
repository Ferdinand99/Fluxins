// SPDX-License-Identifier: AGPL-3.0-or-later
//! The composer: sending messages, and later GIFs and uploads.

mod gif;
mod send;
mod upload;

use super::Backend;

pub enum Cmd {
    Send { channel_id: String, text: String },
}

/// Events of this domain. None yet.
pub enum Ev {}

pub(super) fn handle(backend: &mut Backend, cmd: Cmd) {
    match cmd {
        Cmd::Send { channel_id, text } => backend.send(channel_id, text),
    }
}
