//! Instance discovery (`GET {origin}/.well-known/fluxer`) and endpoint checks.
//!
//! Every URL a token or secret can go to must be encrypted: the origin and
//! `api_public` must be `https`, the gateway `wss`. `media` and `webapp` must be
//! `https` too, or they are ignored. Loopback hosts (`localhost`, `127.0.0.0/8`,
//! `::1`) may use `http`/`ws`, for tests and local development only.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use url::Url;

use crate::error::{Error, Result};
use crate::models::null_as_default;
use crate::ratelimit::{RateLimitConfig, RateLimiter};

/// The highest `api_code_version` this crate speaks.
pub const SUPPORTED_API_CODE_VERSION: u32 = 1;

/// Largest discovery document accepted.
const MAX_DISCOVERY_BYTES: usize = 1024 * 1024;
/// Largest REST response body accepted.
pub(crate) const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// How this client presents itself to the instance.
#[derive(Debug, Clone)]
pub struct ClientInfo {
    /// `User-Agent` header, shown in the account's session list.
    pub user_agent: String,
    /// `X-Fluxer-Client-Installation-ID`: a random ID per installation.
    /// Persist it and pass it back in on the next start.
    pub installation_id: String,
    /// Operating system name for sign-in client properties and gateway Identify.
    pub os: String,
    /// Web app hosts the browser handoff may open even though their registrable
    /// domain differs from the instance's (for example `web.example.com` for an
    /// instance at `chat.example.org`). Empty by default.
    pub trusted_webapp_hosts: Vec<String>,
}

impl Default for ClientInfo {
    fn default() -> Self {
        Self {
            user_agent: format!(
                "Inlet/{} (+https://github.com/Ferdinand99/Inlet)",
                env!("CARGO_PKG_VERSION")
            ),
            installation_id: random_uuid(),
            os: os_name().to_owned(),
            trusted_webapp_hosts: Vec::new(),
        }
    }
}

/// The OS name the official clients send (`Windows`, `Mac OS X`, `Linux`).
pub fn os_name() -> &'static str {
    match std::env::consts::OS {
        "windows" => "Windows",
        "macos" => "Mac OS X",
        "linux" => "Linux",
        other => other,
    }
}

fn random_uuid() -> String {
    let b: [u8; 16] = rand::random();
    let h = hex::encode(b);
    format!(
        "{}-{}-4{}-a{}-{}",
        &h[0..8],
        &h[8..12],
        &h[13..16],
        &h[17..20],
        &h[20..32]
    )
}

/// The `endpoints` of the discovery document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Endpoints {
    /// API base without `/v1`, for the official web app.
    #[serde(default)]
    pub api: Option<String>,
    /// API base for third-party clients, without `/v1`. This crate uses it.
    pub api_public: String,
    /// Gateway WebSocket URL.
    pub gateway: String,
    /// Media proxy base (avatars, icons, attachments).
    #[serde(default)]
    pub media: Option<String>,
    /// Web app base, used for the browser sign-in handoff.
    #[serde(default)]
    pub webapp: Option<String>,
    /// Every other endpoint.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// The `captcha` section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaptchaInfo {
    /// `altcha` or `none`.
    #[serde(default, deserialize_with = "null_as_default")]
    pub provider: String,
}

/// The parts of the discovery document this crate reads; everything else stays in `extra`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Discovery {
    /// API code version; 1 today.
    pub api_code_version: u32,
    /// Service URLs.
    pub endpoints: Endpoints,
    /// CAPTCHA provider.
    #[serde(default)]
    pub captcha: Option<CaptchaInfo>,
    /// Instance features (`account_identity`, `voice_enabled`, ...), raw.
    #[serde(default, deserialize_with = "null_as_default")]
    pub features: Map<String, Value>,
    /// Limit rules, raw. The client has to evaluate them itself.
    #[serde(default)]
    pub limits: Value,
    /// Branding and setup (`app_public.branding.product_name`, ...), raw.
    #[serde(default)]
    pub app_public: Value,
    /// Every other field (`sso`, `gif`, `registration`, `community`, ...).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Discovery {
    /// The instance's own name, from the branding, if it has one.
    pub fn product_name(&self) -> Option<&str> {
        self.app_public
            .get("branding")?
            .get("product_name")?
            .as_str()
    }

    /// Whether password sign-in needs a CAPTCHA.
    pub fn captcha_required(&self) -> bool {
        self.captcha.as_ref().is_some_and(|c| c.provider != "none")
    }

    /// The largest message the default limit rule allows, if the document says.
    pub fn max_message_length(&self) -> Option<u64> {
        self.limits
            .get("rules")?
            .as_array()?
            .iter()
            .find(|r| r.get("id").and_then(Value::as_str) == Some("default"))?
            .get("overrides")?
            .get("max_message_length")?
            .as_u64()
    }
}

/// The hosts a session's token may be sent to. Store them next to the token
/// (for example in the same keychain entry) and pass them to
/// [`crate::Session::from_token_pinned`] on the next start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointPins {
    /// Origin of the API base, for example `https://fluxer.opland.net`.
    pub api: String,
    /// Origin of the gateway, for example `wss://fluxer.opland.net`.
    pub gateway: String,
}

/// [`EndpointPins`] plus the media endpoint, so a changed discovery document
/// cannot move image loading (and with it the person's IP address and what
/// they look at) to another host either. The JSON is backward compatible:
/// `{"api", "gateway", "media"}` reads as [`EndpointPins`] (the extra field is
/// ignored), and pins stored without `media` read as `media: None`, which
/// accepts any media endpoint until the pins are stored again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPins {
    /// Origin of the API base.
    pub api: String,
    /// Origin of the gateway.
    pub gateway: String,
    /// Origin of the media endpoint, `None` for none (or for old stored pins).
    #[serde(default)]
    pub media: Option<String>,
}

impl From<EndpointPins> for SessionPins {
    fn from(p: EndpointPins) -> Self {
        SessionPins {
            api: p.api,
            gateway: p.gateway,
            media: None,
        }
    }
}

impl SessionPins {
    /// The API and gateway part.
    pub fn endpoints(&self) -> EndpointPins {
        EndpointPins {
            api: self.api.clone(),
            gateway: self.gateway.clone(),
        }
    }
}

impl std::fmt::Display for SessionPins {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "API {} and gateway {}", self.api, self.gateway)?;
        if let Some(m) = &self.media {
            write!(f, " and media {m}")?;
        }
        Ok(())
    }
}

impl std::fmt::Display for EndpointPins {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "API {} and gateway {}", self.api, self.gateway)
    }
}

pub(crate) struct InstanceInner {
    pub(crate) origin: String,
    pub(crate) discovery: Discovery,
    pub(crate) etag: Option<String>,
    /// `api_public` + `/v1`, no trailing slash.
    pub(crate) api_base: String,
    /// The checked gateway URL (without connection parameters).
    pub(crate) gateway: Url,
    /// The checked web app base, or the origin.
    pub(crate) webapp: Url,
    /// The checked media base.
    pub(crate) media: Option<String>,
    /// API client: never follows redirects.
    pub(crate) http: reqwest::Client,
    /// Discovery client: follows https-to-https redirects only.
    pub(crate) discovery_http: reqwest::Client,
    pub(crate) info: ClientInfo,
    /// Limiter for requests without a token (sign-in, discovery).
    pub(crate) anon_limiter: Arc<RateLimiter>,
}

/// A discovered Fluxer instance. Cheap to clone.
#[derive(Clone)]
pub struct Instance {
    pub(crate) inner: Arc<InstanceInner>,
}

impl std::fmt::Debug for Instance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Instance")
            .field("origin", &self.inner.origin)
            .field("api_base", &self.inner.api_base)
            .finish_non_exhaustive()
    }
}

/// Whether `host` is a loopback name or address.
fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// Parses `raw` and requires `secure` (or `insecure` on a loopback host).
pub(crate) fn require_scheme(raw: &str, secure: &str, insecure: &str, what: &str) -> Result<Url> {
    let url = Url::parse(raw.trim()).map_err(|e| Error::Url(format!("{what}: {e}")))?;
    if url.host_str().is_none() {
        return Err(Error::Url(format!("{what}: no host in {raw}")));
    }
    let scheme = url.scheme();
    if scheme == secure || (scheme == insecure && is_loopback(&url)) {
        Ok(url)
    } else {
        Err(Error::Url(format!(
            "{what} must use {secure}:// (got {scheme}://); refusing to send secrets unencrypted"
        )))
    }
}

/// The registrable domain of a host by the Public Suffix List (including the
/// private section, so `alice.github.io` and `bob.github.io` differ). A host
/// that is itself a public suffix, or has no known suffix, is returned whole.
/// IP addresses are returned unchanged.
fn registrable_domain(url: &Url) -> Option<String> {
    match url.host()? {
        url::Host::Domain(d) => {
            let d = d.trim_end_matches('.').to_ascii_lowercase();
            Some(psl::domain_str(&d).map_or_else(|| d.clone(), str::to_owned))
        }
        other => Some(other.to_string()),
    }
}

/// Turns `fluxer.example.org`, `https://fluxer.example.org/` and similar into
/// `https://fluxer.example.org`. Plain `http` is only accepted for loopback hosts.
pub fn normalize_origin(origin: &str) -> Result<String> {
    let trimmed = origin.trim();
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_owned()
    } else {
        format!("https://{trimmed}")
    };
    let url = require_scheme(&with_scheme, "https", "http", "instance origin")?;
    Ok(url.origin().ascii_serialization())
}

fn http_builder(info: &ClientInfo) -> reqwest::ClientBuilder {
    let builder = reqwest::Client::builder()
        .user_agent(info.user_agent.clone())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .use_rustls_tls();
    extra_roots(builder)
}

#[cfg(feature = "test-extra-ca")]
fn extra_roots(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    crate::test_ca::add_to_http(builder)
}

#[cfg(not(feature = "test-extra-ca"))]
fn extra_roots(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    builder
}

pub(crate) fn build_http(info: &ClientInfo) -> Result<(reqwest::Client, reqwest::Client)> {
    // Requests with secrets never follow redirects: a 307 would resend the body
    // (password, ticket, poll secret) to wherever it points.
    let api = http_builder(info)
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let discovery = http_builder(info)
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            let ok = attempt.url().scheme() == "https"
                || (attempt.url().scheme() == "http" && is_loopback(attempt.url()));
            if attempt.previous().len() >= 5 || !ok {
                attempt.stop()
            } else {
                attempt.follow()
            }
        }))
        .build()?;
    Ok((api, discovery))
}

/// Reads a response body, failing once it passes `limit` bytes.
pub(crate) async fn read_capped(mut resp: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    if resp.content_length().is_some_and(|n| n > limit as u64) {
        return Err(Error::TooLarge { limit });
    }
    let mut out = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if out.len() + chunk.len() > limit {
            return Err(Error::TooLarge { limit });
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

impl Instance {
    /// Fetches the discovery document of `origin` (for example `fluxer.opland.net`).
    pub async fn discover(origin: &str) -> Result<Instance> {
        Self::discover_with(origin, ClientInfo::default()).await
    }

    /// Like [`Instance::discover`], with a persisted [`ClientInfo`].
    pub async fn discover_with(origin: &str, info: ClientInfo) -> Result<Instance> {
        let origin = normalize_origin(origin)?;
        let (http, discovery_http) = build_http(&info)?;
        let anon_limiter = Arc::new(RateLimiter::new(RateLimitConfig::default()));
        let (discovery, etag) = fetch(&discovery_http, &anon_limiter, &origin, None)
            .await?
            .ok_or_else(|| Error::Decode("304 without a cached document".into()))?;
        Self::build(
            origin,
            discovery,
            etag,
            (http, discovery_http),
            info,
            anon_limiter,
        )
    }

    /// Builds an instance from a discovery document cached earlier, without
    /// network. The same checks as for a fresh document apply.
    pub fn from_cached(
        origin: &str,
        discovery: Discovery,
        etag: Option<String>,
        info: ClientInfo,
    ) -> Result<Instance> {
        let origin = normalize_origin(origin)?;
        let clients = build_http(&info)?;
        let anon_limiter = Arc::new(RateLimiter::new(RateLimitConfig::default()));
        Self::build(origin, discovery, etag, clients, info, anon_limiter)
    }

    fn build(
        origin: String,
        discovery: Discovery,
        etag: Option<String>,
        (http, discovery_http): (reqwest::Client, reqwest::Client),
        info: ClientInfo,
        anon_limiter: Arc<RateLimiter>,
    ) -> Result<Instance> {
        if discovery.api_code_version > SUPPORTED_API_CODE_VERSION {
            return Err(Error::UnsupportedApiVersion(discovery.api_code_version));
        }
        let ep = &discovery.endpoints;
        let api = require_scheme(&ep.api_public, "https", "http", "endpoints.api_public")?;
        let api_base = format!("{}/v1", api.as_str().trim_end_matches('/'));
        let gateway = require_scheme(&ep.gateway, "wss", "ws", "endpoints.gateway")?;
        let origin_url = Url::parse(&origin).map_err(|e| Error::Url(e.to_string()))?;

        let media = ep.media.as_deref().and_then(|m| {
            match require_scheme(m, "https", "http", "endpoints.media") {
                Ok(u) => Some(u.as_str().trim_end_matches('/').to_owned()),
                Err(e) => {
                    tracing::warn!("ignoring media endpoint: {e}");
                    None
                }
            }
        });
        let webapp = ep
            .webapp
            .as_deref()
            .and_then(
                |w| match check_webapp(w, &origin_url, &info.trusted_webapp_hosts) {
                    Ok(u) => Some(u),
                    Err(e) => {
                        tracing::warn!("ignoring webapp endpoint, using the origin: {e}");
                        None
                    }
                },
            )
            .unwrap_or_else(|| origin_url.clone());

        Ok(Instance {
            inner: Arc::new(InstanceInner {
                origin,
                discovery,
                etag,
                api_base,
                gateway,
                webapp,
                media,
                http,
                discovery_http,
                info,
                anon_limiter,
            }),
        })
    }

    /// Fetches the document again with `If-None-Match`. Returns the same
    /// instance when it has not changed. A session keeps the instance it was
    /// created with; use [`crate::Session::check_endpoints`] before switching a
    /// token to the refreshed instance.
    pub async fn refresh(&self) -> Result<Instance> {
        let i = &self.inner;
        match fetch(
            &i.discovery_http,
            &i.anon_limiter,
            &i.origin,
            i.etag.as_deref(),
        )
        .await?
        {
            None => Ok(self.clone()),
            Some((discovery, etag)) => Self::build(
                i.origin.clone(),
                discovery,
                etag,
                (i.http.clone(), i.discovery_http.clone()),
                i.info.clone(),
                i.anon_limiter.clone(),
            ),
        }
    }

    /// The hosts a token for this instance goes to.
    pub fn pins(&self) -> EndpointPins {
        let api = Url::parse(&self.inner.api_base)
            .map(|u| u.origin().ascii_serialization())
            .unwrap_or_default();
        EndpointPins {
            api,
            gateway: self.inner.gateway.origin().ascii_serialization(),
        }
    }

    /// The API, gateway and media hosts. Store these next to the token.
    pub fn session_pins(&self) -> SessionPins {
        let media = self
            .inner
            .media
            .as_deref()
            .and_then(|m| Url::parse(m).ok())
            .map(|u| u.origin().ascii_serialization());
        let p = self.pins();
        SessionPins {
            api: p.api,
            gateway: p.gateway,
            media,
        }
    }

    /// Like [`Instance::verify_pins`], and also checks the media host when the
    /// pins have one.
    pub fn verify_session_pins(&self, pinned: &SessionPins) -> Result<()> {
        let now = self.session_pins();
        let media_ok = pinned.media.is_none() || pinned.media == now.media;
        if now.api == pinned.api && now.gateway == pinned.gateway && media_ok {
            Ok(())
        } else {
            Err(Error::EndpointsChanged {
                old: pinned.to_string(),
                new: now.to_string(),
            })
        }
    }

    /// Errors with [`Error::EndpointsChanged`] unless this instance's API and
    /// gateway hosts are the pinned ones.
    pub fn verify_pins(&self, pinned: &EndpointPins) -> Result<()> {
        let now = self.pins();
        if &now == pinned {
            Ok(())
        } else {
            Err(Error::EndpointsChanged {
                old: pinned.to_string(),
                new: now.to_string(),
            })
        }
    }

    /// The origin, for example `https://fluxer.opland.net`.
    pub fn origin(&self) -> &str {
        &self.inner.origin
    }

    /// The discovery document.
    pub fn discovery(&self) -> &Discovery {
        &self.inner.discovery
    }

    /// The ETag of the discovery document, for caching.
    pub fn etag(&self) -> Option<&str> {
        self.inner.etag.as_deref()
    }

    /// The API base with `/v1`, for example `https://fluxer.opland.net/api/v1`.
    pub fn api_base(&self) -> &str {
        &self.inner.api_base
    }

    /// The checked gateway URL.
    pub fn gateway_url(&self) -> &str {
        self.inner.gateway.as_str()
    }

    /// The client info this instance sends.
    pub fn client_info(&self) -> &ClientInfo {
        &self.inner.info
    }

    /// The web app base URL without a trailing slash. Always `https` (or
    /// loopback), on the instance's registrable domain or a trusted host;
    /// otherwise the origin.
    pub fn webapp(&self) -> &str {
        self.inner.webapp.as_str().trim_end_matches('/')
    }

    /// A page of the web app, for example `login?handoff=1`.
    pub(crate) fn webapp_page(&self, page: &str) -> String {
        let mut base = self.inner.webapp.clone();
        if !base.path().ends_with('/') {
            let p = format!("{}/", base.path());
            base.set_path(&p);
        }
        base.join(page)
            .map(|u| u.to_string())
            .unwrap_or_else(|_| format!("{}/{page}", self.webapp()))
    }

    /// Media proxy URL of a user avatar, or `None` when the user has no avatar.
    /// `size` is a power of two from 16 to 4096; Fluxer serves WebP. See
    /// [`Instance::media`] for animated variants and other assets.
    pub fn avatar_url(
        &self,
        user_id: &str,
        avatar_hash: Option<&str>,
        size: u32,
    ) -> Option<String> {
        let media = self.inner.media.as_deref()?;
        let hash = avatar_hash?;
        if !crate::media::is_snowflake(user_id) || !crate::media::is_hash(hash) {
            return None;
        }
        Some(format!("{media}/avatars/{user_id}/{hash}.webp?size={size}"))
    }

    /// Media proxy URL of a guild icon.
    pub fn guild_icon_url(
        &self,
        guild_id: &str,
        icon_hash: Option<&str>,
        size: u32,
    ) -> Option<String> {
        let media = self.inner.media.as_deref()?;
        let hash = icon_hash?;
        if !crate::media::is_snowflake(guild_id) || !crate::media::is_hash(hash) {
            return None;
        }
        Some(format!("{media}/icons/{guild_id}/{hash}.webp?size={size}"))
    }
}

fn check_webapp(raw: &str, origin: &Url, trusted: &[String]) -> Result<Url> {
    let url = require_scheme(raw, "https", "http", "endpoints.webapp")?;
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    let listed = trusted.iter().any(|t| t.eq_ignore_ascii_case(&host));
    if listed || registrable_domain(&url) == registrable_domain(origin) {
        Ok(url)
    } else {
        Err(Error::Url(format!(
            "endpoints.webapp host {host} is not on the instance's domain"
        )))
    }
}

async fn fetch(
    http: &reqwest::Client,
    limiter: &RateLimiter,
    origin: &str,
    etag: Option<&str>,
) -> Result<Option<(Discovery, Option<String>)>> {
    let url = format!("{origin}/.well-known/fluxer");
    limiter.acquire("GET /.well-known/fluxer").await;
    let mut req = http.get(&url).header("Accept", "application/json");
    if let Some(tag) = etag {
        req = req.header("If-None-Match", tag);
    }
    let resp = req.send().await?;
    if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
        return Ok(None);
    }
    if !resp.status().is_success() {
        return Err(Error::Decode(format!(
            "discovery returned HTTP {}",
            resp.status()
        )));
    }
    let new_etag = resp
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let bytes = read_capped(resp, MAX_DISCOVERY_BYTES).await?;
    let discovery: Discovery = serde_json::from_slice(&bytes)
        .map_err(|e| Error::Decode(format!("discovery document: {e}")))?;
    Ok(Some((discovery, new_etag)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(api: &str, gateway: &str, media: &str, webapp: &str) -> Discovery {
        serde_json::from_value(serde_json::json!({
            "api_code_version": 1,
            "endpoints": {"api_public": api, "gateway": gateway, "media": media, "webapp": webapp},
            "features": null
        }))
        .unwrap()
    }

    #[test]
    fn origins_normalize() {
        assert_eq!(
            normalize_origin("fluxer.opland.net").unwrap(),
            "https://fluxer.opland.net"
        );
        assert_eq!(
            normalize_origin("https://fluxer.opland.net/x/").unwrap(),
            "https://fluxer.opland.net"
        );
        assert_eq!(
            normalize_origin("http://127.0.0.1:8080").unwrap(),
            "http://127.0.0.1:8080"
        );
        assert_eq!(
            normalize_origin("http://localhost:3000").unwrap(),
            "http://localhost:3000"
        );
        assert!(normalize_origin("http://[::1]:3000").is_ok());
        assert!(normalize_origin("ftp://x").is_err());
    }

    #[test]
    fn plain_http_origin_is_refused() {
        let err = normalize_origin("http://fluxer.opland.net").unwrap_err();
        assert!(matches!(err, Error::Url(_)), "{err:?}");
    }

    fn build(d: Discovery) -> Result<Instance> {
        Instance::from_cached("https://chat.example.org", d, None, ClientInfo::default())
    }

    #[test]
    fn insecure_api_or_gateway_is_refused() {
        let ok = "https://chat.example.org";
        assert!(build(doc(
            "http://chat.example.org/api",
            "wss://chat.example.org/gw",
            ok,
            ok
        ))
        .is_err());
        assert!(build(doc(
            "https://chat.example.org/api",
            "ws://chat.example.org/gw",
            ok,
            ok
        ))
        .is_err());
        assert!(build(doc(
            "https://chat.example.org/api",
            "https://chat.example.org/gw",
            ok,
            ok
        ))
        .is_err());
        assert!(build(doc(
            "https://api.other.net/api",
            "wss://gw.other.net/",
            ok,
            ok
        ))
        .is_ok());
    }

    #[test]
    fn bad_webapp_and_media_fall_back() {
        for webapp in [
            "file:///C:/Windows/System32/calc.exe",
            "ms-msdt:/id",
            "search-ms:query=x",
            "http://chat.example.org",
            "https://evil.example.net",
        ] {
            let i = build(doc(
                "https://chat.example.org/api",
                "wss://chat.example.org/gw",
                "http://media.example.org",
                webapp,
            ))
            .unwrap();
            assert_eq!(i.webapp(), "https://chat.example.org", "{webapp}");
            assert_eq!(i.avatar_url("1", Some("h"), 64), None);
        }
        let i = build(doc(
            "https://chat.example.org/api",
            "wss://chat.example.org/gw",
            "https://media.example.org",
            "https://web.example.org/app",
        ))
        .unwrap();
        assert_eq!(i.webapp(), "https://web.example.org/app");
        assert_eq!(
            i.webapp_page("login?handoff=1"),
            "https://web.example.org/app/login?handoff=1"
        );
        assert!(i.avatar_url("1", Some("h"), 64).is_some());
    }

    #[test]
    fn trusted_webapp_host_is_allowed() {
        let info = ClientInfo {
            trusted_webapp_hosts: vec!["web.partner.net".into()],
            ..ClientInfo::default()
        };
        let i = Instance::from_cached(
            "https://chat.example.org",
            doc(
                "https://chat.example.org/api",
                "wss://chat.example.org/gw",
                "https://chat.example.org/media",
                "https://web.partner.net",
            ),
            None,
            info,
        )
        .unwrap();
        assert_eq!(i.webapp(), "https://web.partner.net");
    }

    #[test]
    fn pins_detect_moved_endpoints() {
        let ok = "https://chat.example.org";
        let a = build(doc(
            "https://chat.example.org/api",
            "wss://chat.example.org/gw",
            ok,
            ok,
        ))
        .unwrap();
        let b = build(doc(
            "https://evil.example.net/api",
            "wss://chat.example.org/gw",
            ok,
            ok,
        ))
        .unwrap();
        let pins = a.pins();
        let full = a.session_pins();
        assert!(a.verify_session_pins(&full).is_ok());
        // Old stored pins (no media) still read and still verify.
        let old: SessionPins =
            serde_json::from_str(&serde_json::to_string(&pins).unwrap()).unwrap();
        assert_eq!(old.media, None);
        assert!(a.verify_session_pins(&old).is_ok());
        // New pins read as the old type too.
        let back: EndpointPins =
            serde_json::from_str(&serde_json::to_string(&full).unwrap()).unwrap();
        assert_eq!(back, pins);
        let mut moved = full.clone();
        moved.media = Some("https://elsewhere.example".into());
        assert!(matches!(
            a.verify_session_pins(&moved),
            Err(Error::EndpointsChanged { .. })
        ));
        assert_eq!(pins.api, "https://chat.example.org");
        assert_eq!(pins.gateway, "wss://chat.example.org");
        assert!(a.verify_pins(&pins).is_ok());
        assert!(matches!(
            b.verify_pins(&pins),
            Err(Error::EndpointsChanged { .. })
        ));
    }

    #[test]
    fn registrable_domains() {
        let d = |s: &str| registrable_domain(&Url::parse(s).unwrap()).unwrap();
        assert_eq!(d("https://web.fluxer.app"), "fluxer.app");
        assert_eq!(d("https://a.b.example.co.uk"), "example.co.uk");
        // Shared hosting suffixes: other customers are other domains.
        assert_eq!(d("https://alice.github.io"), "alice.github.io");
        assert_ne!(d("https://alice.github.io"), d("https://bob.github.io"));
        assert_ne!(d("https://a.duckdns.org"), d("https://b.duckdns.org"));
        assert_ne!(d("https://a.pages.dev"), d("https://b.pages.dev"));
        assert_eq!(d("https://localhost"), "localhost");
        assert_eq!(d("http://127.0.0.1:9"), "127.0.0.1");
    }

    #[test]
    fn uuid_shape() {
        let id = random_uuid();
        assert_eq!(id.len(), 36);
        assert_eq!(id.as_bytes()[14], b'4');
    }
}
