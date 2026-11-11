// SPDX-License-Identifier: AGPL-3.0-or-later
//! Social: presence, member lists, relationships and the DM list's actions. The tasks
//! that build these fill one submodule each.

mod members;
pub mod presence;
pub mod relationships;

use super::Backend;

/// Commands of this domain.
#[derive(Clone, Debug, PartialEq)]
pub enum Cmd {
    /// Presence and status (see [`presence`]).
    Presence(presence::Cmd),
    /// Friends, blocks and the DM list's actions (see [`relationships`]).
    Relationships(relationships::Cmd),
}

/// Events of this domain.
#[derive(Clone, Debug, PartialEq)]
pub enum Ev {
    /// Presence and status (see [`presence`]).
    Presence(presence::Ev),
    /// Answers to friend, block and DM actions (see [`relationships`]).
    Relationships(relationships::Ev),
}

impl From<presence::Cmd> for super::Command {
    fn from(cmd: presence::Cmd) -> Self {
        super::Command::Social(Cmd::Presence(cmd))
    }
}

impl From<relationships::Cmd> for super::Command {
    fn from(cmd: relationships::Cmd) -> Self {
        super::Command::Social(Cmd::Relationships(cmd))
    }
}

pub(super) fn handle(backend: &mut Backend, cmd: Cmd) {
    match cmd {
        Cmd::Presence(cmd) => presence::handle(backend, cmd),
        Cmd::Relationships(cmd) => relationships::handle(backend, cmd),
    }
}
