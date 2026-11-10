// SPDX-License-Identifier: AGPL-3.0-or-later
//! Pinned and saved messages, the inbox of recent mentions, and jumping to a message. Empty for now: the task that builds this domain fills it.

use gpui::{AnyElement, Context};

use crate::app::Inlet;

/// This domain's view state. Empty until the domain gets its first view.
#[derive(Default)]
pub struct State {}

/// The pinned messages button in the channel header (see `ui/chat/header_buttons.rs`). None yet.
pub fn pinned_header_button(_this: &Inlet, _cx: &mut Context<Inlet>) -> Option<AnyElement> {
    None
}

/// The inbox button in the channel header (see `ui/chat/header_buttons.rs`). None yet.
pub fn header_button(_this: &Inlet, _cx: &mut Context<Inlet>) -> Option<AnyElement> {
    None
}
