//! Recent mentions (the inbox): messages in guild channels that mentioned
//! the account, newest first. Removing an entry sends RECENT_MENTION_DELETE
//! to every session ([`crate::Dispatch::RecentMentionDelete`]); it does not
//! change the channel's unread mention count (that is the read state).

use reqwest::Method;
use serde_json::json;

use crate::error::{Error, Result};
use crate::models::{Message, Snowflake};
use crate::rest::id;
use crate::Client;

/// Most entries [`Client::mark_mentions_read`] takes per request.
pub const MAX_MENTIONS_PER_READ: usize = 100;

/// Filters for [`Client::recent_mentions`]. The default asks for 25
/// mentions of every kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionsQuery {
    /// Mentions to look up, 1-100. A page can hold fewer when some can no
    /// longer be read.
    pub limit: u8,
    /// Include role mentions.
    pub roles: bool,
    /// Include `@everyone` and `@here`.
    pub everyone: bool,
    /// Include guild channels (all recorded mentions are in guilds).
    pub guilds: bool,
    /// Only mentions with a message ID below this (the last one seen).
    pub before: Option<Snowflake>,
}

impl Default for MentionsQuery {
    fn default() -> Self {
        MentionsQuery {
            limit: 25,
            roles: true,
            everyone: true,
            guilds: true,
            before: None,
        }
    }
}

impl Client {
    /// `GET /users/@me/mentions`: recent mentions the account can still read.
    pub async fn recent_mentions(&self, query: &MentionsQuery) -> Result<Vec<Message>> {
        let mut path = format!(
            "/users/@me/mentions?limit={}&roles={}&everyone={}&guilds={}",
            query.limit.clamp(1, 100),
            query.roles,
            query.everyone,
            query.guilds
        );
        if let Some(b) = &query.before {
            path.push_str("&before=");
            path.push_str(id(b)?);
        }
        self.get_list(&path, "mention").await
    }

    /// `DELETE /users/@me/mentions/{message}`: removes one entry.
    pub async fn delete_recent_mention(&self, message_id: &str) -> Result<()> {
        self.raw(
            Method::DELETE,
            &format!("/users/@me/mentions/{}", id(message_id)?),
            None,
        )
        .await?;
        Ok(())
    }

    /// `POST /users/@me/mentions/read`: removes up to 100 entries in one
    /// request (for "clear all" send them in pages of 100).
    pub async fn mark_mentions_read(&self, message_ids: &[&str]) -> Result<()> {
        if message_ids.is_empty() || message_ids.len() > MAX_MENTIONS_PER_READ {
            return Err(Error::InvalidInput(format!(
                "mentions read: 1-{MAX_MENTIONS_PER_READ} message IDs"
            )));
        }
        for m in message_ids {
            id(m)?;
        }
        self.raw(
            Method::POST,
            "/users/@me/mentions/read",
            Some(&json!({ "message_ids": message_ids })),
        )
        .await?;
        Ok(())
    }
}
