// SPDX-License-Identifier: AGPL-3.0-or-later
//! Read state: when the open channel is acknowledged.
//!
//! The protocol crate already coalesces acknowledgements (one batched request per
//! second for the whole account). This gate sits in front of it and keeps one open,
//! busy channel from asking for an acknowledgement on every message: a channel is
//! acknowledged at most once per [`MIN_GAP`], and only for the newest message seen
//! since. Only channels that were really on screen are ever noted here, so no
//! acknowledgement is sent for a channel nobody looked at.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use inlet_state::{ChannelId, MessageId};

/// Messages arriving within this time after the first one are acknowledged together.
pub const QUIET: Duration = Duration::from_secs(1);
/// Shortest time between two acknowledgements of one channel.
pub const MIN_GAP: Duration = Duration::from_secs(3);
/// Channels whose last acknowledgement time is remembered.
const MAX_REMEMBERED: usize = 1_024;

#[derive(Debug)]
struct Pending {
    message: MessageId,
    first_at: Instant,
}

/// Collects "this channel was read up to this message" and says when to send it.
/// Pure: the caller passes the time and sends what [`AckGate::due`] returns.
#[derive(Debug, Default)]
pub struct AckGate {
    pending: HashMap<ChannelId, Pending>,
    last_sent: HashMap<ChannelId, Instant>,
}

impl AckGate {
    /// The channel is on screen and read up to `message`. Older messages than one
    /// already noted change nothing.
    pub fn note(&mut self, channel: ChannelId, message: MessageId, now: Instant) {
        match self.pending.get_mut(&channel) {
            Some(p) => p.message = p.message.max(message),
            None => {
                self.pending.insert(
                    channel,
                    Pending {
                        message,
                        first_at: now,
                    },
                );
            }
        }
    }

    /// Forgets what is pending for a channel (it was acknowledged some other way).
    pub fn clear(&mut self, channel: ChannelId, now: Instant) {
        self.pending.remove(&channel);
        self.sent(channel, now);
    }

    /// The acknowledgements to send now: channels whose first note is at least
    /// [`QUIET`] old and whose last acknowledgement is at least [`MIN_GAP`] old.
    /// Each channel appears once, with the newest message noted for it.
    pub fn due(&mut self, now: Instant) -> Vec<(ChannelId, MessageId)> {
        let ready: Vec<ChannelId> = self
            .pending
            .iter()
            .filter(|(ch, p)| {
                now.saturating_duration_since(p.first_at) >= QUIET
                    && self
                        .last_sent
                        .get(ch)
                        .is_none_or(|t| now.saturating_duration_since(*t) >= MIN_GAP)
            })
            .map(|(ch, _)| *ch)
            .collect();
        let mut out: Vec<(ChannelId, MessageId)> = ready
            .into_iter()
            .filter_map(|ch| self.pending.remove(&ch).map(|p| (ch, p.message)))
            .collect();
        out.sort();
        for (ch, _) in &out {
            self.sent(*ch, now);
        }
        out
    }

    /// Whether anything waits to be sent.
    #[cfg(test)]
    pub fn is_idle(&self) -> bool {
        self.pending.is_empty()
    }

    fn sent(&mut self, channel: ChannelId, now: Instant) {
        if self.last_sent.len() >= MAX_REMEMBERED && !self.last_sent.contains_key(&channel) {
            // Old entries only matter for 3 s; forgetting the rest is safe.
            self.last_sent
                .retain(|_, t| now.saturating_duration_since(*t) < MIN_GAP);
        }
        self.last_sent.insert(channel, now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const C: ChannelId = ChannelId(10);
    const D: ChannelId = ChannelId(11);

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn ack_is_debounced_and_sent_for_the_last_message_only() {
        let base = Instant::now();
        let mut gate = AckGate::default();

        // A busy channel: twenty messages in 600 ms, then an old one again.
        for i in 0..20u64 {
            gate.note(C, MessageId(100 + i), at(base, i * 30));
        }
        gate.note(C, MessageId(5), at(base, 610));
        assert!(
            gate.due(at(base, 900)).is_empty(),
            "still inside the quiet time"
        );

        // One acknowledgement, for the newest message.
        assert_eq!(gate.due(at(base, 1_000)), vec![(C, MessageId(119))]);
        assert!(gate.due(at(base, 1_100)).is_empty());
        assert!(gate.is_idle());

        // The next message comes right after: it waits for the gap since the last
        // acknowledgement, not only for the quiet time.
        gate.note(C, MessageId(120), at(base, 1_200));
        gate.note(C, MessageId(121), at(base, 1_900));
        assert!(gate.due(at(base, 3_900)).is_empty());
        assert_eq!(gate.due(at(base, 4_000)), vec![(C, MessageId(121))]);

        // A channel nobody noted is never acknowledged.
        assert!(gate.due(at(base, 60_000)).is_empty());
    }

    #[test]
    fn channels_are_gated_one_by_one() {
        let base = Instant::now();
        let mut gate = AckGate::default();
        gate.note(C, MessageId(1), base);
        gate.note(D, MessageId(2), at(base, 500));
        assert_eq!(gate.due(at(base, 1_000)), vec![(C, MessageId(1))]);
        assert_eq!(gate.due(at(base, 1_500)), vec![(D, MessageId(2))]);
    }

    #[test]
    fn an_acknowledgement_sent_another_way_cancels_the_pending_one() {
        let base = Instant::now();
        let mut gate = AckGate::default();
        gate.note(C, MessageId(1), base);
        gate.clear(C, at(base, 100));
        assert!(gate.due(at(base, 2_000)).is_empty());
        // And the gap counts from that acknowledgement.
        gate.note(C, MessageId(2), at(base, 200));
        assert!(gate.due(at(base, 2_500)).is_empty());
        assert_eq!(gate.due(at(base, 3_100)), vec![(C, MessageId(2))]);
    }
}
