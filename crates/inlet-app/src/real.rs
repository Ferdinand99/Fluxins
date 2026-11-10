//! A signed-in session against a real Fluxer instance, through `inlet-protocol`.
//!
//! Converts protocol types into the UI model and gateway dispatches into UI events.
//! The token stays inside the protocol `Session`; it is only read out to store it in
//! the keychain.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::channel::mpsc::UnboundedSender;
use inlet_protocol as p;
use inlet_protocol::models::channel_type;
use tokio::sync::mpsc::UnboundedSender as TokioSender;

use crate::backend::Event;
use crate::model::{self, Attachment, Channel, ChannelKind, Guild, Message};
use crate::token_store;

/// A failure, and whether it ended the session (the person must sign in again).
#[derive(Debug)]
pub struct Failure {
    pub message: String,
    pub session_ended: bool,
}

impl From<p::Error> for Failure {
    fn from(err: p::Error) -> Self {
        Failure { session_ended: ends_session(&err), message: describe(&err) }
    }
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Failure { message, session_ended: false }
    }
}

/// Errors after which the token is gone: the instance rejected it, or the global rate
/// limit was hit (Fluxer revokes the token then).
pub fn ends_session(err: &p::Error) -> bool {
    matches!(err, p::Error::Unauthorized | p::Error::GlobalRateLimited)
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
                format!("{} ({code})", api.message)
            }
        }
        p::Error::Http(_) => format!("Could not reach the instance: {err}"),
        other => other.to_string(),
    }
}

pub async fn discover(instance: &str) -> Result<p::Instance, Failure> {
    Ok(p::Instance::discover_with(instance.trim(), token_store::client_info()).await?)
}

/// Starts a browser sign-in ("desktop handoff").
pub async fn start_handoff(instance: &str) -> Result<(p::Instance, p::Handoff), Failure> {
    let instance = discover(instance).await?;
    let handoff = p::Handoff::start(&instance).await?;
    Ok((instance, handoff))
}

/// Waits until the person has approved the handoff in the browser.
pub async fn wait_handoff(instance: p::Instance, handoff: p::Handoff) -> Result<(p::Instance, p::Token), String> {
    match handoff.wait(Duration::from_secs(3)).await.map_err(|e| describe(&e))? {
        p::HandoffStatus::Completed(signed_in) => Ok((instance, signed_in.token.clone())),
        p::HandoffStatus::Denied => Err("The sign-in was denied in the browser.".into()),
        p::HandoffStatus::Expired => Err("The code expired. Start again.".into()),
        p::HandoffStatus::Pending => Err("The sign-in did not finish.".into()),
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

fn to_channel(c: &p::Channel) -> Channel {
    let name = match (&c.name, c.recipients.is_empty()) {
        (Some(n), _) if !n.is_empty() => n.clone(),
        (_, false) => c.recipients.iter().map(|u| u.display_name().to_string()).collect::<Vec<_>>().join(", "),
        _ if c.kind == channel_type::PERSONAL_NOTES => "personal notes".into(),
        _ => format!("channel {}", c.id),
    };
    Channel {
        id: c.id.clone(),
        name,
        kind: channel_kind(c.kind),
        parent_id: c.parent_id.clone(),
        position: c.position.unwrap_or(0),
    }
}

fn to_message(m: &p::Message) -> Message {
    Message {
        id: m.id.clone(),
        channel_id: m.channel_id.clone(),
        author: m.author.display_name().to_string(),
        timestamp: model::format_timestamp(&m.timestamp),
        content: m.content.clone(),
        attachments: m
            .attachments
            .iter()
            .map(|a| Attachment {
                filename: a.filename.clone(),
                url: a.url.clone().or_else(|| a.proxy_url.clone()).unwrap_or_default(),
            })
            .collect(),
        edited: m.edited_timestamp.is_some(),
    }
}

/// Snowflakes compare as numbers; a shorter string is older.
fn snowflake_key(id: &str) -> (usize, &str) {
    (id.len(), id)
}

pub struct RealSession {
    session: p::Session,
    gateway: p::GatewayHandle,
    events: UnboundedSender<Event>,
    ended: TokioSender<String>,
    channel_cache: HashMap<Option<String>, Vec<Channel>>,
    active_guild: Option<String>,
    forward: tokio::task::JoinHandle<()>,
}

impl RealSession {
    /// Checks the token, loads guilds and starts the gateway. Must run inside the
    /// backend's tokio runtime. `ended` receives a message when the session ends for good.
    pub async fn start(
        instance: p::Instance,
        token: p::Token,
        events: UnboundedSender<Event>,
        ended: TokioSender<String>,
    ) -> Result<Self, Failure> {
        let session = p::Session::from_token(instance, token);
        let me = session.client().me().await?;
        let guilds: Vec<Guild> = session
            .client()
            .guilds()
            .await?
            .into_iter()
            .map(|g| Guild { id: g.id, name: g.name })
            .collect();

        let _ = events.unbounded_send(Event::SignedIn {
            username: me.display_name().to_string(),
            instance: session.instance().origin().to_string(),
        });
        let _ = events.unbounded_send(Event::Guilds(guilds.clone()));
        let guilds = Arc::new(Mutex::new(guilds));

        let (gateway, mut rx) = session.connect_gateway(p::GatewayOptions::default());
        let forward = {
            let events = events.clone();
            let ended = ended.clone();
            tokio::spawn(async move {
                while let Some(event) = rx.recv().await {
                    forward_gateway_event(event, &events, &guilds, &ended);
                }
            })
        };

        Ok(Self { session, gateway, events, ended, channel_cache: HashMap::new(), active_guild: None, forward })
    }

    pub fn instance(&self) -> &str {
        self.session.instance().origin()
    }

    pub fn token(&self) -> &p::Token {
        self.session.token()
    }

    /// Signs out on the server (ends this token) and stops the gateway.
    pub async fn close(self) {
        if let Err(err) = self.session.client().sign_out().await {
            log::warn!("server sign-out failed: {}", describe(&err));
        }
    }

    fn emit(&self, event: Event) {
        let _ = self.events.unbounded_send(event);
    }

    fn fail(&self, prefix: &str, err: p::Error) {
        if ends_session(&err) {
            let _ = self.ended.send(describe(&err));
        } else {
            self.emit(Event::Error(format!("{prefix}{}", describe(&err))));
        }
    }

    pub async fn open_guild(&mut self, guild_id: Option<String>) {
        if let Some(previous) = self.active_guild.take() {
            self.gateway.set_guild_active(&previous, false);
        }
        if let Some(id) = &guild_id {
            // Guilds with more than 250 members are passive until marked active.
            self.gateway.set_guild_active(id, true);
            self.active_guild = Some(id.clone());
        }
        if let Some(cached) = self.channel_cache.get(&guild_id) {
            let channels = cached.clone();
            self.emit(Event::Channels { guild_id, channels });
            return;
        }
        let result = match &guild_id {
            Some(id) => self.session.client().channels(id).await,
            None => self.session.client().dm_channels().await,
        };
        match result {
            Ok(list) => {
                let channels: Vec<Channel> = list.iter().map(to_channel).collect();
                self.channel_cache.insert(guild_id.clone(), channels.clone());
                self.emit(Event::Channels { guild_id, channels });
            }
            Err(err) => self.fail("Could not load channels: ", err),
        }
    }

    pub async fn open_channel(&mut self, channel_id: String) {
        match self.session.client().messages(&channel_id, None, 50).await {
            Ok(list) => {
                // The API returns newest first; the view wants oldest first.
                let mut messages: Vec<Message> = list.iter().map(to_message).collect();
                messages.sort_by(|a, b| snowflake_key(&a.id).cmp(&snowflake_key(&b.id)));
                self.emit(Event::Messages { channel_id, messages });
            }
            Err(err) => self.fail("Could not load messages: ", err),
        }
    }

    pub async fn send(&mut self, channel_id: String, text: String) {
        let nonce = p::new_nonce();
        match self.session.client().send_message(&channel_id, &text, Some(&nonce)).await {
            Ok(message) => self.emit(Event::MessageCreated(to_message(&message))),
            Err(err) => self.fail("Could not send: ", err),
        }
    }
}

impl Drop for RealSession {
    fn drop(&mut self) {
        self.gateway.close();
        self.forward.abort();
    }
}

fn forward_gateway_event(
    event: p::GatewayEvent,
    events: &UnboundedSender<Event>,
    guilds: &Arc<Mutex<Vec<Guild>>>,
    ended: &TokioSender<String>,
) {
    let send = |e: Event| {
        let _ = events.unbounded_send(e);
    };
    match event {
        p::GatewayEvent::State(state) => {
            let text = match state {
                p::ConnectionState::Connecting { attempt: 0 } => "Connecting\u{2026}".to_string(),
                p::ConnectionState::Connecting { attempt } => format!("Connecting (attempt {})\u{2026}", attempt + 1),
                p::ConnectionState::Identifying | p::ConnectionState::Resuming => "Connecting\u{2026}".into(),
                p::ConnectionState::Connected => "Connected".into(),
                p::ConnectionState::Reconnecting { delay, .. } => {
                    format!("Reconnecting in {} s\u{2026}", delay.as_secs())
                }
                p::ConnectionState::Closed(p::CloseReason::AuthenticationFailed) => {
                    let _ = ended.send("The instance ended this session. Sign in again.".into());
                    "Disconnected".into()
                }
                p::ConnectionState::Closed(reason) => format!("Disconnected ({reason:?})"),
            };
            send(Event::Status(text));
        }
        p::GatewayEvent::Dispatch { event, .. } => match event {
            p::Dispatch::MessageCreate(m) => send(Event::MessageCreated(to_message(&m))),
            p::Dispatch::MessageUpdate(m) => send(Event::MessageUpdated(to_message(&m))),
            p::Dispatch::MessageDelete(d) => {
                send(Event::MessageDeleted { channel_id: d.channel_id, message_id: d.id })
            }
            p::Dispatch::GuildCreate(g) => {
                let mut list = guilds.lock().unwrap();
                if !list.iter().any(|x| x.id == g.id) {
                    if let Some(name) = g.name() {
                        list.push(Guild { id: g.id.clone(), name: name.to_string() });
                        send(Event::Guilds(list.clone()));
                    }
                }
            }
            p::Dispatch::GuildDelete(d) if !d.unavailable => {
                let mut list = guilds.lock().unwrap();
                list.retain(|g| g.id != d.id);
                send(Event::Guilds(list.clone()));
            }
            _ => {}
        },
    }
}
