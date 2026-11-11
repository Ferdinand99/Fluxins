//! RELATIONSHIP_REMOVE. (RELATIONSHIP_ADD and RELATIONSHIP_UPDATE carry a
//! whole [`crate::api::relationships::Relationship`]:
//! [`crate::Dispatch::RelationshipAdd`], [`crate::Dispatch::RelationshipUpdate`].)

use serde::{Deserialize, Serialize};

use crate::models::{sf, Snowflake};

/// RELATIONSHIP_REMOVE: a friendship, request or block ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationshipRemove {
    /// The other account's ID.
    #[serde(deserialize_with = "sf")]
    pub id: Snowflake,
}
