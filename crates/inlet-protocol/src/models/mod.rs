//! Data types from the Fluxer HTTP API and gateway.
//!
//! Hand-written from the instance's `/v1/openapi.json` (`UserPartialResponse`,
//! `UserPrivateResponse`, `GuildResponse`, `ChannelResponse`,
//! `MessageResponseSchema`, `MessageAttachmentResponse`) and from the gateway
//! docs (guild ready object). Only the fields a chat client needs are typed;
//! every struct keeps the rest in `extra`, so nothing is lost when Fluxer adds
//! fields. Unknown enum values stay readable as their raw number.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

mod events;
mod guild;
mod message;
mod time;
mod user;

pub use events::*;
pub use guild::*;
pub use message::*;
pub use time::parse_timestamp_ms;
pub use user::*;

/// A Fluxer snowflake ID. The API sends them as decimal strings.
pub type Snowflake = String;

/// Fluxer's snowflake epoch, 2015-01-01T00:00:00Z, in Unix milliseconds.
pub const FLUXER_EPOCH_MS: u64 = 1_420_070_400_000;

/// The creation time of a snowflake in Unix milliseconds, or `None` if the ID is not a number.
pub fn snowflake_timestamp_ms(id: &str) -> Option<u64> {
    let raw: u64 = id.parse().ok()?;
    Some((raw >> 22) + FLUXER_EPOCH_MS)
}

/// A user as other people see them (`UserPartialResponse`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct User {
    /// User ID.
    #[serde(deserialize_with = "sf")]
    pub id: Snowflake,
    /// Username, unique together with the discriminator.
    #[serde(default, deserialize_with = "null_as_default")]
    pub username: String,
    /// Four-digit tag.
    #[serde(default, deserialize_with = "null_as_default")]
    pub discriminator: String,
    /// Display name, if set.
    #[serde(default)]
    pub global_name: Option<String>,
    /// Avatar hash; build the URL with the media endpoint. A hash starting with `a_` is animated.
    #[serde(default)]
    pub avatar: Option<String>,
    /// Fallback avatar color.
    #[serde(default)]
    pub avatar_color: Option<i64>,
    /// Whether this is a bot.
    #[serde(default, deserialize_with = "null_as_default")]
    pub bot: bool,
    /// Whether this is a system account.
    #[serde(default, deserialize_with = "null_as_default")]
    pub system: bool,
    /// Public user flags.
    #[serde(default)]
    pub flags: Option<Value>,
    /// Every other field the API sent.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl User {
    /// The name to show: the display name, or else the username.
    pub fn display_name(&self) -> &str {
        self.global_name
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.username)
    }
}

/// The signed-in account (`UserPrivateResponse`, from `GET /users/@me` and Ready).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CurrentUser {
    /// The public part.
    #[serde(flatten)]
    pub user: User,
}

impl std::ops::Deref for CurrentUser {
    type Target = User;
    fn deref(&self) -> &User {
        &self.user
    }
}

impl CurrentUser {
    /// The account email, if the API sent it.
    pub fn email(&self) -> Option<&str> {
        self.user.extra.get("email")?.as_str()
    }

    /// Whether the account has a second factor.
    pub fn mfa_enabled(&self) -> bool {
        self.user
            .extra
            .get("mfa_enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }
}

/// A guild ("community"), from `GET /users/@me/guilds` (`GuildResponse`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Guild {
    /// Guild ID.
    #[serde(deserialize_with = "sf")]
    pub id: Snowflake,
    /// Name.
    #[serde(default, deserialize_with = "null_as_default")]
    pub name: String,
    /// Icon hash.
    #[serde(default)]
    pub icon: Option<String>,
    /// Owner's user ID.
    #[serde(default, deserialize_with = "opt_sf")]
    pub owner_id: Option<Snowflake>,
    /// Guild feature names.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub features: Vec<String>,
    /// Whether the account visits the guild without being a member.
    #[serde(default, deserialize_with = "null_as_default")]
    pub visiting: bool,
    /// Every other field the API sent.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Channel types (`ChannelType` in the OpenAPI document).
pub mod channel_type {
    /// Text channel in a guild.
    pub const GUILD_TEXT: i64 = 0;
    /// Direct message.
    pub const DM: i64 = 1;
    /// Voice channel.
    pub const GUILD_VOICE: i64 = 2;
    /// Group direct message.
    pub const GROUP_DM: i64 = 3;
    /// Category.
    pub const GUILD_CATEGORY: i64 = 4;
    /// Announcement channel.
    pub const GUILD_ANNOUNCEMENT: i64 = 5;
    /// Thread in an announcement channel.
    pub const ANNOUNCEMENT_THREAD: i64 = 10;
    /// Public thread.
    pub const PUBLIC_THREAD: i64 = 11;
    /// Private thread.
    pub const PRIVATE_THREAD: i64 = 12;
    /// Forum.
    pub const GUILD_FORUM: i64 = 15;
    /// Media channel.
    pub const GUILD_MEDIA: i64 = 16;
    /// Link channel.
    pub const GUILD_LINK: i64 = 998;
    /// Personal notes.
    pub const PERSONAL_NOTES: i64 = 999;
}

/// A channel (`ChannelResponse`): guild channels, DMs and group DMs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Channel {
    /// Channel ID.
    #[serde(deserialize_with = "sf")]
    pub id: Snowflake,
    /// Channel type, see [`channel_type`].
    #[serde(rename = "type")]
    pub kind: i64,
    /// Guild, for guild channels.
    #[serde(default, deserialize_with = "opt_sf")]
    pub guild_id: Option<Snowflake>,
    /// Name (none for DMs).
    #[serde(default)]
    pub name: Option<String>,
    /// Topic.
    #[serde(default)]
    pub topic: Option<String>,
    /// Sort position.
    #[serde(default)]
    pub position: Option<i64>,
    /// Category or parent channel.
    #[serde(default, deserialize_with = "opt_sf")]
    pub parent_id: Option<Snowflake>,
    /// Latest message ID, for unread markers.
    #[serde(default, deserialize_with = "opt_sf")]
    pub last_message_id: Option<Snowflake>,
    /// DM and group DM members.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub recipients: Vec<User>,
    /// Whether the channel is age-restricted.
    #[serde(default, deserialize_with = "null_as_default")]
    pub nsfw: bool,
    /// Slowmode in seconds.
    #[serde(default)]
    pub rate_limit_per_user: Option<i64>,
    /// Every other field the API sent.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Channel {
    /// Whether people can post text messages here (text, announcement, DM, group DM, threads, notes).
    pub fn is_text(&self) -> bool {
        use channel_type::*;
        matches!(
            self.kind,
            GUILD_TEXT
                | DM
                | GROUP_DM
                | GUILD_ANNOUNCEMENT
                | ANNOUNCEMENT_THREAD
                | PUBLIC_THREAD
                | PRIVATE_THREAD
                | PERSONAL_NOTES
        )
    }
}

/// Attachment metadata (`MessageAttachmentResponse`). Files are not downloaded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Attachment {
    /// Attachment ID.
    #[serde(deserialize_with = "sf")]
    pub id: Snowflake,
    /// File name.
    #[serde(default, deserialize_with = "null_as_default")]
    pub filename: String,
    /// MIME type.
    #[serde(default)]
    pub content_type: Option<String>,
    /// Size in bytes.
    #[serde(default, deserialize_with = "null_as_default")]
    pub size: u64,
    /// Signed URL; it expires (see `expires_at`).
    #[serde(default)]
    pub url: Option<String>,
    /// Media proxy URL.
    #[serde(default)]
    pub proxy_url: Option<String>,
    /// Image or video width.
    #[serde(default)]
    pub width: Option<u32>,
    /// Image or video height.
    #[serde(default)]
    pub height: Option<u32>,
    /// Alt text.
    #[serde(default)]
    pub description: Option<String>,
    /// Every other field the API sent.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// A message (`MessageResponseSchema`), also the payload of MESSAGE_CREATE and MESSAGE_UPDATE.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// Message ID.
    #[serde(deserialize_with = "sf")]
    pub id: Snowflake,
    /// Channel ID.
    #[serde(deserialize_with = "sf")]
    pub channel_id: Snowflake,
    /// Guild ID (gateway events only).
    #[serde(default, deserialize_with = "opt_sf")]
    pub guild_id: Option<Snowflake>,
    /// Author.
    pub author: User,
    /// Message type (0 is a normal message, 19 a reply).
    #[serde(rename = "type", default, deserialize_with = "null_as_default")]
    pub kind: i64,
    /// Markdown text.
    #[serde(default, deserialize_with = "null_as_default")]
    pub content: String,
    /// ISO 8601 creation time.
    #[serde(default, deserialize_with = "null_as_default")]
    pub timestamp: String,
    /// ISO 8601 time of the last edit.
    #[serde(default)]
    pub edited_timestamp: Option<String>,
    /// Attachment metadata.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub attachments: Vec<Attachment>,
    /// Embeds, raw.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub embeds: Vec<Value>,
    /// Mentioned users.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub mentions: Vec<User>,
    /// Whether the message is pinned.
    #[serde(default, deserialize_with = "null_as_default")]
    pub pinned: bool,
    /// The nonce the sender chose, used to match an optimistic local message.
    #[serde(default, deserialize_with = "nonce_as_string")]
    pub nonce: Option<String>,
    /// Message flags.
    #[serde(default)]
    pub flags: Option<Value>,
    /// Every other field the API sent (reactions, message_reference, member, ...).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

pub(crate) fn null_as_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// Deserializes a snowflake sent as a string or as a number.
pub(crate) fn sf<'de, D>(d: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    match Value::deserialize(d)? {
        Value::String(s) => Ok(s),
        Value::Number(n) if n.is_u64() => Ok(n.to_string()),
        other => Err(D::Error::custom(format!(
            "expected a snowflake, got {other}"
        ))),
    }
}

/// Like [`sf`], with `null`, a missing field or a bad value as `None`.
pub(crate) fn opt_sf<'de, D>(d: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match Value::deserialize(d)? {
        Value::String(s) => Some(s),
        Value::Number(n) if n.is_u64() => Some(n.to_string()),
        _ => None,
    })
}

/// A list where `null` is empty and elements that do not parse are skipped (and logged).
pub(crate) fn lenient_vec<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    Ok(match Value::deserialize(d)? {
        Value::Array(items) => lenient_list(items, std::any::type_name::<T>()),
        Value::Null => Vec::new(),
        other => {
            tracing::warn!(
                what = std::any::type_name::<T>(),
                "expected a list, got {other}"
            );
            Vec::new()
        }
    })
}

/// Parses each element on its own, skipping (and logging) the ones that do not parse.
pub(crate) fn lenient_list<T: serde::de::DeserializeOwned>(
    items: Vec<Value>,
    what: &str,
) -> Vec<T> {
    let mut out = Vec::with_capacity(items.len());
    for (index, item) in items.into_iter().enumerate() {
        match T::deserialize(item) {
            Ok(v) => out.push(v),
            Err(e) => {
                tracing::warn!(what, index, error = %e, "skipping an element that did not parse")
            }
        }
    }
    out
}

/// An unsigned number sent as a JSON number or a decimal string; anything else is `None`.
pub(crate) fn opt_u64<'de, D>(d: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(value_u64(&Value::deserialize(d)?))
}

/// Like [`opt_u64`], with a missing or bad value as 0.
pub(crate) fn u64_or_zero<'de, D>(d: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(value_u64(&Value::deserialize(d)?).unwrap_or(0))
}

/// A signed number, or `None`.
pub(crate) fn opt_i64<'de, D>(d: D) -> Result<Option<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match Value::deserialize(d)? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    })
}

pub(crate) fn value_u64(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// A boolean where `null` or any other type is `false`.
pub(crate) fn lenient_bool<'de, D>(d: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(matches!(Value::deserialize(d)?, Value::Bool(true)))
}

/// A list of snowflakes (strings or numbers); other elements are skipped.
pub(crate) fn sf_list<'de, D>(d: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match Value::deserialize(d)? {
        Value::Array(items) => items
            .into_iter()
            .filter_map(|v| match v {
                Value::String(s) => Some(s),
                Value::Number(n) if n.is_u64() => Some(n.to_string()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    })
}

/// Parses an optional field of `extra` into `T`, or `None` when absent, null or unparsable.
pub(crate) fn extra_field<T: serde::de::DeserializeOwned>(
    extra: &Map<String, Value>,
    key: &str,
) -> Option<T> {
    let v = extra.get(key)?;
    if v.is_null() {
        return None;
    }
    match T::deserialize(v) {
        Ok(t) => Some(t),
        Err(e) => {
            tracing::warn!(field = key, error = %e, "field did not parse");
            None
        }
    }
}

/// Parses a list field of `extra`, skipping elements that do not parse.
pub(crate) fn extra_list<T: serde::de::DeserializeOwned>(
    extra: &Map<String, Value>,
    key: &str,
) -> Vec<T> {
    match extra.get(key) {
        Some(Value::Array(items)) => parse_values(items, key),
        _ => Vec::new(),
    }
}

/// Parses a list of raw values, skipping elements that do not parse.
pub(crate) fn parse_values<T: serde::de::DeserializeOwned>(items: &[Value], what: &str) -> Vec<T> {
    let mut out = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        match T::deserialize(item) {
            Ok(v) => out.push(v),
            Err(e) => {
                tracing::warn!(what, index, error = %e, "skipping an element that did not parse")
            }
        }
    }
    out
}

/// Compares two snowflakes numerically. IDs that are not numbers compare by
/// length, then as text.
pub fn snowflake_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    match (a.parse::<u64>(), b.parse::<u64>()) {
        (Ok(x), Ok(y)) => x.cmp(&y),
        _ => a.len().cmp(&b.len()).then_with(|| a.cmp(b)),
    }
}

fn nonce_as_string<'de, D>(d: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match Option::<Value>::deserialize(d)? {
        Some(Value::String(s)) => Some(s),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    })
}

/// The guild state in READY, GUILD_CREATE and GUILD_SYNC (gateway "guild ready object").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GuildReady {
    /// Guild ID.
    #[serde(deserialize_with = "sf")]
    pub id: Snowflake,
    /// The guild record, without roles/channels (absent when unavailable).
    #[serde(default)]
    pub properties: Option<Guild>,
    /// Channels the session can view.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub channels: Vec<Channel>,
    /// Roles, raw.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub roles: Vec<Value>,
    /// Members the session needs right away (itself and voice members), raw.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub members: Vec<Value>,
    /// Total members.
    #[serde(default)]
    pub member_count: Option<u64>,
    /// Whether the guild is temporarily unavailable.
    #[serde(default, deserialize_with = "null_as_default")]
    pub unavailable: bool,
    /// Whether the account only visits the guild.
    #[serde(default, deserialize_with = "null_as_default")]
    pub visiting: bool,
    /// Every other field.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl GuildReady {
    /// The guild name, if the guild is available.
    pub fn name(&self) -> Option<&str> {
        self.properties.as_ref().map(|p| p.name.as_str())
    }
}

/// The READY event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ready {
    /// Session ID for resuming.
    pub session_id: String,
    /// The signed-in account.
    pub user: CurrentUser,
    /// Guilds.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub guilds: Vec<GuildReady>,
    /// DM and group DM channels.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub private_channels: Vec<Channel>,
    /// Users referenced elsewhere in the payload.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub users: Vec<User>,
    /// Per-channel read state, raw.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub read_states: Vec<Value>,
    /// Every other field.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// The MESSAGE_DELETE event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageDelete {
    /// The deleted message.
    #[serde(deserialize_with = "sf")]
    pub id: Snowflake,
    /// Its channel.
    #[serde(deserialize_with = "sf")]
    pub channel_id: Snowflake,
    /// Its guild.
    #[serde(default, deserialize_with = "opt_sf")]
    pub guild_id: Option<Snowflake>,
}

/// The TYPING_START event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TypingStart {
    /// Channel.
    #[serde(deserialize_with = "sf")]
    pub channel_id: Snowflake,
    /// Who is typing.
    #[serde(deserialize_with = "sf")]
    pub user_id: Snowflake,
    /// Unix seconds.
    #[serde(default, deserialize_with = "null_as_default")]
    pub timestamp: i64,
    /// Guild.
    #[serde(default, deserialize_with = "opt_sf")]
    pub guild_id: Option<Snowflake>,
    /// The typing user's member object, raw.
    #[serde(default)]
    pub member: Option<Value>,
}

/// The GUILD_DELETE event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GuildDelete {
    /// Guild ID.
    #[serde(deserialize_with = "sf")]
    pub id: Snowflake,
    /// True when the guild is only temporarily unavailable.
    #[serde(default, deserialize_with = "null_as_default")]
    pub unavailable: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_parses_with_extra_fields_and_numeric_nonce() {
        let raw = serde_json::json!({
            "id": "1", "channel_id": "2", "type": 0, "flags": 0,
            "author": {"id": "3", "username": "ana", "discriminator": "0001", "global_name": null,
                       "avatar": null, "avatar_color": null, "flags": 0},
            "content": "hi", "timestamp": "2026-11-10T08:00:00.000Z", "edited_timestamp": null,
            "pinned": false, "mention_everyone": false, "tts": false, "mentions": [], "mention_roles": [],
            "attachments": [{"id": "9", "filename": "a.png", "size": 10, "flags": 0, "content_type": "image/png"}],
            "embeds": null, "nonce": 42, "reactions": []
        });
        let m: Message = serde_json::from_value(raw).unwrap();
        assert_eq!(m.nonce.as_deref(), Some("42"));
        assert_eq!(m.attachments[0].filename, "a.png");
        assert!(m.embeds.is_empty());
        assert!(m.extra.contains_key("reactions"));
        assert_eq!(m.author.display_name(), "ana");
    }

    #[test]
    fn snowflake_time() {
        // 1 << 22 is one millisecond after the epoch.
        assert_eq!(snowflake_timestamp_ms("4194304"), Some(FLUXER_EPOCH_MS + 1));
    }
    #[test]
    fn nulls_count_as_missing() {
        let g: Guild = serde_json::from_value(serde_json::json!({
            "id": "1", "name": null, "features": null, "visiting": null, "owner_id": null
        }))
        .unwrap();
        assert_eq!(g.name, "");
        assert!(g.features.is_empty());
        let u: User = serde_json::from_value(serde_json::json!({
            "id": 5, "username": null, "discriminator": null, "bot": null, "system": null
        }))
        .unwrap();
        assert_eq!(u.id, "5");
        let c: Channel = serde_json::from_value(serde_json::json!({
            "id": 7, "type": 1, "recipients": null, "nsfw": null, "guild_id": 9, "parent_id": null
        }))
        .unwrap();
        assert_eq!(c.guild_id.as_deref(), Some("9"));
        assert!(c.recipients.is_empty());
        let a: Attachment = serde_json::from_value(serde_json::json!({
            "id": "1", "filename": null, "size": null
        }))
        .unwrap();
        assert_eq!(a.size, 0);
    }

    #[test]
    fn one_bad_element_does_not_sink_the_list() {
        let ready: Ready = serde_json::from_value(serde_json::json!({
            "session_id": "s",
            "user": {"id": "1", "username": "me"},
            "guilds": [{"id": "10", "channels": [{"id": "11", "type": 0}, {"name": "no id"}]},
                       {"unavailable": true}],
            "private_channels": null,
            "users": [{"id": "2", "username": "ok"}, {"username": "no id"}, 7],
            "read_states": null
        }))
        .unwrap();
        assert_eq!(ready.guilds.len(), 1);
        assert_eq!(ready.guilds[0].channels.len(), 1);
        assert_eq!(ready.users.len(), 1);
        assert!(ready.private_channels.is_empty());
    }

    #[test]
    fn snowflakes_as_numbers() {
        let m: MessageDelete = serde_json::from_value(
            serde_json::json!({"id": 123, "channel_id": "456", "guild_id": 789}),
        )
        .unwrap();
        assert_eq!(m.id, "123");
        assert_eq!(m.guild_id.as_deref(), Some("789"));
        assert!(serde_json::from_value::<MessageDelete>(
            serde_json::json!({"id": true, "channel_id": "1"})
        )
        .is_err());
    }
}
