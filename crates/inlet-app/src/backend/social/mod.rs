// SPDX-License-Identifier: AGPL-3.0-or-later
//! Social: presence, member lists, profiles and relationships. The tasks that build
//! these fill one submodule each.

pub mod members;
mod presence;
pub mod profile;
mod relationships;

use super::Backend;

/// Commands of this domain.
#[derive(Clone, Debug, PartialEq)]
pub enum Cmd {
    /// The member list on screen and member searches (see [`members`]).
    Members(members::Cmd),
    /// Profiles and notes (see [`profile`]).
    Profile(profile::Cmd),
}

/// Events of this domain.
pub enum Ev {
    /// Profiles and notes (see [`profile`]).
    Profile(profile::Ev),
}

impl From<members::Cmd> for super::Command {
    fn from(cmd: members::Cmd) -> Self {
        super::Command::Social(Cmd::Members(cmd))
    }
}

impl From<profile::Cmd> for super::Command {
    fn from(cmd: profile::Cmd) -> Self {
        super::Command::Social(Cmd::Profile(cmd))
    }
}

/// This domain's part of the backend: the member list subscription and the profile
/// fetch of the active account.
#[derive(Default)]
pub struct State {
    pub members: members::State,
    pub profile: profile::State,
}

impl State {
    /// The active account leaves the screen (background, sign-out).
    pub fn stop(&mut self) {
        self.members.stop();
        self.profile.stop();
    }
}

pub(super) fn handle(backend: &mut Backend, cmd: Cmd) {
    match cmd {
        Cmd::Members(cmd) => members::handle(backend, cmd),
        Cmd::Profile(cmd) => profile::handle(backend, cmd),
    }
}
