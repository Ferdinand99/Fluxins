//! Message helpers: a page around or after a message (jump to a reply,
//! search hit or mention), forwarding, and hiding embeds. Silent messages
//! are [`crate::OutgoingMessage::silent`] (and
//! [`crate::models::strip_silent_prefix`] for "@silent"); marking unread is
//! [`crate::Client::mark_unread`].

use reqwest::Method;
use serde_json::{json, Map, Value};

use crate::error::{Error, Result};
use crate::models::{message_flags, message_reference_type, Message, MessageEdit, Snowflake};
use crate::rest::snowflake;
use crate::Client;

/// Most attachments or embeds picked for one forward.
pub const MAX_FORWARD_PICKS: usize = 10;

/// The message a forward copies ([`Client::forward`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ForwardSource {
    /// The message.
    pub message_id: Snowflake,
    /// Its channel.
    pub channel_id: Snowflake,
    /// Its guild.
    pub guild_id: Option<Snowflake>,
    /// Only these attachments (at most 10); empty forwards all media.
    pub attachment_ids: Vec<Snowflake>,
    /// Only these embeds by index (at most 10); empty forwards all media.
    pub embed_indices: Vec<u32>,
}

impl ForwardSource {
    /// Forwards `message` with all its media.
    pub fn of(message: &Message) -> Self {
        ForwardSource {
            message_id: message.id.clone(),
            channel_id: message.channel_id.clone(),
            guild_id: message.guild_id.clone(),
            ..Default::default()
        }
    }

    fn to_json(&self) -> Result<Value> {
        let mut r = Map::new();
        r.insert("message_id".into(), json!(snowflake(&self.message_id)?));
        r.insert("channel_id".into(), json!(snowflake(&self.channel_id)?));
        if let Some(g) = &self.guild_id {
            r.insert("guild_id".into(), json!(snowflake(g)?));
        }
        r.insert("type".into(), json!(message_reference_type::FORWARD));
        super::id_list(
            &self.attachment_ids,
            MAX_FORWARD_PICKS,
            "forward attachment_ids",
        )?;
        if !self.attachment_ids.is_empty() {
            r.insert("attachment_ids".into(), json!(self.attachment_ids));
        }
        if self.embed_indices.len() > MAX_FORWARD_PICKS {
            return Err(Error::InvalidInput(
                "forward embed_indices: at most 10".into(),
            ));
        }
        if !self.embed_indices.is_empty() {
            r.insert("embed_indices".into(), json!(self.embed_indices));
        }
        Ok(Value::Object(r))
    }
}

impl Client {
    /// `GET /channels/{id}/messages?around={id}`: up to `limit` (1-100)
    /// messages around `message_id`, newest first. Use it to jump to a
    /// message that is not loaded.
    pub async fn messages_around(
        &self,
        channel_id: &str,
        message_id: &str,
        limit: u8,
    ) -> Result<Vec<Message>> {
        self.get_list(
            &format!(
                "/channels/{}/messages?limit={}&around={}",
                snowflake(channel_id)?,
                limit.clamp(1, 100),
                snowflake(message_id)?
            ),
            "message",
        )
        .await
    }

    /// `GET /channels/{id}/messages?after={id}`: up to `limit` (1-100)
    /// messages after `message_id`, newest first (to fill a gap below a jump).
    pub async fn messages_after(
        &self,
        channel_id: &str,
        message_id: &str,
        limit: u8,
    ) -> Result<Vec<Message>> {
        self.get_list(
            &format!(
                "/channels/{}/messages?limit={}&after={}",
                snowflake(channel_id)?,
                limit.clamp(1, 100),
                snowflake(message_id)?
            ),
            "message",
        )
        .await
    }

    /// `POST /channels/{id}/messages` with a `FORWARD` reference: copies
    /// `source` into `channel_id`. A comment goes in a second message, as in
    /// Fluxer's client; `silent` sends the forward without notifications.
    /// Forwarding to several channels is one call per channel (each through
    /// the rate limiter).
    pub async fn forward(
        &self,
        channel_id: &str,
        source: &ForwardSource,
        nonce: Option<&str>,
        silent: bool,
    ) -> Result<Message> {
        let mut body = json!({ "message_reference": source.to_json()? });
        if let Some(n) = nonce {
            body["nonce"] = json!(n);
        }
        if silent {
            body["flags"] = json!(message_flags::SUPPRESS_NOTIFICATIONS);
        }
        self.rate_limiter().coalesce.reset_typing(channel_id);
        self.raw(
            Method::POST,
            &format!("/channels/{}/messages", snowflake(channel_id)?),
            Some(&body),
        )
        .await?
        .json()
    }

    /// Hides (`true`) or shows the embeds of a message by flipping
    /// `SUPPRESS_EMBEDS` in its current flags. The author, or someone with
    /// `MANAGE_MESSAGES`, may do this.
    ///
    /// Only the bits a client may set ([`message_flags::CLIENT_SETTABLE`])
    /// are sent. Fluxer keeps the server-set bits (`HAS_THREAD`,
    /// `CROSSPOSTED` …) itself, but replaces the client-settable ones of an
    /// author's message with the request, so `SUPPRESS_NOTIFICATIONS` and
    /// `VOICE_MESSAGE` are sent back as they are.
    pub async fn set_embeds_suppressed(
        &self,
        message: &Message,
        suppressed: bool,
    ) -> Result<Message> {
        let kept = message.flag_bits() & message_flags::CLIENT_SETTABLE;
        let flags = if suppressed {
            kept | message_flags::SUPPRESS_EMBEDS
        } else {
            kept & !message_flags::SUPPRESS_EMBEDS
        };
        let edit = MessageEdit {
            flags: Some(flags),
            ..MessageEdit::default()
        };
        self.edit_message_with(&message.channel_id, &message.id, &edit)
            .await
    }
}
