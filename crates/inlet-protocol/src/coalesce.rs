//! Debouncing for typing and coalescing for read acknowledgements, so a UI
//! that calls them on every key press or scroll cannot flood the API.
//!
//! The state lives in the account's [`crate::RateLimiter`], so every
//! [`crate::Client`] of one account (on one instance) shares it.
//!
//! - Typing: at most one `POST /channels/{id}/typing` per channel every
//!   [`TYPING_INTERVAL`] (Fluxer shows the indicator for about 10 s).
//! - Acks: [`crate::Client::ack`] only records the newest message per channel.
//!   A background flush after [`ACK_DELAY`] sends what is pending in one
//!   `POST /read-states/ack` (up to 100 channels per request), and two ack
//!   requests of one account are always at least [`ACK_DELAY`] apart.
//! - Search: message and thread searches of one account start at least
//!   [`SEARCH_INTERVAL`] apart. A search whose slot is more than
//!   [`MAX_SEARCH_WAIT`] away returns [`crate::Error::RateLimited`] without a request.
//! - GIF search: GIF searches and suggestions of one account start at least
//!   [`GIF_SEARCH_INTERVAL`] apart, with the same [`MAX_SEARCH_WAIT`].

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
/// Shortest time between two search requests (messages or threads) of one account.
pub const SEARCH_INTERVAL: Duration = Duration::from_secs(1);
/// A search that would wait longer than this for its slot is refused with
/// [`crate::Error::RateLimited`] instead of queued.
pub const MAX_SEARCH_WAIT: Duration = Duration::from_secs(3);
/// Shortest time between two GIF searches or suggestions of one account
/// (Fluxer allows 40 per 10 s).
pub const GIF_SEARCH_INTERVAL: Duration = Duration::from_millis(500);

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
    /// When the last ack request started.
    pub(crate) last_request: Option<Instant>,
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

    /// Claims the next request slot: `None` when a request may start now (and
    /// records it), otherwise how long to wait.
    pub(crate) fn reserve_slot(&mut self, now: Instant) -> Option<Duration> {
        match self.last_request {
            Some(last) if now.saturating_duration_since(last) < ACK_DELAY => {
                Some(ACK_DELAY - now.saturating_duration_since(last))
            }
            _ => {
                self.last_request = Some(now);
                None
            }
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

/// State shared by every client of one account (kept in its rate limiter).
#[derive(Debug, Default)]
pub(crate) struct Shared {
    typing: Mutex<HashMap<Snowflake, Instant>>,
    pub(crate) acks: Mutex<AckState>,
    /// Account traits from `me()`, for limits.
    pub(crate) traits: Mutex<Option<Vec<String>>>,
    /// Start time of the last search slot handed out.
    search: Mutex<Option<Instant>>,
    /// Start time of the last GIF search slot handed out.
    gif_search: Mutex<Option<Instant>>,
}

/// Claims the next slot at least `interval` after the previous one:
/// `Ok(wait)`, or `Err(retry_after)` when it is more than [`MAX_SEARCH_WAIT`]
/// away (nothing is reserved).
fn reserve_slot(
    last: &Mutex<Option<Instant>>,
    interval: Duration,
    now: Instant,
) -> Result<Duration, Duration> {
    let mut last = last.lock().unwrap_or_else(|e| e.into_inner());
    let slot = match *last {
        Some(prev) => (prev + interval).max(now),
        None => now,
    };
    let wait = slot.saturating_duration_since(now);
    if wait > MAX_SEARCH_WAIT {
        return Err(wait);
    }
    *last = Some(slot);
    Ok(wait)
}

/// Keeps the later of two slots, so moving to another state cannot skip an interval.
fn keep_later(mine: &Mutex<Option<Instant>>, theirs: &Mutex<Option<Instant>>) {
    let theirs = *theirs.lock().unwrap_or_else(|e| e.into_inner());
    let mut mine = mine.lock().unwrap_or_else(|e| e.into_inner());
    *mine = match (*mine, theirs) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    };
}

impl Shared {
    /// Claims the next search slot, at least [`SEARCH_INTERVAL`] after the
    /// previous one: `Ok(wait)` before the request may start, or `Err(retry_after)`
    /// when the slot is more than [`MAX_SEARCH_WAIT`] away (nothing is reserved).
    pub(crate) fn reserve_search(&self, now: Instant) -> Result<Duration, Duration> {
        reserve_slot(&self.search, SEARCH_INTERVAL, now)
    }

    /// Claims the next GIF search slot, like [`Shared::reserve_search`] with
    /// [`GIF_SEARCH_INTERVAL`].
    pub(crate) fn reserve_gif_search(&self, now: Instant) -> Result<Duration, Duration> {
        reserve_slot(&self.gif_search, GIF_SEARCH_INTERVAL, now)
    }

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

    /// Takes over another state's pending acks, typing times and traits (when
    /// a client moves from its token's limiter to the account's).
    pub(crate) fn absorb(&self, other: &Shared) {
        let (pending, sent, other_scheduled) = {
            let mut o = other.lock_acks();
            (
                std::mem::take(&mut o.pending),
                std::mem::take(&mut o.sent),
                std::mem::replace(&mut o.scheduled, false),
            )
        };
        {
            let mut mine = self.lock_acks();
            let was_scheduled = mine.scheduled;
            for (c, a) in pending {
                mine.push(&c, a);
            }
            for (c, m) in sent {
                mine.sent.entry(c).or_insert(m);
            }
            // A flush task scheduled on the old state runs against this one
            // (it asks the client for its current limiter).
            mine.scheduled = was_scheduled || other_scheduled;
        }
        let typing: Vec<(Snowflake, Instant)> = other
            .typing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .collect();
        self.typing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend(typing);
        let traits = other
            .traits
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        {
            let mut mine = self.traits.lock().unwrap_or_else(|e| e.into_inner());
            if mine.is_none() {
                *mine = traits;
            }
        }
        // Keep the later search slots so a move cannot skip the interval.
        keep_later(&self.search, &other.search);
        keep_later(&self.gif_search, &other.gif_search);
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

    #[tokio::test(start_paused = true)]
    async fn searches_get_one_slot_per_second() {
        let s = Shared::default();
        let now = Instant::now();
        assert_eq!(s.reserve_search(now), Ok(Duration::ZERO));
        assert_eq!(s.reserve_search(now), Ok(SEARCH_INTERVAL));
        assert_eq!(s.reserve_search(now), Ok(SEARCH_INTERVAL * 2));
        assert_eq!(s.reserve_search(now), Ok(SEARCH_INTERVAL * 3));
        // The next slot is 4 s away: refused, and nothing is reserved.
        assert_eq!(s.reserve_search(now), Err(SEARCH_INTERVAL * 4));
        assert_eq!(s.reserve_search(now), Err(SEARCH_INTERVAL * 4));
        tokio::time::advance(Duration::from_secs(10)).await;
        assert_eq!(s.reserve_search(Instant::now()), Ok(Duration::ZERO));
    }
}
