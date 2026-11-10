// SPDX-License-Identifier: AGPL-3.0-or-later
//! The per-session read tracker: `inlet-state`'s account state, fed with the gateway's
//! events, plus what it takes to act on it (acknowledging, writing settings) and to tell
//! the UI what changed.
//!
//! Request budget (all through the protocol's limiter, which also coalesces
//! acknowledgements into one batched request per second for the account):
//!
//! - Reading a channel: at most one acknowledgement per channel per 3 s, only for
//!   channels that were watched (open and in front), for the newest message only.
//! - "Mark as read" for a channel, a guild or everything: one `ack` per unread channel,
//!   which the protocol sends as batches of up to 100 channels, one batch per second.
//!   Nothing is sent for a channel that is already read.
//! - Settings: changes merge per guild for half a second, then one `PATCH`; at most two
//!   per tick.
//! - The mute clock runs on the same tick (about once a second); nothing else polls.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use inlet_protocol as p;
use inlet_state::{AccountState, AckKind, Changes, ChannelId, GuildId, MessageId, UserId};

use super::settings::{self, Change, SettingsBatch, Target};
use super::{Badge, BadgeUpdate, Ev};
use crate::backend::messages::ack::AckGate;
use crate::backend::session::Failure;
use crate::backend::{accounts, Ui};

/// The time, as the instant the gate counts in and the Unix time the mutes count in.
#[derive(Clone, Copy, Debug)]
pub struct Now {
    pub at: Instant,
    pub ms: i64,
}

impl Now {
    pub fn current() -> Now {
        Now {
            at: Instant::now(),
            ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as i64),
        }
    }
}

/// One tracker per session, shared by the gateway task (events, tick) and the backend
/// loop (commands). Locks are short and never held across an await.
pub type ReadHandle = Arc<Mutex<ReadTracker>>;

pub struct ReadTracker {
    client: p::Client,
    events: Ui,
    /// `None` until the first READY.
    state: Option<AccountState>,
    gate: AckGate,
    /// What the UI holds now, so only real changes are sent.
    shown_channels: HashMap<ChannelId, Badge>,
    shown_guilds: HashMap<Option<GuildId>, Badge>,
    /// The channel being read (open, window in front, at the bottom).
    watching: Option<ChannelId>,
    /// The channel `Opened` was last sent for.
    last_opened: Option<ChannelId>,
    settings: SettingsBatch,
    /// Answers of settings writes (a started request reports here; `tick` reads it).
    done_tx: Sender<Done>,
    done_rx: Receiver<Done>,
    /// The guild and channel the settings window shows.
    settings_view: Option<(Option<GuildId>, Option<ChannelId>)>,
}

/// How a settings write ended.
struct Done {
    guild: Option<GuildId>,
    /// The settings the server answered with, or the reason it failed.
    result: Result<serde_json::Value, String>,
}

/// The guild and `version` of a settings update (an echo, or a write's answer).
fn settings_key(data: &serde_json::Value) -> Option<(Option<GuildId>, Option<i64>)> {
    let guild = match data.get("guild_id") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(s)) => Some(GuildId::parse(s)?),
        Some(serde_json::Value::Number(n)) => Some(GuildId(n.as_u64()?)),
        Some(_) => return None,
    };
    Some((
        guild,
        data.get("version").and_then(serde_json::Value::as_i64),
    ))
}

/// Whether the tracker needs this event. Typing, presences, reactions, member lists and
/// the like never change what it shows, so they are not even applied.
fn relevant(d: &p::Dispatch, me: Option<UserId>) -> bool {
    use p::Dispatch::*;
    match d {
        Ready(_)
        | GuildCreate(_)
        | GuildSync(_)
        | GuildDelete(_)
        | GuildUpdate(_)
        | ChannelCreate(_)
        | ChannelUpdate(_)
        | ChannelDelete(_)
        | ChannelUpdateBulk(_)
        | MessageCreate(_)
        | MessageAck(_)
        | PassiveUpdates(_)
        | GuildRoleCreate(_)
        | GuildRoleUpdate(_)
        | GuildRoleUpdateBulk(_)
        | GuildRoleDelete(_) => true,
        // Only the account's own member matters (its roles decide role mentions).
        GuildMemberAdd(m) | GuildMemberUpdate(m) => {
            me.is_some() && m.user.as_ref().and_then(|u| UserId::parse(&u.id)) == me
        }
        Other { name, .. } => name == "USER_GUILD_SETTINGS_UPDATE",
        _ => false,
    }
}

/// Events after which channels or guilds can be gone, so every badge is checked.
fn removes_things(d: &p::Dispatch) -> bool {
    use p::Dispatch::*;
    matches!(
        d,
        GuildDelete(_) | ChannelDelete(_) | GuildSync(_) | ChannelUpdateBulk(_)
    )
}

fn diff<K: Hash + Eq + Copy>(shown: &mut HashMap<K, Badge>, key: K, new: Badge) -> bool {
    let old = shown.get(&key).copied().unwrap_or_default();
    if old == new {
        return false;
    }
    if new.is_default() {
        shown.remove(&key);
    } else {
        shown.insert(key, new);
    }
    true
}

fn channel_badge(state: &AccountState, ch: ChannelId) -> Badge {
    let u = state.unread(ch);
    let dimmed = state.channel(ch).is_some()
        && state
            .guild_settings()
            .channel_muted(state.channels().guild_of(ch), ch, state.now_ms());
    Badge {
        unread: u.unread,
        mentions: u.mentions,
        muted: u.muted || dimmed,
        dimmed,
    }
}

fn guild_badge(state: &AccountState, g: Option<GuildId>) -> Badge {
    let u = match g {
        Some(g) => state.guild_unread(g),
        None => state.private_unread(),
    };
    Badge {
        unread: u.unread,
        mentions: u.mentions,
        muted: u.muted,
        dimmed: false,
    }
}

/// Every channel and category the account has, guild and private.
fn all_channels(state: &AccountState) -> Vec<ChannelId> {
    let mut out = Vec::new();
    for g in state.guilds() {
        out.extend(
            state
                .guild_channels(g.id)
                .iter()
                .filter_map(|c| ChannelId::parse(&c.id)),
        );
    }
    out.extend(
        state
            .private_channels()
            .iter()
            .filter_map(|c| ChannelId::parse(&c.id)),
    );
    out
}

/// What there is to read in a channel: unread messages or unread mentions.
fn has_unread(state: &AccountState, ch: ChannelId) -> bool {
    let u = state.unread(ch);
    u.unread || u.mentions > 0
}

/// The channels of a guild (or the direct messages) with something unread, each with its
/// newest message.
fn unread_targets(state: &AccountState, guild: Option<GuildId>) -> Vec<(ChannelId, MessageId)> {
    let channels: Vec<ChannelId> = match guild {
        Some(g) => state
            .visible_channels(g)
            .iter()
            .filter_map(|c| ChannelId::parse(&c.id))
            .collect(),
        None => state
            .private_channels()
            .iter()
            .filter_map(|c| ChannelId::parse(&c.id))
            .collect(),
    };
    channels
        .into_iter()
        .filter(|ch| has_unread(state, *ch))
        .filter_map(|ch| Some((ch, state.read_state(ch)?.last_message_id?)))
        .collect()
}

impl ReadTracker {
    pub fn new(client: p::Client, events: Ui) -> Self {
        let (done_tx, done_rx) = channel();
        ReadTracker {
            done_tx,
            done_rx,
            client,
            events,
            state: None,
            gate: AckGate::default(),
            shown_channels: HashMap::new(),
            shown_guilds: HashMap::new(),
            watching: None,
            last_opened: None,
            settings: SettingsBatch::default(),
            settings_view: None,
        }
    }

    /// A gateway event.
    pub fn on_dispatch(&mut self, d: &p::Dispatch, now: Now) {
        let me = self.state.as_ref().map(|s| s.me_id());
        if !relevant(d, me) {
            return;
        }
        // A settings update older than one already seen (a late echo of an earlier write).
        let settings_update = match d {
            p::Dispatch::Other { name, data } if name == "USER_GUILD_SETTINGS_UPDATE" => {
                settings_key(data)
            }
            _ => None,
        };
        if let Some((guild, version)) = settings_update {
            if self.settings.is_stale(guild, version) {
                return;
            }
        }
        let mut changes = if let Some(state) = self.state.as_mut() {
            state.apply(d)
        } else if let p::Dispatch::Ready(r) = d {
            self.state = Some(AccountState::from_ready_at(r, now.ms));
            let mut c = Changes::default();
            c.reset = true;
            c
        } else {
            return;
        };
        // What the server holds is now the confirmed settings; what is asked for and not
        // confirmed yet goes back on top of it.
        if let Some((guild, version)) = settings_update {
            self.confirm_settings(guild, version, &mut changes);
        } else if matches!(d, p::Dispatch::Ready(_)) {
            self.reconfirm_settings(&mut changes);
        }
        // A message in the channel being read is read at once.
        if let p::Dispatch::MessageCreate(m) = d {
            if let (Some(ch), Some(id)) = (ChannelId::parse(&m.channel_id), MessageId::parse(&m.id))
            {
                if self.watching == Some(ch) {
                    changes.merge(self.viewed(ch, Some(id), now));
                }
            }
        }
        self.refresh(&changes, removes_things(d));
        if changes.settings || changes.reset {
            self.push_settings_view();
        }
    }

    /// About once a second: the mute clock, due acknowledgements, due settings writes.
    pub fn tick(&mut self, now: Now) {
        // Answers of settings writes first, so a write that waited for one can start.
        while let Ok(done) = self.done_rx.try_recv() {
            self.settings_done(done, now);
        }
        if let Some(state) = self.state.as_mut() {
            let changes = state.tick(now.ms);
            self.refresh(&changes, false);
            if changes.settings {
                self.push_settings_view();
            }
        }
        for (ch, message) in self.gate.due(now.at) {
            self.client.ack(&ch.to_string(), &message.to_string());
        }
        for (guild, body) in self.settings.start_due(now.at) {
            self.send_settings(guild, body);
        }
    }

    /// The channel being read changed.
    pub fn watch(&mut self, channel: Option<ChannelId>, now: Now) {
        self.watching = channel;
        let Some(ch) = channel else { return };
        let Some(state) = &self.state else { return };
        if self.last_opened != Some(ch) {
            self.last_opened = Some(ch);
            if let Some(entry) = state.read_state(ch) {
                self.emit(Ev::Opened {
                    channel_id: ch.to_string(),
                    last_read: entry.ack_message_id.map(|m| m.to_string()),
                    mentions: entry.mention_count,
                });
            }
        }
        let changes = self.viewed(ch, None, now);
        self.refresh(&changes, false);
    }

    /// The watched channel is read up to `message` (or its newest message). Only a
    /// channel with something unread is noted; the acknowledgement goes out through the
    /// gate on a later tick.
    fn viewed(&mut self, ch: ChannelId, message: Option<MessageId>, now: Now) -> Changes {
        let Some(state) = self.state.as_mut() else {
            return Changes::default();
        };
        if !has_unread(state, ch) {
            return Changes::default();
        }
        let Some(target) = message.or_else(|| state.read_state(ch).and_then(|e| e.last_message_id))
        else {
            return Changes::default();
        };
        let changes = state.mark_read(ch, Some(target), AckKind::Automatic);
        // Nothing changed when a "mark unread" holds: no acknowledgement either.
        if changes.read_state {
            self.gate.note(ch, target, now.at);
        }
        changes
    }

    /// "Mark as read" for a channel.
    pub fn mark_channel_read(&mut self, ch: ChannelId, now: Now) {
        let Some(state) = &self.state else { return };
        if !has_unread(state, ch) {
            return;
        }
        let Some(last) = state.read_state(ch).and_then(|e| e.last_message_id) else {
            return;
        };
        self.ack_many(&[(ch, last)], now);
    }

    /// "Mark as read" for a guild's channels (`None`: the direct messages).
    pub fn mark_guild_read(&mut self, guild: Option<GuildId>, now: Now) {
        let Some(state) = &self.state else { return };
        let targets = unread_targets(state, guild);
        self.ack_many(&targets, now);
    }

    /// "Mark as read" for everything. One `ack` per unread channel; the protocol sends
    /// them as batches of up to 100, one batch per second.
    pub fn mark_all_read(&mut self, now: Now) {
        let Some(state) = &self.state else { return };
        let mut targets: Vec<(ChannelId, MessageId)> = state
            .guilds()
            .flat_map(|g| unread_targets(state, Some(g.id)))
            .collect();
        targets.extend(unread_targets(state, None));
        self.ack_many(&targets, now);
    }

    fn ack_many(&mut self, targets: &[(ChannelId, MessageId)], now: Now) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        let mut changes = Changes::default();
        for (ch, message) in targets {
            changes.merge(state.mark_read(*ch, Some(*message), AckKind::Explicit));
            self.gate.clear(*ch, now.at);
            self.client.ack(&ch.to_string(), &message.to_string());
        }
        self.refresh(&changes, false);
    }

    /// A settings change from the UI: applied here at once, written later.
    pub fn change(&mut self, target: Target, change: Change, now: Now) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        let (guild_id, channel_id) = match &target {
            Target::Guild(g) => (g.as_deref(), None),
            Target::Channel {
                guild_id,
                channel_id,
            } => (guild_id.as_deref(), Some(channel_id.as_str())),
        };
        let guild = match guild_id {
            Some(g) => match GuildId::parse(g) {
                Some(g) if state.guild(g).is_some() => Some(g),
                _ => return,
            },
            None => None,
        };
        let channel = match channel_id {
            Some(c) => match ChannelId::parse(c) {
                // Only a channel the account has, in the guild it was asked for.
                Some(c) if state.channel(c).is_some() && state.channels().guild_of(c) == guild => {
                    Some(c)
                }
                _ => return,
            },
            None => None,
        };
        let Some(intent) = settings::build_intent(guild, channel, change, now.ms) else {
            return;
        };
        // A channel change replaces the server's whole override map, so it is only safe
        // when the local map is that whole map (see `GuildSettings::can_write_overrides`).
        if channel.is_some() && !state.guild_settings().can_write_overrides(guild) {
            self.emit_error(t!("error-notification-too-many"));
            return;
        }
        self.settings
            .add(guild, state.guild_settings().get(guild), intent, now.at);
        let changes = self.reapply_settings(guild);
        self.refresh(&changes, false);
        self.push_settings_view();
    }

    /// Puts the unconfirmed changes of a guild on top of the confirmed settings in the
    /// state, so the sidebar shows what was asked for.
    fn reapply_settings(&mut self, guild: Option<GuildId>) -> Changes {
        let (Some(state), Some(data)) = (self.state.as_mut(), self.settings.effective(guild))
        else {
            return Changes::default();
        };
        state.apply(&p::Dispatch::Other {
            name: "USER_GUILD_SETTINGS_UPDATE".into(),
            data,
        })
    }

    /// The server's word on a guild's settings was just applied to the state (an echo or
    /// the answer of a write): remember it as the confirmed settings, and put what is
    /// still unconfirmed back on top.
    fn confirm_settings(
        &mut self,
        guild: Option<GuildId>,
        version: Option<i64>,
        changes: &mut Changes,
    ) {
        let Some(state) = &self.state else { return };
        self.settings
            .confirm(guild, state.guild_settings().get(guild), version);
        if self.settings.is_busy(guild) {
            changes.merge(self.reapply_settings(guild));
        }
    }

    /// READY came: the server's settings replace the confirmed ones.
    fn reconfirm_settings(&mut self, changes: &mut Changes) {
        let Some(state) = &self.state else { return };
        let busy = self
            .settings
            .reset(|g| state.guild_settings().get(g).clone());
        for guild in busy {
            changes.merge(self.reapply_settings(guild));
        }
    }

    /// A settings write ended: on success the server's answer counts as its echo; on
    /// failure the change is dropped, which shows the confirmed settings again, and the
    /// error is shown.
    fn settings_done(&mut self, done: Done, now: Now) {
        let guild = done.guild;
        // What the server holds if it took the write: the confirmed settings with the
        // write on top. Its own answer, when it gives one, wins field by field.
        let assumed = self.settings.finish(guild, done.result.is_ok());
        match done.result {
            Ok(answer) => {
                let mut obj = match assumed {
                    Some(serde_json::Value::Object(o)) => o,
                    _ => serde_json::Map::new(),
                };
                if let serde_json::Value::Object(a) = answer {
                    obj.extend(a);
                }
                if !obj.is_empty() {
                    obj.insert(
                        "guild_id".into(),
                        guild.map_or(serde_json::Value::Null, |g| g.to_string().into()),
                    );
                    let update = p::Dispatch::Other {
                        name: "USER_GUILD_SETTINGS_UPDATE".into(),
                        data: serde_json::Value::Object(obj),
                    };
                    self.on_dispatch(&update, now);
                }
            }
            Err(reason) => {
                self.emit_error(t!("error-notification-settings", error = reason));
                let changes = self.reapply_settings_back(guild);
                self.refresh(&changes, false);
                self.push_settings_view();
            }
        }
    }

    /// After a failed write: the confirmed settings, with what is still waiting on top.
    fn reapply_settings_back(&mut self, guild: Option<GuildId>) -> Changes {
        // The state holds the failed change; the confirmed settings replace it, then the
        // changes made since go back on top.
        let (Some(state), Some(confirmed)) = (self.state.as_mut(), self.settings.confirmed(guild))
        else {
            return Changes::default();
        };
        let mut changes = state.apply(&p::Dispatch::Other {
            name: "USER_GUILD_SETTINGS_UPDATE".into(),
            data: settings::compose(&confirmed, guild, &[]),
        });
        changes.merge(self.reapply_settings(guild));
        changes
    }

    fn emit_error(&self, text: String) {
        let _ = self.events.unbounded_send(accounts::Ev::Error(text));
    }

    pub fn show_settings(&mut self, guild: Option<GuildId>, channel: Option<ChannelId>) {
        self.settings_view = Some((guild, channel));
        self.push_settings_view();
    }

    pub fn hide_settings(&mut self) {
        self.settings_view = None;
    }

    fn push_settings_view(&mut self) {
        let (Some(state), Some((g, c))) = (&self.state, self.settings_view) else {
            return;
        };
        let view = settings::view(state, g, c);
        self.emit(Ev::Settings(view));
    }

    /// Starts one settings request. Its answer is sent to `tick`, not to the UI, so a
    /// late answer never reaches another session and the next write waits for it.
    fn send_settings(&self, guild: Option<GuildId>, body: serde_json::Value) {
        let client = self.client.clone();
        let done = self.done_tx.clone();
        tokio::spawn(async move {
            let result = client
                .request_json(p::Method::PATCH, &settings::route(guild), Some(&body))
                .await
                .map_err(|err| Failure::from(err).message);
            let _ = done.send(Done { guild, result });
        });
    }

    /// Tells the UI which badges changed. `all` checks every channel and guild instead
    /// of the ones the changes name.
    fn refresh(&mut self, c: &Changes, all: bool) {
        if !(c.reset || c.read_state || all) {
            return;
        }
        let Some(state) = &self.state else { return };
        let mut update = BadgeUpdate {
            reset: c.reset,
            ..Default::default()
        };
        if c.reset {
            self.shown_channels.clear();
            self.shown_guilds.clear();
        }
        let every = c.reset || all || c.read_state_channels.is_all();
        let channels: Vec<ChannelId> = if every {
            let mut ids = all_channels(state);
            // Channels that are gone: their badge goes too.
            ids.extend(self.shown_channels.keys().copied());
            ids.sort();
            ids.dedup();
            ids
        } else {
            c.read_state_channels.iter().copied().collect()
        };
        for ch in channels {
            let badge = channel_badge(state, ch);
            if diff(&mut self.shown_channels, ch, badge) {
                update.channels.push((ch.to_string(), badge));
            }
        }
        let guilds: Vec<Option<GuildId>> = if c.reset || all || c.read_state_guilds.is_all() {
            let mut keys: Vec<Option<GuildId>> = state.guilds().map(|g| Some(g.id)).collect();
            keys.push(None);
            keys.extend(self.shown_guilds.keys().copied());
            keys.sort();
            keys.dedup();
            keys
        } else {
            c.read_state_guilds.iter().copied().collect()
        };
        for g in guilds {
            let badge = guild_badge(state, g);
            if diff(&mut self.shown_guilds, g, badge) {
                update.guilds.push((g.map(|g| g.to_string()), badge));
            }
        }
        if !update.is_empty() {
            self.emit(Ev::Badges(update));
        }
    }

    fn emit(&self, ev: Ev) {
        let _ = self.events.unbounded_send(ev);
    }
}

#[cfg(test)]
mod tests;
