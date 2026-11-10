// SPDX-License-Identifier: AGPL-3.0-or-later
//! Notification level and muting per channel, guild and account. Empty for now: the task that builds this domain fills it.

use gpui::{AnyElement, Context, Window};

use crate::app::Inlet;
use crate::backend;

/// This domain's view state. Empty until the domain gets its first view.
#[derive(Default)]
pub struct State {}

/// The notification settings button in the channel header (see `ui/chat/header_buttons.rs`). None yet.
pub fn header_button(_this: &Inlet, _cx: &mut Context<Inlet>) -> Option<AnyElement> {
    None
}

impl Inlet {
    /// Events from the backend's notifications domain.
    pub(crate) fn on_notifications_event(
        &mut self,
        event: backend::notifications::Ev,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        match event {}
    }
}
