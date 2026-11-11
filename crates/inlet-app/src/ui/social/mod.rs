// SPDX-License-Identifier: AGPL-3.0-or-later
//! Social: presence and status (`presence`). Friends, direct message management and
//! group DMs follow in their own files.

pub mod presence;

use gpui::{Context, Subscription, Window};

use crate::app::Inlet;
use crate::backend;
use crate::backend::social::presence::Ev as PresenceEv;
use crate::ui::overlays::status_picker::Picker;

/// This domain's view state.
#[derive(Default)]
pub struct State {
    pub presence: presence::PresenceState,
    /// The status picker over the user panel, while it is open.
    pub picker: Option<Picker>,
    /// Input in the window ends "away" at once.
    _input: Option<Subscription>,
}

impl State {
    /// The state with the idle watch running.
    pub fn new(cx: &mut Context<Inlet>) -> Self {
        let mut state = State::default();
        state.presence.start(cx);
        state._input = Some(cx.observe_keystrokes(|this, _, _, cx| {
            if this.social.presence.idle.input() {
                this.sync_presence(cx);
                cx.notify();
            }
        }));
        state
    }

    /// Sign-out or a backend restart: the picker closes and the next session sends its
    /// presence again.
    pub fn reset(&mut self) {
        self.picker = None;
        self.presence.reset();
    }
}

impl Inlet {
    /// Events from the backend's social domain.
    pub(crate) fn on_social_event(
        &mut self,
        event: backend::social::Ev,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            backend::social::Ev::Presence(PresenceEv::SaveFailed) => self.status_save_failed(cx),
            backend::social::Ev::Presence(PresenceEv::Saved(settings)) => {
                self.status_saved(&settings, cx)
            }
            backend::social::Ev::Members(ev) => self.on_members_event(ev, cx),
            backend::social::Ev::Profile(ev) => self.on_profile_event(ev, cx),
        }
    }
}
