//! The gateway: one WebSocket per session that delivers live events.
//!
//! [`Gateway::connect`] spawns a task on the current tokio runtime and returns
//! a [`GatewayHandle`] for commands and a receiver of [`GatewayEvent`]s. The
//! task connects to `endpoints.gateway` with
//! `?v=1&encoding=json&compress=zstd-stream&stream=1`, waits for Hello,
//! heartbeats (first beat after a random part of the interval), sends Identify
//! (or Resume when it has a session), and reconnects with exponential backoff
//! and jitter. It resumes within Fluxer's 60-second window and identifies
//! again after Invalid Session. It stops for good on close codes that a retry
//! cannot fix (bad token 4004, version 4012, shard 4010/4011), on
//! [`GatewayHandle::close`], or when the event receiver is dropped.
//!
//! Large guilds (over 250 members) are passive until you call
//! [`GatewayHandle::set_guild_active`]: a passive session gets no
//! MESSAGE_CREATE there except mentions. Call it for the guild on screen.

pub mod codec;

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use rand::Rng;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout, Instant};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::instance::Instance;
use crate::models::{Channel, GuildDelete, GuildReady, Message, MessageDelete, Ready, TypingStart};
use crate::token::Token;
use codec::ZstdStream;

/// Identify session flag: merge DM reaction bursts.
pub const FLAG_DEBOUNCE_MESSAGE_REACTIONS: u32 = 1 << 1;
/// Identify session flag: the client understands threads, forums and media channels.
pub const FLAG_CHANNEL_THREADS: u32 = 1 << 2;
/// Identify session flag: receive guilds the account is visiting.
pub const FLAG_GUILD_VISITS: u32 = 1 << 3;

/// The `properties` of Identify.
#[derive(Debug, Clone)]
pub struct IdentifyProperties {
    /// Operating system.
    pub os: String,
    /// Client library identifier.
    pub browser: String,
    /// Device or application identifier.
    pub device: String,
}

impl Default for IdentifyProperties {
    fn default() -> Self {
        Self {
            os: crate::instance::os_name().to_owned(),
            browser: "Fluxins".to_owned(),
            device: "desktop".to_owned(),
        }
    }
}

/// Gateway settings.
#[derive(Debug, Clone)]
pub struct GatewayOptions {
    /// Identify properties.
    pub properties: IdentifyProperties,
    /// Session flags. Default: the three flags the official client sends.
    pub flags: u32,
    /// Use `zstd-stream` compression (default true).
    pub compress: bool,
    /// Event names the session does not want (at most 256).
    pub ignored_events: Vec<String>,
    /// A guild that starts active.
    pub initial_guild_id: Option<String>,
    /// First reconnect delay.
    pub backoff_base: Duration,
    /// Longest reconnect delay.
    pub backoff_max: Duration,
    /// Capacity of the event channel. When full, the gateway task waits.
    pub event_buffer: usize,
}

impl Default for GatewayOptions {
    fn default() -> Self {
        Self {
            properties: IdentifyProperties::default(),
            flags: FLAG_DEBOUNCE_MESSAGE_REACTIONS | FLAG_CHANNEL_THREADS | FLAG_GUILD_VISITS,
            compress: true,
            ignored_events: Vec::new(),
            initial_guild_id: None,
            backoff_base: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
            event_buffer: 1024,
        }
    }
}

/// Why the gateway stopped for good.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseReason {
    /// [`GatewayHandle::close`] was called or the handle was dropped.
    ClientClosed,
    /// Close 4004: the token is invalid or revoked. Sign in again.
    AuthenticationFailed,
    /// Close 4012: the instance does not speak gateway v1.
    InvalidApiVersion,
    /// Close 4010 or 4011.
    InvalidShard,
}

/// Connection state changes, for a status indicator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionState {
    /// Opening the WebSocket (attempt 0 is the first).
    Connecting {
        /// Reconnect attempt number.
        attempt: u32,
    },
    /// Hello received, Identify sent.
    Identifying,
    /// Hello received, Resume sent.
    Resuming,
    /// READY or RESUMED arrived; events flow.
    Connected,
    /// The connection dropped; next try after `delay`.
    Reconnecting {
        /// Wait before the next attempt.
        delay: Duration,
        /// What happened, for logs.
        reason: String,
    },
    /// Stopped for good. No more events follow.
    Closed(CloseReason),
}

/// A gateway Dispatch, typed where this crate knows the event.
#[derive(Debug, Clone, PartialEq)]
pub enum Dispatch {
    /// Initial state after Identify.
    Ready(Box<Ready>),
    /// A Resume finished replaying missed events.
    Resumed,
    /// A guild became available (or full state after joining).
    GuildCreate(Box<GuildReady>),
    /// A guild's full state after an active/passive switch.
    GuildSync(Box<GuildReady>),
    /// The session lost a guild, or it became unavailable.
    GuildDelete(GuildDelete),
    /// New channel.
    ChannelCreate(Box<Channel>),
    /// Changed channel.
    ChannelUpdate(Box<Channel>),
    /// Deleted channel.
    ChannelDelete(Box<Channel>),
    /// New message.
    MessageCreate(Box<Message>),
    /// Edited message (full object).
    MessageUpdate(Box<Message>),
    /// Deleted message.
    MessageDelete(MessageDelete),
    /// Someone started typing.
    TypingStart(TypingStart),
    /// Any other event, or a known one whose payload did not parse.
    Other {
        /// Event name, for example `PRESENCE_UPDATE`.
        name: String,
        /// Raw payload.
        data: Value,
    },
}

impl Dispatch {
    /// Parses one Dispatch by event name.
    pub fn parse(name: &str, data: Value) -> Dispatch {
        fn typed<T: serde::de::DeserializeOwned>(name: &str, data: Value, wrap: impl FnOnce(T) -> Dispatch) -> Dispatch {
            match serde_json::from_value::<T>(data.clone()) {
                Ok(v) => wrap(v),
                Err(e) => {
                    tracing::warn!(event = name, error = %e, "gateway event did not parse; passing it on raw");
                    Dispatch::Other {
                        name: name.to_owned(),
                        data,
                    }
                }
            }
        }
        match name {
            "READY" => typed(name, data, |v| Dispatch::Ready(Box::new(v))),
            "RESUMED" => Dispatch::Resumed,
            "GUILD_CREATE" => typed(name, data, |v| Dispatch::GuildCreate(Box::new(v))),
            "GUILD_SYNC" => typed(name, data, |v| Dispatch::GuildSync(Box::new(v))),
            "GUILD_DELETE" => typed(name, data, Dispatch::GuildDelete),
            "CHANNEL_CREATE" => typed(name, data, |v| Dispatch::ChannelCreate(Box::new(v))),
            "CHANNEL_UPDATE" => typed(name, data, |v| Dispatch::ChannelUpdate(Box::new(v))),
            "CHANNEL_DELETE" => typed(name, data, |v| Dispatch::ChannelDelete(Box::new(v))),
            "MESSAGE_CREATE" => typed(name, data, |v| Dispatch::MessageCreate(Box::new(v))),
            "MESSAGE_UPDATE" => typed(name, data, |v| Dispatch::MessageUpdate(Box::new(v))),
            "MESSAGE_DELETE" => typed(name, data, Dispatch::MessageDelete),
            "TYPING_START" => typed(name, data, Dispatch::TypingStart),
            _ => Dispatch::Other {
                name: name.to_owned(),
                data,
            },
        }
    }
}

/// What the gateway task reports.
#[derive(Debug, Clone, PartialEq)]
pub enum GatewayEvent {
    /// The connection state changed.
    State(ConnectionState),
    /// An event from the server, with its sequence number.
    Dispatch {
        /// Session sequence.
        seq: u64,
        /// The event.
        event: Dispatch,
    },
}

enum Command {
    Send(Value),
    Close,
}

/// Sends commands to the gateway task. Cheap to clone. Dropping every clone closes the gateway.
#[derive(Debug, Clone)]
pub struct GatewayHandle {
    tx: mpsc::UnboundedSender<Command>,
}

impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Command::Send(v) => write!(f, "Send(op {})", v.get("op").unwrap_or(&Value::Null)),
            Command::Close => f.write_str("Close"),
        }
    }
}

impl GatewayHandle {
    /// Sends a raw command (`op` + `d`). Commands sent while disconnected are dropped.
    pub fn send_raw(&self, op: u8, d: Value) {
        let _ = self.tx.send(Command::Send(json!({ "op": op, "d": d })));
    }

    /// Lazy Request (op 14): makes a guild active (full message and typing
    /// events) or passive. Call it for the guild the person is looking at.
    pub fn set_guild_active(&self, guild_id: &str, active: bool) {
        let mut subs = serde_json::Map::new();
        subs.insert(guild_id.to_owned(), json!({ "active": active, "typing": active }));
        self.send_raw(14, json!({ "subscriptions": subs }));
    }

    /// Presence Update (op 3): `online`, `idle`, `dnd` or `invisible`.
    pub fn set_status(&self, status: &str) {
        self.send_raw(3, json!({ "status": status, "afk": false, "mobile": false }));
    }

    /// Closes the connection and ends the session (no resume).
    pub fn close(&self) {
        let _ = self.tx.send(Command::Close);
    }
}

/// Entry point.
pub struct Gateway;

impl Gateway {
    /// Spawns the gateway task on the current tokio runtime.
    ///
    /// # Panics
    /// Outside a tokio runtime.
    pub fn connect(
        instance: Instance,
        token: Token,
        options: GatewayOptions,
    ) -> (GatewayHandle, mpsc::Receiver<GatewayEvent>) {
        let (event_tx, event_rx) = mpsc::channel(options.event_buffer.max(1));
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let url = instance.discovery().endpoints.gateway.clone();
        let user_agent = instance.client_info().user_agent.clone();
        let task = Task {
            url,
            user_agent,
            token,
            options,
            events: event_tx,
            commands: cmd_rx,
            session: None,
            seq: None,
        };
        tokio::spawn(task.run());
        (GatewayHandle { tx: cmd_tx }, event_rx)
    }
}

/// How one connection ended.
enum Outcome {
    /// Stop for good.
    Stop(CloseReason),
    /// The event receiver is gone; stop quietly.
    ReceiverGone,
    /// Connect again. `fresh` drops the session so the next try identifies.
    Reconnect { reason: String, fresh: bool, was_connected: bool },
}

struct Task {
    url: String,
    user_agent: String,
    token: Token,
    options: GatewayOptions,
    events: mpsc::Sender<GatewayEvent>,
    commands: mpsc::UnboundedReceiver<Command>,
    session: Option<String>,
    seq: Option<u64>,
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn tls_connector() -> Option<tokio_tungstenite::Connector> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .ok()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Some(tokio_tungstenite::Connector::Rustls(Arc::new(config)))
}

/// The gateway URL with the connection parameters added (keeps any existing query).
pub fn gateway_url(base: &str, compress: bool) -> Result<url::Url, url::ParseError> {
    let mut url = url::Url::parse(base)?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("v", "1").append_pair("encoding", "json");
        if compress {
            q.append_pair("compress", "zstd-stream").append_pair("stream", "1");
        }
    }
    Ok(url)
}

impl Task {
    async fn emit(&self, event: GatewayEvent) -> bool {
        self.events.send(event).await.is_ok()
    }

    async fn run(mut self) {
        let mut attempt: u32 = 0;
        loop {
            if !self.emit(GatewayEvent::State(ConnectionState::Connecting { attempt })).await {
                return;
            }
            match self.connect_once().await {
                Outcome::Stop(reason) => {
                    let _ = self.emit(GatewayEvent::State(ConnectionState::Closed(reason))).await;
                    return;
                }
                Outcome::ReceiverGone => return,
                Outcome::Reconnect {
                    reason,
                    fresh,
                    was_connected,
                } => {
                    if fresh {
                        self.session = None;
                        self.seq = None;
                    }
                    attempt = if was_connected { 0 } else { attempt.saturating_add(1) };
                    let delay = self.backoff(attempt);
                    tracing::info!(%reason, ?delay, "gateway reconnecting");
                    if !self
                        .emit(GatewayEvent::State(ConnectionState::Reconnecting { delay, reason }))
                        .await
                    {
                        return;
                    }
                    // Wait, but stop early on Close.
                    let deadline = sleep(delay);
                    tokio::pin!(deadline);
                    loop {
                        tokio::select! {
                            _ = &mut deadline => break,
                            cmd = self.commands.recv() => match cmd {
                                Some(Command::Send(_)) => {}
                                Some(Command::Close) | None => {
                                    let _ = self.emit(GatewayEvent::State(ConnectionState::Closed(CloseReason::ClientClosed))).await;
                                    return;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    fn backoff(&self, attempt: u32) -> Duration {
        let base = self.options.backoff_base.as_secs_f64();
        let max = self.options.backoff_max.as_secs_f64();
        let exp = (base * 2f64.powi(attempt.min(16) as i32)).min(max);
        // Equal jitter: between half and all of the exponential delay.
        let jittered = exp * rand::thread_rng().gen_range(0.5..=1.0);
        Duration::from_secs_f64(jittered)
    }

    async fn connect_once(&mut self) -> Outcome {
        let reconnect = |reason: String| Outcome::Reconnect {
            reason,
            fresh: false,
            was_connected: false,
        };
        let url = match gateway_url(&self.url, self.options.compress) {
            Ok(u) => u,
            Err(e) => return Outcome::Stop(CloseReason::InvalidApiVersion).also_log(&format!("bad gateway URL: {e}")),
        };
        let mut request = match url.as_str().into_client_request() {
            Ok(r) => r,
            Err(e) => return reconnect(format!("request: {e}")),
        };
        if let Ok(ua) = self.user_agent.parse() {
            request.headers_mut().insert("User-Agent", ua);
        }
        let connected = timeout(
            Duration::from_secs(20),
            tokio_tungstenite::connect_async_tls_with_config(request, None, true, tls_connector()),
        )
        .await;
        let mut ws: Ws = match connected {
            Ok(Ok((ws, _))) => ws,
            Ok(Err(e)) => return reconnect(format!("connect failed: {e}")),
            Err(_) => return reconnect("connect timed out".into()),
        };
        let mut codec = if self.options.compress {
            match ZstdStream::new() {
                Ok(c) => Some(c),
                Err(e) => return reconnect(format!("zstd: {e}")),
            }
        } else {
            None
        };

        // Hello.
        let hello = match timeout(Duration::from_secs(20), read_payload(&mut ws, &mut codec)).await {
            Ok(Ok(Some(p))) => p,
            Ok(Ok(None)) => return reconnect("closed before Hello".into()),
            Ok(Err(ReadEnd::Closed(code, reason))) => return self.classify_close(code, reason, false),
            Ok(Err(ReadEnd::Error(e))) => return reconnect(e),
            Err(_) => return reconnect("no Hello within 20 s".into()),
        };
        if hello.get("op").and_then(Value::as_u64) != Some(10) {
            return reconnect("first payload was not Hello".into());
        }
        let interval_ms = hello["d"]["heartbeat_interval"].as_u64().unwrap_or(41_250).max(100);
        let interval = Duration::from_millis(interval_ms);

        // Identify or Resume.
        let first = match &self.session {
            Some(session_id) => {
                let _ = self.emit(GatewayEvent::State(ConnectionState::Resuming)).await;
                json!({ "op": 6, "d": {
                    "token": self.token.expose_secret(),
                    "session_id": session_id,
                    "seq": self.seq.unwrap_or(0),
                }})
            }
            None => {
                let _ = self.emit(GatewayEvent::State(ConnectionState::Identifying)).await;
                self.identify_payload()
            }
        };
        if let Err(e) = send_payload(&mut ws, &mut codec, &first).await {
            return reconnect(e);
        }

        let first_beat = interval.mul_f64(rand::thread_rng().gen_range(0.0..1.0));
        let heartbeat = sleep(first_beat);
        tokio::pin!(heartbeat);
        let mut acked = true;
        let mut connected = false;

        loop {
            tokio::select! {
                _ = &mut heartbeat => {
                    if !acked {
                        let _ = close(&mut ws, 4000).await;
                        return Outcome::Reconnect { reason: "heartbeat not acknowledged".into(), fresh: false, was_connected: connected };
                    }
                    if let Err(e) = send_payload(&mut ws, &mut codec, &json!({ "op": 1, "d": self.seq })).await {
                        return Outcome::Reconnect { reason: e, fresh: false, was_connected: connected };
                    }
                    acked = false;
                    heartbeat.as_mut().reset(Instant::now() + interval);
                }
                cmd = self.commands.recv() => match cmd {
                    Some(Command::Send(payload)) => {
                        if let Err(e) = send_payload(&mut ws, &mut codec, &payload).await {
                            return Outcome::Reconnect { reason: e, fresh: false, was_connected: connected };
                        }
                    }
                    Some(Command::Close) | None => {
                        let _ = close(&mut ws, 1000).await;
                        return Outcome::Stop(CloseReason::ClientClosed);
                    }
                },
                read = read_payload(&mut ws, &mut codec) => {
                    let payload = match read {
                        Ok(Some(p)) => p,
                        Ok(None) => return Outcome::Reconnect { reason: "connection closed".into(), fresh: false, was_connected: connected },
                        Err(ReadEnd::Closed(code, reason)) => return self.classify_close(code, reason, connected),
                        Err(ReadEnd::Error(e)) => return Outcome::Reconnect { reason: e, fresh: false, was_connected: connected },
                    };
                    match payload.get("op").and_then(Value::as_u64) {
                        Some(0) => {
                            let seq = payload.get("s").and_then(Value::as_u64);
                            let name = payload.get("t").and_then(Value::as_str).unwrap_or_default().to_owned();
                            let data = payload.get("d").cloned().unwrap_or(Value::Null);
                            if name == "READY" {
                                if let Some(id) = data.get("session_id").and_then(Value::as_str) {
                                    self.session = Some(id.to_owned());
                                }
                            }
                            if let Some(s) = seq {
                                self.seq = Some(self.seq.map_or(s, |old| old.max(s)));
                            }
                            if (name == "READY" || name == "RESUMED") && !connected {
                                connected = true;
                                if !self.emit(GatewayEvent::State(ConnectionState::Connected)).await {
                                    return Outcome::ReceiverGone;
                                }
                            }
                            let event = Dispatch::parse(&name, data);
                            if !self.emit(GatewayEvent::Dispatch { seq: seq.unwrap_or(0), event }).await {
                                let _ = close(&mut ws, 1000).await;
                                return Outcome::ReceiverGone;
                            }
                        }
                        Some(1) => {
                            if let Err(e) = send_payload(&mut ws, &mut codec, &json!({ "op": 1, "d": self.seq })).await {
                                return Outcome::Reconnect { reason: e, fresh: false, was_connected: connected };
                            }
                        }
                        Some(7) => {
                            let _ = close(&mut ws, 4000).await;
                            return Outcome::Reconnect { reason: "server asked to reconnect".into(), fresh: false, was_connected: connected };
                        }
                        Some(9) => {
                            // The session cannot be resumed; identify again on this socket.
                            self.session = None;
                            self.seq = None;
                            let wait = Duration::from_millis(rand::thread_rng().gen_range(1_000..=5_000));
                            sleep(wait).await;
                            let _ = self.emit(GatewayEvent::State(ConnectionState::Identifying)).await;
                            let identify = self.identify_payload();
                            if let Err(e) = send_payload(&mut ws, &mut codec, &identify).await {
                                return Outcome::Reconnect { reason: e, fresh: true, was_connected: connected };
                            }
                        }
                        Some(11) => acked = true,
                        _ => {}
                    }
                }
            }
        }
    }

    fn identify_payload(&self) -> Value {
        let o = &self.options;
        let mut d = json!({
            "token": self.token.expose_secret(),
            "properties": {
                "os": o.properties.os,
                "browser": o.properties.browser,
                "device": o.properties.device,
            },
            "flags": o.flags,
        });
        if !o.ignored_events.is_empty() {
            d["ignored_events"] = json!(o.ignored_events);
        }
        if let Some(g) = &o.initial_guild_id {
            d["initial_guild_id"] = json!(g);
        }
        json!({ "op": 2, "d": d })
    }

    fn classify_close(&self, code: Option<u16>, reason: String, was_connected: bool) -> Outcome {
        let text = format!("closed with {} {reason}", code.map_or("no code".into(), |c| c.to_string()));
        match code {
            Some(4004) => Outcome::Stop(CloseReason::AuthenticationFailed),
            Some(4012) => Outcome::Stop(CloseReason::InvalidApiVersion),
            Some(4010) | Some(4011) => Outcome::Stop(CloseReason::InvalidShard),
            // Bad sequence or a protocol fault: start a fresh session.
            Some(4002) | Some(4003) | Some(4005) | Some(4007) => Outcome::Reconnect {
                reason: text,
                fresh: true,
                was_connected,
            },
            _ => Outcome::Reconnect {
                reason: text,
                fresh: false,
                was_connected,
            },
        }
    }
}

trait AlsoLog {
    fn also_log(self, msg: &str) -> Self;
}

impl AlsoLog for Outcome {
    fn also_log(self, msg: &str) -> Self {
        tracing::error!("{msg}");
        self
    }
}

enum ReadEnd {
    Closed(Option<u16>, String),
    Error(String),
}

/// Reads the next JSON payload. `Ok(None)` means the stream ended.
async fn read_payload(ws: &mut Ws, codec: &mut Option<ZstdStream>) -> Result<Option<Value>, ReadEnd> {
    loop {
        let msg = match ws.next().await {
            None => return Ok(None),
            Some(Err(e)) => return Err(ReadEnd::Error(format!("read: {e}"))),
            Some(Ok(m)) => m,
        };
        let bytes: Vec<u8> = match msg {
            WsMessage::Text(t) => t.as_str().as_bytes().to_vec(),
            WsMessage::Binary(b) => match codec {
                Some(c) => c
                    .decompress(&b)
                    .map_err(|e| ReadEnd::Error(format!("zstd: {e}")))?,
                None => b.to_vec(),
            },
            WsMessage::Close(frame) => {
                let (code, reason) = frame
                    .map(|f| (Some(u16::from(f.code)), f.reason.as_str().to_owned()))
                    .unwrap_or((None, String::new()));
                return Err(ReadEnd::Closed(code, reason));
            }
            _ => continue,
        };
        if bytes.is_empty() {
            continue;
        }
        return serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| ReadEnd::Error(format!("bad JSON from gateway: {e}")));
    }
}

async fn send_payload(ws: &mut Ws, codec: &mut Option<ZstdStream>, payload: &Value) -> Result<(), String> {
    let text = payload.to_string();
    let msg = match codec {
        Some(c) => WsMessage::binary(c.compress(text.as_bytes()).map_err(|e| format!("zstd: {e}"))?),
        None => WsMessage::text(text),
    };
    ws.send(msg).await.map_err(|e| format!("send: {e}"))
}

async fn close(ws: &mut Ws, code: u16) -> Result<(), String> {
    ws.close(Some(CloseFrame {
        code: CloseCode::from(code),
        reason: "".into(),
    }))
    .await
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_keeps_query() {
        let u = gateway_url("wss://example.org/gateway?region=eu", true).unwrap();
        assert_eq!(
            u.as_str(),
            "wss://example.org/gateway?region=eu&v=1&encoding=json&compress=zstd-stream&stream=1"
        );
    }

    #[test]
    fn unknown_event_passes_through() {
        let d = Dispatch::parse("PRESENCE_UPDATE", json!({"x": 1}));
        assert_eq!(
            d,
            Dispatch::Other {
                name: "PRESENCE_UPDATE".into(),
                data: json!({"x": 1})
            }
        );
        // A known event with a broken payload is passed through raw too.
        assert!(matches!(Dispatch::parse("MESSAGE_CREATE", json!({})), Dispatch::Other { .. }));
    }
}
