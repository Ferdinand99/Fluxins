// SPDX-License-Identifier: AGPL-3.0-or-later
//! What autocomplete offers: a guild's roles, channels and custom emoji, the account's
//! rights per channel that change the composer (mention everyone, ignore slowmode), and
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
    fn from_bits(bits: u64) -> Rights {
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

/// Whether a channel type takes text messages in a guild.
fn is_text(kind: i64) -> bool {
    use p::models::channel_type::{GUILD_ANNOUNCEMENT, GUILD_TEXT};
    kind == GUILD_TEXT || kind == GUILD_ANNOUNCEMENT
}

/// The server's answers for one guild, as [`build`] reads them.
pub struct Fetched<'a> {
    pub guild_id: &'a str,
    pub user_id: &'a str,
    pub roles: &'a [p::Role],
    /// The account's own member; without it nothing extra is allowed.
    pub member: Option<&'a p::GuildMember>,
    pub emojis: &'a [p::GuildEmoji],
    /// The guild's channels with their overwrites; empty when they could not be read.
    pub channels: &'a [p::Channel],
    /// Channel names from the sidebar, used when `channels` is empty.
    pub fallback: Vec<ChannelEntry>,
}

/// Builds a directory from the server's answers. Rights are computed per channel with
/// its overwrites (the guild owner is not known here, so an owner gets only what the
/// roles give; the server decides in the end).
pub fn build(f: Fetched) -> Directory {
    let ctx = f.member.map(|m| p::PermissionContext {
        guild_id: f.guild_id,
        owner_id: None,
        user_id: f.user_id,
        roles: f.roles,
        member_roles: &m.roles,
        timed_out: m.is_timed_out(),
        ..Default::default()
    });
    let mut rights = HashMap::new();
    let mut entries = Vec::new();
    for c in f.channels.iter().filter(|c| is_text(c.kind)) {
        let Some(ctx) = &ctx else { break };
        let parent = c
            .parent_id
            .as_ref()
            .and_then(|pid| f.channels.iter().find(|x| &x.id == pid));
        let bits = ctx.channel(c, parent);
        if bits & p::permissions::VIEW_CHANNEL == 0 {
            continue;
        }
        rights.insert(c.id.clone(), Rights::from_bits(bits));
        if let Some(id) = snowflake(&c.id).and_then(|s| s.parse().ok()) {
            if entries.len() < MAX_CHANNELS {
                entries.push(ChannelEntry {
                    id,
                    name: model::clean_name(c.name.as_deref().unwrap_or_default()),
                });
            }
        }
    }
    if f.channels.is_empty() {
        entries = f.fallback;
    }
    let mut sorted: Vec<&p::Role> = f.roles.iter().filter(|r| r.id != f.guild_id).collect();
    sorted.sort_by_key(|r| std::cmp::Reverse(r.position));
    Directory {
        roles: sorted
            .into_iter()
            .filter_map(|r| {
                Some(RoleEntry {
                    id: snowflake(&r.id)?.parse().ok()?,
                    name: model::clean_name(&r.name),
                    mentionable: r.mentionable,
                })
            })
            .take(MAX_ROLES)
            .collect(),
        channels: entries,
        rights,
        emojis: f
            .emojis
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
    }
}

impl Backend {
    /// Sends the directory of `guild_id`: from the cache, or fetched (four requests:
    /// roles, own member, emoji and channels, at most once per guild every ten minutes).
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
                let fallback = channel_entries(
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
                    if let Some((at, dir)) = inner.guilds.get(&guild) {
                        if at.elapsed() < DIRECTORY_TTL {
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
                    let (roles, member, emojis, channels) = tokio::join!(
                        client.roles(&guild),
                        client.my_guild_member(&guild),
                        client.guild_emojis(&guild),
                        client.channels(&guild),
                    );
                    let member = member.ok();
                    let user_id = member
                        .as_ref()
                        .and_then(|m| m.user.as_ref())
                        .map(|u| u.id.clone())
                        .or_else(|| state.session(gen).map(|(id, _)| id))
                        .unwrap_or_default();
                    let (roles, emojis, channels) = (
                        roles.unwrap_or_default(),
                        emojis.unwrap_or_default(),
                        channels.unwrap_or_default(),
                    );
                    let directory = build(Fetched {
                        guild_id: &guild,
                        user_id: &user_id,
                        roles: &roles,
                        member: member.as_ref(),
                        emojis: &emojis,
                        channels: &channels,
                        fallback,
                    });
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

    fn channel(id: &str, overwrites: serde_json::Value) -> p::Channel {
        serde_json::from_value(json!({
            "id": id, "type": 0, "name": format!("c{id}"), "guild_id": "1",
            "permission_overwrites": overwrites
        }))
        .unwrap()
    }

    const VIEW: u64 = p::permissions::VIEW_CHANNEL | p::permissions::SEND_MESSAGES;

    fn fetched<'a>(
        roles: &'a [p::Role],
        member: Option<&'a p::GuildMember>,
        channels: &'a [p::Channel],
    ) -> Fetched<'a> {
        Fetched {
            guild_id: "1",
            user_id: "7",
            roles,
            member,
            emojis: &[],
            channels,
            fallback: vec![],
        }
    }

    #[test]
    fn rights_follow_the_channel_overwrites() {
        let roles = [
            role("1", "@everyone", 0, VIEW, false),
            role("2", "Mods", 2, MENTION_EVERYONE | BYPASS_SLOWMODE, true),
            role("3", "Secret", 3, 0, false),
        ];
        let deny = (MENTION_EVERYONE | BYPASS_SLOWMODE).to_string();
        let channels = [
            channel("10", json!([])),
            channel("11", json!([{ "id": "2", "type": 0, "allow": "0", "deny": deny }])),
            channel("12", json!([{ "id": "1", "type": 0, "allow": "0",
                                   "deny": p::permissions::VIEW_CHANNEL.to_string() }])),
        ];
        let me = member(&["2"]);
        let dir = build(fetched(&roles, Some(&me), &channels));
        assert_eq!(dir.roles.iter().map(|r| r.id).collect::<Vec<_>>(), [3, 2]);
        assert!(dir.roles.iter().any(|r| r.id == 2 && r.mentionable));
        let open = dir.rights("10");
        assert!(open.mention_everyone && open.bypass_slowmode);
        // The channel denies both: no everyone pills and the countdown runs there.
        assert_eq!(dir.rights("11"), Rights::default());
        // A hidden channel is neither offered nor given rights.
        assert_eq!(dir.rights("12"), Rights::default());
        assert_eq!(
            dir.channels.iter().map(|c| c.id).collect::<Vec<_>>(),
            [10, 11]
        );
        // A plain member gets no extra rights anywhere.
        let plain = member(&[]);
        let dir = build(fetched(&roles, Some(&plain), &channels));
        assert_eq!(dir.rights("10"), Rights::default());
    }

    #[test]
    fn unknown_own_member_gives_no_extra_rights() {
        let roles = [role("1", "@everyone", 0, VIEW | MENTION_EVERYONE, false)];
        let channels = [channel("10", json!([]))];
        let dir = build(fetched(&roles, None, &channels));
        assert_eq!(dir.rights("10"), Rights::default());
        assert!(dir.channels.is_empty());
    }

    #[test]
    fn emoji_with_odd_names_are_left_out() {
        let emojis: Vec<p::GuildEmoji> = serde_json::from_value(json!([
            { "id": "5", "name": "wave", "animated": false },
            { "id": "6", "name": "x:<@1>", "animated": false },
            { "id": "bad", "name": "nope", "animated": false },
        ]))
        .unwrap();
        let mut f = fetched(&[], None, &[]);
        f.emojis = &emojis;
        let dir = build(f);
        assert_eq!(dir.emojis.len(), 1);
        assert_eq!(dir.emojis[0].name, "wave");
    }
}
