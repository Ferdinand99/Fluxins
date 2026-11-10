// SPDX-License-Identifier: AGPL-3.0-or-later
//! The gateway of the active session: starting and restarting it, and forwarding its
//! events to the UI while keeping the guild and channel caches current.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use inlet_protocol as p;
use tokio::task::AbortHandle;

use super::messages::parse::{
    remember_channels, remember_roles, to_channel, to_message, ChannelCache, SharedNames,
};
use super::{accounts, messages, Active, Backend, Internal, Ui};
use crate::model::{self, Channel, Guild};
use crate::reconnect::{Decision, Stop};

impl Backend {
    pub(super) fn spawn_gateway(
        &self,
        gen: u64,
        session: &p::Session,
        guilds: &Arc<Mutex<Vec<Guild>>>,
        cache: &ChannelCache,
        names: &SharedNames,
    ) -> (p::GatewayHandle, AbortHandle) {
        let (gateway, rx) = session.connect_gateway(p::GatewayOptions::default());
        let events = self.events.clone();
        let internal = self.internal.clone();
        let (guilds, cache, names) = (guilds.clone(), cache.clone(), names.clone());
        let forward = self.spawn_guarded("gateway", Some(gen), async move {
            let on_connected = {
                let internal = internal.clone();
                move || {
                    let _ = internal.send(Internal::GatewayConnected { gen });
                }
            };
            let end = forward_gateway(rx, events, guilds, cache, names, on_connected).await;
            let _ = internal.send(Internal::GatewayEnded { gen, end });
        });
        (gateway, forward)
    }

    /// The gateway of the active session stopped: restart it later, or ask the user.
    pub(super) fn gateway_stopped(&mut self, gen: u64, stop: Stop) {
        let Some(Active::Real(r)) = &mut self.active else {
            return;
        };
        if r.gen != gen {
            return;
        }
        r.gateway.close();
        r.forward.abort();
        match r.policy.on_stop(stop, Instant::now()) {
            Decision::RestartAfter(delay) => {
                self.emit(accounts::Ev::Status(t!(
                    "status-reconnecting-in",
                    seconds = delay.as_secs()
                )));
                self.schedule_restart(gen, delay);
            }
            Decision::WaitForUser(message) => {
                self.emit(accounts::Ev::Status(t!("status-disconnected")));
                self.emit(accounts::Ev::GatewayDown(message));
            }
        }
    }

    pub(super) fn schedule_restart(&mut self, gen: u64, delay: Duration) {
        let internal = self.internal.clone();
        let timer = self.spawn_guarded("reconnect timer", None, async move {
            tokio::time::sleep(delay).await;
            let _ = internal.send(Internal::GatewayRestart { gen });
        });
        if let Some(Active::Real(r)) = &mut self.active {
            if let Some(old) = r.restart_timer.replace(timer) {
                old.abort();
            }
        }
    }

    pub(super) fn restart_gateway(&mut self) {
        let Some(Active::Real(r)) = &self.active else {
            return;
        };
        r.forward.abort();
        r.gateway.close();
        let (gateway, forward) =
            self.spawn_gateway(r.gen, &r.session, &r.guilds, &r.cache, &r.names);
        // A new gateway handle does not know the guild on screen yet.
        if let Some(guild) = &r.active_guild {
            gateway.set_guild_active(guild, true);
        }
        if let Some(Active::Real(r)) = &mut self.active {
            r.gateway = gateway;
            r.forward = forward;
            r.restart_timer = None;
            r.policy.started(Instant::now());
        }
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
pub async fn forward_gateway(
    mut rx: tokio::sync::mpsc::Receiver<p::GatewayEvent>,
    events: Ui,
    guilds: Arc<Mutex<Vec<Guild>>>,
    cache: ChannelCache,
    names: SharedNames,
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
            p::GatewayEvent::Dispatch { event, .. } => match event {
                p::Dispatch::Ready(ready) => {
                    // A fresh Identify: rebuild the guild channel lists, keep the DM list
                    // (READY does not carry it), and show the result.
                    let mut map: HashMap<Option<String>, Vec<Channel>> = HashMap::new();
                    for g in &ready.guilds {
                        remember_roles(&names, &g.roles);
                        if !g.unavailable && !g.channels.is_empty() {
                            let list: Vec<Channel> = g.channels.iter().map(to_channel).collect();
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
                    for key in keys {
                        channels_event(key);
                    }
                    if ready_seen {
                        // Messages may have been missed while disconnected.
                        send!(accounts::Ev::Reconnected);
                    }
                    ready_seen = true;
                }
                p::Dispatch::MessageCreate(m) => {
                    send!(messages::Ev::MessageCreated(to_message(&m, &names)))
                }
                p::Dispatch::MessageUpdate(m) => {
                    send!(messages::Ev::MessageUpdated(to_message(&m, &names)))
                }
                p::Dispatch::MessageDelete(d) => send!(messages::Ev::MessageDeleted {
                    channel_id: d.channel_id,
                    message_id: d.id,
                }),
                p::Dispatch::ChannelCreate(c) | p::Dispatch::ChannelUpdate(c) => {
                    remember_channels(&names, &[to_channel(&c)]);
                    if upsert_channel(&cache, &c) {
                        channels_event(c.guild_id.clone());
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
                        channels_event(c.guild_id.clone());
                    }
                }
                p::Dispatch::GuildCreate(g) | p::Dispatch::GuildSync(g) => {
                    remember_roles(&names, &g.roles);
                    if !g.unavailable && !g.channels.is_empty() {
                        let list: Vec<Channel> = g.channels.iter().map(to_channel).collect();
                        remember_channels(&names, &list);
                        cache.lock().unwrap().insert(Some(g.id.clone()), list);
                        channels_event(Some(g.id.clone()));
                    }
                    let mut list = guilds.lock().unwrap();
                    if !list.iter().any(|x| x.id == g.id) {
                        if let Some(name) = g.name() {
                            list.push(Guild {
                                id: g.id.clone(),
                                name: model::clean_name(name),
                            });
                            send!(messages::Ev::Guilds(list.clone()));
                        }
                    }
                }
                p::Dispatch::GuildDelete(d) if !d.unavailable => {
                    cache.lock().unwrap().remove(&Some(d.id.clone()));
                    let mut list = guilds.lock().unwrap();
                    list.retain(|g| g.id != d.id);
                    send!(messages::Ev::Guilds(list.clone()));
                }
                _ => {}
            },
        }
    }
    GatewayEnd::ChannelClosed
}
