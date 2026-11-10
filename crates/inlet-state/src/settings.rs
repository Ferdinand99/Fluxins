//! Account settings (USER_SETTINGS_UPDATE, synced preferences). Filled in by a later task.

use inlet_protocol::Dispatch;

use crate::Changes;

/// Empty for now: [`Settings::apply`] changes nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {}

impl Settings {
    /// Applies an event. Does nothing yet.
    pub fn apply(&mut self, _event: &Dispatch) -> Changes {
        Changes::default()
    }
}
