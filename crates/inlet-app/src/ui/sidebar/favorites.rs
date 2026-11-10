// SPDX-License-Identifier: AGPL-3.0-or-later
//! Favourite channels. Empty for now; its header button is a slot in
//! `ui/chat/header_buttons.rs`.

use gpui::{AnyElement, Context};

use crate::app::Inlet;

/// The "favourite" button in the channel header. None yet.
pub fn header_button(_this: &Inlet, _cx: &mut Context<Inlet>) -> Option<AnyElement> {
    None
}
