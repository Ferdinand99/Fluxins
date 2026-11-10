// SPDX-License-Identifier: AGPL-3.0-or-later
//! Talking to a real Fluxer instance through `inlet-protocol`: connecting, the browser
//! handoff, converting protocol types into the UI model, and forwarding gateway events.
//!
//! Everything here is a free function or a small task body. The backend loop owns the
//! state and decides what runs when (see `backend.rs`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::channel::mpsc::UnboundedSender;
use inlet_protocol as p;
use inlet_protocol::models::channel_type;

use crate::backend::Event;
use crate::model::{self, Attachment, Channel, ChannelKind, Guild, Message};
use crate::token_store;

/// How long the start-up checks may take before Inlet treats the instance as unreachable.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(12);

/// Channel lists per guild (`None` = direct messages), shared by REST tasks and the
/// gateway forwarder.
pub type ChannelCache = Arc<Mutex<HashMap<Option<String>, Vec<Channel>>>>;

/// A failure and what it means for the session.
#[derive(Debug, Clone)]
pub struct Failure {
    pub message: String,
    pub kind: FailureKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// The token is gone (rejected, revoked, or global rate limit): sign in again.
    SessionEnded,
    /// The instance could not be reached or answered with a server error: try again later.
    Offline,
    /// Anything else (a refused request, a bad answer).
    Other,
}

impl From<p::Error> for Failure {
    fn from(err: p::Error) -> Self {
        Failure { kind: classify(&err), message: describe(&err) }
    }
}

impl Failure {
    fn timeout() -> Self {
        Failure { message: "The instance did not answer in time.".into(), kind: FailureKind::Offline }
    }
}

pub fn classify(err: &p::Error) -> FailureKind {
    match err {
        p::Error::Unauthorized | p::Error::GlobalRateLimited | p::Error::EndpointsChanged { .. } => {
            FailureKind::SessionEnded
        }
        p::Error::Http(_) | p::Error::RateLimited { .. } => FailureKind::Offline,
        p::Error::Api(api) if api.status >= 500 || api.status == 0 => FailureKind::Offline,
        _ => FailureKind::Other,
    }
}

/// Turns a protocol error into a sentence for the sign-in screen or the error bar.
pub fn describe(err: &p::Error) -> String {
    match err {
        p::Error::Unauthorized => "The instance did not accept the session. Sign in again.".into(),
        p::Error::GlobalRateLimited => {
            "Fluxer's global rate limit was hit, which ends the session. Sign in again.".into()
        }
        p::Error::Api(api) => {
            let code = api.field_code().unwrap_or(&api.code);
            if api.message.is_empty() {
                format!("The instance refused: {api}")
            } else {
                format!("{} ({code})", model::clean_name(&api.message))
            }
        }
        p::Error::Http(_) => "Could not reach the instance. Check the address and your connection.".into(),
        p::Error::EndpointsChanged { .. } => "This server's addresses changed. Sign in again.".into(),
        p::Error::Url(msg) => format!("The instance address is not usable: {}", model::clean_name(msg)),
        p::Error::TooLarge { .. } => "The instance sent more data than Inlet accepts.".into(),
        other => other.to_string(),
    }
}

async fn with_timeout<T>(fut: impl std::future::Future<Output = Result<T, Failure>>) -> Result<T, Failure> {
    tokio::time::timeout(CONNECT_TIMEOUT, fut).await.unwrap_or_else(|_| Err(Failure::timeout()))
}

pub async fn discover(instance: &str) -> Result<p::Instance, Failure> {
    let info = tokio::task::spawn_blocking(token_store::client_info)
        .await
        .unwrap_or_else(|_| p::ClientInfo::default());
    with_timeout(async { Ok(p::Instance::discover_with(instance.trim(), info).await?) }).await
}

/// What a session is built from.
#[derive(Clone)]
pub enum Credential {
    /// From the keychain, with the hosts it was issued for.
    Stored { token: p::Token, pins: p::EndpointPins },
    /// A finished browser sign-in.
    SignedIn(p::SignedIn),
    /// A token the person pasted.
    Pasted(p::Token),
}


/// A checked session: the token works and the guild list is loaded.
pub struct Connected {
    pub session: p::Session,
    /// Display name.
    pub username: String,
    /// Account name (`username`), shown when confirming a new sign-in.
    pub account: String,
    pub guilds: Vec<Guild>,
}

/// Builds the session (refusing a stored token whose hosts changed), checks the token
/// and loads guilds. Does not start the gateway.
pub async fn connect(instance: p::Instance, credential: Credential) -> Result<Connected, Failure> {
    let session = match credential {
        // A stored token is only ever used with the hosts it was issued for.
        Credential::Stored { token, pins } => p::Session::from_token_pinned(instance, token, &pins)?,
        // A pasted token has no pins yet; it is pinned to this instance when saved.
        Credential::Pasted(token) => p::Session::from_token_unpinned(instance, token),
        Credential::SignedIn(signed_in) => p::Session::from_signed_in(instance, signed_in),
    };
    with_timeout(async {
        let me = session.client().me().await?;
        let guilds = session
            .client()
            .guilds()
            .await?
            .into_iter()
            .map(|g| Guild { id: g.id, name: model::clean_name(&g.name) })
            .collect();
        Ok(Connected {
            username: model::clean_name(me.display_name()),
            account: model::clean_name(&me.username),
            guilds,
            session: session.clone(),
        })
    })
    .await
}

/// Polls a browser handoff until it completes, is denied, expires or is cancelled.
/// Network errors are retried until the code's expiry, so a short outage does not end
/// the sign-in. On cancel, tells the server to drop the code. `None` means cancelled.
pub async fn run_handoff(
    instance: p::Instance,
    handoff: p::Handoff,
    cancel: tokio::sync::oneshot::Receiver<()>,
) -> Option<Result<(p::Instance, p::SignedIn), String>> {
    let deadline = chrono::DateTime::parse_from_rfc3339(handoff.expires_at())
        .ok()
        .and_then(|t| (t.with_timezone(&chrono::Utc) - chrono::Utc::now()).to_std().ok())
        .unwrap_or(Duration::from_secs(5 * 60));
    let poll = async {
        loop {
            match handoff.poll().await {
                Ok(p::HandoffStatus::Pending) => {}
                Ok(p::HandoffStatus::Completed(signed_in)) => return Ok(signed_in),
                Ok(p::HandoffStatus::Denied) => return Err("The sign-in was denied in the browser.".to_string()),
                Ok(p::HandoffStatus::Expired) => return Err("The code expired. Try again.".to_string()),
                Err(err) if classify(&err) == FailureKind::Offline => {
                    log::info!("handoff poll failed, retrying: {}", describe(&err));
                }
                Err(err) => return Err(describe(&err)),
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    };
    let outcome = tokio::select! {
        result = poll => Some(result),
        _ = tokio::time::sleep(deadline) => Some(Err("The code expired. Try again.".to_string())),
        _ = cancel => None,
    };
    match outcome {
        Some(Ok(signed_in)) => Some(Ok((instance, signed_in))),
        Some(Err(message)) => Some(Err(message)),
        None => {
            if let Err(err) = handoff.cancel().await {
                log::info!("handoff cancel failed: {}", describe(&err));
            }
            None
        }
    }
}

/// Ends a token on the server, best effort, with a short timeout. Returns whether it worked.
pub async fn revoke(session: &p::Session) -> bool {
    match tokio::time::timeout(Duration::from_secs(8), session.client().sign_out()).await {
        Ok(Ok(())) => true,
        Ok(Err(err)) => {
            log::warn!("server sign-out failed: {}", describe(&err));
            false
        }
        Err(_) => false,
    }
}

fn channel_kind(kind: i64) -> ChannelKind {
    match kind {
        channel_type::GUILD_TEXT | channel_type::GUILD_ANNOUNCEMENT | channel_type::PERSONAL_NOTES => ChannelKind::Text,
        channel_type::GUILD_VOICE => ChannelKind::Voice,
        channel_type::GUILD_CATEGORY => ChannelKind::Category,
        channel_type::DM => ChannelKind::Dm,
        channel_type::GROUP_DM => ChannelKind::GroupDm,
        _ => ChannelKind::Other,
    }
}

pub fn to_channel(c: &p::Channel) -> Channel {
    let name = match (&c.name, c.recipients.is_empty()) {
        (Some(n), _) if !n.is_empty() => n.clone(),
        (_, false) => c.recipients.iter().map(|u| u.display_name().to_string()).collect::<Vec<_>>().join(", "),
        _ if c.kind == channel_type::PERSONAL_NOTES => "personal notes".into(),
        _ => format!("channel {}", c.id),
    };
    Channel {
        id: c.id.clone(),
        name: model::clean_name(&name),
        kind: channel_kind(c.kind),
        parent_id: c.parent_id.clone(),
        position: c.position.unwrap_or(0),
    }
}

/// Converts and parses a message. Runs on the backend thread, so markdown parsing never
/// blocks the UI.
pub fn to_message(m: &p::Message) -> Message {
    let (timestamp_ms, timestamp) = model::timestamp(&m.timestamp);
    let mut out = Message {
        id: m.id.clone(),
        channel_id: m.channel_id.clone(),
        author_id: m.author.id.clone(),
        author: model::clean_name(m.author.display_name()),
        author_username: model::clean_name(&m.author.username),
        timestamp_ms,
        timestamp,
        content: m.content.clone(),
        blocks: Vec::new(),
        truncated: false,
        attachments: m
            .attachments
            .iter()
            .map(|a| Attachment {
                filename: model::clean_name(&a.filename),
                url: a.url.clone().or_else(|| a.proxy_url.clone()).unwrap_or_default(),
            })
            .collect(),
        edited: m.edited_timestamp.is_some(),
    };
    out.parse();
    out
}

/// Snowflakes compare as numbers; a shorter string is older.
pub fn snowflake_key(id: &str) -> (usize, &str) {
    (id.len(), id)
}

/// Inserts or replaces a channel in the cached list of its guild. Returns false when that
/// list was never loaded (nothing to update).
fn upsert_channel(cache: &ChannelCache, channel: &p::Channel) -> bool {
    let mut map = cache.lock().unwrap();
    let Some(list) = map.get_mut(&channel.guild_id) else { return false };
    let converted = to_channel(channel);
    match list.iter_mut().find(|c| c.id == converted.id) {
        Some(existing) => *existing = converted,
        None => list.push(converted),
    }
    true
}

/// Forwards gateway events to the UI and keeps the guild and channel caches current.
/// Returns when the gateway stops; `Some(message)` means the session ended for good.
pub async fn forward_gateway(
    mut rx: tokio::sync::mpsc::Receiver<p::GatewayEvent>,
    events: UnboundedSender<Event>,
    guilds: Arc<Mutex<Vec<Guild>>>,
    cache: ChannelCache,
) -> Option<String> {
    let send = |e: Event| {
        let _ = events.unbounded_send(e);
    };
    let channels_event = |guild_id: Option<String>| {
        let list = cache.lock().unwrap().get(&guild_id).cloned();
        if let Some(channels) = list {
            send(Event::Channels { guild_id, channels });
        }
    };
    let mut ready_seen = false;
    while let Some(event) = rx.recv().await {
        match event {
            p::GatewayEvent::State(state) => {
                let text = match state {
                    p::ConnectionState::Connecting { attempt: 0 } => "Connecting\u{2026}".to_string(),
                    p::ConnectionState::Connecting { attempt } => {
                        format!("Connecting (attempt {})\u{2026}", attempt + 1)
                    }
                    p::ConnectionState::Identifying | p::ConnectionState::Resuming => "Connecting\u{2026}".into(),
                    p::ConnectionState::Connected => "Connected".into(),
                    p::ConnectionState::Reconnecting { delay, .. } => {
                        format!("Reconnecting in {} s\u{2026}", delay.as_secs())
                    }
                    p::ConnectionState::Closed(p::CloseReason::AuthenticationFailed) => {
                        send(Event::Status("Disconnected".into()));
                        return Some("The instance ended this session. Sign in again.".into());
                    }
                    p::ConnectionState::Closed(p::CloseReason::InvalidUrl(url)) => {
                        send(Event::Error(format!(
                            "The instance's gateway address is not usable: {}",
                            model::clean_name(&url)
                        )));
                        "Disconnected".into()
                    }
                    p::ConnectionState::Closed(reason) => format!("Disconnected ({reason:?})"),
                };
                send(Event::Status(text));
            }
            p::GatewayEvent::Dispatch { event, .. } => match event {
                p::Dispatch::Ready(ready) => {
                    // A fresh Identify: rebuild the channel cache from the guild list.
                    let mut map = HashMap::new();
                    for g in &ready.guilds {
                        if !g.unavailable && !g.channels.is_empty() {
                            map.insert(Some(g.id.clone()), g.channels.iter().map(to_channel).collect());
                        }
                    }
                    *cache.lock().unwrap() = map;
                    if ready_seen {
                        // Messages may have been missed while disconnected.
                        send(Event::Reconnected);
                    }
                    ready_seen = true;
                }
                p::Dispatch::MessageCreate(m) => send(Event::MessageCreated(to_message(&m))),
                p::Dispatch::MessageUpdate(m) => send(Event::MessageUpdated(to_message(&m))),
                p::Dispatch::MessageDelete(d) => {
                    send(Event::MessageDeleted { channel_id: d.channel_id, message_id: d.id })
                }
                p::Dispatch::ChannelCreate(c) | p::Dispatch::ChannelUpdate(c) => {
                    if upsert_channel(&cache, &c) {
                        channels_event(c.guild_id.clone());
                    }
                }
                p::Dispatch::ChannelDelete(c) => {
                    let removed = {
                        let mut map = cache.lock().unwrap();
                        map.get_mut(&c.guild_id).map(|list| list.retain(|x| x.id != c.id)).is_some()
                    };
                    if removed {
                        channels_event(c.guild_id.clone());
                    }
                }
                p::Dispatch::GuildCreate(g) | p::Dispatch::GuildSync(g) => {
                    if !g.unavailable && !g.channels.is_empty() {
                        cache.lock().unwrap().insert(Some(g.id.clone()), g.channels.iter().map(to_channel).collect());
                        channels_event(Some(g.id.clone()));
                    }
                    let mut list = guilds.lock().unwrap();
                    if !list.iter().any(|x| x.id == g.id) {
                        if let Some(name) = g.name() {
                            list.push(Guild { id: g.id.clone(), name: model::clean_name(name) });
                            send(Event::Guilds(list.clone()));
                        }
                    }
                }
                p::Dispatch::GuildDelete(d) if !d.unavailable => {
                    cache.lock().unwrap().remove(&Some(d.id.clone()));
                    let mut list = guilds.lock().unwrap();
                    list.retain(|g| g.id != d.id);
                    send(Event::Guilds(list.clone()));
                }
                _ => {}
            },
        }
    }
    None
}
