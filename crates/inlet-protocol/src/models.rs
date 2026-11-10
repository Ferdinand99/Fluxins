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
    pub id: Snowflake,
    /// Username, unique together with the discriminator.
    pub username: String,
    /// Four-digit tag.
    #[serde(default)]
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
    #[serde(default)]
    pub bot: bool,
    /// Whether this is a system account.
    #[serde(default)]
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
        self.global_name.as_deref().filter(|s| !s.is_empty()).unwrap_or(&self.username)
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
        self.user.extra.get("mfa_enabled").and_then(Value::as_bool).unwrap_or(false)
    }
}

/// A guild ("community"), from `GET /users/@me/guilds` (`GuildResponse`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Guild {
    /// Guild ID.
    pub id: Snowflake,
    /// Name.
    #[serde(default)]
    pub name: String,
    /// Icon hash.
    #[serde(default)]
    pub icon: Option<String>,
    /// Owner's user ID.
    #[serde(default)]
    pub owner_id: Option<Snowflake>,
    /// Guild feature names.
    #[serde(default)]
    pub features: Vec<String>,
    /// Whether the account visits the guild without being a member.
    #[serde(default)]
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
    pub id: Snowflake,
    /// Channel type, see [`channel_type`].
    #[serde(rename = "type")]
    pub kind: i64,
    /// Guild, for guild channels.
    #[serde(default)]
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
    #[serde(default)]
    pub parent_id: Option<Snowflake>,
    /// Latest message ID, for unread markers.
    #[serde(default)]
    pub last_message_id: Option<Snowflake>,
    /// DM and group DM members.
    #[serde(default)]
    pub recipients: Vec<User>,
    /// Whether the channel is age-restricted.
    #[serde(default)]
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
            GUILD_TEXT | DM | GROUP_DM | GUILD_ANNOUNCEMENT | ANNOUNCEMENT_THREAD | PUBLIC_THREAD | PRIVATE_THREAD | PERSONAL_NOTES
        )
    }
}

/// Attachment metadata (`MessageAttachmentResponse`). Files are not downloaded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Attachment {
    /// Attachment ID.
    pub id: Snowflake,
    /// File name.
    #[serde(default)]
    pub filename: String,
    /// MIME type.
    #[serde(default)]
    pub content_type: Option<String>,
    /// Size in bytes.
    #[serde(default)]
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
    pub id: Snowflake,
    /// Channel ID.
    pub channel_id: Snowflake,
    /// Guild ID (gateway events only).
    #[serde(default)]
    pub guild_id: Option<Snowflake>,
    /// Author.
    pub author: User,
    /// Message type (0 is a normal message, 19 a reply).
    #[serde(rename = "type", default)]
    pub kind: i64,
    /// Markdown text.
    #[serde(default)]
    pub content: String,
    /// ISO 8601 creation time.
    pub timestamp: String,
    /// ISO 8601 time of the last edit.
    #[serde(default)]
    pub edited_timestamp: Option<String>,
    /// Attachment metadata.
    #[serde(default, deserialize_with = "null_as_default")]
    pub attachments: Vec<Attachment>,
    /// Embeds, raw.
    #[serde(default, deserialize_with = "null_as_default")]
    pub embeds: Vec<Value>,
    /// Mentioned users.
    #[serde(default, deserialize_with = "null_as_default")]
    pub mentions: Vec<User>,
    /// Whether the message is pinned.
    #[serde(default)]
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

fn null_as_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
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
    pub id: Snowflake,
    /// The guild record, without roles/channels (absent when unavailable).
    #[serde(default)]
    pub properties: Option<Guild>,
    /// Channels the session can view.
    #[serde(default)]
    pub channels: Vec<Channel>,
    /// Roles, raw.
    #[serde(default)]
    pub roles: Vec<Value>,
    /// Members the session needs right away (itself and voice members), raw.
    #[serde(default)]
    pub members: Vec<Value>,
    /// Total members.
    #[serde(default)]
    pub member_count: Option<u64>,
    /// Whether the guild is temporarily unavailable.
    #[serde(default)]
    pub unavailable: bool,
    /// Whether the account only visits the guild.
    #[serde(default)]
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
    #[serde(default)]
    pub guilds: Vec<GuildReady>,
    /// DM and group DM channels.
    #[serde(default)]
    pub private_channels: Vec<Channel>,
    /// Users referenced elsewhere in the payload.
    #[serde(default)]
    pub users: Vec<User>,
    /// Per-channel read state, raw.
    #[serde(default)]
    pub read_states: Vec<Value>,
    /// Every other field.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// The MESSAGE_DELETE event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageDelete {
    /// The deleted message.
    pub id: Snowflake,
    /// Its channel.
    pub channel_id: Snowflake,
    /// Its guild.
    #[serde(default)]
    pub guild_id: Option<Snowflake>,
}

/// The TYPING_START event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TypingStart {
    /// Channel.
    pub channel_id: Snowflake,
    /// Who is typing.
    pub user_id: Snowflake,
    /// Unix seconds.
    #[serde(default)]
    pub timestamp: i64,
    /// Guild.
    #[serde(default)]
    pub guild_id: Option<Snowflake>,
    /// The typing user's member object, raw.
    #[serde(default)]
    pub member: Option<Value>,
}

/// The GUILD_DELETE event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GuildDelete {
    /// Guild ID.
    pub id: Snowflake,
    /// True when the guild is only temporarily unavailable.
    #[serde(default)]
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
}
