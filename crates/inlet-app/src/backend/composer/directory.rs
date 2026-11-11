// SPDX-License-Identifier: AGPL-3.0-or-later
//! What autocomplete offers and the composer's state in the backend.
//!
//! The roles, channels, custom emoji and the account's rights per channel come from the
//! account state on the UI thread (`ui::composer::autocomplete::from_account_state`), so
//! no request is spent on data READY and GUILD_CREATE already gave. Members found by a
//! search over the gateway come through here. Everything comes from the server; the
//! composer never builds a mention from typed text.
//!
//! The backend also keeps each message the composer sent until the server took it, so a
//! Retry from the message list sends it again exactly as it was (mentions, reply,
//! `@silent`), and knows whether the first failure was unclear.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use inlet_protocol as p;

use super::send::{snowflake, Outgoing};
use super::Ev;
use crate::backend::{Active, Backend, Ui};
use crate::model;

/// Member searches whose answer is still awaited.
const MAX_SEARCHES: usize = 16;
/// Members asked for per search.
const SEARCH_LIMIT: u8 = 25;
/// Sent messages remembered for Retry.
const MAX_OUTBOX: usize = 50;
/// Server messages already counted as the copy of a sent message.
const MAX_CLAIMED: usize = 50;
/// How long Fluxer recognises a nonce (`MESSAGE_NONCE_TTL`): a repeat with the same
/// nonce within it is dropped by the server.
pub const NONCE_MEMORY: Duration = Duration::from_secs(5 * 60);

/// A member, for `@` autocomplete.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Person {
    pub id: u64,
    /// Nickname or display name, cleaned.
    pub name: String,
    /// Account name, cleaned.
    pub username: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoleEntry {
    pub id: u64,
    pub name: String,
    /// Anyone may mention it; otherwise only with the right to mention everyone.
    pub mentionable: bool,
}

/// The account's rights in one channel that change the composer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rights {
    /// May mention `@everyone`, `@here` and every role.
    pub mention_everyone: bool,
    /// Ignores slowmode.
    pub bypass_slowmode: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelEntry {
    pub id: u64,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CustomEmoji {
    pub id: u64,
    /// ASCII letters, digits, `_` and `-` only.
    pub name: String,
    pub animated: bool,
}

/// One guild's autocomplete data.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Directory {
    /// The guild's roles but `@everyone`, highest first.
    pub roles: Vec<RoleEntry>,
    /// Text channels the account can see.
    pub channels: Vec<ChannelEntry>,
    pub emojis: Vec<CustomEmoji>,
    /// The account's rights per text channel, with the channel's overwrites. A channel
    /// that is not here gives no extra rights.
    pub rights: HashMap<String, Rights>,
}

impl Directory {
    /// The account's rights in `channel_id` (none when unknown).
    pub fn rights(&self, channel_id: &str) -> Rights {
        self.rights.get(channel_id).copied().unwrap_or_default()
    }
}

/// The composer's state in the backend, shared with its tasks.
#[derive(Clone, Default)]
pub struct State(Arc<Mutex<Inner>>);

/// A message the composer sent that the server has not taken yet.
struct SentMessage {
    message: Outgoing,
    first: Instant,
    /// The first send's wall-clock time, in Unix milliseconds: copies are looked for
    /// from there on.
    first_ms: u64,
    /// Some failure may have reached the server. Once set, it stays: a later clear
    /// refusal does not prove that the first attempt did not arrive.
    ambiguous: bool,
}

/// How a Retry goes out.
pub struct RetryPlan {
    pub message: Outgoing,
    /// Look for a copy that arrived after all before sending, from this Unix time in
    /// milliseconds on (the first send's time).
    pub look_since_ms: Option<u64>,
}

#[derive(Default)]
struct Inner {
    gen: u64,
    session: Option<(String, usize)>,
    /// Open member searches: nonce and guild, oldest first.
    searches: VecDeque<(String, String)>,
    outbox: VecDeque<SentMessage>,
    /// IDs of server messages a late Retry took as its copy.
    claimed: VecDeque<String>,
    /// When the GIF pickers last asked for trending (0) and the featured terms (1).
    gif_home: [Option<Instant>; 2],
}

/// Whether a Retry must first look for the message: the failure was unclear and the
/// server no longer recognises the nonce.
pub fn look_before_retry(ambiguous: bool, since_first: Duration) -> bool {
    ambiguous && since_first > NONCE_MEMORY
}

impl State {
    /// The state of session `gen`. A newer session drops what an older one left; a
    /// late task of an older session gets `None` and changes nothing (generations only
    /// grow).
    fn of(&self, gen: u64) -> Option<MutexGuard<'_, Inner>> {
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if gen < inner.gen {
            return None;
        }
        if gen > inner.gen {
            *inner = Inner {
                gen,
                ..Inner::default()
            };
        }
        Some(inner)
    }

    pub fn session(&self, gen: u64) -> Option<(String, usize)> {
        self.of(gen)?.session.clone()
    }

    /// Whether the GIF picker may ask for trending (`kind` 0) or the featured terms
    /// (`kind` 1) now: at most once per [`super::gif::GIF_HOME_INTERVAL`] per account.
    pub fn gif_home_slot(&self, gen: u64, kind: usize, now: Instant) -> bool {
        let Some(mut inner) = self.of(gen) else {
            return false;
        };
        let Some(last) = inner.gif_home.get_mut(kind) else {
            return false;
        };
        if last.is_some_and(|t| now.saturating_duration_since(t) < super::gif::GIF_HOME_INTERVAL) {
            return false;
        }
        *last = Some(now);
        true
    }

    pub fn set_session(&self, gen: u64, user_id: String, max_len: usize) {
        if let Some(mut inner) = self.of(gen) {
            inner.session = Some((user_id, max_len));
        }
    }

    /// Takes the guild of an open search with this nonce.
    fn take_search(&self, nonce: &str) -> Option<String> {
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let ix = inner.searches.iter().position(|(n, _)| n == nonce)?;
        inner.searches.remove(ix).map(|(_, g)| g)
    }

    /// Keeps a message being sent, once (a retry keeps its first time).
    pub fn remember_send(&self, gen: u64, message: &Outgoing) {
        let Some(mut inner) = self.of(gen) else {
            return;
        };
        if inner
            .outbox
            .iter()
            .any(|s| s.message.nonce == message.nonce)
        {
            return;
        }
        if inner.outbox.len() >= MAX_OUTBOX {
            inner.outbox.pop_front();
        }
        inner.outbox.push_back(SentMessage {
            message: message.clone(),
            first: Instant::now(),
            first_ms: unix_ms(),
            ambiguous: false,
        });
    }

    pub fn mark_failed(&self, gen: u64, nonce: &str, ambiguous: bool) {
        let Some(mut inner) = self.of(gen) else {
            return;
        };
        if let Some(s) = inner.outbox.iter_mut().find(|s| s.message.nonce == nonce) {
            s.ambiguous |= ambiguous;
        }
    }

    pub fn forget_send(&self, gen: u64, nonce: &str) {
        if let Some(mut inner) = self.of(gen) {
            inner.outbox.retain(|s| s.message.nonce != nonce);
        }
    }

    /// Takes server message `id` as the copy of a sent message. `false` when another
    /// send has taken it already (or the session is gone).
    pub fn claim_copy(&self, gen: u64, id: &str) -> bool {
        let Some(mut inner) = self.of(gen) else {
            return false;
        };
        if inner.claimed.iter().any(|c| c == id) {
            return false;
        }
        if inner.claimed.len() >= MAX_CLAIMED {
            inner.claimed.pop_front();
        }
        inner.claimed.push_back(id.to_owned());
        true
    }

    /// The message to send again for a Retry at `now`, and whether to look for it
    /// first. `None` for a message this session's composer did not send.
    pub fn retry_of(&self, gen: u64, nonce: &str, now: Instant) -> Option<RetryPlan> {
        let inner = self.of(gen)?;
        let s = inner.outbox.iter().find(|s| s.message.nonce == nonce)?;
        let late = look_before_retry(s.ambiguous, now.saturating_duration_since(s.first));
        Some(RetryPlan {
            message: s.message.clone(),
            look_since_ms: late.then_some(s.first_ms),
        })
    }
}

/// The wall clock in Unix milliseconds.
pub fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

pub fn members_chunk(state: &State, events: &Ui, chunk: &p::models::GuildMembersChunk) {
    let Some(nonce) = chunk.nonce.as_deref() else {
        return;
    };
    let Some(guild_id) = state.take_search(nonce) else {
        return;
    };
    if guild_id != chunk.guild_id {
        return;
    }
    let members = chunk
        .members
        .iter()
        .take(usize::from(SEARCH_LIMIT) * 4)
        .filter_map(person)
        .collect();
    let _ = events.unbounded_send(Ev::Members { guild_id, members });
}

fn person(m: &p::GuildMember) -> Option<Person> {
    let user = m.user.as_ref()?;
    let id = snowflake(&user.id)?.parse().ok()?;
    let name = m.display_name().unwrap_or_else(|| user.display_name());
    Some(Person {
        id,
        name: model::clean_name(name),
        username: model::clean_name(&user.username),
    })
}

impl Backend {
    /// Asks the gateway for members whose name starts with `query`.
    pub(super) fn search_members(&mut self, guild_id: String, query: String, nonce: String) {
        let query: String = query.trim().chars().take(32).collect();
        if query.is_empty() || nonce.len() > 32 {
            return;
        }
        match &self.active {
            #[cfg(feature = "demo")]
            Some(Active::Demo(_)) => self.emit(Ev::Members {
                members: crate::demo::composer::members(&query),
                guild_id,
            }),
            Some(Active::Real(r)) => {
                if snowflake(&guild_id).is_none() {
                    return;
                }
                let sent = r.gateway.request_guild_members(
                    &guild_id,
                    &query,
                    &[],
                    SEARCH_LIMIT,
                    false,
                    Some(&nonce),
                );
                if sent {
                    let Some(mut inner) = r.composer.of(r.gen) else {
                        return;
                    };
                    if inner.searches.len() >= MAX_SEARCHES {
                        inner.searches.pop_front();
                    }
                    inner.searches.push_back((nonce, guild_id));
                }
            }
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::composer::Mentions;

    fn outgoing(nonce: &str) -> Outgoing {
        Outgoing {
            channel_id: "1".into(),
            nonce: nonce.into(),
            content: "hi <@7>".into(),
            mentions: Mentions {
                users: vec![7],
                ..Default::default()
            },
            reply_to: Some("5".into()),
            silent: true,
        }
    }

    #[test]
    fn a_retry_sends_the_message_as_it_was_first_sent() {
        let state = State::default();
        state.remember_send(1, &outgoing("n1"));
        let plan = state.retry_of(1, "n1", Instant::now()).unwrap();
        assert_eq!(
            plan.message,
            outgoing("n1"),
            "mentions, reply and @silent are kept"
        );
        assert!(plan.look_since_ms.is_none());
        state.forget_send(1, "n1");
        assert!(state.retry_of(1, "n1", Instant::now()).is_none());
        // A newer session forgets everything.
        state.remember_send(1, &outgoing("n2"));
        assert!(state.retry_of(2, "n2", Instant::now()).is_none());
    }

    #[test]
    fn an_unclear_failure_stays_unclear() {
        let state = State::default();
        state.remember_send(1, &outgoing("n1"));
        state.mark_failed(1, "n1", true);
        // A Retry within the nonce window got a clear refusal (a rate limit).
        state.mark_failed(1, "n1", false);
        let late = Instant::now() + Duration::from_secs(6 * 60);
        let plan = state.retry_of(1, "n1", late).unwrap();
        assert!(
            plan.look_since_ms.is_some(),
            "the first attempt may still have arrived"
        );
    }

    #[test]
    fn a_copy_counts_for_one_sent_message() {
        let state = State::default();
        assert!(state.claim_copy(1, "900"));
        assert!(!state.claim_copy(1, "900"), "already taken");
        assert!(state.claim_copy(1, "901"));
        assert!(
            !state.claim_copy(0, "902"),
            "an older session takes nothing"
        );
    }

    #[test]
    fn a_late_task_of_an_older_session_changes_nothing() {
        let state = State::default();
        state.set_session(2, "42".into(), 4000);
        state.remember_send(2, &outgoing("n2"));
        // A send of session 1 finishes after session 2 started.
        state.forget_send(1, "n2");
        state.mark_failed(1, "n2", true);
        state.set_session(1, "7".into(), 2000);
        assert_eq!(state.session(2), Some(("42".to_owned(), 4000)));
        assert!(state.session(1).is_none());
        let plan = state.retry_of(2, "n2", Instant::now()).unwrap();
        assert_eq!(plan.message, outgoing("n2"));
    }

    #[test]
    fn only_a_late_retry_after_an_unclear_failure_looks_first() {
        let six = Duration::from_secs(6 * 60);
        assert!(look_before_retry(true, six));
        assert!(
            !look_before_retry(false, six),
            "a clear refusal is sent again"
        );
        assert!(
            !look_before_retry(true, Duration::from_secs(60)),
            "the server still knows the nonce"
        );
    }
}
