// SPDX-License-Identifier: AGPL-3.0-or-later
//! Social: presence, member lists and relationships. Empty for now: the tasks that build
//! these fill one submodule each.

mod members;
mod presence;
mod relationships;

use super::Backend;

/// Commands of this domain. None yet.
pub enum Cmd {}

/// Events of this domain. None yet.
pub enum Ev {}

pub(super) fn handle(_backend: &mut Backend, cmd: Cmd) {
    match cmd {}
}
