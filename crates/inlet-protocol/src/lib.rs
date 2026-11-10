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
//! // 3. Session: REST + gateway. Store signed_in.token and session.pins()
//! //    together in the OS keychain, and use Session::from_token_pinned next time.
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
//! Fluxer counts its global limit of 50 requests per second **per user** and
//! revokes the token that goes over it. Every request made with a token passes
//! through one [`RateLimiter`] per (instance, user), kept for the life of the
//! process: a hard cap of 20 requests in any one-second window (no config can
//! raise it), with a token bucket of 10/s and burst 20 inside it. That leaves
//! room for Fluxer's own client on the same account. A 429 is retried after
//! `retry_after` (up to 3 times, when the wait is 10 s or less); a 429 with
//! `global: true` returns [`Error::GlobalRateLimited`], which means "sign in
//! again". Every server-given wait is clamped to 5 minutes.
//!
//! # Security
//!
//! - Tokens and secrets only travel over `https`/`wss`. `http`/`ws` is
//!   accepted for loopback hosts only (tests).
//! - Requests never follow redirects, so a body with a password, ticket or
//!   poll secret cannot be resent to another host.
//! - [`Session::pins`] gives the API and gateway hosts a token belongs to.
//!   Store them with the token and use [`Session::from_token_pinned`]: if the
//!   discovery document later names other hosts, the token is not sent there.
//! - `webapp` (for the browser handoff) must be `https` on the instance's
//!   domain or listed in [`ClientInfo::trusted_webapp_hosts`].
//! - Response bodies are capped at 8 MiB and gateway messages at 32 MiB
//!   (also after zstd decompression).
//! - Zeroizing the token is best effort: copies in request bodies, JSON trees
//!   and the zstd window are not wiped.

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
//! # Chat features
//!
//! - Messages: [`Client::send`] (replies, stickers, flags), [`Client::edit_message`],
//!   [`Client::delete_message`], [`Client::send_with_files`] (uploads with progress).
//! - Reactions: [`Client::add_reaction`], [`Client::remove_reaction`],
//!   [`Client::reaction_users`]; pins: [`Client::pins`], [`Client::pin_message`].
//! - Typing and read state: [`Client::trigger_typing`] is debounced (one request
//!   per channel per 8 s) and [`Client::ack`] is coalesced (one batched request
//!   per second at most), so the UI can call both freely ([`coalesce`]).
//! - Permissions: [`permissions::PermissionContext`] hides channels and
//!   disables the composer.
//! - Media: [`Instance::media`] builds avatar, icon, emoji, sticker and
//!   attachment URLs from the instance's checked media endpoint only.
//! - Gateway: member lists ([`GatewayHandle::subscribe_member_list`],
//!   [`models::MemberList`]), presences, reactions, acks, settings, roles,
//!   members and voice states arrive typed in [`Dispatch`].
//!
//! # Not covered yet
//!
//! Voice, pre-signed uploads, threads and forums, WebAuthn.

pub mod altcha;
pub mod auth;
pub mod coalesce;
pub mod error;
pub mod gateway;
pub mod instance;
pub mod limits;
pub mod media;
pub mod models;
pub mod permissions;
pub mod ratelimit;
pub mod rest;
pub mod token;
pub mod upload;


pub use auth::{
    poll_ip_authorization, sign_in_password, sign_in_totp, Handoff, HandoffStatus, IpAuthorization,
    MfaMethod, Session, SignInResult, SignedIn, Ticket,
};
pub use error::{ApiError, Error, Result};
pub use gateway::{
    CloseReason, ConnectionState, Dispatch, Gateway, GatewayEvent, GatewayHandle, GatewayOptions,
    IdentifyProperties,
};
pub use instance::{ClientInfo, Discovery, EndpointPins, Endpoints, Instance, SessionPins};
pub use media::{ImageFormat, ImageOptions, Media, Transform};
pub use models::{
    Attachment, Channel, CurrentUser, Embed, Emoji, FullUserProfile, Guild, GuildEmoji,
    GuildMember, GuildMemberListUpdate, GuildReady, GuildSticker, MemberList, MemberListItem,
    Message, MessageEdit, OutgoingMessage, Presence, Reaction, ReadState, Ready, ReferencedMessage,
    Role, Snowflake, User, UserSettings, UserSettingsUpdate, VoiceState,
};
pub use permissions::PermissionContext;
pub use ratelimit::{RateLimitConfig, RateLimiter};
pub use rest::{new_nonce, Client};
pub use token::Token;
pub use upload::{ProgressFn, UploadFile, UploadLimits};
