//! Instance discovery (`GET {origin}/.well-known/fluxer`).

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::{Error, Result};
use crate::ratelimit::{RateLimitConfig, RateLimiter};

/// The highest `api_code_version` this crate speaks.
pub const SUPPORTED_API_CODE_VERSION: u32 = 1;

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
    #[serde(default)]
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

pub(crate) struct InstanceInner {
    pub(crate) origin: String,
    pub(crate) discovery: Discovery,
    pub(crate) etag: Option<String>,
    /// `api_public` + `/v1`, no trailing slash.
    pub(crate) api_base: String,
    pub(crate) http: reqwest::Client,
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

/// Turns `fluxer.example.org`, `https://fluxer.example.org/` and similar into `https://fluxer.example.org`.
pub fn normalize_origin(origin: &str) -> Result<String> {
    let trimmed = origin.trim();
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_owned()
    } else {
        format!("https://{trimmed}")
    };
    let url = url::Url::parse(&with_scheme).map_err(|e| Error::Url(format!("{origin}: {e}")))?;
    if !matches!(url.scheme(), "https" | "http") || url.host_str().is_none() {
        return Err(Error::Url(format!("{origin}: not an http(s) origin")));
    }
    Ok(url.origin().ascii_serialization())
}

pub(crate) fn build_http(info: &ClientInfo) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(info.user_agent.clone())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .use_rustls_tls()
        .build()
        .map_err(Error::Http)
}

impl Instance {
    /// Fetches the discovery document of `origin` (for example `fluxer.opland.net`).
    pub async fn discover(origin: &str) -> Result<Instance> {
        Self::discover_with(origin, ClientInfo::default()).await
    }

    /// Like [`Instance::discover`], with a persisted [`ClientInfo`].
    pub async fn discover_with(origin: &str, info: ClientInfo) -> Result<Instance> {
        let origin = normalize_origin(origin)?;
        let http = build_http(&info)?;
        let anon_limiter = Arc::new(RateLimiter::new(RateLimitConfig::default()));
        let (discovery, etag) = fetch(&http, &anon_limiter, &origin, None)
            .await?
            .ok_or_else(|| Error::Decode("304 without a cached document".into()))?;
        Self::build(origin, discovery, etag, http, info, anon_limiter)
    }

    /// Builds an instance from a discovery document cached earlier, without network.
    pub fn from_cached(
        origin: &str,
        discovery: Discovery,
        etag: Option<String>,
        info: ClientInfo,
    ) -> Result<Instance> {
        let origin = normalize_origin(origin)?;
        let http = build_http(&info)?;
        let anon_limiter = Arc::new(RateLimiter::new(RateLimitConfig::default()));
        Self::build(origin, discovery, etag, http, info, anon_limiter)
    }

    fn build(
        origin: String,
        discovery: Discovery,
        etag: Option<String>,
        http: reqwest::Client,
        info: ClientInfo,
        anon_limiter: Arc<RateLimiter>,
    ) -> Result<Instance> {
        if discovery.api_code_version > SUPPORTED_API_CODE_VERSION {
            return Err(Error::UnsupportedApiVersion(discovery.api_code_version));
        }
        let api_base = format!(
            "{}/v1",
            discovery.endpoints.api_public.trim_end_matches('/')
        );
        url::Url::parse(&api_base).map_err(|e| Error::Url(format!("api_public: {e}")))?;
        url::Url::parse(&discovery.endpoints.gateway)
            .map_err(|e| Error::Url(format!("gateway: {e}")))?;
        Ok(Instance {
            inner: Arc::new(InstanceInner {
                origin,
                discovery,
                etag,
                api_base,
                http,
                info,
                anon_limiter,
            }),
        })
    }

    /// Fetches the document again with `If-None-Match`. Returns the same
    /// instance when it has not changed.
    pub async fn refresh(&self) -> Result<Instance> {
        let i = &self.inner;
        match fetch(&i.http, &i.anon_limiter, &i.origin, i.etag.as_deref()).await? {
            None => Ok(self.clone()),
            Some((discovery, etag)) => Self::build(
                i.origin.clone(),
                discovery,
                etag,
                i.http.clone(),
                i.info.clone(),
                i.anon_limiter.clone(),
            ),
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

    /// The client info this instance sends.
    pub fn client_info(&self) -> &ClientInfo {
        &self.inner.info
    }

    /// The web app base URL (falls back to the origin).
    pub fn webapp(&self) -> &str {
        self.inner
            .discovery
            .endpoints
            .webapp
            .as_deref()
            .unwrap_or(&self.inner.origin)
            .trim_end_matches('/')
    }

    /// Media proxy URL of a user avatar, or `None` when the user has no avatar.
    /// `size` is a power of two from 16 to 4096; Fluxer serves WebP.
    pub fn avatar_url(
        &self,
        user_id: &str,
        avatar_hash: Option<&str>,
        size: u32,
    ) -> Option<String> {
        let media = self
            .inner
            .discovery
            .endpoints
            .media
            .as_deref()?
            .trim_end_matches('/');
        let hash = avatar_hash?;
        Some(format!("{media}/avatars/{user_id}/{hash}.webp?size={size}"))
    }

    /// Media proxy URL of a guild icon.
    pub fn guild_icon_url(
        &self,
        guild_id: &str,
        icon_hash: Option<&str>,
        size: u32,
    ) -> Option<String> {
        let media = self
            .inner
            .discovery
            .endpoints
            .media
            .as_deref()?
            .trim_end_matches('/');
        let hash = icon_hash?;
        Some(format!("{media}/icons/{guild_id}/{hash}.webp?size={size}"))
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
    let bytes = resp.bytes().await?;
    let discovery: Discovery = serde_json::from_slice(&bytes)
        .map_err(|e| Error::Decode(format!("discovery document: {e}")))?;
    Ok(Some((discovery, new_etag)))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(normalize_origin("ftp://x").is_err());
    }

    #[test]
    fn uuid_shape() {
        let id = random_uuid();
        assert_eq!(id.len(), 36);
        assert_eq!(id.as_bytes()[14], b'4');
    }
}
