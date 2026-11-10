// SPDX-License-Identifier: AGPL-3.0-or-later
//! Changing notification settings. A click is applied to the account state at once, so
//! the sidebar answers it, and written to the server later. What the person asked for is
//! kept apart from what the server confirmed (`settings::SettingsBatch`): the state shows
//! the confirmed settings with the unconfirmed changes on top, an echo of an earlier
//! write cannot undo a later click, and a failed write falls back to the confirmed
//! settings.

use std::collections::BTreeMap;
use std::time::Instant;

use inlet_protocol as p;
use inlet_state::{Changes, ChannelId, GuildId};
use serde_json::{Map, Value};

use super::settings::{self, build_intent, Change, Target};
use crate::app::Inlet;
use crate::backend::notifications::{self as backend, SETTINGS_UPDATE};
use gpui::{Context, Window};

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// The guild and `version` of a settings update (an echo, or a write's answer).
fn settings_key(data: &Value) -> Option<(Option<GuildId>, Option<i64>)> {
    let guild = match data.get("guild_id") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(GuildId::parse(s)?),
        Some(Value::Number(n)) => Some(GuildId(n.as_u64()?)),
        Some(_) => return None,
    };
    Some((guild, data.get("version").and_then(Value::as_i64)))
}

fn settings_update(data: Value) -> p::Dispatch {
    p::Dispatch::Other {
        name: SETTINGS_UPDATE.into(),
        data,
    }
}

impl Inlet {
    /// Events from the backend's notifications domain.
    pub(crate) fn on_notifications_event(
        &mut self,
        event: backend::Ev,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            backend::Ev::SettingsWritten { guild_id, result } => {
                self.on_settings_written(guild_id, result, cx)
            }
        }
    }

    /// A settings change from the settings window.
    pub(crate) fn notification_change(
        &mut self,
        target: Target,
        change: Change,
        cx: &mut Context<Self>,
    ) {
        let Some(account) = self.account.clone() else {
            return;
        };
        let (guild_id, channel_id) = match &target {
            Target::Guild(g) => (g.as_deref(), None),
            Target::Channel {
                guild_id,
                channel_id,
            } => (guild_id.as_deref(), Some(channel_id.as_str())),
        };
        let state = account.read(cx);
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
        let Some(intent) = build_intent(guild, channel, change, now_ms()) else {
            return;
        };
        // A channel change replaces the server's whole override map, so it is only safe
        // when the local map is that whole map (see `GuildSettings::can_write_overrides`).
        if channel.is_some() && !state.guild_settings().can_write_overrides(guild) {
            self.chat.error = Some(t!("error-notification-too-many"));
            cx.notify();
            return;
        }
        self.notifications.writes.add(
            guild,
            state.guild_settings().get(guild),
            intent,
            Instant::now(),
        );
        let changes = self.reapply_settings(guild, cx);
        self.after_changes(changes, false, cx);
        cx.notify();
    }

    /// Puts the unconfirmed changes of a guild on top of the confirmed settings in the
    /// state, so the sidebar shows what was asked for.
    fn reapply_settings(&mut self, guild: Option<GuildId>, cx: &mut Context<Self>) -> Changes {
        let (Some(account), Some(data)) = (
            self.account.clone(),
            self.notifications.writes.effective(guild),
        ) else {
            return Changes::default();
        };
        account.update(cx, |s, _| s.apply(&settings_update(data)))
    }

    /// Whether a settings update is older than one already seen (a late echo of an
    /// earlier write). The caller drops it.
    pub(crate) fn settings_update_is_stale(&self, d: &p::Dispatch) -> bool {
        match d {
            p::Dispatch::Other { name, data } if name == SETTINGS_UPDATE => {
                settings_key(data).is_some_and(|(g, v)| self.notifications.writes.is_stale(g, v))
            }
            _ => false,
        }
    }

    /// A settings update (an echo, or a write's answer) or READY was just applied to the
    /// state: what the server holds is now the confirmed settings, and what is still
    /// unconfirmed goes back on top of it.
    pub(crate) fn settings_applied(
        &mut self,
        d: &p::Dispatch,
        changes: &mut Changes,
        cx: &mut Context<Self>,
    ) {
        let Some(account) = self.account.clone() else {
            return;
        };
        match d {
            p::Dispatch::Other { name, data } if name == SETTINGS_UPDATE => {
                let Some((guild, version)) = settings_key(data) else {
                    return;
                };
                self.notifications.writes.confirm(
                    guild,
                    account.read(cx).guild_settings().get(guild),
                    version,
                );
                if self.notifications.writes.is_busy(guild) {
                    changes.merge(self.reapply_settings(guild, cx));
                }
            }
            p::Dispatch::Ready(ready) => {
                let versions: BTreeMap<Option<GuildId>, i64> = ready
                    .user_guild_settings()
                    .iter()
                    .filter_map(settings_key)
                    .filter_map(|(g, v)| Some((g, v?)))
                    .collect();
                let state = account.read(cx);
                let busy = self
                    .notifications
                    .writes
                    .reset(|g| state.guild_settings().get(g).clone(), &versions);
                for guild in busy {
                    changes.merge(self.reapply_settings(guild, cx));
                }
            }
            _ => {}
        }
    }

    /// Settings writes to start now, and the ones refused.
    pub(super) fn tick_settings_writes(&mut self, now: Instant, _cx: &mut Context<Self>) {
        let started = self.notifications.writes.start_due(now);
        for (guild, body) in started.send {
            self.backend.send(backend::Cmd::WriteSettings {
                guild_id: guild.map(|g| g.to_string()),
                body,
            });
        }
        if !started.refused.is_empty() {
            self.chat.error = Some(t!("error-notification-too-many"));
        }
    }

    /// The backend's word on a settings write. On success the server's answer counts as
    /// its echo; on failure the change is dropped, which shows the confirmed settings
    /// again, and the error is shown.
    pub(crate) fn on_settings_written(
        &mut self,
        guild_id: Option<String>,
        result: Result<Value, String>,
        cx: &mut Context<Self>,
    ) {
        let guild = match guild_id.as_deref() {
            None => None,
            Some(g) => match GuildId::parse(g) {
                Some(g) => Some(g),
                None => return,
            },
        };
        // A late answer of an earlier session finds nothing in flight.
        if !self.notifications.writes.is_in_flight(guild) {
            return;
        }
        // What the server holds if it took the write: the confirmed settings with the
        // write on top. Its own answer, when it gives one, wins field by field.
        let assumed = self.notifications.writes.finish(guild, result.is_ok());
        match result {
            Ok(answer) => {
                let mut obj = match assumed {
                    Some(Value::Object(o)) => o,
                    _ => Map::new(),
                };
                if let Value::Object(a) = answer {
                    obj.extend(a);
                }
                if obj.is_empty() {
                    return;
                }
                obj.insert(
                    "guild_id".into(),
                    guild.map_or(Value::Null, |g| g.to_string().into()),
                );
                self.apply_settings_update(settings_update(Value::Object(obj)), cx);
            }
            Err(reason) => {
                self.chat.error = Some(t!("error-notification-settings", error = reason));
                let changes = self.settings_rolled_back(guild, cx);
                self.after_changes(changes, false, cx);
                cx.notify();
            }
        }
    }

    /// Applies a settings update to the state like an echo from the gateway.
    fn apply_settings_update(&mut self, d: p::Dispatch, cx: &mut Context<Self>) {
        if self.settings_update_is_stale(&d) {
            return;
        }
        let Some(account) = self.account.clone() else {
            return;
        };
        let mut changes = account.update(cx, |s, _| s.apply(&d));
        self.settings_applied(&d, &mut changes, cx);
        self.after_changes(changes, false, cx);
        cx.notify();
    }

    /// After a failed write: the confirmed settings, with what is still waiting on top.
    fn settings_rolled_back(&mut self, guild: Option<GuildId>, cx: &mut Context<Self>) -> Changes {
        let (Some(account), Some(confirmed)) = (
            self.account.clone(),
            self.notifications.writes.confirmed(guild),
        ) else {
            return Changes::default();
        };
        let mut changes = account.update(cx, |s, _| {
            s.apply(&settings_update(settings::compose(&confirmed, guild, &[])))
        });
        changes.merge(self.reapply_settings(guild, cx));
        changes
    }
}
