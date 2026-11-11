//! Account-level events: notes, pinned DMs, group DM participants and memes.
//! (FAVORITE_MEME_CREATE and FAVORITE_MEME_UPDATE carry a whole
//! [`crate::api::expressions::Meme`].)

use serde::{Deserialize, Serialize};

use crate::models::{null_as_default, sf, Snowflake, User};

/// USER_NOTE_UPDATE: the account wrote or cleared a private note.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserNoteUpdate {
    /// The user the note is about.
    #[serde(deserialize_with = "sf")]
    pub id: Snowflake,
    /// The note, empty when cleared.
    #[serde(default, deserialize_with = "null_as_default")]
    pub note: String,
}

/// CHANNEL_RECIPIENT_ADD and CHANNEL_RECIPIENT_REMOVE: someone joined or
/// left a group DM the account is in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelRecipientEvent {
    /// The group DM.
    #[serde(deserialize_with = "sf")]
    pub channel_id: Snowflake,
    /// Who joined or left.
    pub user: User,
}

/// FAVORITE_MEME_DELETE: the account deleted a meme.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FavoriteMemeDelete {
    /// The deleted meme.
    #[serde(deserialize_with = "sf")]
    pub meme_id: Snowflake,
}
