// SPDX-License-Identifier: AGPL-3.0-or-later
//! Friends, direct message management and group DMs. Empty for now: the task that builds this domain fills it.

use gpui::{Context, Window};

use crate::app::Inlet;
use crate::backend;

/// This domain's view state. Empty until the domain gets its first view.
#[derive(Default)]
pub struct State {}

impl Inlet {
    /// Events from the backend's social domain.
    pub(crate) fn on_social_event(
        &mut self,
        event: backend::social::Ev,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            backend::social::Ev::Profile(ev) => self.on_profile_event(ev, cx),
        }
    }
}
