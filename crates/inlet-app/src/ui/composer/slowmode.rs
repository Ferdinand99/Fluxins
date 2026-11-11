// SPDX-License-Identifier: AGPL-3.0-or-later
//! Slowmode, as in Fluxer (`useSlowmode`, `Slowmode.ts`): in a guild channel with a
//! slowmode, sending starts a countdown, and nothing can be sent until it reaches zero.
//! Members who may ignore slowmode never wait. Edits and the commands that do not post in
//! the channel (`/nick`, `/msg`) are not held back. When the server says to wait (a send
//! was refused), its time wins.
//!
//! Times are milliseconds on a monotonic clock that the caller passes in.

use std::collections::HashMap;

/// Channels remembered at most.
const MAX_CHANNELS: usize = 256;

#[derive(Default)]
pub struct Slowmode {
    /// When the account last sent in a channel.
    last_send: HashMap<String, u64>,
    /// Until when the server asked to wait.
    server_until: HashMap<String, u64>,
}

impl Slowmode {
    /// A message was sent in `channel` at `now_ms`.
    pub fn record_send(&mut self, channel: &str, now_ms: u64) {
        if self.last_send.len() >= MAX_CHANNELS && !self.last_send.contains_key(channel) {
            self.last_send.clear();
        }
        self.last_send.insert(channel.to_owned(), now_ms);
    }

    /// The server refused a send and asked to wait `secs`.
    pub fn server_wait(&mut self, channel: &str, secs: u64, now_ms: u64) {
        if self.server_until.len() >= MAX_CHANNELS && !self.server_until.contains_key(channel) {
            self.server_until.clear();
        }
        let until = now_ms.saturating_add(secs.min(21_600) * 1000);
        self.server_until.insert(channel.to_owned(), until);
    }

    /// Whole seconds left before the account may send again (rounded up). `rate_secs` is
    /// the channel's slowmode; `bypass` is the right to ignore it.
    pub fn remaining_secs(&self, channel: &str, rate_secs: u32, bypass: bool, now_ms: u64) -> u32 {
        let mut until = self.server_until.get(channel).copied().unwrap_or(0);
        if rate_secs > 0 && !bypass {
            if let Some(at) = self.last_send.get(channel) {
                until = until.max(at.saturating_add(u64::from(rate_secs) * 1000));
            }
        }
        let left = until.saturating_sub(now_ms);
        left.div_ceil(1000).min(u64::from(u32::MAX)) as u32
    }

    /// Forgets everything (sign-out).
    pub fn clear(&mut self) {
        self.last_send.clear();
        self.server_until.clear();
    }
}

/// `m:ss` (or `h:mm:ss`) for the countdown.
pub fn clock(secs: u32) -> String {
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slowmode_blocks_send_until_zero() {
        let mut s = Slowmode::default();
        assert_eq!(s.remaining_secs("c", 10, false, 0), 0, "nothing sent yet");
        s.record_send("c", 1_000);
        assert_eq!(s.remaining_secs("c", 10, false, 1_000), 10);
        assert_eq!(s.remaining_secs("c", 10, false, 6_000), 5);
        assert_eq!(s.remaining_secs("c", 10, false, 10_999), 1, "rounded up");
        assert_eq!(
            s.remaining_secs("c", 10, false, 11_000),
            0,
            "zero: sending works"
        );
        assert_eq!(
            s.remaining_secs("other", 10, false, 2_000),
            0,
            "per channel"
        );
        assert_eq!(
            s.remaining_secs("c", 10, true, 2_000),
            0,
            "the right to bypass"
        );
        assert_eq!(s.remaining_secs("c", 0, false, 2_000), 0, "no slowmode");
    }

    #[test]
    fn the_server_wait_wins_and_is_kept_without_a_slowmode() {
        let mut s = Slowmode::default();
        s.record_send("c", 0);
        s.server_wait("c", 30, 0);
        assert_eq!(s.remaining_secs("c", 10, false, 0), 30);
        assert_eq!(s.remaining_secs("c", 0, true, 29_500), 1);
        assert_eq!(s.remaining_secs("c", 0, true, 30_000), 0);
    }

    #[test]
    fn the_clock_reads_like_fluxer() {
        assert_eq!(clock(5), "0:05");
        assert_eq!(clock(75), "1:15");
        assert_eq!(clock(3_725), "1:02:05");
    }
}
