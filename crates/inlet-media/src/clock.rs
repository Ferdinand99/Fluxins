//! The time source of the failure back-off, so that tests can move time by hand.

use std::fmt::Debug;
use std::time::Instant;

/// Tells the time. The client reads it only to decide when a remembered failure
/// ends; timeouts and decode budgets use the real clock. Implement it to control
/// time in tests.
pub trait Clock: Send + Sync + Debug + 'static {
    /// The current moment.
    fn now(&self) -> Instant;
}

/// The real clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}
