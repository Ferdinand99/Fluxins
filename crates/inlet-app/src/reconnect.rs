// SPDX-License-Identifier: AGPL-3.0-or-later
//! When to start the gateway again after it stopped.
//!
//! The protocol crate already resumes and reconnects on its own. It only stops for good
//! when going on would not help: an invalid or revoked token, an unsupported gateway
//! version (4012), an invalid shard (4010/4011) or an unusable gateway URL. Restarting
//! after such a stop would just send Identify after Identify, which can get the user's
//! address throttled or banned. So Inlet restarts the gateway only after an unexpected
//! stop (the event channel closed without a close reason, or the forwarding task
//! panicked), with a growing delay and an hourly cap, and otherwise asks the user.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// First automatic restart delay; also the minimum time between two gateway starts,
/// so two Identify payloads are never closer than this across restarts.
pub const MIN_DELAY: Duration = Duration::from_secs(5);
/// Longest automatic restart delay.
pub const MAX_DELAY: Duration = Duration::from_secs(5 * 60);
/// Automatic restarts allowed per hour before Inlet waits for the user.
pub const MAX_PER_HOUR: usize = 5;
const HOUR: Duration = Duration::from_secs(60 * 60);

/// Why the gateway stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// The event channel closed without a close reason.
    Unexpected,
    /// The task forwarding gateway events panicked.
    Panicked,
    /// The protocol stopped on purpose (4012, 4010, 4011, invalid URL, …). Never retried.
    Terminal(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Start the gateway again after this delay.
    RestartAfter(Duration),
    /// Do not restart; show this message and a "Reconnect" button.
    WaitForUser(String),
}

/// Restart bookkeeping for one session. Lives with the session, not with a gateway
/// task, so the spacing holds across restarts.
#[derive(Debug, Default)]
pub struct ReconnectPolicy {
    restarts: VecDeque<Instant>,
    consecutive: u32,
    panics: u32,
    last_start: Option<Instant>,
}

impl ReconnectPolicy {
    /// Records that a gateway was started now.
    pub fn started(&mut self, now: Instant) {
        self.last_start = Some(now);
    }

    /// The gateway reached READY: the next failure starts the backoff from the beginning.
    pub fn connected(&mut self) {
        self.consecutive = 0;
        self.panics = 0;
    }

    /// Time left before another start keeps the minimum spacing.
    pub fn spacing(&self, now: Instant) -> Duration {
        match self.last_start {
            Some(last) => MIN_DELAY.saturating_sub(now.saturating_duration_since(last)),
            None => Duration::ZERO,
        }
    }

    /// The user pressed "Reconnect": allowed at once apart from the spacing.
    pub fn manual(&mut self, now: Instant) -> Duration {
        self.consecutive = 0;
        self.panics = 0;
        self.spacing(now)
    }

    pub fn on_stop(&mut self, stop: Stop, now: Instant) -> Decision {
        match stop {
            Stop::Terminal(message) => return Decision::WaitForUser(message),
            Stop::Panicked => {
                self.panics += 1;
                if self.panics >= 2 {
                    return Decision::WaitForUser(
                        "The connection part of Inlet failed twice in a row.".into(),
                    );
                }
            }
            Stop::Unexpected => {}
        }
        while self
            .restarts
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) >= HOUR)
        {
            self.restarts.pop_front();
        }
        if self.restarts.len() >= MAX_PER_HOUR {
            return Decision::WaitForUser(
                "The connection dropped several times in the last hour.".into(),
            );
        }
        let backoff = MIN_DELAY
            .saturating_mul(1u32 << self.consecutive.min(6))
            .min(MAX_DELAY);
        self.consecutive += 1;
        self.restarts.push_back(now);
        Decision::RestartAfter(backoff.max(self.spacing(now)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_closes_never_restart() {
        // 4012 (unsupported version), 4010/4011 and an invalid URL all arrive as Terminal:
        // no restart means no second Identify and no loop without network.
        let mut p = ReconnectPolicy::default();
        let now = Instant::now();
        p.started(now);
        for reason in [
            "gateway version not supported (4012)",
            "invalid shard (4010)",
            "invalid URL",
        ] {
            assert!(matches!(
                p.on_stop(Stop::Terminal(reason.into()), now),
                Decision::WaitForUser(_)
            ));
        }
    }

    #[test]
    fn unexpected_stops_back_off_and_cap_per_hour() {
        let mut p = ReconnectPolicy::default();
        let t0 = Instant::now();
        let delays: Vec<Duration> = (0..MAX_PER_HOUR as u64)
            .map(
                |i| match p.on_stop(Stop::Unexpected, t0 + Duration::from_secs(i)) {
                    Decision::RestartAfter(d) => d,
                    other => panic!("expected a restart, got {other:?}"),
                },
            )
            .collect();
        assert_eq!(delays[0], MIN_DELAY);
        assert!(delays.windows(2).all(|w| w[1] >= w[0]));
        assert!(delays.iter().all(|d| *d <= MAX_DELAY));
        assert!(matches!(
            p.on_stop(Stop::Unexpected, t0 + Duration::from_secs(10)),
            Decision::WaitForUser(_)
        ));
        // An hour later the budget is back.
        assert!(matches!(
            p.on_stop(Stop::Unexpected, t0 + HOUR + Duration::from_secs(10)),
            Decision::RestartAfter(_)
        ));
    }

    #[test]
    fn repeated_panics_stop_restarting() {
        let mut p = ReconnectPolicy::default();
        let now = Instant::now();
        assert!(matches!(
            p.on_stop(Stop::Panicked, now),
            Decision::RestartAfter(_)
        ));
        assert!(matches!(
            p.on_stop(Stop::Panicked, now),
            Decision::WaitForUser(_)
        ));
    }

    #[test]
    fn spacing_holds_across_restarts() {
        let mut p = ReconnectPolicy::default();
        let now = Instant::now();
        p.started(now);
        assert_eq!(
            p.manual(now + Duration::from_secs(2)),
            Duration::from_secs(3)
        );
        assert_eq!(p.manual(now + Duration::from_secs(9)), Duration::ZERO);
    }

    #[test]
    fn backoff_never_exceeds_five_minutes() {
        let mut p = ReconnectPolicy {
            consecutive: 30,
            ..Default::default()
        };
        match p.on_stop(Stop::Unexpected, Instant::now()) {
            Decision::RestartAfter(d) => assert_eq!(d, MAX_DELAY),
            other => panic!("{other:?}"),
        }
    }
}
