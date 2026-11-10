// SPDX-License-Identifier: AGPL-3.0-or-later
//! Writing notification settings (mutes, levels, suppressions) and reading them back
//! for the settings window.
//!
//! A click becomes an [`Intent`]: what the person asked for, not a copy of the server's
//! settings. For each guild the intents that the server has not confirmed yet are kept
//! apart from the last settings the server confirmed, and what the UI shows is always
//! the confirmed settings with those intents on top. That makes the display independent
//! of the order echoes arrive in, and a failed write falls back to the confirmed
//! settings by dropping its intent.
//!
//! Writes: at most one `PATCH` per guild is in flight; changes made meanwhile wait, and
//! their body is built when they are sent, from the settings the server has confirmed by
//! then. Changes to one guild within [`QUIET`] merge into one request, and at most
//! [`MAX_PER_TICK`] start per tick, far below Fluxer's limit (30 per 10 s). The channel
//! override map is sent whole, as the server replaces it.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use inlet_state::guild_settings::{
    notifications, ChannelOverride, GuildNotificationSettings, Mute,
};
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

/// What the person asked for in one channel or category.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChannelIntent {
    pub mute: Option<Mute>,
    pub level: Option<i64>,
}

/// What the person asked for in one guild: settings fields of the guild itself (as a
/// partial settings object) and per-channel changes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Intent {
    pub fields: Map<String, Value>,
    pub channels: BTreeMap<ChannelId, ChannelIntent>,
}

impl Intent {
    /// Adds a later intent: its choices win where both set the same thing.
    fn merge(&mut self, later: Intent) {
        self.fields.extend(later.fields);
        for (ch, c) in later.channels {
            let e = self.channels.entry(ch).or_default();
            if c.mute.is_some() {
                e.mute = c.mute;
            }
            if c.level.is_some() {
                e.level = c.level;
            }
        }
    }
}

/// The intent of a change, or `None` when it makes no sense (a bad level, a suppression
/// on a channel, a mute that is zero or absurdly long). `now_ms` is the time a timed
/// mute counts from.
pub fn build_intent(
    guild: Option<GuildId>,
    channel: Option<ChannelId>,
    change: Change,
    now_ms: i64,
) -> Option<Intent> {
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
    let mut intent = Intent::default();
    match (channel, change) {
        (None, Change::Mute { for_ms }) => {
            let m = mute(for_ms)?;
            intent.fields.insert("muted".into(), json!(true));
            intent.fields.insert("mute_config".into(), m.config_json());
        }
        (None, Change::Unmute) => {
            intent.fields.insert("muted".into(), json!(false));
            intent.fields.insert("mute_config".into(), Value::Null);
        }
        (None, Change::Level(l)) if level_ok(l) => {
            intent
                .fields
                .insert("message_notifications".into(), json!(l));
        }
        (None, Change::SuppressEveryone(on)) if guild.is_some() => {
            intent.fields.insert("suppress_everyone".into(), json!(on));
        }
        (None, Change::SuppressRoles(on)) if guild.is_some() => {
            intent.fields.insert("suppress_roles".into(), json!(on));
        }
        (Some(ch), Change::Mute { for_ms }) => {
            let m = mute(for_ms)?;
            intent.channels.insert(
                ch,
                ChannelIntent {
                    mute: Some(m),
                    level: None,
                },
            );
        }
        (Some(ch), Change::Unmute) => {
            intent.channels.insert(
                ch,
                ChannelIntent {
                    mute: Some(Mute::default()),
                    level: None,
                },
            );
        }
        (Some(ch), Change::Level(l)) if level_ok(l) => {
            intent.channels.insert(
                ch,
                ChannelIntent {
                    mute: None,
                    level: Some(l),
                },
            );
        }
        _ => return None,
    }
    Some(intent)
}

/// The complete `channel_overrides` map: the settings' own, with the intents applied.
/// An override that ends up as the defaults is left out.
fn overrides_json(base: &GuildNotificationSettings, intents: &[&Intent]) -> Value {
    let mut all = base.channel_overrides.clone();
    for intent in intents {
        for (ch, c) in &intent.channels {
            let mut o = all.get(ch).copied().unwrap_or_default();
            if let Some(m) = c.mute {
                o.mute = m;
            }
            if let Some(l) = c.level {
                o.message_notifications = l;
            }
            if o == ChannelOverride::default() {
                all.remove(ch);
            } else {
                all.insert(*ch, o);
            }
        }
    }
    Value::Object(
        all.iter()
            .map(|(id, o)| (id.to_string(), o.input_json()))
            .collect(),
    )
}

/// The whole settings object of a guild with the intents applied, in the shape of a
/// `USER_GUILD_SETTINGS_UPDATE`: what the UI shows while the intents are unconfirmed.
pub fn compose(
    base: &GuildNotificationSettings,
    guild: Option<GuildId>,
    intents: &[&Intent],
) -> Value {
    let mut obj = Map::new();
    obj.insert(
        "guild_id".into(),
        guild.map_or(Value::Null, |g| g.to_string().into()),
    );
    obj.insert("muted".into(), json!(base.mute.muted));
    obj.insert("mute_config".into(), base.mute.config_json());
    obj.insert(
        "message_notifications".into(),
        json!(base.message_notifications),
    );
    obj.insert("suppress_everyone".into(), json!(base.suppress_everyone));
    obj.insert("suppress_roles".into(), json!(base.suppress_roles));
    for intent in intents {
        obj.extend(intent.fields.clone());
    }
    // A map that is known to be cut is left out: the state keeps its flag, and a later
    // read of the map would otherwise look whole.
    if !base.lossy {
        obj.insert("channel_overrides".into(), overrides_json(base, intents));
    }
    Value::Object(obj)
}

/// The body of the `PATCH` for an intent: its guild fields, and the whole override map
/// when it changes a channel (the server replaces the map; without a channel change the
/// map is left out and the server keeps its own).
fn body(base: &GuildNotificationSettings, intent: &Intent) -> Value {
    let mut obj = intent.fields.clone();
    if !intent.channels.is_empty() {
        obj.insert("channel_overrides".into(), overrides_json(base, &[intent]));
    }
    Value::Object(obj)
}

/// What `SettingsBatch::start_due` starts.
#[derive(Debug, Default, PartialEq)]
pub struct Started {
    /// Requests to send: the guild and the body.
    pub send: Vec<(Option<GuildId>, Value)>,
    /// Guilds whose channel changes were dropped because the local override map is cut.
    pub refused: Vec<Option<GuildId>>,
}

/// One guild's write state.
struct GuildWrite {
    /// The settings as the server last confirmed them (an echo, a write's answer, READY).
    confirmed: GuildNotificationSettings,
    /// The newest `version` seen; an update with a lower one is old news.
    version: Option<i64>,
    /// Sent, not answered yet. At most one request per guild is in flight.
    in_flight: Option<Intent>,
    /// Asked for since, not sent yet, with when the first of it was asked for.
    pending: Option<(Intent, Instant)>,
}

impl GuildWrite {
    fn has_intents(&self) -> bool {
        self.in_flight.is_some() || self.pending.is_some()
    }
}

/// Settings writes of every guild.
#[derive(Default)]
pub struct SettingsBatch {
    guilds: BTreeMap<Option<GuildId>, GuildWrite>,
}

impl SettingsBatch {
    /// Queues an intent. `current` is the guild's settings as the state has them now; it
    /// is taken as the confirmed settings the first time (nothing was unconfirmed then).
    pub fn add(
        &mut self,
        guild: Option<GuildId>,
        current: &GuildNotificationSettings,
        intent: Intent,
        now: Instant,
    ) {
        let w = self.guilds.entry(guild).or_insert_with(|| GuildWrite {
            confirmed: current.clone(),
            version: None,
            in_flight: None,
            pending: None,
        });
        match &mut w.pending {
            Some((p, _)) => p.merge(intent),
            None => w.pending = Some((intent, now)),
        }
    }

    /// The settings object the UI should show for a guild: the confirmed settings with
    /// the unconfirmed intents on top. `None` for a guild with no unconfirmed intent.
    pub fn effective(&self, guild: Option<GuildId>) -> Option<Value> {
        let w = self.guilds.get(&guild)?;
        if !w.has_intents() {
            return None;
        }
        let intents: Vec<&Intent> = w
            .in_flight
            .iter()
            .chain(w.pending.as_ref().map(|(p, _)| p))
            .collect();
        Some(compose(&w.confirmed, guild, &intents))
    }

    /// Whether an update with this `version` is older than one already seen.
    pub fn is_stale(&self, guild: Option<GuildId>, version: Option<i64>) -> bool {
        match (self.guilds.get(&guild).and_then(|w| w.version), version) {
            (Some(known), Some(v)) => v < known,
            _ => false,
        }
    }

    /// The settings the server last confirmed for a guild, if it has a write going on or had one.
    pub fn confirmed(&self, guild: Option<GuildId>) -> Option<GuildNotificationSettings> {
        self.guilds.get(&guild).map(|w| w.confirmed.clone())
    }

    /// Whether a guild has a write that is not confirmed yet.
    pub fn is_busy(&self, guild: Option<GuildId>) -> bool {
        self.guilds.get(&guild).is_some_and(GuildWrite::has_intents)
    }

    /// The server confirmed these settings (an echo or the answer to a write).
    pub fn confirm(
        &mut self,
        guild: Option<GuildId>,
        settings: &GuildNotificationSettings,
        version: Option<i64>,
    ) {
        if let Some(w) = self.guilds.get_mut(&guild) {
            w.confirmed = settings.clone();
            w.version = w.version.max(version);
        }
    }

    /// A new READY: what the server holds now is the confirmed settings of every guild
    /// that has a write going on, with the versions READY carries (an answer to a write
    /// that was out before READY and is older than what READY shows is then old news).
    /// Returns those guilds.
    pub fn reset(
        &mut self,
        settings: impl Fn(Option<GuildId>) -> GuildNotificationSettings,
        versions: &BTreeMap<Option<GuildId>, i64>,
    ) -> Vec<Option<GuildId>> {
        for (g, w) in &mut self.guilds {
            w.confirmed = settings(*g);
            w.version = versions.get(g).copied();
        }
        self.guilds
            .iter()
            .filter(|(_, w)| w.has_intents())
            .map(|(g, _)| *g)
            .collect()
    }

    /// The requests to start now: guilds with nothing in flight whose first waiting
    /// change is at least [`QUIET`] old, the oldest first, at most [`MAX_PER_TICK`].
    /// What they carry moves to "in flight"; the body is built here, from the settings
    /// the server has confirmed by now.
    pub fn start_due(&mut self, now: Instant) -> Started {
        let mut ready: Vec<(Option<GuildId>, Instant)> = self
            .guilds
            .iter()
            .filter(|(_, w)| w.in_flight.is_none())
            .filter_map(|(g, w)| {
                let (_, first) = w.pending.as_ref()?;
                (now.saturating_duration_since(*first) >= QUIET).then_some((*g, *first))
            })
            .collect();
        ready.sort_by_key(|(_, at)| *at);
        ready.truncate(MAX_PER_TICK);
        let mut started = Started::default();
        for (g, _) in ready {
            let Some(w) = self.guilds.get_mut(&g) else {
                continue;
            };
            let Some((mut intent, _)) = w.pending.take() else {
                continue;
            };
            // A channel change replaces the server's whole override map: with a local map
            // that is not the whole map it would delete what is missing. Dropped here too,
            // so a change that got in before the map was known to be cut is not sent.
            if w.confirmed.lossy && !intent.channels.is_empty() {
                intent.channels.clear();
                started.refused.push(g);
            }
            if intent == Intent::default() {
                continue;
            }
            let b = body(&w.confirmed, &intent);
            w.in_flight = Some(intent);
            started.send.push((g, b));
        }
        started
    }

    /// The write in flight for a guild ended. When the server took it (`ok`), returns
    /// the settings it holds then as far as the answer did not say (the confirmed
    /// settings with the intent applied).
    pub fn finish(&mut self, guild: Option<GuildId>, ok: bool) -> Option<Value> {
        let w = self.guilds.get_mut(&guild)?;
        let intent = w.in_flight.take()?;
        ok.then(|| compose(&w.confirmed, guild, &[&intent]))
    }

    /// Whether a write is in flight for a guild (a late answer for none is ignored).
    pub fn is_in_flight(&self, guild: Option<GuildId>) -> bool {
        self.guilds
            .get(&guild)
            .is_some_and(|w| w.in_flight.is_some())
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
    use inlet_state::guild_settings::GuildSettings;

    fn settings(v: Value) -> GuildNotificationSettings {
        GuildSettings::from_ready(&[v])
            .get(Some(GuildId(1)))
            .clone()
    }

    fn intent(g: Option<GuildId>, ch: Option<ChannelId>, c: Change) -> Intent {
        build_intent(g, ch, c, 1_000).unwrap()
    }

    #[test]
    fn changes_become_intents() {
        let g = Some(GuildId(1));
        let i = intent(
            g,
            None,
            Change::Mute {
                for_ms: Some(900_000),
            },
        );
        assert_eq!(i.fields["muted"], json!(true));
        assert_eq!(
            i.fields["mute_config"],
            json!({"end_time": 901_000, "selected_time_window": 900_000})
        );
        // Until turned off: muted, no config.
        let i = intent(g, None, Change::Mute { for_ms: None });
        assert_eq!(i.fields["mute_config"], Value::Null);
        let i = intent(g, None, Change::Unmute);
        assert_eq!(
            (i.fields["muted"].clone(), i.fields["mute_config"].clone()),
            (json!(false), Value::Null)
        );
        let i = intent(g, None, Change::Level(1));
        assert_eq!(i.fields.len(), 1);
        assert_eq!(i.fields["message_notifications"], json!(1));
        let i = intent(g, None, Change::SuppressEveryone(true));
        assert_eq!(i.fields["suppress_everyone"], json!(true));
        let i = intent(g, Some(ChannelId(6)), Change::Level(2));
        assert!(i.fields.is_empty());
        assert_eq!(i.channels[&ChannelId(6)].level, Some(2));

        // Nonsense is refused.
        let no = |g, ch, c| build_intent(g, ch, c, 0).is_none();
        assert!(no(g, None, Change::Level(9)));
        assert!(no(g, None, Change::Level(-1)));
        assert!(no(g, None, Change::Mute { for_ms: Some(0) }));
        assert!(no(
            g,
            None,
            Change::Mute {
                for_ms: Some(i64::MAX)
            }
        ));
        assert!(no(g, Some(ChannelId(6)), Change::SuppressRoles(true)));
        assert!(no(None, None, Change::SuppressRoles(true)));
    }

    #[test]
    fn what_the_ui_shows_is_the_confirmed_settings_with_the_intents_on_top() {
        let base = settings(json!({"guild_id": "1", "channel_overrides": {
            "5": {"collapsed": true, "message_notifications": 3, "muted": false}}}));
        let g = Some(GuildId(1));
        let mut b = SettingsBatch::default();
        let t = Instant::now();
        assert!(b.effective(g).is_none());
        b.add(g, &base, intent(g, Some(ChannelId(6)), Change::Level(2)), t);
        b.add(
            g,
            &base,
            intent(g, Some(ChannelId(7)), Change::Mute { for_ms: None }),
            t,
        );
        let v = b.effective(g).unwrap();
        // The other override travels whole, the two changes are both in it.
        assert_eq!(v["channel_overrides"]["5"]["collapsed"], json!(true));
        assert_eq!(
            v["channel_overrides"]["6"]["message_notifications"],
            json!(2)
        );
        assert_eq!(v["channel_overrides"]["7"]["muted"], json!(true));

        // The server confirms {5, 6}: channel 7 is still on top.
        let confirmed = settings(json!({"guild_id": "1", "channel_overrides": {
            "5": {"collapsed": true},
            "6": {"message_notifications": 2}}}));
        b.confirm(g, &confirmed, Some(4));
        let v = b.effective(g).unwrap();
        assert_eq!(
            v["channel_overrides"]["6"]["message_notifications"],
            json!(2)
        );
        assert_eq!(v["channel_overrides"]["7"]["muted"], json!(true));
    }

    #[test]
    fn two_writes_for_one_guild_are_never_in_flight_together() {
        let base = settings(json!({"guild_id": "1"}));
        let g = Some(GuildId(1));
        let t = Instant::now();
        let at = |ms: u64| t + Duration::from_millis(ms);
        let mut b = SettingsBatch::default();

        for i in 0..50i64 {
            b.add(
                g,
                &base,
                intent(g, None, Change::Level(i % 3)),
                at(i as u64),
            );
        }
        b.add(
            g,
            &base,
            intent(g, None, Change::SuppressRoles(true)),
            at(60),
        );
        assert!(b.start_due(at(400)).send.is_empty(), "merging");
        let sent = b.start_due(at(600)).send;
        assert_eq!(sent.len(), 1, "one request for 51 changes");
        assert_eq!(
            sent[0].1,
            json!({"message_notifications": 1, "suppress_roles": true})
        );
        assert!(b.is_in_flight(g));

        // More changes while it is out wait, however long.
        b.add(
            g,
            &base,
            intent(g, None, Change::SuppressEveryone(true)),
            at(700),
        );
        assert!(b.start_due(at(60_000)).send.is_empty());
        assert!(b.is_in_flight(g));

        // The answer comes: the waiting change goes out, built from what is confirmed.
        b.finish(g, true);
        let sent = b.start_due(at(60_000)).send;
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].1, json!({"suppress_everyone": true}));
    }

    #[test]
    fn many_guilds_start_two_per_tick_oldest_first() {
        let base = settings(json!({"guild_id": "1"}));
        let t = Instant::now();
        let mut b = SettingsBatch::default();
        for g in 0..5u64 {
            let g = Some(GuildId(g));
            b.add(
                g,
                &base,
                intent(g, None, Change::Mute { for_ms: None }),
                t + Duration::from_millis(g.map_or(0, |g| g.0)),
            );
        }
        let now = t + Duration::from_secs(5);
        let first: Vec<_> = b.start_due(now).send.into_iter().map(|(g, _)| g).collect();
        assert_eq!(first, vec![Some(GuildId(0)), Some(GuildId(1))]);
        assert_eq!(b.start_due(now).send.len(), 2);
        assert_eq!(b.start_due(now).send.len(), 1);
        assert!(b.start_due(now).send.is_empty());
    }

    #[test]
    fn a_failed_write_drops_back_to_the_confirmed_settings() {
        let base = settings(json!({"guild_id": "1"}));
        let g = Some(GuildId(1));
        let t = Instant::now();
        let mut b = SettingsBatch::default();
        b.add(g, &base, intent(g, None, Change::Level(2)), t);
        let _ = b.start_due(t + QUIET);
        b.add(
            g,
            &base,
            intent(g, None, Change::SuppressRoles(true)),
            t + QUIET,
        );
        assert_eq!(b.effective(g).unwrap()["message_notifications"], json!(2));

        // The write fails: its change is gone, the one made since stays.
        assert!(b.finish(g, false).is_none());
        let v = b.effective(g).unwrap();
        assert_eq!(v["message_notifications"], json!(3));
        assert_eq!(v["suppress_roles"], json!(true));
        // Nothing waits any more once that one is sent and done.
        let _ = b.start_due(t + QUIET * 3);
        b.finish(g, true);
        assert!(b.effective(g).is_none());
    }

    #[test]
    fn old_news_is_recognised_by_version() {
        let base = settings(json!({"guild_id": "1"}));
        let g = Some(GuildId(1));
        let mut b = SettingsBatch::default();
        // No entry yet: nothing to be old against.
        assert!(!b.is_stale(g, Some(1)));
        b.add(g, &base, intent(g, None, Change::Level(1)), Instant::now());
        assert!(!b.is_stale(g, Some(1)));
        b.confirm(g, &base, Some(5));
        assert!(b.is_stale(g, Some(4)));
        assert!(!b.is_stale(g, Some(5)));
        assert!(!b.is_stale(g, None));
    }

    #[test]
    fn a_cut_override_map_is_never_replaced_not_even_by_a_change_made_before_it_was_known() {
        // The server's map has an entry that cannot be read: the local map is cut.
        let cut = settings(json!({"guild_id": "1", "channel_overrides": {
            "5": {"muted": true}, "not-an-id": {"muted": true}}}));
        assert!(cut.lossy);
        let g = Some(GuildId(1));
        let t = Instant::now();
        let mut b = SettingsBatch::default();
        // A guild field and a channel change, queued while the map still looked whole.
        let mut i = intent(g, None, Change::SuppressRoles(true));
        i.merge(intent(g, Some(ChannelId(6)), Change::Level(2)));
        b.add(g, &settings(json!({"guild_id": "1"})), i, t);
        // READY then shows the cut map as the confirmed one.
        b.reset(|_| cut.clone(), &BTreeMap::new());

        let started = b.start_due(t + QUIET);
        assert_eq!(started.refused, vec![g]);
        // The guild field goes out; the channel part, which would delete "not-an-id", does not.
        assert_eq!(started.send, vec![(g, json!({"suppress_roles": true}))]);

        // What the UI shows leaves the cut map alone, so it stays marked as cut.
        let shown = compose(&cut, g, &[&intent(g, None, Change::SuppressRoles(true))]);
        assert!(shown.get("channel_overrides").is_none());
    }

    #[test]
    fn versions_from_ready_make_an_older_answer_old_news() {
        let base = settings(json!({"guild_id": "1"}));
        let g = Some(GuildId(1));
        let mut b = SettingsBatch::default();
        b.add(g, &base, intent(g, None, Change::Level(1)), Instant::now());
        // READY shows version 7 (for example a change from another client).
        let busy = b.reset(|_| base.clone(), &BTreeMap::from([(g, 7)]));
        assert_eq!(busy, vec![g]);
        assert!(b.is_stale(g, Some(6)));
        assert!(!b.is_stale(g, Some(7)));
        // A guild READY says nothing about starts without a version.
        let h = Some(GuildId(2));
        b.add(h, &base, intent(h, None, Change::Level(1)), Instant::now());
        b.reset(|_| base.clone(), &BTreeMap::new());
        assert!(!b.is_stale(h, Some(1)));
    }
}
