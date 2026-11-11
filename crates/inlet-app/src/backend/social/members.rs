// SPDX-License-Identifier: AGPL-3.0-or-later
//! Member lists on the network side: the op 14 subscription of the member list on
//! screen, and member searches (op 8). What to show is decided on the UI thread
//! (`ui/members`); this module paces what goes to the gateway.
//!
//! Request budget (no REST; everything is gateway commands):
//!
//! - Member list (op 14): only the rows on screen, in at most three ranges of 100 rows
//!   (the first page and the pages on screen, `ui::members::list::window`). The UI
//!   asks again only when that changes; here the wishes are coalesced: at most one
//!   member list change every [`COALESCE`] (300 ms), the newest wish wins, and
//!   wishes in between are never sent. Opening a channel of another guild sends one
//!   unsubscribe for the old guild with it. The protocol also keeps every bounded
//!   command under 4 a second and sends the subscription again after a reconnect.
//! - When the account goes to the background or signs out, its member list is
//!   unsubscribed (one op 14), so a passive account gets no member list updates.
//! - Member search (op 8): one per search the person types, after the UI's pause;
//!   the protocol sends at most 10 per 10 s and keeps only the newest 4.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use inlet_protocol as p;
use tokio::task::AbortHandle;
use tokio::time::Instant;

use crate::backend::{Active, Backend, Ui};

/// Shortest time between two member list subscriptions (op 14 is coalesced over
/// 300 ms).
pub const COALESCE: Duration = Duration::from_millis(300);
/// Members a search asks for.
pub const SEARCH_LIMIT: u8 = 25;

/// Member list commands.
#[derive(Clone, Debug, PartialEq)]
pub enum Cmd {
    /// Show these rows of `channel_id`'s member list (`ranges` inclusive, each at
    /// most 100 rows). Replaces the list shown before. `fresh`: the UI has no rows of
    /// this list (it was just built again), so the server must send every range, not
    /// only the ones it did not have.
    Window {
        guild_id: String,
        channel_id: String,
        ranges: Vec<[u32; 2]>,
        fresh: bool,
    },
    /// No member list on screen any more.
    Close,
    /// Members of `guild_id` whose name starts with `query`; GUILD_MEMBERS_CHUNK with
    /// `nonce` answers through the gateway.
    Search {
        guild_id: String,
        query: String,
        nonce: String,
    },
}

/// Member list events.
#[derive(Clone, Debug, PartialEq)]
pub enum Ev {
    /// The gateway was told to subscribe to these ranges of the list: the server
    /// has them now, and the UI keeps only their rows.
    Sent {
        guild_id: String,
        channel_id: String,
        ranges: Vec<[u32; 2]>,
    },
}

/// One subscription on one gateway.
#[derive(Clone, Debug, PartialEq)]
pub struct Sub<G> {
    pub gateway: G,
    pub guild: String,
    pub channel: String,
    pub ranges: Vec<[u32; 2]>,
    /// The UI has no rows of the list: send every range again.
    pub fresh: bool,
}

/// What to send.
#[derive(Clone, Debug, PartialEq)]
pub enum Op<G> {
    Subscribe(Sub<G>),
    Unsubscribe {
        gateway: G,
        guild: String,
        channel: String,
    },
}

/// The coalescing of member list wishes, without a clock of its own.
pub struct Pacer<G> {
    /// What the gateway was last told.
    sent: Option<Sub<G>>,
    /// The newest wish not sent yet (`Some(None)`: no member list).
    wanted: Option<Option<Sub<G>>>,
    last_sent: Option<Instant>,
    /// A timer waits to send `wanted`.
    pub timer: bool,
}

impl<G> Default for Pacer<G> {
    fn default() -> Self {
        Pacer {
            sent: None,
            wanted: None,
            last_sent: None,
            timer: false,
        }
    }
}

impl<G: Clone> Pacer<G> {
    /// A new wish: it replaces any wish not sent yet (a wish for a fresh list stays
    /// fresh until it is sent). Returns when it may go out (`now` or later).
    pub fn want(&mut self, mut wish: Option<Sub<G>>, now: Instant) -> Instant {
        if let (Some(Some(before)), Some(new)) = (&self.wanted, wish.as_mut()) {
            if before.fresh && before.guild == new.guild && before.channel == new.channel {
                new.fresh = true;
            }
        }
        self.wanted = Some(wish);
        match self.last_sent {
            Some(at) if at + COALESCE > now => at + COALESCE,
            _ => now,
        }
    }

    /// The ops for the newest wish when its time has come (nothing before).
    ///
    /// Fluxer keeps one list per guild and, when the ranges change, syncs only the
    /// ranges it did not have. So another guild's list is dropped first, and a fresh
    /// list of the same channel is dropped and subscribed again, so every range comes.
    pub fn flush(&mut self, now: Instant) -> Vec<Op<G>> {
        if self.last_sent.is_some_and(|at| at + COALESCE > now) {
            return Vec::new();
        }
        let Some(wish) = self.wanted.take() else {
            return Vec::new();
        };
        let mut ops = Vec::new();
        match (self.sent.take(), wish) {
            (Some(old), Some(new)) => {
                let same_list = old.guild == new.guild && old.channel == new.channel;
                if old.guild != new.guild || (new.fresh && same_list) {
                    ops.push(unsubscribe(old));
                }
                ops.push(Op::Subscribe(new.clone()));
                self.sent = Some(new);
            }
            (None, Some(new)) => {
                ops.push(Op::Subscribe(new.clone()));
                self.sent = Some(new);
            }
            (Some(old), None) => ops.push(unsubscribe(old)),
            (None, None) => {}
        }
        if !ops.is_empty() {
            self.last_sent = Some(now);
        }
        ops
    }

    /// The account leaves the screen: its list is unsubscribed at once and nothing
    /// waiting is sent.
    pub fn stop(&mut self) -> Vec<Op<G>> {
        self.wanted = None;
        self.sent.take().map(unsubscribe).into_iter().collect()
    }
}

fn unsubscribe<G>(s: Sub<G>) -> Op<G> {
    Op::Unsubscribe {
        gateway: s.gateway,
        guild: s.guild,
        channel: s.channel,
    }
}

/// An account's gateway, and its event sender to tell the UI what was sent.
#[derive(Clone)]
pub struct Link {
    gateway: p::GatewayHandle,
    events: Ui,
}

/// The member list part of the backend.
#[derive(Default)]
pub struct State {
    pacer: Arc<Mutex<Pacer<Link>>>,
    timer: Option<AbortHandle>,
}

/// Sends the ops. Called with the pacer locked: gateway commands only go into the
/// gateway's queue (they never wait), and the lock keeps sends in pacer order. Every
/// subscription is reported to the UI before the server can answer it.
fn run(ops: Vec<Op<Link>>) {
    for op in ops {
        match op {
            Op::Subscribe(s) => {
                if s.gateway
                    .gateway
                    .subscribe_member_list(&s.guild, &s.channel, &s.ranges)
                {
                    let _ = s
                        .gateway
                        .events
                        .unbounded_send(super::Ev::Members(Ev::Sent {
                            guild_id: s.guild,
                            channel_id: s.channel,
                            ranges: s.ranges,
                        }));
                }
            }
            Op::Unsubscribe {
                gateway,
                guild,
                channel,
            } => gateway.gateway.unsubscribe_member_list(&guild, &channel),
        }
    }
}

impl State {
    /// The active account goes away (background, sign-out): its member list is
    /// unsubscribed and a waiting wish is dropped.
    pub fn stop(&mut self) {
        if let Some(t) = self.timer.take() {
            t.abort();
        }
        // The ops go out under the lock, as the timer's do: a timer that woke just
        // before this stop has either sent its subscription already (and this
        // unsubscribes it) or finds nothing left to send.
        let mut p = self.pacer.lock().unwrap_or_else(|e| e.into_inner());
        p.timer = false;
        run(p.stop());
    }
}

pub(super) fn handle(backend: &mut Backend, cmd: Cmd) {
    let gateway = match &mut backend.active {
        #[cfg(feature = "demo")]
        Some(Active::Demo(m)) => {
            m.member_command(cmd);
            return;
        }
        Some(Active::Real(r)) => r.gateway.clone(),
        // No session: nothing to subscribe to, and no search to answer.
        None => return,
    };
    let wish = match cmd {
        Cmd::Search {
            guild_id,
            query,
            nonce,
        } => {
            let query: String = query.trim().chars().take(100).collect();
            if !query.is_empty() {
                gateway.request_guild_members(
                    &guild_id,
                    &query,
                    &[],
                    SEARCH_LIMIT,
                    true,
                    Some(&nonce),
                );
            }
            return;
        }
        Cmd::Window {
            guild_id,
            channel_id,
            ranges,
            fresh,
        } => Some(Sub {
            gateway: Link {
                gateway,
                events: backend.events.clone(),
            },
            guild: guild_id,
            channel: channel_id,
            ranges,
            fresh,
        }),
        Cmd::Close => None,
    };
    let now = Instant::now();
    let shared = backend.social.members.pacer.clone();
    let mut p = shared.lock().unwrap_or_else(|e| e.into_inner());
    let due = p.want(wish, now);
    if due <= now {
        run(p.flush(now));
        return;
    }
    if p.timer {
        // The waiting timer sends the newest wish.
        return;
    }
    p.timer = true;
    drop(p);
    let pacer = shared.clone();
    let task = backend.spawn_guarded("member list", None, async move {
        tokio::time::sleep_until(due).await;
        // Sent under the lock, so a `stop` cannot come between the flush and the send.
        let mut p = pacer.lock().unwrap_or_else(|e| e.into_inner());
        p.timer = false;
        run(p.flush(Instant::now()));
    });
    backend.social.members.timer = Some(task);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sub(guild: &str, channel: &str, page: u32) -> Option<Sub<u8>> {
        Some(Sub {
            gateway: 1,
            guild: guild.into(),
            channel: channel.into(),
            ranges: vec![[0, 99], [page * 100, page * 100 + 99]],
            fresh: false,
        })
    }

    fn fresh(s: Option<Sub<u8>>) -> Option<Sub<u8>> {
        s.map(|s| Sub { fresh: true, ..s })
    }

    /// The list hidden and shown again within 300 ms is built empty. Fluxer would
    /// send nothing for ranges it already has, so the list is dropped and subscribed
    /// again, and every range comes. A list scrolled as usual is never dropped.
    #[test]
    fn a_list_built_again_is_subscribed_from_scratch() {
        let mut p = Pacer::<u8>::default();
        let t0 = Instant::now();
        p.want(sub("1", "2", 5), t0);
        assert_eq!(p.flush(t0).len(), 1);
        // Hidden, then shown again (fresh) before 300 ms have passed.
        let t1 = t0 + Duration::from_millis(50);
        p.want(None, t1);
        p.want(fresh(sub("1", "2", 0)), t1 + Duration::from_millis(50));
        // A scroll right after keeps the wish fresh.
        let due = p.want(sub("1", "2", 0), t1 + Duration::from_millis(100));
        assert_eq!(
            p.flush(due),
            vec![
                Op::Unsubscribe {
                    gateway: 1,
                    guild: "1".into(),
                    channel: "2".into()
                },
                Op::Subscribe(fresh(sub("1", "2", 0)).unwrap()),
            ]
        );
        // From then on, windows are plain subscriptions.
        let due = p.want(sub("1", "2", 1), due);
        assert_eq!(p.flush(due), vec![Op::Subscribe(sub("1", "2", 1).unwrap())]);
        // A fresh list in another channel of the guild needs no unsubscribe (the
        // server replaces the guild's list and syncs it whole).
        let due = p.want(fresh(sub("1", "3", 0)), due);
        assert_eq!(p.flush(due).len(), 1);
    }

    /// Scrolling through a long list for two seconds (a new window every frame) sends
    /// at most one subscription per 300 ms, and the last one is the window on screen.
    #[test]
    fn member_list_subscriptions_are_coalesced_over_300_ms() {
        let mut p = Pacer::<u8>::default();
        let t0 = Instant::now();
        let mut sent = Vec::new();
        let mut due: Option<Instant> = None;
        for frame in 0..120u32 {
            let now = t0 + Duration::from_millis(u64::from(frame) * 16);
            if due.is_some_and(|d| d <= now) {
                sent.extend(p.flush(now).into_iter().map(|op| (now, op)));
                due = None;
            }
            let at = p.want(sub("1", "2", frame / 10), now);
            if at <= now {
                sent.extend(p.flush(now).into_iter().map(|op| (now, op)));
            } else {
                due.get_or_insert(at);
            }
        }
        let end = t0 + Duration::from_secs(3);
        sent.extend(p.flush(end).into_iter().map(|op| (end, op)));
        assert!(sent.len() <= 2_000 / 300 + 2, "{} sent", sent.len());
        for pair in sent.windows(2) {
            assert!(pair[1].0 - pair[0].0 >= COALESCE);
        }
        assert_eq!(
            sent.last().unwrap().1,
            Op::Subscribe(sub("1", "2", 11).unwrap())
        );
        // Nothing waits any more.
        assert!(p.flush(end + Duration::from_secs(1)).is_empty());
    }

    #[test]
    fn another_guild_unsubscribes_the_old_one_and_stop_leaves() {
        let mut p = Pacer::<u8>::default();
        let t0 = Instant::now();
        assert_eq!(p.want(sub("1", "2", 0), t0), t0);
        assert_eq!(p.flush(t0).len(), 1);
        // Another channel of the same guild replaces the list (the server keeps one
        // list per guild): one op 14.
        let t1 = t0 + COALESCE;
        p.want(sub("1", "3", 0), t1);
        assert_eq!(p.flush(t1), vec![Op::Subscribe(sub("1", "3", 0).unwrap())]);
        // Another guild: the old guild's list is dropped first.
        let t2 = t1 + COALESCE;
        p.want(sub("9", "4", 0), t2);
        let ops = p.flush(t2);
        assert_eq!(
            ops[0],
            Op::Unsubscribe {
                gateway: 1,
                guild: "1".into(),
                channel: "3".into()
            }
        );
        assert_eq!(ops.len(), 2);
        // Too early: nothing.
        p.want(None, t2 + Duration::from_millis(10));
        assert!(p.flush(t2 + Duration::from_millis(10)).is_empty());
        // Leaving sends the unsubscribe at once and forgets the wish.
        assert_eq!(p.stop().len(), 1);
        assert!(p.flush(t2 + Duration::from_secs(5)).is_empty());
        assert!(p.stop().is_empty());
    }
}
