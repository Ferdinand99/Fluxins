// SPDX-License-Identifier: AGPL-3.0-or-later
//! Social: presence, member lists, profiles, relationships and the DM list's actions.
//! The tasks that build these fill one submodule each.

pub mod members;
pub mod presence;
pub mod profile;
pub mod relationships;

use super::Backend;

/// Commands of this domain.
#[derive(Clone, Debug, PartialEq)]
pub enum Cmd {
    /// Presence and status (see [`presence`]).
    Presence(presence::Cmd),
    /// Friends, blocks and the DM list's actions (see [`relationships`]).
    Relationships(relationships::Cmd),
    /// The member list on screen and member searches (see [`members`]).
    Members(members::Cmd),
    /// Profiles and notes (see [`profile`]).
    Profile(profile::Cmd),
}

/// Events of this domain.
#[derive(Clone, Debug)]
pub enum Ev {
    /// Presence and status (see [`presence`]).
    Presence(presence::Ev),
    /// Answers to friend, block and DM actions (see [`relationships`]).
    Relationships(relationships::Ev),
    /// What the member list subscribed to (see [`members`]).
    Members(members::Ev),
    /// Profiles and notes (see [`profile`]).
    Profile(profile::Ev),
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
        Cmd::Presence(cmd) => presence::handle(backend, cmd),
        Cmd::Relationships(cmd) => relationships::handle(backend, cmd),
        Cmd::Members(cmd) => members::handle(backend, cmd),
        Cmd::Profile(cmd) => profile::handle(backend, cmd),
    }
}
