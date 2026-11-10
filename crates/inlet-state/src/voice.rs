//! Voice states. Filled in by a later task.

use inlet_protocol::Dispatch;

use crate::Changes;

/// Empty for now: [`Voice::apply`] changes nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Voice {}

impl Voice {
    /// Applies an event. Does nothing yet.
    pub fn apply(&mut self, _event: &Dispatch) -> Changes {
        Changes::default()
    }
}
