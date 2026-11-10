// SPDX-License-Identifier: AGPL-3.0-or-later
//! The buttons on the right of the channel header, as a fixed list.
//!
//! Every entry is a function in the domain that owns the button, so the tasks that add
//! threads, pinned messages, members, search and the inbox each change only their own
//! module, never this list. A function returns `None` while its button does not exist
//! yet (or does not apply to the open channel); the header then draws nothing for it.

use gpui::{AnyElement, Context};

use crate::app::Inlet;
use crate::ui::{inbox, members, notifications, search, sidebar, threads};

/// One header button: `None` hides it.
pub type HeaderButton = fn(&Inlet, &mut Context<Inlet>) -> Option<AnyElement>;

/// Left to right, in Fluxer's order.
pub const BUTTONS: [HeaderButton; 7] = [
    threads::header_button,
    notifications::header_button,
    inbox::pinned_header_button,
    sidebar::favorites::header_button,
    members::header_button,
    search::header_button,
    inbox::header_button,
];

/// The buttons to draw for the open channel.
pub fn render(this: &Inlet, cx: &mut Context<Inlet>) -> Vec<AnyElement> {
    BUTTONS
        .iter()
        .filter_map(|button| button(this, cx))
        .collect()
}
