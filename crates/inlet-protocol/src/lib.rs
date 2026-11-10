//! Talks to a Fluxer instance: discovery, sign-in, REST and gateway.
//!
//! Everything is async on tokio and has no UI types. A UI with its own
//! executor (gpui) runs a tokio runtime on a background thread and talks to
//! it over channels; see "Using it from a UI" below.
//!
//! # Flow
//!
//! ```no_run
//! # async fn demo() -> inlet_protocol::Result<()> {
//! use inlet_protocol::{Instance, Handoff, HandoffStatus, Session, GatewayOptions};
//! use std::time::Duration;
//!
//! // 1. Discovery: GET {origin}/.well-known/fluxer. The API base is
//! //    endpoints.api_public + "/v1", the gateway is endpoints.gateway.
//! let instance = Instance::discover("fluxer.opland.net").await?;
//!
//! // 2. Sign in. The browser handoff works on every instance:
//! let handoff = Handoff::start(&instance).await?;
//! println!("Open {} and enter {}", handoff.browser_url(), handoff.display_code());
//! let signed_in = match handoff.wait(Duration::from_secs(3)).await? {
//!     HandoffStatus::Completed(s) => s,
//!     other => panic!("{other:?}"),
//! };
//! // (Or: sign_in_password -> NeedsMfa -> sign_in_totp. See the auth module.)
//!
//! // 3. Session: REST + gateway. Store signed_in.token in the OS keychain and
//! //    use Session::from_token next time.
//! let session = Session::from_signed_in(instance, signed_in);
//! let me = session.client().me().await?;
//! let guilds = session.client().guilds().await?;
//! let (gateway, mut events) = session.connect_gateway(GatewayOptions::default());
//! while let Some(event) = events.recv().await {
//!     // GatewayEvent::State(..) or GatewayEvent::Dispatch { seq, event }
//! }
//! # Ok(()) }
//! ```
//!
//! # Rate limits (read this)
//!
//! Fluxer revokes a **user token** when it goes over the global limit of
//! 50 requests per second. Every request made with a token passes through one
//! [`RateLimiter`] per token (shared by all clients of the same token) with a
//! hard cap of 10 requests per second and a burst of 20, so no one-second
//! window can hold more than 30. A 429 is retried after `retry_after` (up to
//! 3 times, when the wait is 10 s or less); a 429 with `global: true` returns
//! [`Error::GlobalRateLimited`], which means "sign in again".
//!
//! # Sign-in on fluxer.opland.net
//!
//! Password sign-in needs an ALTCHA proof-of-work CAPTCHA there. This crate
//! solves it ([`altcha`]), but the solver is only tested against mocks. The
//! browser handoff ([`Handoff`]) is the path that is known to work and supports
//! every sign-in method (passkeys, SSO, email codes); use it as the primary
//! button and password sign-in as the second.
//!
//! # Using it from a UI
//!
//! Run a multi-thread tokio runtime on a background thread. Send requests to
//! it over a channel and get results back (for gpui: `cx.background_executor`
//! awaiting a `futures::channel::oneshot`, or a `smol`/`async-channel`
//! receiver polled from a gpui task). The gateway already delivers events on a
//! [`tokio::sync::mpsc::Receiver`]; `Receiver::blocking_recv` works from a
//! plain thread too.
//!
//! # Not covered yet
//!
//! Uploads, reactions, edits/deletes (use [`Client::request_json`]), voice,
//! member lists, read states, WebAuthn.

pub mod altcha;
pub mod auth;
pub mod error;
pub mod gateway;
pub mod instance;
pub mod models;
pub mod ratelimit;
pub mod rest;
pub mod token;

pub use auth::{
    poll_ip_authorization, sign_in_password, sign_in_totp, Handoff, HandoffStatus, IpAuthorization,
    MfaMethod, Session, SignInResult, SignedIn, Ticket,
};
pub use error::{ApiError, Error, Result};
pub use gateway::{
    CloseReason, ConnectionState, Dispatch, Gateway, GatewayEvent, GatewayHandle, GatewayOptions,
    IdentifyProperties,
};
pub use instance::{ClientInfo, Discovery, Endpoints, Instance};
pub use models::{
    Attachment, Channel, CurrentUser, Guild, GuildReady, Message, Ready, Snowflake, User,
};
pub use ratelimit::{RateLimitConfig, RateLimiter};
pub use rest::{new_nonce, Client};
pub use token::Token;
