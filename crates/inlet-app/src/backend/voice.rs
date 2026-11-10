// SPDX-License-Identifier: AGPL-3.0-or-later
//! Voice and video (through `inlet-voice`). Empty for now: the task that builds this domain fills it.

use super::Backend;

/// Commands of this domain. None yet.
pub enum Cmd {}

/// Events of this domain. None yet.
pub enum Ev {}

pub(super) fn handle(_backend: &mut Backend, cmd: Cmd) {
    match cmd {}
}
