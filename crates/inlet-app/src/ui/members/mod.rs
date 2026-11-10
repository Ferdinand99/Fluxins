// SPDX-License-Identifier: AGPL-3.0-or-later
//! The member list beside the chat. Empty for now: the task that builds this domain fills it.

use gpui::{AnyElement, Context};

use crate::app::Inlet;

/// This domain's view state. Empty until the domain gets its first view.
#[derive(Default)]
pub struct State {}

/// The member list button in the channel header (see `ui/chat/header_buttons.rs`). None yet.
pub fn header_button(_this: &Inlet, _cx: &mut Context<Inlet>) -> Option<AnyElement> {
    None
}
