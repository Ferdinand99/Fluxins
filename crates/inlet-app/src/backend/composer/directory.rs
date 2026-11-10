// SPDX-License-Identifier: AGPL-3.0-or-later
//! What autocomplete offers: a guild's roles, channels and custom emoji, the account's
//! guild-level rights that change the composer (mention everyone, ignore slowmode), and
//! member search over the gateway. Everything here comes from the server; the composer
//! never builds a mention from typed text.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use inlet_protocol as p;
use p::permissions::{BYPASS_SLOWMODE, MENTION_EVERYONE};

use super::send::snowflake;
use super::Ev;
use crate::backend::{Active, Backend, Ui};
use crate::model::{self, Channel};

/// How long a guild's directory is reused before it is fetched again.
const DIRECTORY_TTL: Duration = Duration::from_secs(10 * 60);
/// Guild directories kept per session.
const MAX_GUILDS: usize = 32;
/// Member searches whose answer is still awaited.
const MAX_SEARCHES: usize = 16;
/// Members asked for per search.
const SEARCH_LIMIT: u8 = 25;
/// Caps on what one directory holds.
const MAX_ROLES: usize = 1_000;
const MAX_CHANNELS: usize = 2_000;
const MAX_EMOJIS: usize = 2_000;

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
    /// Roles the account may mention, highest first.
    pub roles: Vec<RoleEntry>,
    /// Text channels.
    pub channels: Vec<ChannelEntry>,
    pub emojis: Vec<CustomEmoji>,
    /// The account may mention `@everyone`, `@here` and every role.
    pub mention_everyone: bool,
    /// The account ignores slowmode.
    pub bypass_slowmode: bool,
}

/// The composer's cache in the backend, shared with its tasks.
#[derive(Clone, Default)]
pub struct State(Arc<Mutex<Inner>>);

#[derive(Default)]
struct Inner {
    gen: u64,
    session: Option<(String, usize)>,
    guilds: HashMap<String, (Instant, Directory)>,
    loading: HashSet<String>,
    /// Open member searches: nonce and guild, oldest first.
    searches: VecDeque<(String, String)>,
}

impl State {
    /// The cache of session `gen`; anything from another session is dropped first.
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
}

/// A GUILD_MEMBERS_CHUNK from the gateway: if it answers one of the composer's searches,
/// the members go to the UI.
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

/// Text channels with a snowflake ID, for `#` autocomplete.
pub fn channel_entries(channels: &[Channel]) -> Vec<ChannelEntry> {
    channels
        .iter()
        .filter(|c| c.is_messageable())
        .filter_map(|c| {
            Some(ChannelEntry {
                id: snowflake(&c.id)?.parse().ok()?,
                name: c.name.clone(),
            })
        })
        .take(MAX_CHANNELS)
        .collect()
}

/// Whether a custom emoji name can be sent as it is.
fn plain_emoji_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Builds a directory from the server's answers. `member` is the account's own member.
pub fn build(
    guild_id: &str,
    user_id: &str,
    roles: &[p::Role],
    member: Option<&p::GuildMember>,
    emojis: &[p::GuildEmoji],
    channels: Vec<ChannelEntry>,
) -> Directory {
    // Without the own member the rights are unknown: allow nothing extra.
    let bits = member.map_or(0, |m| {
        p::PermissionContext {
            guild_id,
            owner_id: None,
            user_id,
            roles,
            member_roles: &m.roles,
            timed_out: m.is_timed_out(),
            ..Default::default()
        }
        .guild()
    });
    let mention_everyone = bits & MENTION_EVERYONE != 0;
    let mut sorted: Vec<&p::Role> = roles.iter().filter(|r| r.id != guild_id).collect();
    sorted.sort_by_key(|r| std::cmp::Reverse(r.position));
    Directory {
        roles: sorted
            .into_iter()
            .filter(|r| r.mentionable || mention_everyone)
            .filter_map(|r| {
                Some(RoleEntry {
                    id: snowflake(&r.id)?.parse().ok()?,
                    name: model::clean_name(&r.name),
                })
            })
            .take(MAX_ROLES)
            .collect(),
        channels,
        emojis: emojis
            .iter()
            .filter(|e| plain_emoji_name(&e.name))
            .filter_map(|e| {
                Some(CustomEmoji {
                    id: snowflake(&e.id)?.parse().ok()?,
                    name: e.name.clone(),
                    animated: e.animated,
                })
            })
            .take(MAX_EMOJIS)
            .collect(),
        mention_everyone,
        bypass_slowmode: bits & BYPASS_SLOWMODE != 0,
    }
}

impl Backend {
    /// Sends the directory of `guild_id`: from the cache, or fetched (three requests, at
    /// most once per guild every ten minutes).
    pub(super) fn directory(&mut self, guild_id: String) {
        match &self.active {
            #[cfg(feature = "demo")]
            Some(Active::Demo(_)) => self.emit(Ev::Directory {
                directory: crate::demo::composer::directory(&guild_id),
                guild_id,
            }),
            Some(Active::Real(r)) => {
                let Some(guild) = snowflake(&guild_id).map(str::to_owned) else {
                    return;
                };
                let channels = channel_entries(
                    r.cache
                        .lock()
                        .unwrap()
                        .get(&Some(guild.clone()))
                        .map(Vec::as_slice)
                        .unwrap_or_default(),
                );
                let gen = r.gen;
                {
                    let mut inner = self.composer.of(gen);
                    if let Some((at, dir)) = inner.guilds.get_mut(&guild) {
                        if at.elapsed() < DIRECTORY_TTL {
                            dir.channels = channels;
                            let directory = dir.clone();
                            drop(inner);
                            self.emit(Ev::Directory {
                                guild_id: guild,
                                directory,
                            });
                            return;
                        }
                    }
                    if !inner.loading.insert(guild.clone()) {
                        return;
                    }
                }
                let (session, events, state) = (
                    r.session.clone(),
                    self.events.clone(),
                    self.composer.clone(),
                );
                self.spawn_guarded("directory", None, async move {
                    let client = session.client();
                    let (roles, member, emojis) = tokio::join!(
                        client.roles(&guild),
                        client.my_guild_member(&guild),
                        client.guild_emojis(&guild),
                    );
                    let member = member.ok();
                    let user_id = member
                        .as_ref()
                        .and_then(|m| m.user.as_ref())
                        .map(|u| u.id.clone())
                        .or_else(|| state.session(gen).map(|(id, _)| id))
                        .unwrap_or_default();
                    let directory = build(
                        &guild,
                        &user_id,
                        &roles.unwrap_or_default(),
                        member.as_ref(),
                        &emojis.unwrap_or_default(),
                        channels,
                    );
                    {
                        let mut inner = state.of(gen);
                        inner.loading.remove(&guild);
                        if inner.guilds.len() >= MAX_GUILDS {
                            if let Some(oldest) = inner
                                .guilds
                                .iter()
                                .min_by_key(|(_, (at, _))| *at)
                                .map(|(k, _)| k.clone())
                            {
                                inner.guilds.remove(&oldest);
                            }
                        }
                        inner
                            .guilds
                            .insert(guild.clone(), (Instant::now(), directory.clone()));
                    }
                    let _ = events.unbounded_send(Ev::Directory {
                        guild_id: guild,
                        directory,
                    });
                });
            }
            None => {}
        }
    }

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
    use serde_json::json;

    fn role(id: &str, name: &str, position: i64, perms: u64, mentionable: bool) -> p::Role {
        serde_json::from_value(json!({
            "id": id, "name": name, "position": position,
            "permissions": perms.to_string(), "mentionable": mentionable
        }))
        .unwrap()
    }

    fn member(roles: &[&str]) -> p::GuildMember {
        serde_json::from_value(json!({
            "user": { "id": "7", "username": "me" },
            "roles": roles,
        }))
        .unwrap()
    }

    #[test]
    fn only_mentionable_roles_without_the_right_to_mention_everyone() {
        let roles = [
            role("1", "@everyone", 0, 0, false),
            role("2", "Mods", 2, 0, true),
            role("3", "Secret", 3, 0, false),
        ];
        let dir = build("1", "7", &roles, Some(&member(&[])), &[], vec![]);
        assert_eq!(dir.roles.iter().map(|r| r.id).collect::<Vec<_>>(), [2]);
        assert!(!dir.mention_everyone);
        assert!(!dir.bypass_slowmode);

        let roles = [
            role("1", "@everyone", 0, 0, false),
            role("2", "Mods", 2, MENTION_EVERYONE | BYPASS_SLOWMODE, true),
            role("3", "Secret", 3, 0, false),
        ];
        let dir = build("1", "7", &roles, Some(&member(&["2"])), &[], vec![]);
        assert_eq!(dir.roles.iter().map(|r| r.id).collect::<Vec<_>>(), [3, 2]);
        assert!(dir.mention_everyone && dir.bypass_slowmode);
    }

    #[test]
    fn unknown_own_member_gives_no_extra_rights() {
        let roles = [role("1", "@everyone", 0, MENTION_EVERYONE, false)];
        let dir = build("1", "7", &roles, None, &[], vec![]);
        assert!(!dir.mention_everyone && !dir.bypass_slowmode);
    }

    #[test]
    fn emoji_with_odd_names_are_left_out() {
        let emojis: Vec<p::GuildEmoji> = serde_json::from_value(json!([
            { "id": "5", "name": "wave", "animated": false },
            { "id": "6", "name": "x:<@1>", "animated": false },
            { "id": "bad", "name": "nope", "animated": false },
        ]))
        .unwrap();
        let dir = build("1", "7", &[], None, &emojis, vec![]);
        assert_eq!(dir.emojis.len(), 1);
        assert_eq!(dir.emojis[0].name, "wave");
    }
}
