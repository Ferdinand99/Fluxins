// SPDX-License-Identifier: AGPL-3.0-or-later
//! Social: presence, member lists and relationships. The tasks that build these fill
//! one submodule each.

mod members;
pub mod presence;
mod relationships;

use super::Backend;

/// Commands of this domain.
#[derive(Clone, Debug, PartialEq)]
pub enum Cmd {
    /// Presence and status (see [`presence`]).
    Presence(presence::Cmd),
}

/// Events of this domain.
#[derive(Clone, Debug, PartialEq)]
pub enum Ev {
    /// Presence and status (see [`presence`]).
    Presence(presence::Ev),
}

impl From<presence::Cmd> for super::Command {
    fn from(cmd: presence::Cmd) -> Self {
        super::Command::Social(Cmd::Presence(cmd))
    }
}

pub(super) fn handle(backend: &mut Backend, cmd: Cmd) {
    match cmd {
        Cmd::Presence(cmd) => presence::handle(backend, cmd),
    }
}
