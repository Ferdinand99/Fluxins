//! Invites: look one up and join through it.
//!
//! An invite code goes into a URL path, so it is checked before any request
//! ([`invite_code`]): 1-256 characters, only ASCII letters, digits, `-` and
//! `_` (Fluxer makes 8 random letters and digits, and custom invite URLs are
//! lowercase letters, digits and hyphens). Anything else, such as `../` or
//! `?`, is [`crate::Error::InvalidInput`] without a request.
//!
//! [`parse_invite`] takes a bare code or an invite link of this instance
//! (`{endpoints.invite}/{code}` or `{webapp}/invite/{code}`). A link to another
//! origin that has the shape of a Fluxer invite is not an invite of this
//! instance: it comes back as [`InviteLink::OtherInstance`] with the origin,
//! so the UI can hand it to the account of that instance or open it in the
//! browser, and its code is never sent to this one.

use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use url::Url;

use crate::error::{Error, Result};
use crate::instance::require_scheme;
use crate::models::{lenient_bool, opt_u64, sf, u64_or_zero, Snowflake, User};
use crate::{Client, Instance};

/// The public short-link host of Fluxer, where `/{code}` is an invite. On other
/// hosts only `/invite/{code}` counts, so `https://github.com/foo` is not one.
pub const SHORT_INVITE_HOST: &str = "fluxer.gg";

/// Longest invite code Fluxer accepts in a path.
pub const MAX_INVITE_CODE_LEN: usize = 256;

/// Values of [`Invite::kind`].
pub mod invite_type {
    /// Joins a guild.
    pub const GUILD: u64 = 0;
    /// Joins a group DM.
    pub const GROUP_DM: u64 = 1;
}

/// Checks an invite code for use in a path: 1-256 ASCII letters, digits,
/// `-` or `_`.
pub fn invite_code(code: &str) -> Result<&str> {
    if code.is_empty()
        || code.len() > MAX_INVITE_CODE_LEN
        || !code
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(Error::InvalidInput("not an invite code".into()));
    }
    Ok(code)
}

/// What a piece of text is, as an invite ([`parse_invite`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum InviteLink {
    /// An invite of this instance: the checked code.
    Code(String),
    /// An `https` link to another origin whose path has the shape of an
    /// invite (`/invite/{code}`, or `/{code}` on [`SHORT_INVITE_HOST`]). It
    /// is not this instance's invite and its code must not be sent here.
    ///
    /// The shape is all that is known: any site can use such a path. The UI
    /// must match `origin` to one of the accounts' instances before it
    /// treats the link as an invite, and otherwise open the link in the
    /// browser (after its usual confirmation) or ignore it.
    OtherInstance {
        /// Scheme, host (lowercase) and port of the link, for example
        /// `https://other.example` or `https://chat.example:8443`. The port
        /// is left out when it is the default of the scheme.
        origin: String,
        /// The checked code.
        code: String,
    },
    /// Neither a code nor an invite link.
    NotAnInvite,
}

/// What `input` is: a bare code or an invite link of this instance
/// (`{endpoints.invite}/{code}`, `{endpoints.invite}/invite/{code}` or
/// `{webapp}/invite/{code}`) gives [`InviteLink::Code`]; an `https` link
/// elsewhere with the path of an invite gives [`InviteLink::OtherInstance`]
/// with its origin. Any other link is [`InviteLink::NotAnInvite`].
pub fn parse_invite(input: &str, instance: &Instance) -> InviteLink {
    let input = input.trim();
    if invite_code(input).is_ok() {
        return InviteLink::Code(input.to_owned());
    }
    // https only (http on a loopback host, for tests), and no credentials.
    let Ok(url) = require_scheme(input, "https", "http", "invite link") else {
        return InviteLink::NotAnInvite;
    };
    if !url.username().is_empty() || url.password().is_some() {
        return InviteLink::NotAnInvite;
    }
    let segments: Vec<&str> = url
        .path_segments()
        .map(|s| s.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    let same_origin = |base: &str| {
        require_scheme(base, "https", "http", "invite base")
            .ok()
            .is_some_and(|b| {
                b.scheme() == url.scheme()
                    && b.host_str() == url.host_str()
                    && b.port_or_known_default() == url.port_or_known_default()
            })
    };
    let invite_base = instance
        .discovery()
        .endpoints
        .extra
        .get("invite")
        .and_then(Value::as_str);
    let invite_host = invite_base.is_some_and(same_origin);
    let invite_root = invite_host
        && invite_base
            .is_some_and(|b| Url::parse(b).is_ok_and(|b| b.path().trim_matches('/').is_empty()));
    let ours = invite_host || same_origin(instance.webapp());
    let code = match segments.as_slice() {
        // {endpoints.invite}/{code}, when the invite base has no path of its own.
        // On another host only the short-link host has this form.
        [code] if invite_root || (!ours && url.host_str() == Some(SHORT_INVITE_HOST)) => {
            Some(*code)
        }
        // {endpoints.invite}/invite/{code} or {webapp}/invite/{code}.
        ["invite", code] => Some(*code),
        _ => None,
    }
    .and_then(|c| invite_code(c).ok())
    .map(str::to_owned);
    match code {
        Some(code) if ours => InviteLink::Code(code),
        Some(code) => InviteLink::OtherInstance {
            origin: url.origin().ascii_serialization(),
            code,
        },
        None => InviteLink::NotAnInvite,
    }
}

/// The guild of a guild invite.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InviteGuild {
    /// Guild ID.
    #[serde(deserialize_with = "sf")]
    pub id: Snowflake,
    /// Name.
    #[serde(default)]
    pub name: Option<String>,
    /// Icon hash ([`crate::Media::guild_icon`]).
    #[serde(default)]
    pub icon: Option<String>,
    /// Every other field (banner, splash, features, ...).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// The channel an invite points at.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InviteChannel {
    /// Channel ID.
    #[serde(deserialize_with = "sf")]
    pub id: Snowflake,
    /// Name (`None` for an unnamed group DM).
    #[serde(default)]
    pub name: Option<String>,
    /// Channel type ([`crate::models::channel_type`]).
    #[serde(rename = "type", default, deserialize_with = "u64_or_zero")]
    pub kind: u64,
    /// Every other field (`recipients` of a group DM invite, ...).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// An invite (`GET` and `POST /invites/{code}`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Invite {
    /// The code.
    pub code: String,
    /// An [`invite_type`] value.
    #[serde(rename = "type", default, deserialize_with = "u64_or_zero")]
    pub kind: u64,
    /// The guild (guild invites).
    #[serde(default, deserialize_with = "crate::models::lenient_opt")]
    pub guild: Option<InviteGuild>,
    /// The channel.
    #[serde(default, deserialize_with = "crate::models::lenient_opt")]
    pub channel: Option<InviteChannel>,
    /// Who made it (`None` for a custom invite URL).
    #[serde(default, deserialize_with = "crate::models::lenient_opt")]
    pub inviter: Option<User>,
    /// Members of the guild, or recipients of the group DM.
    #[serde(default, deserialize_with = "opt_u64")]
    pub member_count: Option<u64>,
    /// Members online.
    #[serde(default, deserialize_with = "opt_u64")]
    pub presence_count: Option<u64>,
    /// When it stops working (ISO 8601), `None` for never.
    #[serde(default)]
    pub expires_at: Option<String>,
    /// Joining gives temporary membership.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub temporary: bool,
    /// Every other field.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Client {
    /// `GET /invites/{code}`: what the invite leads to, without using it.
    pub async fn invite(&self, code: &str) -> Result<Invite> {
        self.raw(
            Method::GET,
            &format!("/invites/{}", invite_code(code)?),
            None,
        )
        .await?
        .json()
    }

    /// `POST /invites/{code}`: joins the guild or group DM (GUILD_CREATE or
    /// CHANNEL_CREATE follows on the gateway). Already being in it is not an error.
    pub async fn accept_invite(&self, code: &str) -> Result<Invite> {
        self.raw(
            Method::POST,
            &format!("/invites/{}", invite_code(code)?),
            None,
        )
        .await?
        .json()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_path_safe() {
        for good in ["aB3dEf7h", "my-guild", "x", "a_b"] {
            assert_eq!(invite_code(good).unwrap(), good);
        }
        let long = "a".repeat(257);
        for bad in [
            "",
            "../users/@me",
            "a/b",
            "a?b",
            "a#b",
            "a b",
            "%2e%2e",
            "a.b",
            "ä",
            &long,
        ] {
            assert!(invite_code(bad).is_err(), "{bad:?}");
        }
    }
}
