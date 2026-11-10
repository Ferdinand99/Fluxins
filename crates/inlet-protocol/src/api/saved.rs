//! Saved messages: private bookmarks of the account (Fluxer's "Saved").
//!
//! The list is checked against the account's permissions on every read: an
//! entry the account can no longer read comes back with status
//! `missing_permissions` and no message. SAVED_MESSAGE_CREATE and
//! SAVED_MESSAGE_DELETE keep other sessions in step
//! ([`crate::Dispatch::SavedMessageCreate`]).

use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::error::Result;
use crate::models::{lenient_opt, null_as_default, sf, Message, Snowflake};
use crate::rest::id;
use crate::Client;

/// Values of [`SavedMessage::status`].
pub mod saved_message_status {
    /// The message is included.
    pub const AVAILABLE: &str = "available";
    /// The account cannot read the message any more.
    pub const MISSING_PERMISSIONS: &str = "missing_permissions";
}

/// One saved message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedMessage {
    /// Entry ID (the message ID).
    #[serde(deserialize_with = "sf")]
    pub id: Snowflake,
    /// The channel it was saved from.
    #[serde(deserialize_with = "sf")]
    pub channel_id: Snowflake,
    /// The saved message.
    #[serde(deserialize_with = "sf")]
    pub message_id: Snowflake,
    /// A [`saved_message_status`] value.
    #[serde(default, deserialize_with = "null_as_default")]
    pub status: String,
    /// The message as the account can see it; `None` when it is not
    /// available (or did not parse).
    #[serde(default, deserialize_with = "lenient_opt")]
    pub message: Option<Message>,
    /// Every other field.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl SavedMessage {
    /// Whether the message can be shown.
    pub fn is_available(&self) -> bool {
        self.status == saved_message_status::AVAILABLE && self.message.is_some()
    }
}

impl Client {
    /// `GET /users/@me/saved-messages`: up to `limit` (1-100) entries, newest
    /// first, with a message ID below `before`. A page can be shorter than
    /// `limit` when saved messages were deleted.
    pub async fn saved_messages(
        &self,
        before: Option<&str>,
        limit: u8,
    ) -> Result<Vec<SavedMessage>> {
        let mut path = format!("/users/@me/saved-messages?limit={}", limit.clamp(1, 100));
        if let Some(b) = before {
            path.push_str("&before=");
            path.push_str(id(b)?);
        }
        self.get_list(&path, "saved message").await
    }

    /// `POST /users/@me/saved-messages`: saves a message. Fluxer allows 50
    /// per account by default (`MAX_BOOKMARKS` past that).
    pub async fn save_message(&self, channel_id: &str, message_id: &str) -> Result<()> {
        let body = json!({ "channel_id": id(channel_id)?, "message_id": id(message_id)? });
        self.raw(Method::POST, "/users/@me/saved-messages", Some(&body))
            .await?;
        Ok(())
    }

    /// `DELETE /users/@me/saved-messages/{message}`: removes a saved message
    /// (succeeds whether or not it was saved).
    pub async fn unsave_message(&self, message_id: &str) -> Result<()> {
        self.raw(
            Method::DELETE,
            &format!("/users/@me/saved-messages/{}", id(message_id)?),
            None,
        )
        .await?;
        Ok(())
    }
}
