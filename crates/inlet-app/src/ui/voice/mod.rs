// SPDX-License-Identifier: AGPL-3.0-or-later
//! Voice and video calls. Empty for now: the task that builds this domain fills it.

use gpui::{Context, Window};

use crate::app::Inlet;
use crate::backend;

/// This domain's view state. Empty until the domain gets its first view.
#[derive(Default)]
pub struct State {}

impl Inlet {
    /// Events from the backend's voice domain.
    pub(crate) fn on_voice_event(
        &mut self,
        event: backend::voice::Ev,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        match event {}
    }
}
