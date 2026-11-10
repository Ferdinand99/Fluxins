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
use p::permissions::{BYPASS_SLOWMODE, MENTION_EVERYONE};

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

impl Rights {
    /// From permission bits of a channel.
    pub fn from_bits(bits: u64) -> Rights {
        Rights {
            mention_everyone: bits & MENTION_EVERYONE != 0,
            bypass_slowmode: bits & BYPASS_SLOWMODE != 0,
        }
    }
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
    /// The last failure may have reached the server.
    ambiguous: bool,
}

#[derive(Default)]
struct Inner {
    gen: u64,
    session: Option<(String, usize)>,
    /// Open member searches: nonce and guild, oldest first.
    searches: VecDeque<(String, String)>,
    outbox: VecDeque<SentMessage>,
}

/// Whether a Retry must first look for the message: the failure was unclear and the
/// server no longer recognises the nonce.
pub fn look_before_retry(ambiguous: bool, since_first: Duration) -> bool {
    ambiguous && since_first > NONCE_MEMORY
}

impl State {
    /// The state of session `gen`; anything from another session is dropped first.
    fn of(&self, gen: u64) -> MutexGuard<'_, Inner> {
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if inner.gen != gen {
            *inner = Inner {
                gen,
                ..Inner::default()
            };
        }
        inner
    }

    pub fn session(&self, gen: u64) -> Option<(String, usize)> {
        self.of(gen).session.clone()
    }

    pub fn set_session(&self, gen: u64, user_id: String, max_len: usize) {
        self.of(gen).session = Some((user_id, max_len));
    }

    /// Takes the guild of an open search with this nonce.
    fn take_search(&self, nonce: &str) -> Option<String> {
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let ix = inner.searches.iter().position(|(n, _)| n == nonce)?;
        inner.searches.remove(ix).map(|(_, g)| g)
    }

    /// Keeps a message being sent, once (a retry keeps its first time).
    pub fn remember_send(&self, gen: u64, message: &Outgoing) {
        let mut inner = self.of(gen);
        if inner.outbox.iter().any(|s| s.message.nonce == message.nonce) {
            return;
        }
        if inner.outbox.len() >= MAX_OUTBOX {
            inner.outbox.pop_front();
        }
        inner.outbox.push_back(SentMessage {
            message: message.clone(),
            first: Instant::now(),
            ambiguous: false,
        });
    }

    pub fn mark_failed(&self, gen: u64, nonce: &str, ambiguous: bool) {
        if let Some(s) = self
            .of(gen)
            .outbox
            .iter_mut()
            .find(|s| s.message.nonce == nonce)
        {
            s.ambiguous = ambiguous;
        }
    }

    pub fn forget_send(&self, gen: u64, nonce: &str) {
        self.of(gen).outbox.retain(|s| s.message.nonce != nonce);
    }

    /// The message to send again for a Retry, and whether to look for it first.
    pub fn retry_of(&self, gen: u64, nonce: &str) -> Option<(Outgoing, bool)> {
        let inner = self.of(gen);
        let s = inner.outbox.iter().find(|s| s.message.nonce == nonce)?;
        Some((
            s.message.clone(),
            look_before_retry(s.ambiguous, s.first.elapsed()),
        ))
    }
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
                    let mut inner = self.composer.of(r.gen);
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
        let (again, look) = state.retry_of(1, "n1").unwrap();
        assert_eq!(again, outgoing("n1"), "mentions, reply and @silent are kept");
        assert!(!look);
        state.forget_send(1, "n1");
        assert!(state.retry_of(1, "n1").is_none());
        // Another session forgets everything.
        state.remember_send(1, &outgoing("n2"));
        assert!(state.retry_of(2, "n2").is_none());
    }

    #[test]
    fn only_a_late_retry_after_an_unclear_failure_looks_first() {
        let six = Duration::from_secs(6 * 60);
        assert!(look_before_retry(true, six));
        assert!(!look_before_retry(false, six), "a clear refusal is sent again");
        assert!(!look_before_retry(true, Duration::from_secs(60)), "the server still knows the nonce");
    }
}
