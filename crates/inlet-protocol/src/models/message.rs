//! Message parts: embeds, reactions, replies, stickers, pins, and what the
//! client sends (`OutgoingMessage`, `MessageEdit`).

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use super::{
    extra_field, extra_list, lenient_bool, lenient_vec, null_as_default, opt_sf, opt_u64, sf,
    sf_list, value_u64, Attachment, GuildMember, Message, Snowflake, User,
};

/// Message flags (`flags` on a message).
pub mod message_flags {
    /// Published to following channels.
    pub const CROSSPOSTED: u64 = 1 << 0;
    /// A copy from a followed announcement channel.
    pub const IS_CROSSPOST: u64 = 1 << 1;
    /// Embeds are hidden.
    pub const SUPPRESS_EMBEDS: u64 = 1 << 2;
    /// The source of a crosspost was deleted.
    pub const SOURCE_MESSAGE_DELETED: u64 = 1 << 3;
    /// Sent without a notification.
    pub const SUPPRESS_NOTIFICATIONS: u64 = 1 << 12;
    /// A voice message.
    pub const VOICE_MESSAGE: u64 = 1 << 13;
}

/// Message types (`type` on a message).
pub mod message_type {
    /// Ordinary message.
    pub const DEFAULT: i64 = 0;
    /// Someone was added to a group DM or thread.
    pub const RECIPIENT_ADD: i64 = 1;
    /// Someone was removed from a group DM or thread.
    pub const RECIPIENT_REMOVE: i64 = 2;
    /// A call.
    pub const CALL: i64 = 3;
    /// A group DM or thread was renamed.
    pub const CHANNEL_NAME_CHANGE: i64 = 4;
    /// A group DM icon changed.
    pub const CHANNEL_ICON_CHANGE: i64 = 5;
    /// A message was pinned.
    pub const CHANNEL_PINNED_MESSAGE: i64 = 6;
    /// A member joined.
    pub const USER_JOIN: i64 = 7;
    /// The channel started following an announcement channel.
    pub const CHANNEL_FOLLOW_ADD: i64 = 12;
    /// A thread was created from the channel.
    pub const THREAD_CREATED: i64 = 18;
    /// A reply.
    pub const REPLY: i64 = 19;
    /// The first message of a thread started from a message.
    pub const THREAD_STARTER_MESSAGE: i64 = 21;
}

/// Attachment flags (`flags` on an attachment or embed media).
pub mod attachment_flags {
    /// A spoiler.
    pub const IS_SPOILER: u64 = 1 << 3;
    /// Explicit media.
    pub const CONTAINS_EXPLICIT_MEDIA: u64 = 1 << 4;
    /// Animated.
    pub const IS_ANIMATED: u64 = 1 << 5;
}

/// An emoji in a reaction or reaction event: a custom emoji (`id` set) or a Unicode emoji.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Emoji {
    /// Custom emoji ID, `None` for a Unicode emoji.
    #[serde(
        default,
        deserialize_with = "opt_sf",
        skip_serializing_if = "Option::is_none"
    )]
    pub id: Option<Snowflake>,
    /// The custom emoji name, or the Unicode emoji itself.
    #[serde(default, deserialize_with = "null_as_default")]
    pub name: String,
    /// Whether the custom emoji is animated. In reaction events it is only
    /// sent with the first reaction, so `false` may mean "unknown".
    #[serde(default, deserialize_with = "lenient_bool")]
    pub animated: bool,
}

impl Emoji {
    /// A Unicode emoji.
    pub fn unicode(emoji: impl Into<String>) -> Emoji {
        Emoji {
            id: None,
            name: emoji.into(),
            animated: false,
        }
    }

    /// A custom emoji.
    pub fn custom(id: impl Into<String>, name: impl Into<String>, animated: bool) -> Emoji {
        Emoji {
            id: Some(id.into()),
            name: name.into(),
            animated,
        }
    }

    /// Whether this is a custom emoji.
    pub fn is_custom(&self) -> bool {
        self.id.is_some()
    }

    /// The reaction path value before encoding: `name:id` for a custom emoji,
    /// the emoji itself otherwise.
    pub fn path_value(&self) -> String {
        match &self.id {
            Some(id) => format!("{}:{id}", self.name),
            None => self.name.clone(),
        }
    }

    /// Whether two emoji are the same reaction (custom by ID, Unicode by text).
    pub fn same_reaction(&self, other: &Emoji) -> bool {
        match (&self.id, &other.id) {
            (Some(a), Some(b)) => a == b,
            (None, None) => self.name == other.name,
            _ => false,
        }
    }
}

/// One reaction on a message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reaction {
    /// The emoji.
    pub emoji: Emoji,
    /// How many users reacted with it.
    #[serde(default, deserialize_with = "super::u64_or_zero")]
    pub count: u64,
    /// Whether the signed-in account reacted with it.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub me: bool,
}

/// Message reference types.
pub mod message_reference_type {
    /// A reply, a published copy's source or a thread.
    pub const DEFAULT: i64 = 0;
    /// A forward; the content is in `message_snapshots`.
    pub const FORWARD: i64 = 1;
}

/// The message a reply (or forward) points to (`message_reference`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageReference {
    /// The referenced channel.
    #[serde(default, deserialize_with = "opt_sf")]
    pub channel_id: Option<Snowflake>,
    /// The referenced message.
    #[serde(default, deserialize_with = "opt_sf")]
    pub message_id: Option<Snowflake>,
    /// The referenced guild.
    #[serde(default, deserialize_with = "opt_sf")]
    pub guild_id: Option<Snowflake>,
    /// See [`message_reference_type`].
    #[serde(rename = "type", default, deserialize_with = "null_as_default")]
    pub kind: i64,
}

/// What `referenced_message` says about a reply.
#[derive(Debug, Clone, PartialEq)]
pub enum ReferencedMessage {
    /// The message is not a reply (the field is absent).
    NotAReply,
    /// The message it replied to is gone (the field is `null`), or did not parse.
    Deleted,
    /// The message it replies to.
    Message(Box<Message>),
}

/// A sticker sent with a message (`sticker item`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StickerItem {
    /// Sticker ID.
    #[serde(deserialize_with = "sf")]
    pub id: Snowflake,
    /// Name.
    #[serde(default, deserialize_with = "null_as_default")]
    pub name: String,
    /// Whether it is animated.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub animated: bool,
}

/// A channel the content links to (`mention_channels`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelMention {
    /// Channel ID.
    #[serde(deserialize_with = "sf")]
    pub id: Snowflake,
    /// Channel name.
    #[serde(default, deserialize_with = "null_as_default")]
    pub name: String,
    /// Channel type.
    #[serde(rename = "type", default, deserialize_with = "null_as_default")]
    pub kind: i64,
}

/// Embed author.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbedAuthor {
    /// Name.
    #[serde(default, deserialize_with = "null_as_default")]
    pub name: String,
    /// Link (third-party; open it in the browser only after the person clicks).
    #[serde(default)]
    pub url: Option<String>,
    /// Icon URL (third-party).
    #[serde(default)]
    pub icon_url: Option<String>,
    /// Icon URL through the media proxy. Use [`crate::Media::external`] to check it.
    #[serde(default)]
    pub proxy_icon_url: Option<String>,
}

/// Embed image, thumbnail, video or audio.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbedMedia {
    /// The original URL (third-party).
    #[serde(default, deserialize_with = "null_as_default")]
    pub url: String,
    /// The URL through the media proxy. Use [`crate::Media::external`] or
    /// [`crate::Media::attachment`] to check it before loading it.
    #[serde(default)]
    pub proxy_url: Option<String>,
    /// Width in pixels.
    #[serde(default, deserialize_with = "opt_u64")]
    pub width: Option<u64>,
    /// Height in pixels.
    #[serde(default, deserialize_with = "opt_u64")]
    pub height: Option<u64>,
    /// Length in seconds.
    #[serde(default, deserialize_with = "opt_u64")]
    pub duration: Option<u64>,
    /// Alt text.
    #[serde(default)]
    pub description: Option<String>,
    /// Media type.
    #[serde(default)]
    pub content_type: Option<String>,
    /// Encoded placeholder image.
    #[serde(default)]
    pub placeholder: Option<String>,
    /// [`attachment_flags`] (explicit, animated).
    #[serde(default, deserialize_with = "opt_u64")]
    pub flags: Option<u64>,
}

/// Embed footer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbedFooter {
    /// Text.
    #[serde(default, deserialize_with = "null_as_default")]
    pub text: String,
    /// Icon URL (third-party).
    #[serde(default)]
    pub icon_url: Option<String>,
    /// Icon URL through the media proxy.
    #[serde(default)]
    pub proxy_icon_url: Option<String>,
}

/// Embed field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbedField {
    /// Name.
    #[serde(default, deserialize_with = "null_as_default")]
    pub name: String,
    /// Value (markdown).
    #[serde(default, deserialize_with = "null_as_default")]
    pub value: String,
    /// Whether it shows inline.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub inline: bool,
}

/// Embed provider (the site a link preview is from).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbedProvider {
    /// Name.
    #[serde(default, deserialize_with = "null_as_default")]
    pub name: String,
    /// URL.
    #[serde(default)]
    pub url: Option<String>,
}

/// An embed: a link preview Fluxer built or a rich embed someone sent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Embed {
    /// `rich`, `link`, `image`, `video`, `audio`, `gifv`, `article` or `bluesky`.
    #[serde(rename = "type", default, deserialize_with = "null_as_default")]
    pub kind: String,
    /// Title.
    #[serde(default)]
    pub title: Option<String>,
    /// Description (markdown).
    #[serde(default)]
    pub description: Option<String>,
    /// Link of the title.
    #[serde(default)]
    pub url: Option<String>,
    /// ISO 8601 timestamp.
    #[serde(default)]
    pub timestamp: Option<String>,
    /// Colour as 24-bit RGB.
    #[serde(default, deserialize_with = "opt_u64")]
    pub color: Option<u64>,
    /// Author.
    #[serde(default, deserialize_with = "lenient_opt")]
    pub author: Option<EmbedAuthor>,
    /// Provider.
    #[serde(default, deserialize_with = "lenient_opt")]
    pub provider: Option<EmbedProvider>,
    /// Thumbnail.
    #[serde(default, deserialize_with = "lenient_opt")]
    pub thumbnail: Option<EmbedMedia>,
    /// Main image.
    #[serde(default, deserialize_with = "lenient_opt")]
    pub image: Option<EmbedMedia>,
    /// Video.
    #[serde(default, deserialize_with = "lenient_opt")]
    pub video: Option<EmbedMedia>,
    /// Audio.
    #[serde(default, deserialize_with = "lenient_opt")]
    pub audio: Option<EmbedMedia>,
    /// Footer.
    #[serde(default, deserialize_with = "lenient_opt")]
    pub footer: Option<EmbedFooter>,
    /// Fields.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub fields: Vec<EmbedField>,
    /// Whether the embed has explicit media.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub nsfw: bool,
    /// At most one nested embed (link previews).
    #[serde(default, deserialize_with = "lenient_vec")]
    pub children: Vec<Embed>,
    /// Every other field (for example sanitized oEmbed `html`, which a native
    /// client should not render).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// An optional object where a value that does not parse counts as absent.
pub(crate) fn lenient_opt<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let v = Value::deserialize(d)?;
    if v.is_null() {
        return Ok(None);
    }
    Ok(T::deserialize(v).ok())
}

/// A pinned message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelPin {
    /// The message (without `referenced_message` and `reactions`).
    pub message: Message,
    /// When it was pinned (ISO 8601). Pass it as `before` for the next page.
    #[serde(default, deserialize_with = "null_as_default")]
    pub pinned_at: String,
}

/// A page of pins, newest first.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PinsPage {
    /// The pins.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub items: Vec<ChannelPin>,
    /// Whether there are older pins.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub has_more: bool,
}

/// A page of users who reacted, by ascending ID.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ReactionUsersPage {
    /// The users.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub items: Vec<User>,
    /// Whether there is another page.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub has_more: bool,
    /// The `after` value for the next page.
    #[serde(default, deserialize_with = "opt_sf")]
    pub next_after: Option<Snowflake>,
}

impl Message {
    /// The embeds, typed. Embeds that do not parse are skipped.
    pub fn parsed_embeds(&self) -> Vec<Embed> {
        super::parse_values(&self.embeds, "embed")
    }

    /// The reactions on the message.
    pub fn reactions(&self) -> Vec<Reaction> {
        extra_list(&self.extra, "reactions")
    }

    /// The message this one replies to or forwards.
    pub fn message_reference(&self) -> Option<MessageReference> {
        extra_field(&self.extra, "message_reference")
    }

    /// Whether this is a reply (type 19, or a default reference with a message ID).
    pub fn is_reply(&self) -> bool {
        self.kind == message_type::REPLY
            || self.message_reference().is_some_and(|r| {
                r.kind == message_reference_type::DEFAULT && r.message_id.is_some()
            }) && self.extra.contains_key("referenced_message")
    }

    /// The message a reply points to.
    pub fn referenced_message(&self) -> ReferencedMessage {
        match self.extra.get("referenced_message") {
            None => ReferencedMessage::NotAReply,
            Some(Value::Null) => ReferencedMessage::Deleted,
            Some(v) => match Message::deserialize(v) {
                Ok(m) => ReferencedMessage::Message(Box::new(m)),
                Err(e) => {
                    tracing::warn!(error = %e, "referenced_message did not parse");
                    ReferencedMessage::Deleted
                }
            },
        }
    }

    /// Stickers sent with the message.
    pub fn stickers(&self) -> Vec<StickerItem> {
        extra_list(&self.extra, "stickers")
    }

    /// Channels the content links to.
    pub fn mention_channels(&self) -> Vec<ChannelMention> {
        extra_list(&self.extra, "mention_channels")
    }

    /// IDs of mentioned roles.
    pub fn mention_roles(&self) -> Vec<Snowflake> {
        match self.extra.get("mention_roles") {
            Some(v) => sf_list(v.clone()).unwrap_or_default(),
            None => Vec::new(),
        }
    }

    /// Whether the message mentions everyone.
    pub fn mention_everyone(&self) -> bool {
        self.extra.get("mention_everyone") == Some(&Value::Bool(true))
    }

    /// The author's member object in a guild channel (gateway events; without `user`).
    pub fn member(&self) -> Option<GuildMember> {
        extra_field(&self.extra, "member")
    }

    /// Webhook that sent the message.
    pub fn webhook_id(&self) -> Option<Snowflake> {
        self.extra
            .get("webhook_id")
            .and_then(|v| opt_sf(v.clone()).ok().flatten())
    }

    /// The [`message_flags`] as bits.
    pub fn flag_bits(&self) -> u64 {
        self.flags.as_ref().and_then(value_u64).unwrap_or(0)
    }

    /// Whether the account can edit this message: only its own `DEFAULT` and
    /// `REPLY` messages, and not forwards.
    pub fn can_edit(&self, current_user_id: &str) -> bool {
        self.author.id == current_user_id
            && matches!(self.kind, message_type::DEFAULT | message_type::REPLY)
            && !self
                .message_reference()
                .is_some_and(|r| r.kind == message_reference_type::FORWARD)
    }
}

impl Attachment {
    /// The [`attachment_flags`] as bits.
    pub fn flag_bits(&self) -> u64 {
        self.extra.get("flags").and_then(value_u64).unwrap_or(0)
    }

    /// Whether the attachment is a spoiler.
    pub fn is_spoiler(&self) -> bool {
        self.flag_bits() & attachment_flags::IS_SPOILER != 0
    }

    /// Whether the attachment is animated.
    pub fn is_animated(&self) -> bool {
        self.flag_bits() & attachment_flags::IS_ANIMATED != 0
    }

    /// Whether media inspection marked the file as explicit.
    pub fn is_explicit(&self) -> bool {
        self.flag_bits() & attachment_flags::CONTAINS_EXPLICIT_MEDIA != 0
            || self.extra.get("nsfw") == Some(&Value::Bool(true))
    }

    /// Whether the content type is an image.
    pub fn is_image(&self) -> bool {
        self.content_type
            .as_deref()
            .is_some_and(|t| t.starts_with("image/"))
    }

    /// Whether the content type is a video.
    pub fn is_video(&self) -> bool {
        self.content_type
            .as_deref()
            .is_some_and(|t| t.starts_with("video/"))
    }

    /// Whether the content type is audio.
    pub fn is_audio(&self) -> bool {
        self.content_type
            .as_deref()
            .is_some_and(|t| t.starts_with("audio/"))
    }

    /// Display title.
    pub fn title(&self) -> Option<&str> {
        self.extra.get("title")?.as_str()
    }

    /// Audio or video length in seconds.
    pub fn duration_secs(&self) -> Option<u64> {
        self.extra.get("duration").and_then(value_u64)
    }

    /// Encoded placeholder image.
    pub fn placeholder(&self) -> Option<&str> {
        self.extra.get("placeholder")?.as_str()
    }

    /// When the attachment expires (ISO 8601), if it does.
    pub fn expires_at(&self) -> Option<&str> {
        self.extra.get("expires_at")?.as_str()
    }

    /// Whether the attachment has expired (its `url` is then null).
    pub fn expired(&self) -> bool {
        self.extra.get("expired") == Some(&Value::Bool(true))
    }
}

/// A reply target for [`OutgoingMessage`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyTo {
    /// The message to reply to.
    pub message_id: Snowflake,
    /// Whether the reply mentions (notifies) the author of that message.
    pub mention_author: bool,
}

/// A message to send. Build it with the helpers and pass it to
/// [`crate::Client::send`]. At least one of content, stickers or files is
/// needed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OutgoingMessage {
    /// Markdown text.
    pub content: Option<String>,
    /// Nonce for matching the MESSAGE_CREATE echo; reuse it when retrying.
    pub nonce: Option<String>,
    /// Reply target.
    pub reply_to: Option<ReplyTo>,
    /// Sticker IDs (at most 3).
    pub sticker_ids: Vec<Snowflake>,
    /// [`message_flags`] to set (for example `SUPPRESS_NOTIFICATIONS`).
    pub flags: Option<u64>,
    /// Text-to-speech (needs `SEND_TTS_MESSAGES`).
    pub tts: bool,
    /// Suppress every mention (sends `allowed_mentions: {}`).
    pub suppress_mentions: bool,
}

impl OutgoingMessage {
    /// A text message.
    pub fn text(content: impl Into<String>) -> Self {
        OutgoingMessage {
            content: Some(content.into()),
            ..Default::default()
        }
    }

    /// Sets the nonce.
    pub fn with_nonce(mut self, nonce: impl Into<String>) -> Self {
        self.nonce = Some(nonce.into());
        self
    }

    /// Makes this a reply.
    pub fn reply_to(mut self, message_id: impl Into<String>, mention_author: bool) -> Self {
        self.reply_to = Some(ReplyTo {
            message_id: message_id.into(),
            mention_author,
        });
        self
    }

    /// The JSON body of Create message. `attachments` is the metadata of
    /// uploaded files (see [`crate::Client::send_with_files`]).
    pub(crate) fn to_json(&self, attachments: Option<Value>) -> Value {
        let mut body = Map::new();
        if let Some(c) = &self.content {
            body.insert("content".into(), json!(c));
        }
        if let Some(n) = &self.nonce {
            body.insert("nonce".into(), json!(n));
        }
        if let Some(r) = &self.reply_to {
            body.insert(
                "message_reference".into(),
                json!({ "message_id": r.message_id, "type": message_reference_type::DEFAULT }),
            );
        }
        if self.suppress_mentions {
            body.insert("allowed_mentions".into(), json!({}));
        } else if self.reply_to.as_ref().is_some_and(|r| !r.mention_author) {
            body.insert(
                "allowed_mentions".into(),
                json!({ "parse": ["users", "roles", "everyone"], "replied_user": false }),
            );
        }
        if !self.sticker_ids.is_empty() {
            body.insert("sticker_ids".into(), json!(self.sticker_ids));
        }
        if let Some(f) = self.flags {
            body.insert("flags".into(), json!(f));
        }
        if self.tts {
            body.insert("tts".into(), json!(true));
        }
        if let Some(a) = attachments {
            body.insert("attachments".into(), a);
        }
        Value::Object(body)
    }
}

/// Changes to a message ([`crate::Client::edit_message_with`]). Fields left
/// `None` stay the same.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MessageEdit {
    /// New text.
    pub content: Option<String>,
    /// New [`message_flags`] (for example to set or clear `SUPPRESS_EMBEDS`).
    pub flags: Option<u64>,
}

impl MessageEdit {
    pub(crate) fn to_json(&self) -> Value {
        let mut body = Map::new();
        if let Some(c) = &self.content {
            body.insert("content".into(), json!(c));
        }
        if let Some(f) = self.flags {
            body.insert("flags".into(), json!(f));
        }
        Value::Object(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Value {
        json!({
            "id": "1", "channel_id": "2", "type": 19, "flags": 4,
            "author": {"id": "3", "username": "ana"},
            "content": "re", "timestamp": "2026-11-10T08:00:00.000Z",
            "mention_roles": ["7", 8, null], "mention_everyone": true,
            "message_reference": {"channel_id": "2", "message_id": "0", "type": 0},
            "referenced_message": {"id": "0", "channel_id": "2", "author": {"id": "4"}, "content": "orig"},
            "reactions": [{"emoji": {"name": "👍"}, "count": 2, "me": true},
                          {"emoji": {"id": "55", "name": "party", "animated": true}, "count": "3"},
                          {"broken": true}],
            "stickers": [{"id": "9", "name": "wave", "animated": false}],
            "embeds": [{"type": "rich", "title": "Release", "url": "https://example.com", "color": 5793266,
                        "author": {"name": "ada", "icon_url": "https://x/a.png"},
                        "fields": [{"name": "A", "value": "B", "inline": true}, {"name": null}],
                        "footer": {"text": "foot"}, "image": {"url": "https://x/i.png", "proxy_url": "https://m/external/s/x", "width": 10, "height": "20"},
                        "thumbnail": "garbage", "provider": {"name": "Ex"}, "timestamp": "2026-01-01T00:00:00Z",
                        "video": {"url": "https://x/v.mp4", "duration": 5},
                        "children": [{"type": "link", "title": "inner"}]},
                       "not an embed"],
            "member": {"nick": "Ana", "roles": ["7"], "joined_at": "2026-01-01T00:00:00Z"}
        })
    }

    #[test]
    fn typed_parts_of_a_message() {
        let m: Message = serde_json::from_value(base()).unwrap();
        assert!(m.is_reply());
        assert_eq!(m.flag_bits(), message_flags::SUPPRESS_EMBEDS);
        assert_eq!(m.mention_roles(), vec!["7".to_string(), "8".to_string()]);
        assert!(m.mention_everyone());
        let r = m.message_reference().unwrap();
        assert_eq!(r.message_id.as_deref(), Some("0"));
        match m.referenced_message() {
            ReferencedMessage::Message(orig) => assert_eq!(orig.content, "orig"),
            other => panic!("{other:?}"),
        }
        let reactions = m.reactions();
        assert_eq!(reactions.len(), 2);
        assert!(reactions[0].me);
        assert_eq!(reactions[0].emoji.path_value(), "👍");
        assert_eq!(reactions[1].count, 3);
        assert_eq!(reactions[1].emoji.path_value(), "party:55");
        assert_eq!(m.stickers()[0].name, "wave");
        let embeds = m.parsed_embeds();
        assert_eq!(embeds.len(), 1);
        let e = &embeds[0];
        assert_eq!(e.kind, "rich");
        assert_eq!(e.color, Some(5_793_266));
        assert_eq!(e.author.as_ref().unwrap().name, "ada");
        assert_eq!(e.fields.len(), 2);
        assert!(e.fields[0].inline);
        assert_eq!(e.footer.as_ref().unwrap().text, "foot");
        assert_eq!(e.image.as_ref().unwrap().height, Some(20));
        assert!(e.thumbnail.is_none());
        assert_eq!(e.video.as_ref().unwrap().duration, Some(5));
        assert_eq!(e.provider.as_ref().unwrap().name, "Ex");
        assert_eq!(e.children[0].title.as_deref(), Some("inner"));
        let member = m.member().unwrap();
        assert_eq!(member.nick.as_deref(), Some("Ana"));
        assert!(member.user.is_none());
        assert!(!m.can_edit("4"));
        assert!(m.can_edit("3"));
    }

    #[test]
    fn deleted_and_absent_reference() {
        let mut v = base();
        v["referenced_message"] = Value::Null;
        let m: Message = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(m.referenced_message(), ReferencedMessage::Deleted);
        v.as_object_mut().unwrap().remove("referenced_message");
        v.as_object_mut().unwrap().remove("message_reference");
        v["type"] = json!(0);
        let m: Message = serde_json::from_value(v).unwrap();
        assert_eq!(m.referenced_message(), ReferencedMessage::NotAReply);
        assert!(!m.is_reply());
    }

    #[test]
    fn attachment_flags_and_kinds() {
        let a: Attachment = serde_json::from_value(json!({
            "id": "1", "filename": "a.gif", "content_type": "image/gif", "size": 5,
            "flags": (attachment_flags::IS_SPOILER | attachment_flags::IS_ANIMATED),
            "title": "T", "duration": 3, "expires_at": null, "placeholder": "abc"
        }))
        .unwrap();
        assert!(a.is_spoiler() && a.is_animated() && a.is_image());
        assert!(!a.is_video() && !a.is_explicit() && !a.expired());
        assert_eq!(a.title(), Some("T"));
        assert_eq!(a.duration_secs(), Some(3));
        assert_eq!(a.expires_at(), None);
    }

    #[test]
    fn outgoing_message_json() {
        let m = OutgoingMessage::text("hi")
            .with_nonce("42")
            .reply_to("100", false);
        let v = m.to_json(None);
        assert_eq!(v["content"], "hi");
        assert_eq!(v["nonce"], "42");
        assert_eq!(v["message_reference"]["message_id"], "100");
        assert_eq!(v["allowed_mentions"]["replied_user"], false);
        let plain = OutgoingMessage::text("x").reply_to("1", true).to_json(None);
        assert!(plain.get("allowed_mentions").is_none());
        let edit = MessageEdit {
            content: Some("new".into()),
            flags: None,
        };
        assert_eq!(edit.to_json(), json!({"content": "new"}));
    }
}
