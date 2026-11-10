//! Threads and thread membership. Filled in by a later task.

use inlet_protocol::Dispatch;

use crate::Changes;

/// Empty for now: [`Threads::apply`] changes nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Threads {}

impl Threads {
    /// Applies an event. Does nothing yet.
    pub fn apply(&mut self, _event: &Dispatch) -> Changes {
        Changes::default()
    }
}
