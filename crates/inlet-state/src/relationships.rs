//! Relationships (friends, blocks, notes). Filled in by a later task; until then nobody counts as blocked.

use inlet_protocol::Dispatch;

use crate::Changes;

/// Empty for now: [`Relationships::apply`] changes nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Relationships {}

impl Relationships {
    /// Applies an event. Does nothing yet.
    pub fn apply(&mut self, _event: &Dispatch) -> Changes {
        Changes::default()
    }
}

impl Relationships {
    /// Whether the account blocked `user`. Always `false` until relationships are tracked.
    pub fn is_blocked(&self, _user: crate::UserId) -> bool {
        false
    }
}
