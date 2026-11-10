// SPDX-License-Identifier: AGPL-3.0-or-later
//! The gateway of the active session: starting and restarting it, and forwarding its
//! events to the UI while keeping the guild and channel caches current.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use inlet_protocol as p;
use tokio::task::AbortHandle;

use super::accounts::BACKGROUND_IGNORED_EVENTS;
use super::messages::parse::{
    remember_channels, remember_roles, to_channel, to_message, ChannelCache, SharedNames,
};
use super::{accounts, messages, Backend, Internal, RealActive, Ui};
use crate::model::{self, Channel, Guild};
use crate::reconnect::{Decision, Stop};

/// The gateway settings of an account: a background account does not ask for typing
/// events (and has no active guild, see `accounts`).
pub fn gateway_options(background: bool) -> p::GatewayOptions {
    let mut options = p::GatewayOptions::default();
    if background {
        options.ignored_events = BACKGROUND_IGNORED_EVENTS
            .iter()
            .map(|e| (*e).to_string())
            .collect();
    }
    options
}

impl Backend {
    /// Starts the gateway of an account. `background` picks its options; the task reads
    /// `foreground` for every event, so the account can change role while it runs.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn spawn_gateway(
        &self,
        gen: u64,
        session: &p::Session,
        guilds: &Arc<Mutex<Vec<Guild>>>,
        cache: &ChannelCache,
        names: &SharedNames,
        events: &Ui,
        foreground: &Arc<AtomicBool>,
        background: bool,
    ) -> (p::GatewayHandle, AbortHandle) {
        let (gateway, rx) = session.connect_gateway(gateway_options(background));
        let events = events.clone();
        let internal = self.internal.clone();
        let (guilds, cache, names) = (guilds.clone(), cache.clone(), names.clone());
        let foreground = foreground.clone();
        let forward = self.spawn_guarded("gateway", Some(gen), async move {
            let on_connected = {
                let internal = internal.clone();
                move || {
                    let _ = internal.send(Internal::GatewayConnected { gen });
                }
            };
            let end =
                forward_gateway(rx, events, guilds, cache, names, foreground, on_connected).await;
            let _ = internal.send(Internal::GatewayEnded { gen, end });
        });
        (gateway, forward)
    }

    /// The gateway of an account stopped: restart it later, or (for the active account)
    /// ask the person.
    pub(super) fn gateway_stopped(&mut self, gen: u64, stop: Stop) {
        let Some(r) = self.engine_mut(gen) else {
            return;
        };
        r.gateway.close();
        r.forward.abort();
        let events = r.events.clone();
        match r.policy.on_stop(stop, Instant::now()) {
            Decision::RestartAfter(delay) => {
                let _ = events.unbounded_send(accounts::Ev::Status(t!(
                    "status-reconnecting-in",
                    seconds = delay.as_secs()
                )));
                self.schedule_restart(gen, delay);
            }
            Decision::WaitForUser(message) => {
                r.down = true;
                let _ = events.unbounded_send(accounts::Ev::Status(t!("status-disconnected")));
                let _ = events.unbounded_send(accounts::Ev::GatewayDown(message));
            }
        }
    }

    pub(super) fn schedule_restart(&mut self, gen: u64, delay: Duration) {
        let internal = self.internal.clone();
        let timer = self.spawn_guarded("reconnect timer", None, async move {
            tokio::time::sleep(delay).await;
            let _ = internal.send(Internal::GatewayRestart { gen });
        });
        if let Some(r) = self.engine_mut(gen) {
            if let Some(old) = r.restart_timer.replace(timer) {
                old.abort();
            }
        } else {
            timer.abort();
        }
    }

    /// Starts the gateway of session `gen` again, with the options of its current role.
    pub(super) fn restart_gateway(&mut self, gen: u64) {
        let Some(r) = self.engine(gen) else {
            return;
        };
        r.forward.abort();
        r.gateway.close();
        let background = !r.is_foreground();
        let (gateway, forward) = self.spawn_gateway(
            r.gen,
            &r.session,
            &r.guilds,
            &r.cache,
            &r.names,
            &r.events,
            &r.foreground,
            background,
        );
        // A new gateway handle does not know the guild on screen yet.
        if let Some(guild) = r.active_guild.as_ref().filter(|_| !background) {
            gateway.set_guild_active(guild, true);
        }
        if let Some(r) = self.engine_mut(gen) {
            r.gateway = gateway;
            r.forward = forward;
            r.restart_timer = None;
            r.ignores_typing = background;
            r.down = false;
            r.policy.started(Instant::now());
        }
    }
}

impl RealActive {
    /// Whether this account is the active one.
    pub(super) fn is_foreground(&self) -> bool {
        self.foreground.load(Ordering::Relaxed)
    }
}

/// Inserts or replaces a channel in the cached list of its guild. Returns false when that
/// list was never loaded (nothing to update).
fn upsert_channel(cache: &ChannelCache, channel: &p::Channel) -> bool {
    let mut map = cache.lock().unwrap();
    let Some(list) = map.get_mut(&channel.guild_id) else {
        return false;
    };
    let converted = to_channel(channel);
    match list.iter_mut().find(|c| c.id == converted.id) {
        Some(existing) => *existing = converted,
        None => list.push(converted),
    }
    true
}

/// How a gateway's event stream ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayEnd {
    /// The token is no longer valid (4004): the session is over.
    SessionEnded(String),
    /// The protocol stopped on purpose and retrying would not help.
    Terminal(String),
    /// The stream closed without a close reason.
    ChannelClosed,
}

/// What a terminal close reason means for the person.
pub fn close_message(reason: &p::CloseReason) -> Option<String> {
    Some(match reason {
        p::CloseReason::ClientClosed => return None,
        p::CloseReason::AuthenticationFailed => {
            t!("gateway-auth-failed")
        }
        p::CloseReason::InvalidApiVersion => {
            t!("gateway-api-version")
        }
        p::CloseReason::InvalidShard => {
            t!("gateway-invalid-shard")
        }
        p::CloseReason::InvalidUrl(url) => {
            t!("gateway-invalid-url", url = model::clean_name(url))
        }
        other => t!("gateway-stopped", reason = format!("{other:?}")),
    })
}

/// Forwards gateway events to the UI and keeps the guild and channel caches current.
/// `connected` is called on every READY/RESUMED. Returns how the gateway stopped.
///
/// While `foreground` is false (a background account) the caches are still kept, but
/// messages are not rendered, typing events are dropped, and no channel or guild lists
/// go to the UI: only the events its account state needs for unread counts.
pub async fn forward_gateway(
    mut rx: tokio::sync::mpsc::Receiver<p::GatewayEvent>,
    events: Ui,
    guilds: Arc<Mutex<Vec<Guild>>>,
    cache: ChannelCache,
    names: SharedNames,
    foreground: Arc<AtomicBool>,
    connected: impl Fn(),
) -> GatewayEnd {
    macro_rules! send {
        ($e:expr) => {{
            let _ = events.unbounded_send($e);
        }};
    }
    let channels_event = |guild_id: Option<String>| {
        let list = cache.lock().unwrap().get(&guild_id).cloned();
        if let Some(channels) = list {
            send!(messages::Ev::Channels { guild_id, channels });
        }
    };
    let mut ready_seen = false;
    while let Some(event) = rx.recv().await {
        match event {
            p::GatewayEvent::State(state) => {
                let text = match state {
                    p::ConnectionState::Connecting { attempt: 0 } => t!("status-connecting"),
                    p::ConnectionState::Connecting { attempt } => {
                        t!("status-connecting-attempt", attempt = attempt + 1)
                    }
                    p::ConnectionState::Identifying | p::ConnectionState::Resuming => {
                        t!("status-connecting")
                    }
                    p::ConnectionState::Connected => {
                        connected();
                        t!("status-connected")
                    }
                    p::ConnectionState::Reconnecting { delay, .. } => {
                        t!("status-gateway-reconnecting-in", seconds = delay.as_secs())
                    }
                    p::ConnectionState::Closed(reason) => {
                        send!(accounts::Ev::Status(t!("status-disconnected")));
                        return match (&reason, close_message(&reason)) {
                            (p::CloseReason::AuthenticationFailed, Some(m)) => {
                                GatewayEnd::SessionEnded(m)
                            }
                            (_, Some(m)) => GatewayEnd::Terminal(m),
                            (_, None) => GatewayEnd::Terminal(String::new()),
                        };
                    }
                };
                send!(accounts::Ev::Status(text));
            }
            p::GatewayEvent::Dispatch { event, .. } => {
                let fg = foreground.load(Ordering::Relaxed);
                if !fg && matches!(event, p::Dispatch::TypingStart(_)) {
                    continue;
                }
                let mut reconnected = false;
                // Messages the event carries, converted and parsed here, off the UI thread.
                let mut rendered = Vec::new();
                // Channel lists and the guild list to send once the account state has the
                // event, so a channel picked from a list is always known to the state.
                let mut lists: Vec<Option<String>> = Vec::new();
                let mut guild_list = None;
                match &event {
                    p::Dispatch::Ready(ready) => {
                        // A fresh Identify: rebuild the guild channel lists, keep the DM
                        // list (READY does not carry it), and show the result.
                        let mut map: HashMap<Option<String>, Vec<Channel>> = HashMap::new();
                        for g in &ready.guilds {
                            remember_roles(&names, &g.roles);
                            if !g.unavailable && !g.channels.is_empty() {
                                let list: Vec<Channel> =
                                    g.channels.iter().map(to_channel).collect();
                                remember_channels(&names, &list);
                                map.insert(Some(g.id.clone()), list);
                            }
                        }
                        let keys: Vec<Option<String>> = {
                            let mut cache = cache.lock().unwrap();
                            if let Some(dms) = cache.remove(&None) {
                                map.insert(None, dms);
                            }
                            *cache = map;
                            cache.keys().cloned().collect()
                        };
                        lists.extend(keys);
                        // The guild list of an account restored without REST (background)
                        // comes from READY.
                        let mut list = guilds.lock().unwrap();
                        let before = list.len();
                        for g in &ready.guilds {
                            if g.unavailable || list.iter().any(|x| x.id == g.id) {
                                continue;
                            }
                            if let Some(name) = g.name() {
                                list.push(Guild {
                                    id: g.id.clone(),
                                    name: model::clean_name(name),
                                });
                            }
                        }
                        if list.len() != before {
                            guild_list = Some(list.clone());
                        }
                        drop(list);
                        // Messages may have been missed while disconnected.
                        reconnected = ready_seen;
                        ready_seen = true;
                    }
                    p::Dispatch::MessageCreate(m) | p::Dispatch::MessageUpdate(m) if fg => {
                        rendered.push(to_message(m, &names));
                    }
                    p::Dispatch::ChannelCreate(c) | p::Dispatch::ChannelUpdate(c) => {
                        remember_channels(&names, &[to_channel(c)]);
                        if upsert_channel(&cache, c) {
                            lists.push(c.guild_id.clone());
                        }
                    }
                    p::Dispatch::ChannelDelete(c) => {
                        let removed = {
                            let mut map = cache.lock().unwrap();
                            map.get_mut(&c.guild_id)
                                .map(|list| list.retain(|x| x.id != c.id))
                                .is_some()
                        };
                        if removed {
                            lists.push(c.guild_id.clone());
                        }
                    }
                    p::Dispatch::GuildCreate(g) | p::Dispatch::GuildSync(g) => {
                        remember_roles(&names, &g.roles);
                        if !g.unavailable && !g.channels.is_empty() {
                            let list: Vec<Channel> = g.channels.iter().map(to_channel).collect();
                            remember_channels(&names, &list);
                            cache.lock().unwrap().insert(Some(g.id.clone()), list);
                            lists.push(Some(g.id.clone()));
                        }
                        let mut list = guilds.lock().unwrap();
                        if !list.iter().any(|x| x.id == g.id) {
                            if let Some(name) = g.name() {
                                list.push(Guild {
                                    id: g.id.clone(),
                                    name: model::clean_name(name),
                                });
                                guild_list = Some(list.clone());
                            }
                        }
                    }
                    p::Dispatch::GuildDelete(d) if !d.unavailable => {
                        cache.lock().unwrap().remove(&Some(d.id.clone()));
                        let mut list = guilds.lock().unwrap();
                        list.retain(|g| g.id != d.id);
                        guild_list = Some(list.clone());
                    }
                    _ => {}
                }
                // Every event goes to the account state on the UI thread first (and READY
                // before `Reconnected`), then the lists the sidebar shows.
                send!(messages::Ev::Dispatch {
                    dispatch: Box::new(event),
                    rendered,
                });
                if !fg {
                    continue;
                }
                for key in lists {
                    channels_event(key);
                }
                if let Some(list) = guild_list {
                    send!(messages::Ev::Guilds(list));
                }
                if reconnected {
                    send!(accounts::Ev::Reconnected);
                }
            }
        }
    }
    GatewayEnd::ChannelClosed
}
