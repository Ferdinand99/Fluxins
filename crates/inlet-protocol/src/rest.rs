//! The REST client.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use reqwest::header::HeaderValue;
use reqwest::Method;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};

use crate::error::{ApiError, Error, Result};
use crate::instance::{read_capped, Instance, MAX_BODY_BYTES};
use crate::models::{lenient_list, Channel, CurrentUser, Guild, Message};
use crate::ratelimit::{
    clamp_server_secs, route_key, LimiterKey, RateLimitConfig, RateLimiter, MAX_SERVER_WAIT,
};
use crate::token::Token;

/// Feature names the official client sends in `X-Fluxer-Features`. Without
/// `channel_threads`, thread and forum routes return 404.
pub const FLUXER_FEATURES: &str = "view_channel_members_permission,channel_threads";
/// Capabilities the official client sends in `X-Fluxer-Capabilities`.
pub const FLUXER_CAPABILITIES: &str = "guild_visits";

/// A 429 longer than this is returned as [`Error::RateLimited`] instead of waited out.
const MAX_AUTO_WAIT: Duration = Duration::from_secs(10);
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
        if let Some(body) = req.body {
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
            limiter.record_429(&route, wait, global);
            tracing::warn!(route = %route, ?wait, global, "rate limited by the instance");
            if global && token.is_some() {
                return Err(Error::GlobalRateLimited);
            }
            attempts += 1;
            if attempts > MAX_429_RETRIES || wait > MAX_AUTO_WAIT {
                return Err(Error::RateLimited { retry_after: wait });
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

    async fn raw(&self, method: Method, path: &str, body: Option<&Value>) -> Result<RawResponse> {
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
            },
        )
        .await?;
        if !raw.is_success() {
            return Err(Error::Api(Box::new(raw.api_error())));
        }
        Ok(raw)
    }

    async fn get_list<T: DeserializeOwned>(&self, path: &str, what: &str) -> Result<Vec<T>> {
        self.raw(Method::GET, path, None).await?.list(what)
    }

    /// `GET /users/@me`: the signed-in account. Also a cheap token check, and
    /// it links this client to the account's shared rate limiter.
    pub async fn me(&self) -> Result<CurrentUser> {
        let me: CurrentUser = self.raw(Method::GET, "/users/@me", None).await?.json()?;
        self.bind_user(&me.id);
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
        self.get_list(&format!("/guilds/{}/channels", enc(guild_id)), "channel")
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
        let mut path = format!("/channels/{}/messages?limit={limit}", enc(channel_id));
        if let Some(b) = before {
            path.push_str("&before=");
            path.push_str(&enc(b));
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
        self.raw(
            Method::POST,
            &format!("/channels/{}/messages", enc(channel_id)),
            Some(&body),
        )
        .await?
        .json()
    }

    /// `POST /channels/{id}/typing`: shows the typing indicator for about 10 seconds.
    pub async fn trigger_typing(&self, channel_id: &str) -> Result<()> {
        self.raw(
            Method::POST,
            &format!("/channels/{}/typing", enc(channel_id)),
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

/// Percent-encodes a path segment. Snowflakes pass through unchanged.
fn enc(segment: &str) -> String {
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
