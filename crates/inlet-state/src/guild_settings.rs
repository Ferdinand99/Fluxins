//! Per-guild notification settings: mutes, notification levels and mention
//! suppression (READY `user_guild_settings`, USER_GUILD_SETTINGS_UPDATE).
//!
//! The protocol crate passes these on raw, so they are parsed here, as
//! leniently as Fluxer's client reads them (`UserGuildSettings.ts`): missing
//! fields take Fluxer's defaults, `channel_overrides` may be a list or a map,
//! and an update only replaces the fields it carries. The entry with
//! `guild_id: null` holds the settings of direct messages.

use std::collections::BTreeMap;

use inlet_protocol::models::parse_timestamp_ms;
use serde_json::Value;

use crate::ids::{ChannelId, GuildId};
use crate::limits::{MAX_CHANNEL_OVERRIDES, MAX_GUILD_SETTINGS};

/// Notification levels (`MessageNotifications` in Fluxer).
pub mod notifications {
    /// Unset.
    pub const NULL: i64 = -1;
    /// Every message.
    pub const ALL_MESSAGES: i64 = 0;
    /// Only mentions.
    pub const ONLY_MENTIONS: i64 = 1;
    /// Nothing.
    pub const NO_MESSAGES: i64 = 2;
    /// Follow the parent (category or guild).
    pub const INHERIT: i64 = 3;
}

/// A mute: on, and until when (`None` = until turned off).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mute {
    /// `muted` in the payload.
    pub muted: bool,
    /// `mute_config.end_time` in Unix milliseconds.
    pub end_ms: Option<i64>,
}

impl Mute {
    /// Whether the mute is in force at `now_ms` (Fluxer's `isMuteActive`).
    pub fn active(&self, now_ms: i64) -> bool {
        self.muted && self.end_ms.is_none_or(|end| end > now_ms)
    }

    fn read(obj: &serde_json::Map<String, Value>, old: Mute) -> Mute {
        let muted = match obj.get("muted") {
            Some(v) => v.as_bool().unwrap_or(false),
            None => return old,
        };
        // An end time that does not parse means "until turned off", as in Fluxer.
        let end_ms = obj
            .get("mute_config")
            .and_then(|c| c.get("end_time"))
            .and_then(Value::as_str)
            .and_then(parse_timestamp_ms);
        Mute { muted, end_ms }
    }
}

/// One channel's override.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelOverride {
    /// The channel's own mute.
    pub mute: Mute,
    /// Notification level, [`notifications::INHERIT`] when not set.
    pub message_notifications: i64,
}

impl Default for ChannelOverride {
    fn default() -> Self {
        ChannelOverride {
            mute: Mute::default(),
            message_notifications: notifications::INHERIT,
        }
    }
}

/// One guild's settings (or the direct messages' settings).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuildNotificationSettings {
    /// `@everyone` and `@here` do not count as mentions.
    pub suppress_everyone: bool,
    /// Role mentions do not count as mentions.
    pub suppress_roles: bool,
    /// The whole guild is muted.
    pub mute: Mute,
    /// Notification level for the guild.
    pub message_notifications: i64,
    /// Channel and category overrides.
    pub channel_overrides: BTreeMap<ChannelId, ChannelOverride>,
}

impl Default for GuildNotificationSettings {
    fn default() -> Self {
        GuildNotificationSettings {
            suppress_everyone: false,
            suppress_roles: false,
            mute: Mute::default(),
            message_notifications: notifications::ALL_MESSAGES,
            channel_overrides: BTreeMap::new(),
        }
    }
}

impl GuildNotificationSettings {
    /// Applies a (full or partial) settings object on top of `self`.
    fn merge(&mut self, obj: &serde_json::Map<String, Value>) {
        if let Some(v) = obj.get("suppress_everyone") {
            self.suppress_everyone = v.as_bool().unwrap_or(false);
        }
        if let Some(v) = obj.get("suppress_roles") {
            self.suppress_roles = v.as_bool().unwrap_or(false);
        }
        self.mute = Mute::read(obj, self.mute);
        if let Some(v) = obj.get("message_notifications") {
            self.message_notifications = v.as_i64().unwrap_or(notifications::ALL_MESSAGES);
        }
        if let Some(v) = obj.get("channel_overrides") {
            self.channel_overrides = read_overrides(v);
        }
    }
}

fn read_overrides(v: &Value) -> BTreeMap<ChannelId, ChannelOverride> {
    let mut out = BTreeMap::new();
    let mut add = |key: Option<&str>, item: &Value| {
        if out.len() >= MAX_CHANNEL_OVERRIDES {
            return;
        }
        let Some(obj) = item.as_object() else { return };
        let id = obj
            .get("channel_id")
            .and_then(Value::as_str)
            .or(key)
            .and_then(ChannelId::parse);
        let Some(id) = id else { return };
        let mut o = ChannelOverride {
            mute: Mute::read(obj, Mute::default()),
            ..Default::default()
        };
        if let Some(level) = obj.get("message_notifications").and_then(Value::as_i64) {
            o.message_notifications = level;
        }
        out.insert(id, o);
    };
    match v {
        Value::Array(items) => items.iter().for_each(|i| add(None, i)),
        Value::Object(map) => map.iter().for_each(|(k, i)| add(Some(k), i)),
        _ => {}
    }
    out
}

/// Every guild's settings; the key `None` is direct messages.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GuildSettings {
    by_guild: BTreeMap<Option<GuildId>, GuildNotificationSettings>,
}

static DEFAULT_SETTINGS: std::sync::LazyLock<GuildNotificationSettings> =
    std::sync::LazyLock::new(GuildNotificationSettings::default);

impl GuildSettings {
    /// Reads READY's `user_guild_settings`.
    pub fn from_ready(items: &[Value]) -> Self {
        let mut s = GuildSettings::default();
        for item in items {
            s.update(item);
        }
        s
    }

    /// Applies one settings object (READY entry or USER_GUILD_SETTINGS_UPDATE).
    /// Returns the guild it was for (`Some(None)` for direct messages), or
    /// `None` when the payload was not usable.
    pub fn update(&mut self, item: &Value) -> Option<Option<GuildId>> {
        let obj = item.as_object()?;
        let guild = match obj.get("guild_id") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(GuildId::parse(s)?),
            Some(Value::Number(n)) => Some(GuildId(n.as_u64()?)),
            Some(_) => return None,
        };
        if !self.by_guild.contains_key(&guild) && self.by_guild.len() >= MAX_GUILD_SETTINGS {
            return None;
        }
        self.by_guild.entry(guild).or_default().merge(obj);
        Some(guild)
    }

    /// The settings of a guild (`None` = direct messages), or Fluxer's defaults.
    pub fn get(&self, guild: Option<GuildId>) -> &GuildNotificationSettings {
        self.by_guild.get(&guild).unwrap_or(&DEFAULT_SETTINGS)
    }

    /// Whether the whole guild is muted.
    pub fn guild_muted(&self, guild: Option<GuildId>, now_ms: i64) -> bool {
        self.get(guild).mute.active(now_ms)
    }

    /// Whether the channel itself is muted.
    pub fn channel_muted(&self, guild: Option<GuildId>, channel: ChannelId, now_ms: i64) -> bool {
        self.get(guild)
            .channel_overrides
            .get(&channel)
            .is_some_and(|o| o.mute.active(now_ms))
    }

    /// Fluxer's `isGuildOrChannelMuted`.
    pub fn guild_or_channel_muted(
        &self,
        guild: Option<GuildId>,
        channel: ChannelId,
        now_ms: i64,
    ) -> bool {
        self.guild_muted(guild, now_ms) || self.channel_muted(guild, channel, now_ms)
    }

    /// Fluxer's `isMutedAtAnyLevel`: guild, parent category or channel.
    pub fn muted_at_any_level(
        &self,
        guild: Option<GuildId>,
        channel: ChannelId,
        parent: Option<ChannelId>,
        now_ms: i64,
    ) -> bool {
        self.guild_or_channel_muted(guild, channel, now_ms)
            || (guild.is_some() && parent.is_some_and(|p| self.channel_muted(guild, p, now_ms)))
    }

    /// The guild's notification level, with INHERIT and NULL resolved to the
    /// guild's default (`default_message_notifications`).
    pub fn guild_notifications(&self, guild: Option<GuildId>, guild_default: i64) -> i64 {
        let level = self.get(guild).message_notifications;
        if level == notifications::INHERIT || level == notifications::NULL {
            if guild.is_none() {
                notifications::ALL_MESSAGES
            } else {
                guild_default
            }
        } else {
            level
        }
    }

    /// Whether a mute starts or ends in `(from_ms, to_ms]`.
    pub(crate) fn mute_boundary_between(&self, from_ms: i64, to_ms: i64) -> bool {
        let crosses = |m: &Mute| m.muted && m.end_ms.is_some_and(|e| e > from_ms && e <= to_ms);
        self.by_guild
            .values()
            .any(|g| crosses(&g.mute) || g.channel_overrides.values().any(|o| crosses(&o.mute)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_lists_maps_partial_updates_and_mute_ends() {
        let mut s = GuildSettings::from_ready(&[
            json!({"guild_id": "1", "muted": true, "mute_config": {"end_time": "1970-01-01T00:00:10Z"},
                   "suppress_everyone": true,
                   "channel_overrides": [{"channel_id": "5", "muted": true, "message_notifications": 2}]}),
            json!({"guild_id": null, "channel_overrides": {"9": {"muted": true}}}),
            json!({"guild_id": "bad"}),
            json!(7),
        ]);
        let g = Some(GuildId(1));
        assert!(s.guild_muted(g, 9_999));
        assert!(!s.guild_muted(g, 10_000));
        assert!(s.channel_muted(g, ChannelId(5), 0));
        assert!(s.get(g).suppress_everyone);
        assert!(s.channel_muted(None, ChannelId(9), 0));
        assert!(s.muted_at_any_level(g, ChannelId(6), Some(ChannelId(5)), 20_000));
        assert!(s.mute_boundary_between(0, 10_000));
        assert!(!s.mute_boundary_between(10_000, 20_000));

        // A partial update keeps the other fields.
        s.update(&json!({"guild_id": "1", "muted": false}));
        assert!(!s.guild_muted(g, 0));
        assert!(s.get(g).suppress_everyone);
        assert!(s.channel_muted(g, ChannelId(5), 0));
        assert_eq!(s.guild_notifications(g, 1), 0);
        s.update(&json!({"guild_id": "1", "message_notifications": 3}));
        assert_eq!(s.guild_notifications(g, 1), 1);
        assert_eq!(
            s.guild_notifications(Some(GuildId(2)), 1),
            notifications::ALL_MESSAGES
        );
    }
}
