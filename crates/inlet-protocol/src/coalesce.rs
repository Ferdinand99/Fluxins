//! Debouncing for typing and coalescing for read acknowledgements, so a UI
//! that calls them on every key press or scroll cannot flood the API.
//!
//! - Typing: at most one `POST /channels/{id}/typing` per channel every
//!   [`TYPING_INTERVAL`] (Fluxer shows the indicator for about 10 s).
//! - Acks: [`crate::Client::ack`] only records the newest message per channel.
//!   One background flush per [`ACK_DELAY`] sends everything pending in one
//!   `POST /read-states/ack` (up to 100 channels per request).

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;

use crate::models::{snowflake_cmp, Snowflake};

/// Shortest time between two typing requests for one channel.
pub const TYPING_INTERVAL: Duration = Duration::from_secs(8);
/// How long acks are collected before they are sent.
pub const ACK_DELAY: Duration = Duration::from_secs(1);
/// Channels per `POST /read-states/ack`.
pub(crate) const ACK_BATCH: usize = 100;
/// Channels remembered for typing and sent acks.
const MAX_TRACKED: usize = 4_096;

/// One pending acknowledgement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingAck {
    pub(crate) message_id: Snowflake,
    pub(crate) mention_count: u64,
    pub(crate) manual: bool,
}

#[derive(Debug, Default)]
pub(crate) struct AckState {
    pub(crate) pending: BTreeMap<Snowflake, PendingAck>,
    /// The newest message acknowledged per channel, to skip repeats.
    pub(crate) sent: HashMap<Snowflake, Snowflake>,
    /// A flush task is scheduled.
    pub(crate) scheduled: bool,
}

impl AckState {
    /// Records an ack. Returns `true` when a flush needs to be scheduled.
    pub(crate) fn push(&mut self, channel_id: &str, ack: PendingAck) -> bool {
        if !ack.manual {
            if let Some(done) = self.sent.get(channel_id) {
                if snowflake_cmp(&ack.message_id, done).is_le() {
                    return false;
                }
            }
            if let Some(p) = self.pending.get(channel_id) {
                if !p.manual && snowflake_cmp(&ack.message_id, &p.message_id).is_le() {
                    return false;
                }
            }
        }
        self.pending.insert(channel_id.to_owned(), ack);
        if self.scheduled {
            false
        } else {
            self.scheduled = true;
            true
        }
    }

    /// Takes up to [`ACK_BATCH`] pending acks.
    pub(crate) fn take_batch(&mut self) -> Vec<(Snowflake, PendingAck)> {
        let keys: Vec<Snowflake> = self.pending.keys().take(ACK_BATCH).cloned().collect();
        keys.into_iter()
            .filter_map(|k| self.pending.remove(&k).map(|v| (k, v)))
            .collect()
    }

    /// Records a sent batch.
    pub(crate) fn mark_sent(&mut self, batch: &[(Snowflake, PendingAck)]) {
        if self.sent.len() + batch.len() > MAX_TRACKED {
            self.sent.clear();
        }
        for (c, a) in batch {
            if a.manual {
                // A manual ack moves the marker back; later acks must go out again.
                self.sent.insert(c.clone(), a.message_id.clone());
                continue;
            }
            match self.sent.get(c) {
                Some(old) if snowflake_cmp(&a.message_id, old).is_le() => {}
                _ => {
                    self.sent.insert(c.clone(), a.message_id.clone());
                }
            }
        }
    }

    /// Puts a failed batch back unless newer acks arrived meanwhile.
    pub(crate) fn restore(&mut self, batch: Vec<(Snowflake, PendingAck)>) {
        for (c, a) in batch {
            self.pending.entry(c).or_insert(a);
        }
    }
}

/// State shared by every clone of one [`crate::Client`].
#[derive(Debug, Default)]
pub(crate) struct Shared {
    typing: Mutex<HashMap<Snowflake, Instant>>,
    pub(crate) acks: Mutex<AckState>,
    /// Account traits from `me()`, for limits.
    pub(crate) traits: Mutex<Option<Vec<String>>>,
}

impl Shared {
    /// Whether a typing request for `channel_id` may go out now; records it if so.
    pub(crate) fn typing_due(&self, channel_id: &str) -> bool {
        let mut map = self.typing.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        if let Some(last) = map.get(channel_id) {
            if now.saturating_duration_since(*last) < TYPING_INTERVAL {
                return false;
            }
        }
        if map.len() >= MAX_TRACKED {
            map.retain(|_, t| now.saturating_duration_since(*t) < TYPING_INTERVAL);
        }
        map.insert(channel_id.to_owned(), now);
        true
    }

    /// Forgets the typing time of a channel (after a message is sent, or a failed request).
    pub(crate) fn reset_typing(&self, channel_id: &str) {
        self.typing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(channel_id);
    }

    pub(crate) fn lock_acks(&self) -> std::sync::MutexGuard<'_, AckState> {
        self.acks.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ack(id: &str) -> PendingAck {
        PendingAck {
            message_id: id.into(),
            mention_count: 0,
            manual: false,
        }
    }

    #[test]
    fn acks_keep_the_newest_and_skip_repeats() {
        let mut s = AckState::default();
        assert!(s.push("1", ack("10")));
        assert!(!s.push("1", ack("12")));
        assert!(!s.push("1", ack("11")));
        assert!(!s.push("2", ack("5")));
        assert_eq!(s.pending["1"].message_id, "12");
        let batch = s.take_batch();
        assert_eq!(batch.len(), 2);
        s.mark_sent(&batch);
        s.scheduled = false;
        // Already acknowledged: nothing to do.
        assert!(!s.push("1", ack("12")));
        assert!(s.pending.is_empty());
        // A manual ack (mark unread) always goes out.
        assert!(s.push(
            "1",
            PendingAck {
                message_id: "3".into(),
                mention_count: 1,
                manual: true
            }
        ));
        // A failed batch comes back, but newer acks win.
        let b = s.take_batch();
        s.push("1", ack("20"));
        s.restore(b);
        assert_eq!(s.pending["1"].message_id, "20");
    }

    #[tokio::test(start_paused = true)]
    async fn typing_is_debounced_per_channel() {
        let s = Shared::default();
        assert!(s.typing_due("1"));
        assert!(!s.typing_due("1"));
        assert!(s.typing_due("2"));
        tokio::time::advance(TYPING_INTERVAL - Duration::from_millis(1)).await;
        assert!(!s.typing_due("1"));
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(s.typing_due("1"));
        s.reset_typing("1");
        assert!(s.typing_due("1"));
    }
}
