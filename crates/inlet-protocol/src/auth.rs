//! Sign-in: password (+ TOTP), new-IP authorization, and the browser handoff.
//!
//! Two ways in:
//!
//! 1. **Browser handoff** ([`Handoff`]): the app shows a 12-character code and
//!    opens `{webapp}/login?handoff=1`; the person signs in there (any method:
//!    passkey, SSO, email code) and types the code; the app polls and gets a
//!    token. This always works and is the recommended path.
//! 2. **Password** ([`sign_in_password`]): needs the ALTCHA proof-of-work,
//!    which this crate solves (see [`crate::altcha`]). The solver follows
//!    altcha-lib but is only tested against mocks so far; if the instance
//!    rejects it, the result is [`SignInResult::Captcha`] and the UI should fall
//!    back to the handoff. Accounts with a second factor get
//!    [`SignInResult::NeedsMfa`], finished with [`sign_in_totp`]. A sign-in from
//!    a new IP may need the email link ([`SignInResult::IpAuthorizationRequired`],
//!    then [`poll_ip_authorization`]).

use std::fmt;
use std::time::Duration;

use base64::Engine;
use reqwest::Method;
use serde_json::{json, Value};

use crate::error::{ApiError, Error, Result};
use crate::instance::{EndpointPins, Instance};
use crate::models::{Snowflake, User};
use crate::rest::{send, Client, RawResponse, Request};
use crate::token::Token;

/// How long the CAPTCHA solver may run.
const CAPTCHA_TIMEOUT: Duration = Duration::from_secs(60);

/// A short-lived secret (MFA ticket, IP ticket, handoff poll secret). Redacted in `Debug`.
#[derive(Clone, PartialEq, Eq)]
pub struct Ticket(String);

impl Ticket {
    /// A ticket from a raw value, for tests.
    #[doc(hidden)]
    pub fn for_tests(value: &str) -> Ticket {
        Ticket(value.to_owned())
    }

    /// The raw value.
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Ticket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Ticket(<redacted>)")
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.0);
    }
}

/// A finished sign-in.
#[derive(Debug, Clone)]
pub struct SignedIn {
    /// The new session token. Store it in the OS keychain.
    pub token: Token,
    /// The account ID.
    pub user_id: Snowflake,
    /// The account, when the API sent it.
    pub user: Option<Box<User>>,
}

/// A second-factor method.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MfaMethod {
    /// Authenticator app code; use [`sign_in_totp`].
    Totp,
    /// Passkey / security key (not supported by this crate; use the handoff).
    Webauthn,
    /// Backup code; send it to [`sign_in_totp`] as the code.
    BackupCodes,
    /// Something newer.
    Other(String),
}

/// What a sign-in step returned.
#[derive(Debug)]
#[non_exhaustive]
pub enum SignInResult {
    /// Signed in.
    Ok(SignedIn),
    /// The account has a second factor. The ticket lasts 5 minutes or 5 wrong codes.
    NeedsMfa {
        /// Pass to [`sign_in_totp`].
        ticket: Ticket,
        /// What the account can use.
        methods: Vec<MfaMethod>,
    },
    /// New IP address: Fluxer emailed a link. Poll [`poll_ip_authorization`] until it is used.
    IpAuthorizationRequired {
        /// Pass to [`poll_ip_authorization`].
        ticket: Ticket,
    },
    /// A CAPTCHA was required and could not be passed: the answer was not
    /// accepted, the challenge was too hard or expired, or the algorithm is
    /// unknown. Use the browser handoff.
    Captcha,
    /// The API refused (wrong password, invalid code, SSO required, suspended, ...).
    /// [`ApiError::field_code`] has codes like `INVALID_EMAIL_OR_PASSWORD` or `INVALID_CODE`.
    Error(ApiError),
}

fn client_properties(instance: &Instance) -> String {
    let props = json!({ "os": instance.client_info().os });
    base64::engine::general_purpose::STANDARD.encode(props.to_string())
}

fn signed_in_from(v: &Value) -> Option<SignedIn> {
    let token = v.get("token")?.as_str()?;
    if token.is_empty() {
        return None;
    }
    Some(SignedIn {
        token: Token::new(token),
        user_id: match v.get("user_id") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Number(n)) => n.to_string(),
            _ => String::new(),
        },
        user: v
            .get("user")
            .and_then(|u| serde_json::from_value(u.clone()).ok()),
    })
}

/// Sends an unauthenticated POST and solves CAPTCHAs on the way (at most twice).
async fn post_with_captcha(
    instance: &Instance,
    path: &str,
    body: &Value,
) -> Result<(RawResponse, bool)> {
    let props = client_properties(instance);
    let mut captcha_token: Option<String> = None;
    let mut solves = 0;
    loop {
        let mut headers: Vec<(&'static str, String)> =
            vec![("X-Fluxer-Client-Properties", props.clone())];
        if let Some(t) = &captcha_token {
            headers.push(("X-Captcha-Token", t.clone()));
        }
        let raw = send(
            instance,
            &instance.inner.anon_limiter,
            None,
            Request {
                method: Method::POST,
                path,
                body: Some(body),
                headers: &headers,
                upload: None,
            },
        )
        .await?;
        if raw.status == 400 {
            if let Some(challenge) = crate::altcha::challenge_from_error(&raw.value()) {
                if solves >= 2 {
                    return Ok((raw, true));
                }
                solves += 1;
                tracing::info!("solving the sign-in CAPTCHA");
                match crate::altcha::solve(challenge, CAPTCHA_TIMEOUT).await {
                    Ok(t) => captcha_token = Some(t),
                    Err(e) => {
                        tracing::warn!("CAPTCHA not solved: {e}");
                        return Ok((raw, true));
                    }
                }
                continue;
            }
        }
        return Ok((raw, false));
    }
}

fn interpret_login(raw: RawResponse, captcha_failed: bool) -> Result<SignInResult> {
    if captcha_failed {
        return Ok(SignInResult::Captcha);
    }
    let v = raw.value();
    if raw.is_success() {
        if let Some(ok) = signed_in_from(&v) {
            return Ok(SignInResult::Ok(ok));
        }
        if v.get("mfa").and_then(Value::as_bool) == Some(true) {
            let ticket = v
                .get("ticket")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::Decode("MFA response without ticket".into()))?;
            let methods = v
                .get("allowed_methods")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(|m| match m {
                            "totp" => MfaMethod::Totp,
                            "webauthn" => MfaMethod::Webauthn,
                            "backup_codes" => MfaMethod::BackupCodes,
                            other => MfaMethod::Other(other.to_owned()),
                        })
                        .collect()
                })
                .unwrap_or_default();
            return Ok(SignInResult::NeedsMfa {
                ticket: Ticket(ticket.to_owned()),
                methods,
            });
        }
        return Err(Error::Decode(
            "sign-in response has neither token nor mfa".into(),
        ));
    }
    let err = raw.api_error();
    if err.code == "IP_AUTHORIZATION_REQUIRED" {
        if let Some(ticket) = v.get("ticket").and_then(Value::as_str) {
            return Ok(SignInResult::IpAuthorizationRequired {
                ticket: Ticket(ticket.to_owned()),
            });
        }
    }
    Ok(SignInResult::Error(err))
}

/// `POST /auth/login`. `login` is an email address or, on a username instance, a username.
/// The password is only sent to the instance; this crate keeps no copy.
pub async fn sign_in_password(
    instance: &Instance,
    login: &str,
    password: &str,
) -> Result<SignInResult> {
    let body = json!({ "login": login, "password": password });
    let (raw, captcha_failed) = post_with_captcha(instance, "/auth/login", &body).await?;
    interpret_login(raw, captcha_failed)
}

/// `POST /auth/login/mfa/totp`: finishes a sign-in with an authenticator or backup code.
pub async fn sign_in_totp(
    instance: &Instance,
    ticket: &Ticket,
    code: &str,
) -> Result<SignInResult> {
    let body = json!({ "ticket": ticket.expose_secret(), "code": code.trim() });
    let (raw, captcha_failed) = post_with_captcha(instance, "/auth/login/mfa/totp", &body).await?;
    interpret_login(raw, captcha_failed)
}

/// State of a sign-in waiting for the new-IP email link.
#[derive(Debug)]
#[non_exhaustive]
pub enum IpAuthorization {
    /// Not yet. Poll again in a few seconds (limit: 60 per minute).
    Pending,
    /// The link was used.
    Completed(SignedIn),
}

/// `GET /auth/ip-authorization/poll`.
pub async fn poll_ip_authorization(
    instance: &Instance,
    ticket: &Ticket,
) -> Result<IpAuthorization> {
    let path = format!(
        "/auth/ip-authorization/poll?ticket={}",
        url::form_urlencoded::byte_serialize(ticket.expose_secret().as_bytes()).collect::<String>()
    );
    let raw = anon(instance, Method::GET, &path, None, false).await?;
    if !raw.is_success() {
        return Err(Error::Api(Box::new(raw.api_error())));
    }
    let v = raw.value();
    if v.get("completed").and_then(Value::as_bool) == Some(true) {
        if let Some(ok) = signed_in_from(&v) {
            return Ok(IpAuthorization::Completed(ok));
        }
    }
    Ok(IpAuthorization::Pending)
}

async fn anon(
    instance: &Instance,
    method: Method,
    path: &str,
    body: Option<&Value>,
    origin_header: bool,
) -> Result<RawResponse> {
    let headers: Vec<(&'static str, String)> = if origin_header {
        // The official desktop shell sends the web app origin on handoff POSTs.
        vec![("Origin", web_origin(instance))]
    } else {
        Vec::new()
    };
    send(
        instance,
        &instance.inner.anon_limiter,
        None,
        Request {
            method,
            path,
            body,
            headers: &headers,
            upload: None,
        },
    )
    .await
}

fn web_origin(instance: &Instance) -> String {
    url::Url::parse(instance.webapp())
        .map(|u| u.origin().ascii_serialization())
        .unwrap_or_else(|_| instance.origin().to_owned())
}

/// Result of polling a [`Handoff`].
#[derive(Debug)]
pub enum HandoffStatus {
    /// The person has not approved yet.
    Pending,
    /// Approved: here is the token.
    Completed(SignedIn),
    /// The person denied it on the signed-in device.
    Denied,
    /// The code expired (5 minutes) or is unknown. Start a new one.
    Expired,
}

/// How long a handoff code lives on the server.
const HANDOFF_LIFETIME: Duration = Duration::from_secs(5 * 60);
/// Failed polls in a row that [`Handoff::wait`] tolerates.
const HANDOFF_MAX_FAILURES: u32 = 3;

/// A pending browser sign-in ("desktop handoff", `/v1/auth/handoff/*`).
///
/// 1. [`Handoff::start`], show [`Handoff::display_code`] and open [`Handoff::browser_url`].
/// 2. Call [`Handoff::wait`] (or [`Handoff::poll`] on a timer) until it completes.
/// 3. Show who signed in (`SignedIn::user`) before storing the token, so a
///    person who saw the code cannot slip in their own account unnoticed.
#[derive(Debug)]
pub struct Handoff {
    instance: Instance,
    code: String,
    expires_at: String,
    poll_secret: Ticket,
    deadline: tokio::time::Instant,
}

impl Handoff {
    /// `POST /auth/handoff/initiate` without a return URI (code method).
    pub async fn start(instance: &Instance) -> Result<Handoff> {
        let body = json!({});
        let raw = anon(
            instance,
            Method::POST,
            "/auth/handoff/initiate",
            Some(&body),
            true,
        )
        .await?;
        if !raw.is_success() {
            return Err(Error::Api(Box::new(raw.api_error())));
        }
        let v = raw.value();
        let code = v
            .get("code")
            .and_then(Value::as_str)
            .filter(|c| {
                !c.is_empty()
                    && c.len() <= 64
                    && c.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            })
            .ok_or_else(|| Error::Decode("handoff initiate without a valid code".into()))?
            .to_owned();
        // Only POST .../status with the poll secret returns the token.
        let poll_secret = v
            .get("poll_secret")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty() && s.len() <= 256)
            .map(|s| Ticket(s.to_owned()))
            .ok_or_else(|| {
                Error::Decode(
                    "the instance gave no handoff poll secret; it is too old for Inlet".into(),
                )
            })?;
        Ok(Handoff {
            instance: instance.clone(),
            code,
            expires_at: v
                .get("expires_at")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            poll_secret,
            deadline: tokio::time::Instant::now() + HANDOFF_LIFETIME,
        })
    }

    /// The raw code.
    pub fn code(&self) -> &str {
        &self.code
    }

    /// The code as Fluxer shows it: two groups of six (`ABCDEF-GHIJKL`).
    pub fn display_code(&self) -> String {
        let clean: String = self
            .code
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .collect::<String>()
            .to_uppercase();
        if clean.len() == 12 {
            format!("{}-{}", &clean[..6], &clean[6..])
        } else {
            clean
        }
    }

    /// When the code expires (ISO 8601).
    pub fn expires_at(&self) -> &str {
        &self.expires_at
    }

    /// The page to open in the browser: `{webapp}/login?handoff=1`. The web app
    /// URL is checked (https, on the instance's domain), so this is safe to hand
    /// to the OS.
    pub fn browser_url(&self) -> String {
        self.instance.webapp_page("login?handoff=1")
    }

    /// Asks once with `POST .../status` and the poll secret.
    pub async fn poll(&self) -> Result<HandoffStatus> {
        let path = format!("/auth/handoff/{}/status", self.code);
        let body = json!({ "poll_secret": self.poll_secret.expose_secret() });
        let raw = anon(&self.instance, Method::POST, &path, Some(&body), true).await?;
        if !raw.is_success() {
            return Err(Error::Api(Box::new(raw.api_error())));
        }
        let v = raw.value();
        Ok(
            match v.get("status").and_then(Value::as_str).unwrap_or("pending") {
                "completed" => match signed_in_from(&v) {
                    Some(ok) => HandoffStatus::Completed(ok),
                    None => {
                        return Err(Error::Decode(
                            "the handoff completed without a token".into(),
                        ))
                    }
                },
                "denied" => HandoffStatus::Denied,
                "expired" => HandoffStatus::Expired,
                _ => HandoffStatus::Pending,
            },
        )
    }

    /// Polls every `every` (at least one second) until the handoff is no longer
    /// pending, or returns [`HandoffStatus::Expired`] when the 5-minute code
    /// lifetime is over. Up to three network or server errors in a row are
    /// retried with a growing pause; rate limits and other API refusals are returned.
    pub async fn wait(&self, every: Duration) -> Result<HandoffStatus> {
        let every = every.max(Duration::from_secs(1));
        let mut failures = 0u32;
        loop {
            if tokio::time::Instant::now() >= self.deadline {
                return Ok(HandoffStatus::Expired);
            }
            match self.poll().await {
                Ok(HandoffStatus::Pending) => {
                    failures = 0;
                    tokio::time::sleep(every).await;
                }
                Ok(done) => return Ok(done),
                Err(e) if is_transient(&e) && failures < HANDOFF_MAX_FAILURES => {
                    failures += 1;
                    tracing::warn!("handoff poll failed ({failures}): {e}");
                    tokio::time::sleep(every * 2u32.pow(failures)).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// `DELETE /auth/handoff/{code}`.
    pub async fn cancel(self) -> Result<()> {
        let body = json!({ "poll_secret": self.poll_secret.expose_secret() });
        let path = format!("/auth/handoff/{}", self.code);
        let raw = anon(&self.instance, Method::DELETE, &path, Some(&body), true).await?;
        if raw.is_success() {
            Ok(())
        } else {
            Err(Error::Api(Box::new(raw.api_error())))
        }
    }
}

fn is_transient(e: &Error) -> bool {
    match e {
        Error::Http(_) => true,
        Error::Api(api) => api.status >= 500,
        _ => false,
    }
}

/// A signed-in session: the instance, the token and the REST client. Cheap to clone.
///
/// The session is bound to the API and gateway hosts of the instance it was
/// created with ([`Session::pins`]). Store the pins next to the token and use
/// [`Session::from_token_pinned`] on the next start, so a changed discovery
/// document cannot send the token to another host.
#[derive(Debug, Clone)]
pub struct Session {
    client: Client,
}

impl Session {
    /// A session from a stored token **without** endpoint pinning. It trusts
    /// whatever API and gateway hosts the instance names right now, so a
    /// changed discovery document can send the token elsewhere. Prefer
    /// [`Session::from_token_pinned`] with the pins stored next to the token;
    /// use this only for a token that has no pins yet (for example one pasted
    /// by hand), and store [`Session::pins`] right after.
    ///
    /// Makes no request; call [`Client::me`] to check that the token works.
    pub fn from_token_unpinned(instance: Instance, token: Token) -> Session {
        Session {
            client: Client::new(instance, token),
        }
    }

    /// Old name of [`Session::from_token_unpinned`].
    #[deprecated(
        note = "use `from_token_pinned`, or `from_token_unpinned` for a token without pins"
    )]
    pub fn from_token(instance: Instance, token: Token) -> Session {
        Self::from_token_unpinned(instance, token)
    }

    /// A session from a stored token and the pins stored with it. Fails with
    /// [`Error::EndpointsChanged`] when the instance now names other hosts; then
    /// ask the person to sign in again and do not use the stored token.
    pub fn from_token_pinned(
        instance: Instance,
        token: Token,
        pins: &EndpointPins,
    ) -> Result<Session> {
        instance.verify_pins(pins)?;
        Ok(Self::from_token_unpinned(instance, token))
    }

    /// A session from a finished sign-in.
    pub fn from_signed_in(instance: Instance, signed_in: SignedIn) -> Session {
        Session {
            client: Client::for_user(instance, signed_in.token.clone(), &signed_in.user_id),
        }
    }

    /// The API and gateway hosts this session's token belongs to.
    pub fn pins(&self) -> EndpointPins {
        self.instance().pins()
    }

    /// Checks that a refreshed instance still uses this session's hosts.
    pub fn check_endpoints(&self, refreshed: &Instance) -> Result<()> {
        refreshed.verify_pins(&self.pins())
    }

    /// The REST client.
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// The instance.
    pub fn instance(&self) -> &Instance {
        self.client.instance()
    }

    /// The token.
    pub fn token(&self) -> &Token {
        self.client.token()
    }

    /// Starts the gateway on the current tokio runtime. See [`crate::gateway`].
    pub fn connect_gateway(
        &self,
        options: crate::gateway::GatewayOptions,
    ) -> (
        crate::gateway::GatewayHandle,
        tokio::sync::mpsc::Receiver<crate::gateway::GatewayEvent>,
    ) {
        crate::gateway::Gateway::connect(self.instance().clone(), self.token().clone(), options)
    }
}
