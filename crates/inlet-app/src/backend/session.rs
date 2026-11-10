// SPDX-License-Identifier: AGPL-3.0-or-later
//! Talking to a Fluxer instance through `inlet-protocol` before and around a session:
//! discovery, building and checking a session, the browser handoff, and ending a token
//! on the server. Failures are turned into sentences for the person here.

use std::time::Duration;

use inlet_protocol as p;

use crate::model::{self, Guild};
use crate::token_store;

/// How long the start-up checks may take before Inlet treats the instance as unreachable.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(12);

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
        Failure {
            kind: classify(&err),
            message: describe(&err),
        }
    }
}

impl Failure {
    fn timeout() -> Self {
        Failure {
            message: t!("error-timeout"),
            kind: FailureKind::Offline,
        }
    }
}

pub fn classify(err: &p::Error) -> FailureKind {
    match err {
        p::Error::Unauthorized
        | p::Error::GlobalRateLimited
        | p::Error::EndpointsChanged { .. } => FailureKind::SessionEnded,
        p::Error::Http(_) | p::Error::RateLimited { .. } => FailureKind::Offline,
        p::Error::Api(api) if api.status >= 500 || api.status == 0 => FailureKind::Offline,
        _ => FailureKind::Other,
    }
}

/// Turns a protocol error into a sentence for the sign-in screen or the error bar.
pub fn describe(err: &p::Error) -> String {
    match err {
        p::Error::Unauthorized => t!("error-unauthorized"),
        p::Error::GlobalRateLimited => t!("error-global-rate-limit"),
        p::Error::Api(api) => {
            let code = api.field_code().unwrap_or(&api.code);
            if api.message.is_empty() {
                t!("error-api-refused", error = api.to_string())
            } else {
                format!("{} ({code})", model::clean_name(&api.message))
            }
        }
        p::Error::Http(_) => t!("error-unreachable"),
        p::Error::EndpointsChanged { .. } => t!("error-endpoints-changed"),
        p::Error::Url(msg) => t!("error-bad-address", reason = model::clean_name(msg)),
        p::Error::TooLarge { .. } => t!("error-too-large"),
        other => other.to_string(),
    }
}

async fn with_timeout<T>(
    fut: impl std::future::Future<Output = Result<T, Failure>>,
) -> Result<T, Failure> {
    tokio::time::timeout(CONNECT_TIMEOUT, fut)
        .await
        .unwrap_or_else(|_| Err(Failure::timeout()))
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
    Stored {
        token: p::Token,
        pins: p::SessionPins,
        /// Pasted by the person: never signed out on the server, only forgotten.
        pasted: bool,
    },
    /// A finished browser sign-in.
    SignedIn(p::SignedIn),
    /// A token the person pasted.
    Pasted(p::Token),
}

/// A checked session: the token works and the guild list is loaded.
pub struct Connected {
    pub session: p::Session,
    /// The signed-in account's ID.
    pub user_id: String,
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
        Credential::Stored { token, pins, .. } => {
            p::Session::from_token_session_pinned(instance, token, &pins)?
        }
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
            .map(|g| Guild {
                id: g.id,
                name: model::clean_name(&g.name),
            })
            .collect();
        Ok(Connected {
            user_id: me.id.clone(),
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
        .and_then(|t| {
            (t.with_timezone(&chrono::Utc) - chrono::Utc::now())
                .to_std()
                .ok()
        })
        .unwrap_or(Duration::from_secs(5 * 60));
    let poll = async {
        loop {
            match handoff.poll().await {
                Ok(p::HandoffStatus::Pending) => {}
                Ok(p::HandoffStatus::Completed(signed_in)) => return Ok(signed_in),
                Ok(p::HandoffStatus::Denied) => return Err(t!("sign-in-denied")),
                Ok(p::HandoffStatus::Expired) => return Err(t!("sign-in-code-expired")),
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
        _ = tokio::time::sleep(deadline) => Some(Err(t!("sign-in-code-expired"))),
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

impl Credential {
    /// The token came from the paste field. Inlet may only forget such a token: it
    /// belongs to another client, which must stay signed in.
    pub fn is_pasted(&self) -> bool {
        matches!(
            self,
            Credential::Pasted(_) | Credential::Stored { pasted: true, .. }
        )
    }
}
