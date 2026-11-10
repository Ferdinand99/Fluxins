//! The gateway: one WebSocket per session that delivers live events.
//!
//! [`Gateway::connect`] spawns a task on the current tokio runtime and returns
//! a [`GatewayHandle`] for commands and a receiver of [`GatewayEvent`]s. The
//! task connects to `endpoints.gateway` (which must be `wss://`, or `ws://` on
//! a loopback host) with `?v=1&encoding=json&compress=zstd-stream&stream=1`,
//! waits for Hello, heartbeats (first beat after a random part of the
//! interval), sends Identify (or Resume when it has a session), and reconnects
//! with exponential backoff and jitter.
//!
//! - It resumes within Fluxer's 60-second window and identifies again after
//!   Invalid Session, at most once every 5 seconds.
//! - The backoff only resets after a connection has been stable for 60 s.
//!   Close 4008 (rate limited) and protocol errors wait at least 30 s.
//! - It stops for good on close codes a retry cannot fix (4004 bad token,
//!   4010/4011 shard, 4012 version), on [`GatewayHandle::close`], when every
//!   handle is dropped, or when the event receiver is dropped.
//! - Heartbeats keep running when the event channel is full: events wait in
//!   a buffer of up to 4,096; past that the task drops the socket and resumes
//!   once the receiver has caught up.
//!
//! Large guilds (over 250 members) are passive until you call
//! [`GatewayHandle::set_guild_active`]: a passive session gets no
//! MESSAGE_CREATE there except mentions. The task remembers active guilds and
//! the status, and sends them again after every Identify and Resume. Commands
//! sent while disconnected are kept (up to 64) and sent once connected.

pub mod codec;

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use rand::Rng;
use serde_json::{json, Value};
use tokio::sync::{mpsc, watch};
use tokio::time::{sleep, timeout, Instant};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::instance::{require_scheme, Instance};
use crate::models::{Channel, GuildDelete, GuildReady, Message, MessageDelete, Ready, TypingStart};
use crate::token::Token;
use codec::ZstdStream;

/// Identify session flag: merge DM reaction bursts.
pub const FLAG_DEBOUNCE_MESSAGE_REACTIONS: u32 = 1 << 1;
/// Identify session flag: the client understands threads, forums and media channels.
pub const FLAG_CHANNEL_THREADS: u32 = 1 << 2;
/// Identify session flag: receive guilds the account is visiting.
pub const FLAG_GUILD_VISITS: u32 = 1 << 3;

/// Shortest time between two Identify payloads.
const IDENTIFY_SPACING: Duration = Duration::from_secs(5);
/// A connection that lasted this long after READY resets the backoff.
const STABLE_AFTER: Duration = Duration::from_secs(60);
/// Shortest wait after close 4008 or a protocol error.
const LONG_BACKOFF: Duration = Duration::from_secs(30);
/// How long to wait for READY/RESUMED after Identify/Resume.
const READY_TIMEOUT: Duration = Duration::from_secs(90);
/// Events kept while the receiver is behind. Past this the connection is
/// dropped and resumed later (the server replays what was missed).
const MAX_PENDING: usize = 4_096;
/// Commands kept while disconnected.
const MAX_QUEUED: usize = 64;
/// Capacity of the command channel.
const COMMAND_BUFFER: usize = 256;
/// Active guilds remembered for replay (the UI shows one at a time). They go
/// out in a single Lazy Request, which stays well under the 16 KiB limit.
const MAX_ACTIVE_GUILDS: usize = 50;
/// Fluxer runs at most 4 bounded commands (op 8, 14, 15, 16, 28) at once per
/// socket and drops the rest silently: send at most this many per window.
const BOUNDED_PER_WINDOW: usize = 4;
const BOUNDED_WINDOW: Duration = Duration::from_secs(1);
/// Fluxer accepts 5 Presence Updates per 20 s and drops the rest.
const PRESENCE_PER_WINDOW: usize = 5;
const PRESENCE_WINDOW: Duration = Duration::from_secs(20);
/// Allowed `heartbeat_interval` range.
const HEARTBEAT_MIN: Duration = Duration::from_secs(1);
const HEARTBEAT_MAX: Duration = Duration::from_secs(120);

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
            browser: "Inlet".to_owned(),
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
    /// Capacity of the event channel. When it is full, the task keeps reading
    /// and heartbeating and holds up to 4,096 more events in memory; past that
    /// it drops the socket and resumes once the receiver has caught up (the
    /// server replays what was missed). Read the receiver promptly: with large
    /// messages the held events can take a lot of memory.
    pub event_buffer: usize,
    /// Largest gateway message accepted, compressed or not (default 32 MiB).
    pub max_message_size: usize,
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
            max_message_size: codec::DEFAULT_MAX_MESSAGE,
        }
    }
}

/// Why the gateway stopped for good.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CloseReason {
    /// [`GatewayHandle::close`] was called or every handle was dropped.
    ClientClosed,
    /// Close 4004: the token is invalid or revoked. Sign in again.
    AuthenticationFailed,
    /// Close 4012: the instance does not speak gateway v1.
    InvalidApiVersion,
    /// Close 4010 or 4011.
    InvalidShard,
    /// The gateway URL is not usable (for example not `wss://`).
    InvalidUrl(String),
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
#[non_exhaustive]
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
        fn typed<T: serde::de::DeserializeOwned>(
            name: &str,
            data: Value,
            wrap: impl FnOnce(T) -> Dispatch,
        ) -> Dispatch {
            match T::deserialize(&data) {
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
    Raw(Value),
    GuildActive(String, bool),
    Status(String),
}

impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Command::Raw(v) => write!(f, "Raw(op {})", v.get("op").unwrap_or(&Value::Null)),
            Command::GuildActive(g, a) => write!(f, "GuildActive({g}, {a})"),
            Command::Status(s) => write!(f, "Status({s})"),
        }
    }
}

/// Sends commands to the gateway task. Cheap to clone. Dropping every clone closes the gateway.
#[derive(Debug, Clone)]
pub struct GatewayHandle {
    tx: mpsc::Sender<Command>,
    close: Arc<watch::Sender<bool>>,
}

impl GatewayHandle {
    fn push(&self, cmd: Command) {
        if let Err(e) = self.tx.try_send(cmd) {
            tracing::warn!("gateway command dropped: {e}");
        }
    }

    /// Sends a raw command (`op` + `d`). Commands sent while disconnected are
    /// queued (up to 64) and sent after the next READY or RESUMED.
    pub fn send_raw(&self, op: u8, d: Value) {
        self.push(Command::Raw(json!({ "op": op, "d": d })));
    }

    /// Lazy Request (op 14): makes a guild active (full message and typing
    /// events) or passive. Call it for the guild the person is looking at.
    /// The gateway remembers it and sends it again after a reconnect.
    pub fn set_guild_active(&self, guild_id: &str, active: bool) {
        self.push(Command::GuildActive(guild_id.to_owned(), active));
    }

    /// Presence Update (op 3): `online`, `idle`, `dnd` or `invisible`. Remembered across reconnects.
    pub fn set_status(&self, status: &str) {
        self.push(Command::Status(status.to_owned()));
    }

    /// Closes the connection and ends the session (no resume).
    pub fn close(&self) {
        let _ = self.close.send(true);
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
        let (cmd_tx, cmd_rx) = mpsc::channel(COMMAND_BUFFER);
        let (close_tx, close_rx) = watch::channel(false);
        let task = Task {
            url: instance.gateway_url().to_owned(),
            user_agent: instance.client_info().user_agent.clone(),
            token,
            options,
            events: event_tx,
            commands: cmd_rx,
            close: close_rx,
            session: None,
            seq: None,
            last_identify: None,
            active_guilds: BTreeMap::new(),
            status: None,
            queued: VecDeque::new(),
            pending: VecDeque::new(),
        };
        tokio::spawn(task.run());
        (
            GatewayHandle {
                tx: cmd_tx,
                close: Arc::new(close_tx),
            },
            event_rx,
        )
    }
}

/// How one connection ended.
enum Outcome {
    /// Stop for good.
    Stop(CloseReason),
    /// The event receiver is gone; stop quietly.
    ReceiverGone,
    /// Connect again.
    Reconnect {
        reason: String,
        /// Drop the session so the next try identifies.
        fresh: bool,
        /// The connection was stable long enough to reset the backoff.
        stable: bool,
        /// Wait at least this long.
        min_delay: Duration,
    },
}

impl Outcome {
    fn retry(reason: impl Into<String>) -> Self {
        Outcome::Reconnect {
            reason: reason.into(),
            fresh: false,
            stable: false,
            min_delay: Duration::ZERO,
        }
    }
}

struct Task {
    url: String,
    user_agent: String,
    token: Token,
    options: GatewayOptions,
    events: mpsc::Sender<GatewayEvent>,
    commands: mpsc::Receiver<Command>,
    close: watch::Receiver<bool>,
    session: Option<String>,
    seq: Option<u64>,
    last_identify: Option<Instant>,
    /// Guilds the UI made active; replayed after Identify and Resume.
    active_guilds: BTreeMap<String, bool>,
    /// Last status the UI set; replayed after Identify and Resume.
    status: Option<String>,
    /// Raw commands sent while disconnected.
    queued: VecDeque<Value>,
    /// Events waiting for room in the event channel.
    pending: VecDeque<GatewayEvent>,
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// TLS for the gateway: rustls with the ring provider and the Mozilla roots.
pub fn tls_connector() -> Option<tokio_tungstenite::Connector> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
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
            q.append_pair("compress", "zstd-stream")
                .append_pair("stream", "1");
        }
    }
    Ok(url)
}

/// Clamps Hello's `heartbeat_interval` (milliseconds) to 1-120 s.
fn heartbeat_interval(hello: &Value) -> Duration {
    let ms = hello["d"]["heartbeat_interval"]
        .as_f64()
        .filter(|v| v.is_finite())
        .unwrap_or(41_250.0);
    Duration::from_millis(ms.clamp(
        HEARTBEAT_MIN.as_millis() as f64,
        HEARTBEAT_MAX.as_millis() as f64,
    ) as u64)
}

impl Task {
    async fn emit(&self, event: GatewayEvent) -> bool {
        self.events.send(event).await.is_ok()
    }

    async fn flush_pending(&mut self) -> bool {
        while let Some(ev) = self.pending.pop_front() {
            if self.events.send(ev).await.is_err() {
                return false;
            }
        }
        true
    }

    /// True after `close()` or once every handle is gone.
    fn closed(&self) -> bool {
        *self.close.borrow() || self.close.has_changed().is_err()
    }

    async fn run(mut self) {
        let mut attempt: u32 = 0;
        loop {
            if self.closed() {
                let _ = self
                    .emit(GatewayEvent::State(ConnectionState::Closed(
                        CloseReason::ClientClosed,
                    )))
                    .await;
                return;
            }
            if !self
                .emit(GatewayEvent::State(ConnectionState::Connecting { attempt }))
                .await
            {
                return;
            }
            let outcome = self.connect_once().await;
            if !self.flush_pending().await {
                return;
            }
            match outcome {
                Outcome::Stop(reason) => {
                    let _ = self
                        .emit(GatewayEvent::State(ConnectionState::Closed(reason)))
                        .await;
                    return;
                }
                Outcome::ReceiverGone => return,
                Outcome::Reconnect {
                    reason,
                    fresh,
                    stable,
                    min_delay,
                } => {
                    if fresh {
                        self.session = None;
                        self.seq = None;
                    }
                    attempt = if stable { 0 } else { attempt.saturating_add(1) };
                    let delay = self.backoff(attempt).max(min_delay);
                    tracing::info!(%reason, ?delay, "gateway reconnecting");
                    if !self
                        .emit(GatewayEvent::State(ConnectionState::Reconnecting {
                            delay,
                            reason,
                        }))
                        .await
                    {
                        return;
                    }
                    // Wait, keep track of commands, and stop early on close.
                    let deadline = sleep(delay);
                    tokio::pin!(deadline);
                    loop {
                        tokio::select! {
                            _ = &mut deadline => break,
                            _ = self.close.changed() => break,
                            cmd = self.commands.recv() => match cmd {
                                Some(c) => self.remember(c),
                                None => {
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
        if attempt == 0 {
            return Duration::ZERO;
        }
        let base = self.options.backoff_base.as_secs_f64();
        let max = self.options.backoff_max.as_secs_f64();
        let exp = (base * 2f64.powi(attempt.min(16) as i32 - 1)).min(max);
        // Equal jitter: between half and all of the exponential delay.
        let jittered = exp * rand::thread_rng().gen_range(0.5..=1.0);
        Duration::from_secs_f64(jittered)
    }

    /// Records a command while disconnected.
    fn remember(&mut self, cmd: Command) {
        match cmd {
            Command::GuildActive(g, true) => {
                if !self.active_guilds.contains_key(&g)
                    && self.active_guilds.len() >= MAX_ACTIVE_GUILDS
                {
                    self.active_guilds.pop_first();
                }
                self.active_guilds.insert(g, true);
            }
            Command::GuildActive(g, false) => {
                self.active_guilds.remove(&g);
            }
            Command::Status(s) => self.status = Some(s),
            Command::Raw(v) => {
                if self.queued.len() >= MAX_QUEUED {
                    tracing::warn!("gateway command queue full; dropping the oldest");
                    self.queued.pop_front();
                }
                self.queued.push_back(v);
            }
        }
    }

    /// Payloads for a command while connected (and records the wanted state).
    fn live(&mut self, cmd: Command) -> Option<Value> {
        let payload = match &cmd {
            Command::GuildActive(g, a) => Some(lazy_request(std::iter::once((g.as_str(), *a)))),
            Command::Status(s) => Some(presence(s)),
            Command::Raw(v) => Some(v.clone()),
        };
        if !matches!(cmd, Command::Raw(_)) {
            self.remember(cmd);
        }
        payload
    }

    /// What to send after READY or RESUMED: subscriptions, status, queued commands.
    fn replay(&mut self) -> Vec<Value> {
        let mut out = Vec::new();
        let active: Vec<(String, bool)> = self
            .active_guilds
            .iter()
            .map(|(g, a)| (g.clone(), *a))
            .collect();
        if !active.is_empty() {
            out.push(lazy_request(active.iter().map(|(g, a)| (g.as_str(), *a))));
        }
        if let Some(s) = &self.status {
            out.push(presence(s));
        }
        out.extend(self.queued.drain(..));
        out
    }

    fn ws_config(&self) -> WebSocketConfig {
        let mut config = WebSocketConfig::default();
        config.max_message_size = Some(self.options.max_message_size);
        config.max_frame_size = Some(self.options.max_message_size);
        config
    }

    async fn connect_once(&mut self) -> Outcome {
        let checked = require_scheme(&self.url, "wss", "ws", "gateway URL")
            .map_err(|e| e.to_string())
            .and_then(|_| gateway_url(&self.url, self.options.compress).map_err(|e| e.to_string()));
        let url = match checked {
            Ok(u) => u,
            Err(e) => return Outcome::Stop(CloseReason::InvalidUrl(e)),
        };
        let mut request = match url.as_str().into_client_request() {
            Ok(r) => r,
            Err(e) => return Outcome::retry(format!("request: {e}")),
        };
        if let Ok(ua) = self.user_agent.parse() {
            request.headers_mut().insert("User-Agent", ua);
        }
        let connected = timeout(
            Duration::from_secs(20),
            tokio_tungstenite::connect_async_tls_with_config(
                request,
                Some(self.ws_config()),
                true,
                tls_connector(),
            ),
        )
        .await;
        let mut ws: Ws = match connected {
            Ok(Ok((ws, _))) => ws,
            Ok(Err(e)) => return Outcome::retry(format!("connect failed: {e}")),
            Err(_) => return Outcome::retry("connect timed out"),
        };
        let mut codec = if self.options.compress {
            match ZstdStream::with_limit(self.options.max_message_size) {
                Ok(c) => Some(c),
                Err(e) => return Outcome::retry(format!("zstd: {e}")),
            }
        } else {
            None
        };

        // Hello.
        let hello = match timeout(Duration::from_secs(20), read_payload(&mut ws, &mut codec)).await
        {
            Ok(Ok(Some(p))) => p,
            Ok(Ok(None)) => return Outcome::retry("closed before Hello"),
            Ok(Err(ReadEnd::Closed(code, reason))) => {
                return self.classify_close(code, reason, None)
            }
            Ok(Err(ReadEnd::Error(e))) => return Outcome::retry(e),
            Ok(Err(ReadEnd::BadData(e))) => {
                return Outcome::Reconnect {
                    reason: e,
                    fresh: false,
                    stable: false,
                    min_delay: LONG_BACKOFF,
                }
            }
            Err(_) => return Outcome::retry("no Hello within 20 s"),
        };
        if hello.get("op").and_then(Value::as_u64) != Some(10) {
            return Outcome::retry("first payload was not Hello");
        }
        let interval = heartbeat_interval(&hello);

        // Identify or Resume.
        let first = match self.session.clone() {
            Some(session_id) => {
                self.pending
                    .push_back(GatewayEvent::State(ConnectionState::Resuming));
                json!({ "op": 6, "d": {
                    "token": self.token.expose_secret(),
                    "session_id": session_id,
                    "seq": self.seq.unwrap_or(0),
                }})
            }
            None => {
                self.identify_spacing().await;
                self.pending
                    .push_back(GatewayEvent::State(ConnectionState::Identifying));
                self.identify_payload()
            }
        };
        if let Err(e) = send_payload(&mut ws, &mut codec, &first).await {
            return Outcome::retry(e);
        }

        let first_beat = interval.mul_f64(rand::thread_rng().gen_range(0.0..1.0));
        let heartbeat = sleep(first_beat);
        tokio::pin!(heartbeat);
        let ready_deadline = sleep(READY_TIMEOUT);
        tokio::pin!(ready_deadline);
        let mut acked = true;
        let mut connected_at: Option<Instant> = None;
        let mut pacer = Pacer::default();

        loop {
            let stable = connected_at.is_some_and(|t| t.elapsed() >= STABLE_AFTER);
            let reconnect = |reason: String, fresh: bool, min_delay: Duration| Outcome::Reconnect {
                reason,
                fresh,
                stable,
                min_delay,
            };
            let wake = pacer.next_wakeup();
            tokio::select! {
                _ = tokio::time::sleep_until(wake.unwrap_or_else(|| Instant::now() + HEARTBEAT_MAX)), if wake.is_some() => {
                    if let Err(e) = flush(&mut ws, &mut codec, &mut pacer, self.status.as_deref()).await {
                        return reconnect(e, false, Duration::ZERO);
                    }
                }
                _ = &mut heartbeat => {
                    if !acked {
                        let _ = close(&mut ws, 4000).await;
                        return reconnect("heartbeat not acknowledged".into(), false, Duration::ZERO);
                    }
                    if let Err(e) = send_payload(&mut ws, &mut codec, &json!({ "op": 1, "d": self.seq })).await {
                        return reconnect(e, false, Duration::ZERO);
                    }
                    acked = false;
                    heartbeat.as_mut().reset(Instant::now() + interval);
                }
                _ = &mut ready_deadline, if connected_at.is_none() => {
                    let _ = close(&mut ws, 4000).await;
                    return reconnect("no READY in time".into(), true, Duration::ZERO);
                }
                _ = self.close.changed() => {
                    let _ = close(&mut ws, 1000).await;
                    return Outcome::Stop(CloseReason::ClientClosed);
                }
                permit = self.events.clone().reserve_owned(), if !self.pending.is_empty() => match permit {
                    Ok(p) => {
                        if let Some(ev) = self.pending.pop_front() {
                            let _ = p.send(ev);
                        }
                    }
                    Err(_) => {
                        let _ = close(&mut ws, 1000).await;
                        return Outcome::ReceiverGone;
                    }
                },
                cmd = self.commands.recv() => match cmd {
                    Some(c) if connected_at.is_some() => {
                        if let Some(payload) = self.live(c) {
                            pacer.push(payload);
                        }
                        if let Err(e) = flush(&mut ws, &mut codec, &mut pacer, self.status.as_deref()).await {
                            return reconnect(e, false, Duration::ZERO);
                        }
                    }
                    // Commands before READY close the socket with 4003: keep them.
                    Some(c) => self.remember(c),
                    None => {
                        let _ = close(&mut ws, 1000).await;
                        return Outcome::Stop(CloseReason::ClientClosed);
                    }
                },
                read = read_payload(&mut ws, &mut codec) => {
                    let payload = match read {
                        Ok(Some(p)) => p,
                        Ok(None) => return reconnect("connection closed".into(), false, Duration::ZERO),
                        Err(ReadEnd::Closed(code, reason)) => return self.classify_close(code, reason, connected_at),
                        Err(ReadEnd::Error(e)) => return reconnect(e, false, Duration::ZERO),
                        Err(ReadEnd::BadData(e)) => {
                            // A bad or oversized frame from the server: wait longer.
                            return reconnect(e, false, LONG_BACKOFF);
                        }
                    };
                    match payload.get("op").and_then(Value::as_u64) {
                        Some(0) => {
                            let seq = payload.get("s").and_then(Value::as_u64);
                            let mut payload = payload;
                            let name = payload.get("t").and_then(Value::as_str).unwrap_or_default().to_owned();
                            let data = payload.get_mut("d").map(Value::take).unwrap_or(Value::Null);
                            if name == "READY" {
                                if let Some(id) = data.get("session_id").and_then(Value::as_str) {
                                    self.session = Some(id.to_owned());
                                }
                            }
                            if let Some(s) = seq {
                                self.seq = Some(self.seq.map_or(s, |old| old.max(s)));
                            }
                            let ready_now = (name == "READY" || name == "RESUMED") && connected_at.is_none();
                            if ready_now {
                                connected_at = Some(Instant::now());
                                self.pending.push_back(GatewayEvent::State(ConnectionState::Connected));
                            }
                            let event = Dispatch::parse(&name, data);
                            self.pending.push_back(GatewayEvent::Dispatch { seq: seq.unwrap_or(0), event });
                            if self.pending.len() > MAX_PENDING {
                                let _ = close(&mut ws, 4000).await;
                                return reconnect("the event receiver is too slow".into(), false, Duration::ZERO);
                            }
                            if ready_now {
                                pacer = Pacer::default();
                                for p in self.replay() {
                                    pacer.push(p);
                                }
                                if let Err(e) = flush(&mut ws, &mut codec, &mut pacer, self.status.as_deref()).await {
                                    return reconnect(e, false, Duration::ZERO);
                                }
                            }
                        }
                        Some(1) => {
                            if let Err(e) = send_payload(&mut ws, &mut codec, &json!({ "op": 1, "d": self.seq })).await {
                                return reconnect(e, false, Duration::ZERO);
                            }
                        }
                        Some(7) => {
                            let _ = close(&mut ws, 4000).await;
                            return reconnect("server asked to reconnect".into(), false, Duration::ZERO);
                        }
                        Some(9) => {
                            // The session cannot be resumed; identify again on this socket.
                            self.session = None;
                            self.seq = None;
                            connected_at = None;
                            ready_deadline.as_mut().reset(Instant::now() + READY_TIMEOUT);
                            let wait = Duration::from_millis(rand::thread_rng().gen_range(1_000..=5_000));
                            sleep(wait).await;
                            self.identify_spacing().await;
                            self.pending.push_back(GatewayEvent::State(ConnectionState::Identifying));
                            let identify = self.identify_payload();
                            if let Err(e) = send_payload(&mut ws, &mut codec, &identify).await {
                                return reconnect(e, true, Duration::ZERO);
                            }
                        }
                        Some(11) => acked = true,
                        _ => {}
                    }
                }
            }
        }
    }

    /// Waits so two Identify payloads are at least [`IDENTIFY_SPACING`] apart.
    async fn identify_spacing(&mut self) {
        if let Some(last) = self.last_identify {
            let since = last.elapsed();
            if since < IDENTIFY_SPACING {
                sleep(IDENTIFY_SPACING - since).await;
            }
        }
        self.last_identify = Some(Instant::now());
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

    /// Maps a close code (see Fluxer's `gateway/opcodes-and-close-codes`) to what happens next.
    fn classify_close(
        &self,
        code: Option<u16>,
        reason: String,
        connected_at: Option<Instant>,
    ) -> Outcome {
        let stable = connected_at.is_some_and(|t| t.elapsed() >= STABLE_AFTER);
        let text = format!(
            "closed with {} {reason}",
            code.map_or("no code".into(), |c| c.to_string())
        );
        let reconnect = |fresh: bool, min_delay: Duration| Outcome::Reconnect {
            reason: text.clone(),
            fresh,
            stable,
            min_delay,
        };
        match code {
            // Authentication failed: the token is invalid or revoked.
            Some(4004) => Outcome::Stop(CloseReason::AuthenticationFailed),
            // Invalid API version.
            Some(4012) => Outcome::Stop(CloseReason::InvalidApiVersion),
            // Invalid shard / sharding required.
            Some(4010) | Some(4011) => Outcome::Stop(CloseReason::InvalidShard),
            // Rate limited (connections, payloads, sessions, identify).
            Some(4008) => reconnect(false, LONG_BACKOFF),
            // Unknown opcode, decode error, not authenticated, already
            // authenticated: a protocol mismatch; start fresh and slowly.
            Some(4001) | Some(4002) | Some(4003) | Some(4005) => reconnect(true, LONG_BACKOFF),
            // Invalid sequence: start a fresh session.
            Some(4007) => reconnect(true, Duration::ZERO),
            // 4000 (unknown / node draining), 4009 (session timeout), network: resume.
            _ => reconnect(false, Duration::ZERO),
        }
    }
}

/// Paces outgoing commands to Fluxer's per-socket limits (see the constants).
#[derive(Default)]
struct Pacer {
    /// Commands waiting, in order.
    outbox: VecDeque<Value>,
    bounded_sent: VecDeque<Instant>,
    presence_sent: VecDeque<Instant>,
    /// A Presence Update is wanted; the latest status is sent when allowed.
    presence_due: bool,
}

fn is_bounded(payload: &Value) -> bool {
    matches!(
        payload.get("op").and_then(Value::as_u64),
        Some(8 | 14 | 15 | 16 | 28)
    )
}

fn prune(sent: &mut VecDeque<Instant>, window: Duration, now: Instant) {
    while sent
        .front()
        .is_some_and(|t| now.saturating_duration_since(*t) >= window)
    {
        sent.pop_front();
    }
}

impl Pacer {
    fn push(&mut self, payload: Value) {
        if payload.get("op").and_then(Value::as_u64) == Some(3) {
            // Coalesced: only the latest status matters.
            self.presence_due = true;
            return;
        }
        if self.outbox.len() >= MAX_QUEUED {
            tracing::warn!("gateway outbox full; dropping the oldest command");
            self.outbox.pop_front();
        }
        self.outbox.push_back(payload);
    }

    /// Takes every payload that may go out now.
    fn take_ready(&mut self, status: Option<&str>) -> Vec<Value> {
        let now = Instant::now();
        prune(&mut self.bounded_sent, BOUNDED_WINDOW, now);
        prune(&mut self.presence_sent, PRESENCE_WINDOW, now);
        let mut out = Vec::new();
        while let Some(front) = self.outbox.front() {
            if is_bounded(front) {
                if self.bounded_sent.len() >= BOUNDED_PER_WINDOW {
                    break;
                }
                self.bounded_sent.push_back(now);
            }
            out.extend(self.outbox.pop_front());
        }
        if self.presence_due && self.presence_sent.len() < PRESENCE_PER_WINDOW {
            if let Some(s) = status {
                out.push(presence(s));
                self.presence_sent.push_back(now);
            }
            self.presence_due = false;
        }
        out
    }

    /// When something waiting may go out, if anything waits.
    fn next_wakeup(&self) -> Option<Instant> {
        let bounded = self
            .outbox
            .front()
            .filter(|_| self.bounded_sent.len() >= BOUNDED_PER_WINDOW)
            .and_then(|_| self.bounded_sent.front())
            .map(|t| *t + BOUNDED_WINDOW);
        let presence = (self.presence_due && self.presence_sent.len() >= PRESENCE_PER_WINDOW)
            .then(|| self.presence_sent.front().map(|t| *t + PRESENCE_WINDOW))
            .flatten();
        match (bounded, presence) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

async fn flush(
    ws: &mut Ws,
    codec: &mut Option<ZstdStream>,
    pacer: &mut Pacer,
    status: Option<&str>,
) -> Result<(), String> {
    for payload in pacer.take_ready(status) {
        send_payload(ws, codec, &payload).await?;
    }
    Ok(())
}

fn lazy_request<'a>(guilds: impl Iterator<Item = (&'a str, bool)>) -> Value {
    let mut subs = serde_json::Map::new();
    for (g, active) in guilds {
        subs.insert(g.to_owned(), json!({ "active": active, "typing": active }));
    }
    json!({ "op": 14, "d": { "subscriptions": subs } })
}

fn presence(status: &str) -> Value {
    json!({ "op": 3, "d": { "status": status, "afk": false, "mobile": false } })
}

enum ReadEnd {
    Closed(Option<u16>, String),
    /// Network trouble.
    Error(String),
    /// The server sent something unusable (bad JSON, bad zstd, too large).
    BadData(String),
}

/// Reads the next JSON payload. `Ok(None)` means the stream ended.
async fn read_payload(
    ws: &mut Ws,
    codec: &mut Option<ZstdStream>,
) -> Result<Option<Value>, ReadEnd> {
    loop {
        let msg = match ws.next().await {
            None => return Ok(None),
            Some(Err(e)) => {
                use tokio_tungstenite::tungstenite::Error as WsError;
                return Err(match e {
                    WsError::Capacity(_) => ReadEnd::BadData(format!("read: {e}")),
                    _ => ReadEnd::Error(format!("read: {e}")),
                });
            }
            Some(Ok(m)) => m,
        };
        let parsed = match msg {
            WsMessage::Text(t) => serde_json::from_str::<Value>(t.as_str()),
            WsMessage::Binary(b) => match codec {
                Some(c) => {
                    let bytes = c
                        .decompress(&b)
                        .map_err(|e| ReadEnd::BadData(format!("zstd: {e}")))?;
                    if bytes.is_empty() {
                        continue;
                    }
                    serde_json::from_slice(&bytes)
                }
                None => serde_json::from_slice(&b),
            },
            WsMessage::Close(frame) => {
                let (code, reason) = frame
                    .map(|f| (Some(u16::from(f.code)), f.reason.as_str().to_owned()))
                    .unwrap_or((None, String::new()));
                return Err(ReadEnd::Closed(code, reason));
            }
            _ => continue,
        };
        return parsed
            .map(Some)
            .map_err(|e| ReadEnd::BadData(format!("bad JSON from gateway: {e}")));
    }
}

async fn send_payload(
    ws: &mut Ws,
    codec: &mut Option<ZstdStream>,
    payload: &Value,
) -> Result<(), String> {
    let text = payload.to_string();
    let msg = match codec {
        Some(c) => WsMessage::binary(
            c.compress(text.as_bytes())
                .map_err(|e| format!("zstd: {e}"))?,
        ),
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
    fn heartbeat_interval_is_clamped() {
        let h = |v: Value| heartbeat_interval(&json!({"op": 10, "d": {"heartbeat_interval": v}}));
        assert_eq!(h(json!(41_250)), Duration::from_millis(41_250));
        assert_eq!(h(json!(u64::MAX)), HEARTBEAT_MAX);
        assert_eq!(h(json!(1e300)), HEARTBEAT_MAX);
        assert_eq!(h(json!(-5)), HEARTBEAT_MIN);
        assert_eq!(h(json!(0)), HEARTBEAT_MIN);
        assert_eq!(h(json!("x")), Duration::from_millis(41_250));
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
        assert!(matches!(
            Dispatch::parse("MESSAGE_CREATE", json!({})),
            Dispatch::Other { .. }
        ));
    }

    #[test]
    fn replayed_lazy_request_fits_one_payload() {
        let guilds: Vec<(String, bool)> = (0..MAX_ACTIVE_GUILDS)
            .map(|i| (format!("{}", 1_189_375_284_394_692_608u64 + i as u64), true))
            .collect();
        let payload = lazy_request(guilds.iter().map(|(g, a)| (g.as_str(), *a)));
        assert!(payload.to_string().len() < 16_384);
    }

    #[tokio::test(start_paused = true)]
    async fn pacer_limits_bounded_commands_and_presence() {
        let mut pacer = Pacer::default();
        for i in 0..10 {
            pacer.push(lazy_request(std::iter::once((format!("{i}").as_str(), true))));
        }
        for _ in 0..7 {
            pacer.push(presence("idle"));
        }
        let first = pacer.take_ready(Some("idle"));
        // 4 lazy requests and one coalesced presence update.
        assert_eq!(first.iter().filter(|p| p["op"] == 14).count(), 4);
        assert_eq!(first.iter().filter(|p| p["op"] == 3).count(), 1);
        let wake = pacer.next_wakeup().expect("more waiting");
        assert!(wake - Instant::now() <= BOUNDED_WINDOW);
        tokio::time::advance(BOUNDED_WINDOW).await;
        assert_eq!(pacer.take_ready(Some("idle")).len(), 4);
        tokio::time::advance(BOUNDED_WINDOW).await;
        assert_eq!(pacer.take_ready(Some("idle")).len(), 2);
        assert!(pacer.next_wakeup().is_none());

        // Presence: at most 5 per 20 s.
        let mut sent = 0;
        for _ in 0..8 {
            pacer.push(presence("dnd"));
            sent += pacer.take_ready(Some("dnd")).len();
        }
        // One went out above, so four more fit in this window.
        assert_eq!(sent, 4);
        let wake = pacer.next_wakeup().expect("presence waiting");
        tokio::time::advance(wake - Instant::now()).await;
        assert_eq!(pacer.take_ready(Some("dnd")).len(), 1);
    }
}
