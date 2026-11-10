//! Errors.

use serde::Deserialize;

/// An error body from the Fluxer API (`{"code": "...", "message": "..."}`).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ApiError {
    /// HTTP status of the response.
    #[serde(skip)]
    pub status: u16,
    /// Machine-readable code, for example `INVALID_FORM_BODY` or `UNKNOWN_CHANNEL`.
    #[serde(default)]
    pub code: String,
    /// Human-readable message in the request locale.
    #[serde(default)]
    pub message: String,
    /// Field errors for validation failures, as sent by the API.
    #[serde(default)]
    pub errors: Option<serde_json::Value>,
}

impl ApiError {
    /// The first field-level error code, if any (for example `INVALID_EMAIL_OR_PASSWORD`).
    pub fn field_code(&self) -> Option<&str> {
        let errors = self.errors.as_ref()?.as_array()?;
        errors.iter().find_map(|e| e.get("code")?.as_str())
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTTP {} {}: {}", self.status, self.code, self.message)
    }
}

/// Everything that can go wrong in this crate.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Network or TLS failure, or a timeout. The URL is stripped, because a
    /// query string can carry a ticket.
    #[error("network error: {0}")]
    Http(reqwest::Error),
    /// The instance answered with an error.
    #[error("API error: {0}")]
    Api(Box<ApiError>),
    /// The token is not valid any more (HTTP 401). Sign in again.
    #[error("the session token was rejected; sign in again")]
    Unauthorized,
    /// The global rate limit denied a request. For a user token the API revokes
    /// the token at this point, so the UI should ask the person to sign in again.
    #[error("the global rate limit was hit; the token is probably revoked")]
    GlobalRateLimited,
    /// A route stayed rate limited after the automatic retries.
    #[error("rate limited, retry after {retry_after:?}")]
    RateLimited {
        /// How long the API asked us to wait.
        retry_after: std::time::Duration,
    },
    /// The response did not have the expected shape.
    #[error("unexpected response: {0}")]
    Decode(String),
    /// The instance URL or a discovered URL is not usable.
    #[error("invalid URL: {0}")]
    Url(String),
    /// The instance needs a newer client (`api_code_version` is higher than we support).
    #[error("instance API code version {0} is not supported")]
    UnsupportedApiVersion(u32),
    /// Solving the CAPTCHA failed or took too long.
    #[error("captcha could not be solved: {0}")]
    Captcha(String),
    /// The instance now names a different API or gateway host than the one this
    /// token was signed in with. The token is not sent there. Ask the person to
    /// confirm the instance and sign in again.
    #[error("the instance endpoints changed from {old} to {new}; sign in again")]
    EndpointsChanged {
        /// The pinned endpoints.
        old: String,
        /// What the instance says now.
        new: String,
    },
    /// An argument cannot be sent (for example an emoji that is not valid in a
    /// reaction path, or too many files). Nothing was sent.
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// A response or gateway payload was larger than the limit.
    #[error("response too large (over {limit} bytes)")]
    TooLarge {
        /// The limit in bytes.
        limit: usize,
    },
}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        Error::Http(e.without_url())
    }
}

/// Result type of this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;
