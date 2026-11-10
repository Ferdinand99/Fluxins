//! SAVED_MESSAGE_DELETE and RECENT_MENTION_DELETE. (SAVED_MESSAGE_CREATE
//! carries a whole message: [`crate::Dispatch::SavedMessageCreate`].)

use serde::{Deserialize, Serialize};

use crate::models::{sf, Snowflake};

/// SAVED_MESSAGE_DELETE: the account unsaved a message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedMessageDelete {
    /// The message that is no longer saved.
    #[serde(deserialize_with = "sf")]
    pub message_id: Snowflake,
}

/// RECENT_MENTION_DELETE: the account removed a message from its recent mentions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentMentionDelete {
    /// The message removed from recent mentions.
    #[serde(deserialize_with = "sf")]
    pub message_id: Snowflake,
}
