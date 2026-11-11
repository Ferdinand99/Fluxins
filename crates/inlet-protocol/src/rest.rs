//! The REST client.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use reqwest::header::HeaderValue;
use reqwest::Method;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};

use crate::coalesce::{PendingAck, ACK_DELAY};
use crate::error::{ApiError, Error, Result};
use crate::instance::{read_capped, Instance, MAX_BODY_BYTES};
use crate::models::{
    lenient_list, Channel, CurrentUser, Emoji, FullUserProfile, Guild, GuildEmoji, GuildMember,
    GuildSticker, Message, MessageEdit, OutgoingMessage, PinsPage, ReactionUsersPage, ReadState,
    Role, User, UserSettings, UserSettingsUpdate,
};
use crate::ratelimit::{
    clamp_server_secs, route_key, LimiterKey, RateLimitConfig, RateLimiter, MAX_SERVER_WAIT,
};
use crate::token::Token;
use crate::upload::{ProgressFn, UploadFile, UploadLimits};
use tokio::time::Instant;

/// Feature names the official client sends in `X-Fluxer-Features`. Without
/// `channel_threads`, thread and forum routes return 404.
pub const FLUXER_FEATURES: &str = "view_channel_members_permission,channel_threads";
/// Capabilities the official client sends in `X-Fluxer-Capabilities`.
pub const FLUXER_CAPABILITIES: &str = "guild_visits";

/// A 429 longer than this is returned as [`Error::RateLimited`] instead of waited out.
const MAX_AUTO_WAIT: Duration = Duration::from_secs(10);
/// Longest `retry_after` reported in [`Error::RateLimited`]. The limiter
/// itself never blocks longer than [`MAX_SERVER_WAIT`], but some buckets are
/// longer (group DM creation: 10 per hour), and the UI should show the real wait.
pub const MAX_REPORTED_WAIT: Duration = Duration::from_secs(6 * 3600);
/// Retries after a route 429.
const MAX_429_RETRIES: u32 = 3;
/// Most pages `guilds()` fetches (200 guilds each).
const MAX_GUILD_PAGES: usize = 25;

/// A raw response after rate limit handling.
pub(crate) struct RawResponse {
    pub(crate) status: u16,
    pub(crate) body: Vec<u8>,
}

impl RawResponse {
    pub(crate) fn json<T: DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_slice(&self.body)
            .map_err(|e| Error::Decode(format!("{e} (HTTP {})", self.status)))
    }

    /// A JSON list, parsed element by element; bad elements are skipped.
    pub(crate) fn list<T: DeserializeOwned>(&self, what: &str) -> Result<Vec<T>> {
        let items: Vec<Value> = self.json()?;
        Ok(lenient_list(items, what))
    }

    pub(crate) fn value(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    pub(crate) fn api_error(&self) -> ApiError {
        let mut err: ApiError = serde_json::from_slice(&self.body).unwrap_or(ApiError {
            status: 0,
            code: String::new(),
            message: String::from_utf8_lossy(&self.body)
                .chars()
                .take(200)
                .collect(),
            errors: None,
        });
        err.status = self.status;
        err
    }

    pub(crate) fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// One request description.
pub(crate) struct Request<'a> {
    pub(crate) method: Method,
    /// Path below `/v1`, starting with `/`, with an optional query.
    pub(crate) path: &'a str,
    pub(crate) body: Option<&'a Value>,
    pub(crate) headers: &'a [(&'static str, String)],
    /// A multipart body (uploads); replaces `body`.
    pub(crate) upload: Option<&'a crate::upload::Multipart>,
}

fn header_f64(headers: &reqwest::header::HeaderMap, name: &str) -> Option<f64> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<f64>().ok())
}

/// Sends a request through the limiter, and handles 429 and 401.
pub(crate) async fn send(
    instance: &Instance,
    limiter: &RateLimiter,
    token: Option<&Token>,
    req: Request<'_>,
) -> Result<RawResponse> {
    let inner = &instance.inner;
    let route = route_key(&req.method, req.path);
    let url = format!("{}{}", inner.api_base, req.path);
    let auth = match token {
        Some(t) => {
            let mut v = HeaderValue::from_str(t.expose_secret()).map_err(|_| {
                Error::Decode("the token has characters a header cannot hold".into())
            })?;
            v.set_sensitive(true);
            Some(v)
        }
        None => None,
    };
    let mut attempts = 0;
    loop {
        limiter.acquire(&route).await;
        let mut builder = inner
            .http
            .request(req.method.clone(), &url)
            .header("Accept", "application/json")
            .header("X-Fluxer-Features", FLUXER_FEATURES)
            .header("X-Fluxer-Capabilities", FLUXER_CAPABILITIES)
            .header(
                "X-Fluxer-Client-Installation-ID",
                &inner.info.installation_id,
            );
        if let Some(v) = &auth {
            builder = builder.header("Authorization", v.clone());
        }
        for (name, value) in req.headers {
            builder = builder.header(*name, value);
        }
        if let Some(m) = req.upload {
            builder = builder
                .header("Content-Type", m.content_type())
                .header("Content-Length", m.len())
                .timeout(upload_timeout(m.len()))
                .body(m.body());
        } else if let Some(body) = req.body {
            builder = builder.json(body);
        }
        let resp = builder.send().await?;
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let body = read_capped(resp, MAX_BODY_BYTES).await?;

        limiter.record_headers(
            &route,
            header_f64(&headers, "x-ratelimit-remaining")
                .filter(|v| v.is_finite())
                .map(|v| v.clamp(0.0, f64::from(u32::MAX)) as u32),
            header_f64(&headers, "x-ratelimit-reset-after")
                .map(|v| clamp_server_secs(v, MAX_SERVER_WAIT)),
        );

        if status == 429 {
            let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let retry_after = parsed
                .get("retry_after")
                .and_then(Value::as_f64)
                .or_else(|| header_f64(&headers, "retry-after"))
                .unwrap_or(1.0);
            let global = parsed
                .get("global")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                || headers
                    .get("x-ratelimit-global")
                    .and_then(|v| v.to_str().ok())
                    == Some("true");
            let wait =
                clamp_server_secs(retry_after, MAX_SERVER_WAIT).max(Duration::from_millis(1));
            // What the caller is told: the server's wait up to MAX_REPORTED_WAIT
            // (a nonsense value falls back to the clamped wait).
            let reported = if retry_after.is_finite() && retry_after > 0.0 {
                clamp_server_secs(retry_after, MAX_REPORTED_WAIT).max(wait)
            } else {
                wait
            };
            limiter.record_429(&route, wait, global);
            tracing::warn!(route = %route, ?wait, global, "rate limited by the instance");
            if global && token.is_some() {
                return Err(Error::GlobalRateLimited);
            }
            if req.upload.is_some() {
                // An upload body is never sent twice; the caller decides.
                return Err(Error::RateLimited {
                    retry_after: reported,
                });
            }
            attempts += 1;
            if attempts > MAX_429_RETRIES || wait > MAX_AUTO_WAIT {
                return Err(Error::RateLimited {
                    retry_after: reported,
                });
            }
            continue;
        }
        if status == 401 && token.is_some() {
            return Err(Error::Unauthorized);
        }
        if (300..400).contains(&status) {
            // Redirects are never followed (they could carry secrets elsewhere).
            return Err(Error::Decode(format!(
                "the API answered with a redirect (HTTP {status}); not following it"
            )));
        }
        return Ok(RawResponse { status, body });
    }
}

/// Time allowed for an upload request: 2 minutes plus the time at 1 Mbit/s
/// (8 s per MiB), at most 90 minutes (512 MiB at about 0.8 Mbit/s).
fn upload_timeout(len: u64) -> Duration {
    Duration::from_secs(120u64.saturating_add(len / (128 * 1024)).min(90 * 60))
}

/// The REST client for one signed-in session. Cheap to clone. Every client of
/// the same account on the same instance shares one rate limiter (see
/// [`crate::ratelimit`]).
#[derive(Clone)]
pub struct Client {
    instance: Instance,
    token: Arc<Token>,
    limiter: Arc<Mutex<Arc<RateLimiter>>>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("instance", &self.instance)
            .field("token", &*self.token)
            .finish()
    }
}

impl Client {
    /// A client with the default limits (10 requests per second, burst 20).
    pub fn new(instance: Instance, token: Token) -> Self {
        Self::with_rate_limit(instance, token, RateLimitConfig::default())
    }

    /// A client with custom limits, clamped to the hard cap. If the account
    /// already has a limiter, that limiter (and its config) is shared instead.
    pub fn with_rate_limit(instance: Instance, token: Token, config: RateLimitConfig) -> Self {
        let key = LimiterKey::Token(instance.origin().to_owned(), token.fingerprint());
        let limiter = RateLimiter::shared(key, config);
        Self {
            instance,
            token: Arc::new(token),
            limiter: Arc::new(Mutex::new(limiter)),
        }
    }

    /// A client for a known user ID (from a sign-in): shares the account's limiter at once.
    pub(crate) fn for_user(instance: Instance, token: Token, user_id: &str) -> Self {
        let client = Self::new(instance, token);
        if !user_id.is_empty() {
            client.bind_user(user_id);
        }
        client
    }

    /// Moves this client (and its clones) onto the account's shared limiter.
    fn bind_user(&self, user_id: &str) {
        let mut slot = self.limiter.lock().unwrap_or_else(|e| e.into_inner());
        let shared = RateLimiter::adopt_user(self.instance.origin(), user_id, &slot);
        if !Arc::ptr_eq(&shared, &slot) {
            // Keep pending acks and typing times when moving to the account's state.
            shared.coalesce.absorb(&slot.coalesce);
        }
        *slot = shared;
    }

    /// The instance.
    pub fn instance(&self) -> &Instance {
        &self.instance
    }

    /// The token.
    pub fn token(&self) -> &Token {
        &self.token
    }

    /// The shared limiter in use.
    pub fn rate_limiter(&self) -> Arc<RateLimiter> {
        self.limiter
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Sends any request and returns the parsed JSON (or `Value::Null` for 204).
    /// Use it for routes this crate has no method for yet.
    pub async fn request_json(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value> {
        let raw = self.raw(method, path, body).await?;
        if raw.body.is_empty() {
            return Ok(Value::Null);
        }
        raw.json()
    }

    pub(crate) async fn raw(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<RawResponse> {
        let limiter = self.rate_limiter();
        let raw = send(
            &self.instance,
            &limiter,
            Some(&self.token),
            Request {
                method,
                path,
                body,
                headers: &[],
                upload: None,
            },
        )
        .await?;
        if !raw.is_success() {
            return Err(Error::Api(Box::new(raw.api_error())));
        }
        Ok(raw)
    }

    pub(crate) async fn get_list<T: DeserializeOwned>(
        &self,
        path: &str,
        what: &str,
    ) -> Result<Vec<T>> {
        self.raw(Method::GET, path, None).await?.list(what)
    }

    /// `GET /users/@me`: the signed-in account. Also a cheap token check, and
    /// it links this client to the account's shared rate limiter.
    pub async fn me(&self) -> Result<CurrentUser> {
        let me: CurrentUser = self.raw(Method::GET, "/users/@me", None).await?.json()?;
        self.bind_user(&me.id);
        *self
            .rate_limiter()
            .coalesce
            .traits
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(me.traits());
        Ok(me)
    }

    /// `GET /users/@me/guilds`: every guild of the account (pages of 200, at most 25 pages).
    pub async fn guilds(&self) -> Result<Vec<Guild>> {
        let mut all: Vec<Guild> = Vec::new();
        let mut after: Option<u64> = None;
        for _ in 0..MAX_GUILD_PAGES {
            let path = match after {
                None => "/users/@me/guilds?limit=200".to_owned(),
                Some(id) => format!("/users/@me/guilds?limit=200&after={id}"),
            };
            let raw = self.raw(Method::GET, &path, None).await?;
            let items: Vec<Value> = raw.json()?;
            let full_page = items.len() >= 200;
            let page: Vec<Guild> = lenient_list(items, "guild");
            let last = page.iter().filter_map(|g| g.id.parse::<u64>().ok()).max();
            all.extend(page);
            match (full_page, last) {
                // Stop when the cursor does not move forward.
                (true, Some(id)) if after.is_none_or(|a| id > a) => after = Some(id),
                _ => return Ok(all),
            }
        }
        tracing::warn!("guild list stopped after {MAX_GUILD_PAGES} pages");
        Ok(all)
    }

    /// `GET /guilds/{id}/channels`: the guild's channels the account can see.
    pub async fn channels(&self, guild_id: &str) -> Result<Vec<Channel>> {
        self.get_list(&format!("/guilds/{}/channels", id(guild_id)?), "channel")
            .await
    }

    /// `GET /users/@me/channels`: DMs and group DMs.
    pub async fn dm_channels(&self) -> Result<Vec<Channel>> {
        self.get_list("/users/@me/channels", "dm channel").await
    }

    /// `GET /channels/{id}/messages`: up to `limit` (1-100) messages before
    /// `before`, newest first, as the API returns them.
    pub async fn messages(
        &self,
        channel_id: &str,
        before: Option<&str>,
        limit: u8,
    ) -> Result<Vec<Message>> {
        let limit = limit.clamp(1, 100);
        let mut path = format!("/channels/{}/messages?limit={limit}", id(channel_id)?);
        if let Some(b) = before {
            path.push_str("&before=");
            path.push_str(id(b)?);
        }
        self.get_list(&path, "message").await
    }

    /// `POST /channels/{id}/messages` with text content. Pass a `nonce` (see
    /// [`new_nonce`]) to match the MESSAGE_CREATE echo to an optimistic message.
    pub async fn send_message(
        &self,
        channel_id: &str,
        content: &str,
        nonce: Option<&str>,
    ) -> Result<Message> {
        let mut body = json!({ "content": content });
        if let Some(n) = nonce {
            body["nonce"] = Value::String(n.to_owned());
        }
        self.rate_limiter().coalesce.reset_typing(channel_id);
        self.raw(
            Method::POST,
            &format!("/channels/{}/messages", id(channel_id)?),
            Some(&body),
        )
        .await?
        .json()
    }

    /// `POST /channels/{id}/typing`: shows the typing indicator for about 10
    /// seconds. Debounced: call it on every key press; at most one request
    /// per channel goes out every 8 s ([`crate::coalesce::TYPING_INTERVAL`]),
    /// and the others return `Ok(())` at once.
    pub async fn trigger_typing(&self, channel_id: &str) -> Result<()> {
        let channel_id = id(channel_id)?;
        if !self.rate_limiter().coalesce.typing_due(channel_id) {
            return Ok(());
        }
        // A failed request also waits the full interval, so a channel where
        // typing is refused (no permission, timeout) is not retried per key press.
        self.raw(
            Method::POST,
            &format!("/channels/{}/typing", id(channel_id)?),
            None,
        )
        .await?;
        Ok(())
    }

    /// `POST /auth/logout`: revokes this token on the server.
    pub async fn sign_out(&self) -> Result<()> {
        self.raw(Method::POST, "/auth/logout", None).await?;
        Ok(())
    }
}

/// Most users `reaction_users` asks for per page.
const MAX_REACTION_PAGE: u8 = 100;

impl Client {
    async fn raw_upload(
        &self,
        path: &str,
        upload: &crate::upload::Multipart,
    ) -> Result<RawResponse> {
        let limiter = self.rate_limiter();
        let raw = send(
            &self.instance,
            &limiter,
            Some(&self.token),
            Request {
                method: Method::POST,
                path,
                body: None,
                headers: &[],
                upload: Some(upload),
            },
        )
        .await?;
        if !raw.is_success() {
            return Err(Error::Api(Box::new(raw.api_error())));
        }
        Ok(raw)
    }

    // ----- Messages -----

    /// `GET /channels/{id}/messages/{id}`: one message.
    pub async fn message(&self, channel_id: &str, message_id: &str) -> Result<Message> {
        self.raw(
            Method::GET,
            &format!("/channels/{}/messages/{}", id(channel_id)?, id(message_id)?),
            None,
        )
        .await?
        .json()
    }

    /// `POST /channels/{id}/messages` with replies, stickers, flags and mention
    /// control. See [`OutgoingMessage`].
    pub async fn send(&self, channel_id: &str, message: &OutgoingMessage) -> Result<Message> {
        let body = message.to_json(None)?;
        self.rate_limiter().coalesce.reset_typing(channel_id);
        self.raw(
            Method::POST,
            &format!("/channels/{}/messages", id(channel_id)?),
            Some(&body),
        )
        .await?
        .json()
    }

    /// A reply to `reply_to`. `mention_author` decides whether its author is notified.
    pub async fn reply(
        &self,
        channel_id: &str,
        reply_to: &str,
        content: &str,
        mention_author: bool,
        nonce: Option<&str>,
    ) -> Result<Message> {
        let mut m = OutgoingMessage::text(content).reply_to(reply_to, mention_author);
        m.nonce = nonce.map(str::to_owned);
        self.send(channel_id, &m).await
    }

    /// `PATCH /channels/{id}/messages/{id}`: new text for an own message.
    pub async fn edit_message(
        &self,
        channel_id: &str,
        message_id: &str,
        content: &str,
    ) -> Result<Message> {
        let edit = MessageEdit {
            content: Some(content.to_owned()),
            ..MessageEdit::default()
        };
        self.edit_message_with(channel_id, message_id, &edit).await
    }

    /// `PATCH /channels/{id}/messages/{id}` with any [`MessageEdit`].
    pub async fn edit_message_with(
        &self,
        channel_id: &str,
        message_id: &str,
        edit: &MessageEdit,
    ) -> Result<Message> {
        self.raw(
            Method::PATCH,
            &format!("/channels/{}/messages/{}", id(channel_id)?, id(message_id)?),
            Some(&edit.to_json()?),
        )
        .await?
        .json()
    }

    /// `DELETE /channels/{id}/messages/{id}`.
    pub async fn delete_message(&self, channel_id: &str, message_id: &str) -> Result<()> {
        self.raw(
            Method::DELETE,
            &format!("/channels/{}/messages/{}", id(channel_id)?, id(message_id)?),
            None,
        )
        .await?;
        Ok(())
    }

    // ----- Reactions -----

    fn reaction_path(channel_id: &str, message_id: &str, emoji: &Emoji) -> Result<String> {
        Ok(format!(
            "/channels/{}/messages/{}/reactions/{}",
            id(channel_id)?,
            id(message_id)?,
            reaction_segment(emoji)?
        ))
    }

    /// `PUT …/reactions/{emoji}/@me`: adds the account's reaction.
    pub async fn add_reaction(
        &self,
        channel_id: &str,
        message_id: &str,
        emoji: &Emoji,
    ) -> Result<()> {
        let path = format!(
            "{}/@me",
            Self::reaction_path(channel_id, message_id, emoji)?
        );
        self.raw(Method::PUT, &path, None).await?;
        Ok(())
    }

    /// `DELETE …/reactions/{emoji}/@me`: removes the account's reaction.
    pub async fn remove_reaction(
        &self,
        channel_id: &str,
        message_id: &str,
        emoji: &Emoji,
    ) -> Result<()> {
        let path = format!(
            "{}/@me",
            Self::reaction_path(channel_id, message_id, emoji)?
        );
        self.raw(Method::DELETE, &path, None).await?;
        Ok(())
    }

    /// `GET …/reactions/{emoji}/users`: who reacted, `limit` (1-100) after `after`.
    pub async fn reaction_users(
        &self,
        channel_id: &str,
        message_id: &str,
        emoji: &Emoji,
        after: Option<&str>,
        limit: u8,
    ) -> Result<ReactionUsersPage> {
        let mut path = format!(
            "{}/users?limit={}",
            Self::reaction_path(channel_id, message_id, emoji)?,
            limit.clamp(1, MAX_REACTION_PAGE)
        );
        if let Some(a) = after {
            path.push_str("&after=");
            path.push_str(id(a)?);
        }
        self.raw(Method::GET, &path, None).await?.json()
    }

    // ----- Pins -----

    /// `GET /channels/{id}/messages/pins`: up to `limit` (1-50) pins, newest
    /// first, pinned before `before` (the `pinned_at` of the last pin seen).
    pub async fn pins(
        &self,
        channel_id: &str,
        before: Option<&str>,
        limit: u8,
    ) -> Result<PinsPage> {
        let mut path = format!(
            "/channels/{}/messages/pins?limit={}",
            id(channel_id)?,
            limit.clamp(1, 50)
        );
        if let Some(b) = before {
            path.push_str("&before=");
            path.push_str(&enc_query(b));
        }
        self.raw(Method::GET, &path, None).await?.json()
    }

    /// `PUT /channels/{id}/pins/{id}`.
    pub async fn pin_message(&self, channel_id: &str, message_id: &str) -> Result<()> {
        self.raw(
            Method::PUT,
            &format!("/channels/{}/pins/{}", id(channel_id)?, id(message_id)?),
            None,
        )
        .await?;
        Ok(())
    }

    /// `DELETE /channels/{id}/pins/{id}`.
    pub async fn unpin_message(&self, channel_id: &str, message_id: &str) -> Result<()> {
        self.raw(
            Method::DELETE,
            &format!("/channels/{}/pins/{}", id(channel_id)?, id(message_id)?),
            None,
        )
        .await?;
        Ok(())
    }

    /// `POST /channels/{id}/pins/ack`: marks the pins as seen.
    pub async fn ack_pins(&self, channel_id: &str) -> Result<()> {
        self.raw(
            Method::POST,
            &format!("/channels/{}/pins/ack", id(channel_id)?),
            None,
        )
        .await?;
        Ok(())
    }

    // ----- Read states -----

    /// Marks `channel_id` read up to `message_id`. Coalesced per account: nothing
    /// is sent right away; the newest message per channel goes out with the
    /// others in one `POST /read-states/ack` after [`crate::coalesce::ACK_DELAY`],
    /// and the account never sends two ack requests less than that apart
    /// (100 channels per request). Acks for a message at or before one already
    /// acknowledged are dropped, and so are IDs that are not snowflakes. Call it
    /// as often as you like (for example on every scroll). Needs a tokio runtime
    /// for the background flush; without one, call [`Client::flush_acks`].
    pub fn ack(&self, channel_id: &str, message_id: &str) {
        self.queue_ack(
            channel_id,
            PendingAck {
                message_id: message_id.to_owned(),
                mention_count: 0,
                manual: false,
            },
        );
    }

    /// Marks the messages after `message_id` unread ("mark unread"), keeping
    /// `mention_count` mentions. Coalesced like [`Client::ack`].
    pub fn mark_unread(&self, channel_id: &str, message_id: &str, mention_count: u64) {
        self.queue_ack(
            channel_id,
            PendingAck {
                message_id: message_id.to_owned(),
                mention_count,
                manual: true,
            },
        );
    }

    fn queue_ack(&self, channel_id: &str, ack: PendingAck) {
        if id(channel_id).is_err() || id(&ack.message_id).is_err() || channel_id == "@me" {
            tracing::warn!("dropping an ack with an ID that is not a snowflake");
            return;
        }
        let limiter = self.rate_limiter();
        let schedule = limiter.coalesce.lock_acks().push(channel_id, ack);
        if !schedule {
            return;
        }
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                let client = self.clone();
                handle.spawn(async move {
                    tokio::time::sleep(ACK_DELAY).await;
                    if let Err(e) = client.flush_acks_inner(None).await {
                        tracing::warn!(error = %e, "sending read acknowledgements failed");
                    }
                });
            }
            Err(_) => limiter.coalesce.lock_acks().scheduled = false,
        }
    }

    /// Sends the acks pending now (for example before signing out) and
    /// returns the read states the server reports. Requests stay at least
    /// [`crate::coalesce::ACK_DELAY`] apart, so more than 100 pending channels
    /// take a second per extra 100.
    pub async fn flush_acks(&self) -> Result<Vec<ReadState>> {
        let pending = self.rate_limiter().coalesce.lock_acks().pending.len();
        let rounds = pending.div_ceil(crate::coalesce::ACK_BATCH).max(1);
        self.flush_acks_inner(Some(rounds)).await
    }

    /// Sends pending acks, at most `rounds` requests (`None`: until none are left).
    async fn flush_acks_inner(&self, rounds: Option<usize>) -> Result<Vec<ReadState>> {
        let limiter = self.rate_limiter();
        let shared = &limiter.coalesce;
        let mut out = Vec::new();
        let mut sent_rounds = 0usize;
        loop {
            if rounds.is_some_and(|r| sent_rounds >= r) {
                return Ok(out);
            }
            // One ack request per ACK_DELAY per account, across every flusher.
            let batch = loop {
                let wait = {
                    let mut state = shared.lock_acks();
                    if state.pending.is_empty() {
                        state.scheduled = false;
                        return Ok(out);
                    }
                    state.reserve_slot(Instant::now())
                };
                match wait {
                    Some(d) => tokio::time::sleep(d).await,
                    None => break shared.lock_acks().take_batch(),
                }
            };
            if batch.is_empty() {
                continue;
            }
            sent_rounds += 1;
            let entries: Vec<Value> = batch
                .iter()
                .map(|(channel, a)| {
                    let mut e = json!({ "channel_id": channel, "message_id": a.message_id });
                    if a.mention_count > 0 {
                        e["mention_count"] = json!(a.mention_count);
                    }
                    if a.manual {
                        e["manual"] = json!(true);
                    }
                    e
                })
                .collect();
            let body = json!({ "read_states": entries });
            match self
                .raw(Method::POST, "/read-states/ack", Some(&body))
                .await
            {
                Ok(raw) => {
                    shared.lock_acks().mark_sent(&batch);
                    if !raw.body.is_empty() {
                        if let Some(Value::Array(items)) = raw.value().get_mut("read_states") {
                            out.extend(lenient_list::<ReadState>(
                                std::mem::take(items),
                                "read state",
                            ));
                        }
                    }
                }
                Err(e) => {
                    let mut state = shared.lock_acks();
                    if matches!(e, Error::Http(_) | Error::RateLimited { .. }) {
                        // Worth another try with the next ack or flush.
                        state.restore(batch);
                    }
                    state.scheduled = false;
                    return Err(e);
                }
            }
        }
    }

    // ----- Account -----

    /// `GET /users/@me/settings`.
    pub async fn user_settings(&self) -> Result<UserSettings> {
        self.raw(Method::GET, "/users/@me/settings", None)
            .await?
            .json()
    }

    /// `PATCH /users/@me/settings`: changes the fields that are set and
    /// returns the new settings (USER_SETTINGS_UPDATE follows on the gateway).
    pub async fn update_user_settings(&self, update: &UserSettingsUpdate) -> Result<UserSettings> {
        let body = serde_json::to_value(update).map_err(|e| Error::Decode(e.to_string()))?;
        self.raw(Method::PATCH, "/users/@me/settings", Some(&body))
            .await?
            .json()
    }

    /// `GET /users/{id}`.
    pub async fn user(&self, user_id: &str) -> Result<User> {
        self.raw(Method::GET, &format!("/users/{}", id(user_id)?), None)
            .await?
            .json()
    }

    /// `GET /users/{id}/profile`, with the member profile of `guild_id` and
    /// optionally mutual guilds and friends.
    pub async fn user_profile(
        &self,
        user_id: &str,
        guild_id: Option<&str>,
        with_mutuals: bool,
    ) -> Result<FullUserProfile> {
        let mut query: Vec<String> = Vec::new();
        if let Some(g) = guild_id {
            query.push(format!("guild_id={}", id(g)?));
        }
        if with_mutuals {
            query.push("with_mutual_guilds=true&with_mutual_friends=true".to_owned());
        }
        let mut path = format!("/users/{}/profile", id(user_id)?);
        if !query.is_empty() {
            path.push('?');
            path.push_str(&query.join("&"));
        }
        self.raw(Method::GET, &path, None).await?.json()
    }

    // ----- Guilds -----

    /// `GET /guilds/{id}/members/{id}`.
    pub async fn guild_member(&self, guild_id: &str, user_id: &str) -> Result<GuildMember> {
        self.raw(
            Method::GET,
            &format!("/guilds/{}/members/{}", id(guild_id)?, id(user_id)?),
            None,
        )
        .await?
        .json()
    }

    /// `GET /guilds/{id}/members/@me`.
    pub async fn my_guild_member(&self, guild_id: &str) -> Result<GuildMember> {
        self.raw(
            Method::GET,
            &format!("/guilds/{}/members/@me", id(guild_id)?),
            None,
        )
        .await?
        .json()
    }

    /// `GET /guilds/{id}/members`: up to `limit` (1-1000) members after the
    /// user ID `after`. For the member list pane use the gateway
    /// ([`crate::GatewayHandle::subscribe_member_list`]).
    pub async fn guild_members(
        &self,
        guild_id: &str,
        after: Option<&str>,
        limit: u16,
    ) -> Result<Vec<GuildMember>> {
        let mut path = format!(
            "/guilds/{}/members?limit={}",
            id(guild_id)?,
            limit.clamp(1, 1000)
        );
        if let Some(a) = after {
            path.push_str("&after=");
            path.push_str(id(a)?);
        }
        self.get_list(&path, "member").await
    }

    /// `GET /guilds/{id}/roles`: every role, highest first.
    pub async fn roles(&self, guild_id: &str) -> Result<Vec<Role>> {
        self.get_list(&format!("/guilds/{}/roles", id(guild_id)?), "role")
            .await
    }

    /// `GET /guilds/{id}/emojis`.
    pub async fn guild_emojis(&self, guild_id: &str) -> Result<Vec<GuildEmoji>> {
        self.get_list(&format!("/guilds/{}/emojis", id(guild_id)?), "emoji")
            .await
    }

    /// `GET /guilds/{id}/stickers`.
    pub async fn guild_stickers(&self, guild_id: &str) -> Result<Vec<GuildSticker>> {
        self.get_list(&format!("/guilds/{}/stickers", id(guild_id)?), "sticker")
            .await
    }

    // ----- Uploads -----

    /// The upload limits for this account: from the instance's limit rules
    /// and the account traits seen in the last [`Client::me`] (defaults
    /// before that). The total per message is at most
    /// [`crate::limits::MAX_UPLOAD_TOTAL`] (512 MiB) whatever the instance allows.
    pub fn upload_limits(&self) -> UploadLimits {
        let traits = self
            .rate_limiter()
            .coalesce
            .traits
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_default();
        let d = self.instance.discovery();
        let max_file_size = d.max_attachment_file_size(&traits);
        let max_files = d.max_attachments_per_message(&traits);
        UploadLimits {
            max_file_size,
            max_files,
            max_total_size: max_file_size
                .saturating_mul(max_files)
                .min(crate::limits::MAX_UPLOAD_TOTAL),
        }
    }

    /// `POST /channels/{id}/messages` as `multipart/form-data` with files.
    ///
    /// Checks the file count, each size and the total against
    /// [`Client::upload_limits`] first ([`Error::InvalidInput`],
    /// [`Error::TooLarge`]), and goes through the rate limiter. A 429 is not
    /// retried: the body is not sent again, and [`Error::RateLimited`] carries
    /// `retry_after` so the UI can offer "try again". `progress` gets
    /// `(sent, total)` bytes from a separate task (see [`ProgressFn`]).
    ///
    /// The files must be in memory. Streaming from disk
    /// (`UploadFile::from_path`) and Fluxer's pre-signed uploads for large
    /// files are later work.
    pub async fn send_with_files(
        &self,
        channel_id: &str,
        message: &OutgoingMessage,
        files: &[UploadFile],
        progress: Option<ProgressFn>,
    ) -> Result<Message> {
        let channel_id = id(channel_id)?;
        if files.is_empty() {
            return self.send(channel_id, message).await;
        }
        let limits = self.upload_limits();
        if files.len() as u64 > limits.max_files {
            return Err(Error::InvalidInput(format!(
                "{} files; this account can attach {} per message",
                files.len(),
                limits.max_files
            )));
        }
        let too_large = |limit: u64| Error::TooLarge {
            limit: usize::try_from(limit).unwrap_or(usize::MAX),
        };
        if files.iter().any(|f| f.data.len() > limits.max_file_size) {
            return Err(too_large(limits.max_file_size));
        }
        let total = files
            .iter()
            .fold(0u64, |sum, f| sum.saturating_add(f.data.len()));
        if total > limits.max_total_size {
            return Err(too_large(limits.max_total_size));
        }
        let json = message.to_json(None)?;
        self.rate_limiter().coalesce.reset_typing(channel_id);
        let body = crate::upload::Multipart::message(json, files, progress);
        self.raw_upload(&format!("/channels/{channel_id}/messages"), &body)
            .await?
            .json()
    }
}

/// The `{emoji}` path segment of a reaction route: `name:id` for a custom
/// emoji, the emoji itself otherwise, percent-encoded, 1-64 characters.
fn reaction_segment(emoji: &Emoji) -> Result<String> {
    let bad = |why: &str| Err(Error::InvalidInput(format!("reaction emoji: {why}")));
    if let Some(id) = &emoji.id {
        if !crate::media::is_snowflake(id) {
            return bad("custom emoji ID is not a snowflake");
        }
        if emoji.name.is_empty()
            || emoji.name.len() > 32
            || !emoji
                .name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return bad("custom emoji name");
        }
    } else if emoji.name.is_empty() || emoji.name.chars().any(char::is_control) {
        return bad("empty or control characters");
    }
    let value = emoji.path_value();
    if value.chars().count() > 64 {
        return bad("longer than 64 characters");
    }
    Ok(percent_encode_all(&value))
}

/// Percent-encodes everything except RFC 3986 unreserved characters.
fn percent_encode_all(s: &str) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// A snowflake ID (or `@me`) for a path or query, or [`Error::InvalidInput`].
/// IDs come from the server and the UI; anything else (`..`, `/`, `?`) could
/// make the request hit another route.
pub(crate) fn id(value: &str) -> Result<&str> {
    if value == "@me" || crate::media::is_snowflake(value) {
        Ok(value)
    } else {
        Err(Error::InvalidInput("an ID is not a snowflake".into()))
    }
}

/// A snowflake ID (not `@me`) for a path, query or body where only a real
/// ID makes sense (message, channel, thread or tag IDs), or
/// [`Error::InvalidInput`].
pub(crate) fn snowflake(value: &str) -> Result<&str> {
    if crate::media::is_snowflake(value) {
        Ok(value)
    } else {
        Err(Error::InvalidInput("an ID is not a snowflake".into()))
    }
}

/// Percent-encodes a query value.
pub(crate) fn enc_query(segment: &str) -> String {
    url::form_urlencoded::byte_serialize(segment.as_bytes()).collect()
}

/// A fresh message nonce: a snowflake for the current time, as the official client uses.
pub fn new_nonce() -> String {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let since = now_ms.saturating_sub(crate::models::FLUXER_EPOCH_MS);
    let low: u64 = rand::random::<u64>() & 0x3f_ffff;
    ((since << 22) | low).to_string()
}
