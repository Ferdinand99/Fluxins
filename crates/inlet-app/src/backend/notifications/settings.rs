// SPDX-License-Identifier: AGPL-3.0-or-later
//! Writing notification settings (mutes, levels, suppressions) and reading them back
//! for the settings window.
//!
//! Every change is applied to the local state at once, so the sidebar answers the
//! click, and queued as a partial settings object. Changes to one guild's settings
//! within [`QUIET`] merge into one `PATCH`, and at most [`MAX_PER_TICK`] go out per
//! tick, so clicking through every option cannot get near Fluxer's limit (30 per 10 s).
//! The channel override map is sent whole, as the server replaces it, built from the
//! settings the account already has (see `GuildNotificationSettings::overrides_with`).

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use inlet_state::guild_settings::{notifications, GuildSettings, Mute};
use inlet_state::{AccountState, ChannelId, GuildId};
use serde_json::{json, Map, Value};

/// Changes to one guild's settings this close together become one request.
pub const QUIET: Duration = Duration::from_millis(500);
/// Settings requests started per tick (a tick is about one second).
pub const MAX_PER_TICK: usize = 2;
/// The longest timed mute accepted.
const MAX_MUTE_MS: i64 = 366 * 24 * 3_600_000;

/// What a change is about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// A guild, or the direct messages (`None`).
    Guild(Option<String>),
    /// A channel or category; `guild_id` is `None` for a direct message.
    Channel {
        guild_id: Option<String>,
        channel_id: String,
    },
}

/// What to change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    /// Mute for this many milliseconds, or until turned off (`None`).
    Mute {
        for_ms: Option<i64>,
    },
    Unmute,
    /// A notification level: 0 all messages, 1 only mentions, 2 nothing, 3 inherit.
    Level(i64),
    /// Guilds only.
    SuppressEveryone(bool),
    /// Guilds only.
    SuppressRoles(bool),
}

/// The partial settings object a change writes, or `None` when it makes no sense (a bad
/// level, a suppression on a channel, a mute that is zero or absurdly long, a channel
/// ID that is not one). `now_ms` is the time a timed mute counts from.
pub fn build_patch(
    settings: &GuildSettings,
    guild: Option<GuildId>,
    channel: Option<ChannelId>,
    change: Change,
    now_ms: i64,
) -> Option<Map<String, Value>> {
    let mute = |for_ms: Option<i64>| -> Option<Mute> {
        if let Some(ms) = for_ms {
            if ms <= 0 || ms > MAX_MUTE_MS {
                return None;
            }
        }
        Some(Mute {
            muted: true,
            end_ms: for_ms.map(|ms| now_ms.saturating_add(ms)),
            window_ms: for_ms.unwrap_or(0),
        })
    };
    let level_ok = |l: i64| (notifications::ALL_MESSAGES..=notifications::INHERIT).contains(&l);
    let mut patch = Map::new();
    match (channel, change) {
        (None, Change::Mute { for_ms }) => {
            let m = mute(for_ms)?;
            patch.insert("muted".into(), json!(true));
            patch.insert("mute_config".into(), m.config_json());
        }
        (None, Change::Unmute) => {
            patch.insert("muted".into(), json!(false));
            patch.insert("mute_config".into(), Value::Null);
        }
        (None, Change::Level(l)) if level_ok(l) => {
            patch.insert("message_notifications".into(), json!(l));
        }
        (None, Change::SuppressEveryone(on)) if guild.is_some() => {
            patch.insert("suppress_everyone".into(), json!(on));
        }
        (None, Change::SuppressRoles(on)) if guild.is_some() => {
            patch.insert("suppress_roles".into(), json!(on));
        }
        (Some(ch), Change::Mute { for_ms }) => {
            let m = mute(for_ms)?;
            patch.insert(
                "channel_overrides".into(),
                settings.get(guild).overrides_with(ch, |o| o.mute = m),
            );
        }
        (Some(ch), Change::Unmute) => {
            patch.insert(
                "channel_overrides".into(),
                settings
                    .get(guild)
                    .overrides_with(ch, |o| o.mute = Mute::default()),
            );
        }
        (Some(ch), Change::Level(l)) if level_ok(l) => {
            patch.insert(
                "channel_overrides".into(),
                settings
                    .get(guild)
                    .overrides_with(ch, |o| o.message_notifications = l),
            );
        }
        _ => return None,
    }
    Some(patch)
}

/// The route of a guild's (or the direct messages') settings.
pub fn route(guild: Option<GuildId>) -> String {
    match guild {
        Some(g) => format!("/users/@me/guilds/{g}/settings"),
        None => "/users/@me/guilds/@me/settings".to_string(),
    }
}

struct Entry {
    patch: Map<String, Value>,
    first_at: Instant,
}

/// Settings changes waiting to be sent, merged per guild.
#[derive(Default)]
pub struct SettingsBatch {
    pending: BTreeMap<Option<GuildId>, Entry>,
}

impl SettingsBatch {
    /// Queues `patch` for a guild. Fields of an earlier patch that this one repeats are
    /// replaced (a channel override map already contains the earlier changes).
    pub fn add(&mut self, guild: Option<GuildId>, patch: Map<String, Value>, now: Instant) {
        match self.pending.get_mut(&guild) {
            Some(e) => e.patch.extend(patch),
            None => {
                self.pending.insert(
                    guild,
                    Entry {
                        patch,
                        first_at: now,
                    },
                );
            }
        }
    }

    /// The requests to start now: guilds whose first change is at least [`QUIET`] old,
    /// the oldest first, at most [`MAX_PER_TICK`].
    pub fn due(&mut self, now: Instant) -> Vec<(Option<GuildId>, Value)> {
        let mut ready: Vec<(Option<GuildId>, Instant)> = self
            .pending
            .iter()
            .filter(|(_, e)| now.saturating_duration_since(e.first_at) >= QUIET)
            .map(|(g, e)| (*g, e.first_at))
            .collect();
        ready.sort_by_key(|(_, at)| *at);
        ready.truncate(MAX_PER_TICK);
        ready
            .into_iter()
            .filter_map(|(g, _)| self.pending.remove(&g).map(|e| (g, Value::Object(e.patch))))
            .collect()
    }

    #[cfg(test)]
    pub fn is_idle(&self) -> bool {
        self.pending.is_empty()
    }
}

/// What the settings window shows for a guild and one of its channels.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SettingsView {
    pub guild_id: Option<String>,
    pub channel_id: Option<String>,
    pub guild_muted: bool,
    /// When a timed guild mute ends (Unix milliseconds); `None` for "until turned off".
    pub guild_mute_until_ms: Option<i64>,
    /// The guild's level as stored: 0, 1, 2 or 3 (use the default).
    pub guild_level: i64,
    /// The level the guild really has: its own, or the default of the guild.
    pub guild_effective_level: i64,
    pub suppress_everyone: bool,
    pub suppress_roles: bool,
    pub channel_muted: bool,
    pub channel_mute_until_ms: Option<i64>,
    /// The channel's level as stored: 0, 1, 2 or 3 (use the category's or guild's).
    pub channel_level: i64,
    /// The level the channel gets when it uses the default.
    pub channel_inherited_level: i64,
}

/// Reads the settings window's values from the state.
pub fn view(
    state: &AccountState,
    guild: Option<GuildId>,
    channel: Option<ChannelId>,
) -> SettingsView {
    let all = state.guild_settings();
    let now = state.now_ms();
    let gs = all.get(guild);
    let default = guild
        .and_then(|g| state.guild(g))
        .map_or(notifications::ALL_MESSAGES, |g| g.default_notifications());
    let effective = all.guild_notifications(guild, default);
    let stored = match gs.message_notifications {
        notifications::NULL => notifications::INHERIT,
        l => l,
    };
    let mut v = SettingsView {
        guild_id: guild.map(|g| g.to_string()),
        channel_id: channel.map(|c| c.to_string()),
        guild_muted: gs.mute.active(now),
        guild_mute_until_ms: gs.mute.end_ms.filter(|_| gs.mute.active(now)),
        guild_level: stored,
        guild_effective_level: effective,
        suppress_everyone: gs.suppress_everyone,
        suppress_roles: gs.suppress_roles,
        channel_level: notifications::INHERIT,
        channel_inherited_level: effective,
        ..Default::default()
    };
    if let Some(ch) = channel {
        let o = gs.channel_overrides.get(&ch);
        v.channel_muted = o.is_some_and(|o| o.mute.active(now));
        v.channel_mute_until_ms = o.filter(|o| o.mute.active(now)).and_then(|o| o.mute.end_ms);
        v.channel_level = o.map_or(notifications::INHERIT, |o| o.message_notifications);
        // A direct message has nothing above it: "default" means every message.
        // In a guild the category's own level comes before the guild's.
        if guild.is_none() {
            v.channel_inherited_level = notifications::ALL_MESSAGES;
        } else if let Some(cat) = state.channels().parent_of(ch) {
            if let Some(level) = gs
                .channel_overrides
                .get(&cat)
                .map(|o| o.message_notifications)
                .filter(|l| *l != notifications::INHERIT && *l != notifications::NULL)
            {
                v.channel_inherited_level = level;
            }
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(v: Value) -> GuildSettings {
        GuildSettings::from_ready(&[v])
    }

    #[test]
    fn changes_become_partial_objects() {
        let s = settings(json!({"guild_id": "1", "channel_overrides": {
            "5": {"collapsed": true, "message_notifications": 3, "muted": false, "mute_config": null}}}));
        let g = Some(GuildId(1));

        let p = build_patch(
            &s,
            g,
            None,
            Change::Mute {
                for_ms: Some(900_000),
            },
            1_000,
        )
        .unwrap();
        assert_eq!(p["muted"], json!(true));
        assert_eq!(
            p["mute_config"],
            json!({"end_time": 901_000, "selected_time_window": 900_000})
        );
        // Until turned off: muted, no config.
        let p = build_patch(&s, g, None, Change::Mute { for_ms: None }, 1_000).unwrap();
        assert_eq!(p["mute_config"], Value::Null);
        let p = build_patch(&s, g, None, Change::Unmute, 0).unwrap();
        assert_eq!(
            (p["muted"].clone(), p["mute_config"].clone()),
            (json!(false), Value::Null)
        );
        let p = build_patch(&s, g, None, Change::Level(1), 0).unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(p["message_notifications"], json!(1));
        let p = build_patch(&s, g, None, Change::SuppressEveryone(true), 0).unwrap();
        assert_eq!(p["suppress_everyone"], json!(true));

        // A channel change carries the other overrides whole.
        let p = build_patch(&s, g, Some(ChannelId(6)), Change::Level(2), 0).unwrap();
        assert_eq!(p["channel_overrides"]["5"]["collapsed"], json!(true));
        assert_eq!(
            p["channel_overrides"]["6"]["message_notifications"],
            json!(2)
        );

        // Nonsense is refused.
        assert!(build_patch(&s, g, None, Change::Level(9), 0).is_none());
        assert!(build_patch(&s, g, None, Change::Level(-1), 0).is_none());
        assert!(build_patch(&s, g, None, Change::Mute { for_ms: Some(0) }, 0).is_none());
        assert!(build_patch(
            &s,
            g,
            None,
            Change::Mute {
                for_ms: Some(i64::MAX)
            },
            0
        )
        .is_none());
        assert!(build_patch(&s, g, Some(ChannelId(6)), Change::SuppressRoles(true), 0).is_none());
        assert!(build_patch(&s, None, None, Change::SuppressRoles(true), 0).is_none());
    }

    #[test]
    fn a_burst_of_changes_is_one_request_per_guild() {
        let base = Instant::now();
        let mut b = SettingsBatch::default();
        for i in 0..50u64 {
            let mut p = Map::new();
            p.insert("message_notifications".into(), json!(i % 3));
            b.add(Some(GuildId(1)), p, base + Duration::from_millis(i));
        }
        let mut p = Map::new();
        p.insert("suppress_roles".into(), json!(true));
        b.add(Some(GuildId(1)), p, base + Duration::from_millis(60));
        assert!(b.due(base + Duration::from_millis(400)).is_empty());
        let due = b.due(base + Duration::from_millis(600));
        assert_eq!(due.len(), 1);
        assert_eq!(
            due[0].1,
            json!({"message_notifications": 1, "suppress_roles": true})
        );
        assert!(b.is_idle());

        // Many guilds at once: two per tick, the oldest first.
        for g in 0..5u64 {
            let mut p = Map::new();
            p.insert("muted".into(), json!(true));
            b.add(Some(GuildId(g)), p, base + Duration::from_millis(g));
        }
        let t = base + Duration::from_secs(5);
        let first: Vec<_> = b.due(t).into_iter().map(|(g, _)| g).collect();
        assert_eq!(first, vec![Some(GuildId(0)), Some(GuildId(1))]);
        assert_eq!(b.due(t).len(), 2);
        assert_eq!(b.due(t).len(), 1);
        assert!(b.is_idle());
    }

    #[test]
    fn routes_name_the_guild_or_the_direct_messages() {
        assert_eq!(route(Some(GuildId(7))), "/users/@me/guilds/7/settings");
        assert_eq!(route(None), "/users/@me/guilds/@me/settings");
    }
}
