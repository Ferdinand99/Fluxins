//! Thread events (`THREAD_*`) and FORUM_UNREADS. THREAD_CREATE and
//! THREAD_UPDATE carry a whole thread ([`crate::Channel`]); see
//! [`crate::api::threads`] for the thread helpers and how to keep the
//! account's membership on a stored thread.

use serde::{Deserialize, Serialize};

use crate::api::threads::ThreadMember;
use crate::models::{
    lenient_bool, lenient_opt, lenient_vec, opt_sf, opt_u64, sf, sf_list, u64_or_zero, Channel,
    GuildMember, Presence, Snowflake,
};

/// Most entries Fluxer sends in one THREAD_MEMBER_LIST_UPDATE; more are dropped.
pub const MAX_THREAD_MEMBER_LIST: usize = 1_000;

/// THREAD_DELETE.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadDelete {
    /// The thread.
    #[serde(deserialize_with = "sf")]
    pub id: Snowflake,
    /// Its guild.
    #[serde(default, deserialize_with = "opt_sf")]
    pub guild_id: Option<Snowflake>,
    /// Its parent channel.
    #[serde(default, deserialize_with = "opt_sf")]
    pub parent_id: Option<Snowflake>,
    /// The thread type.
    #[serde(rename = "type", default, deserialize_with = "crate::models::opt_i64")]
    pub kind: Option<i64>,
}

/// THREAD_LIST_SYNC: the account's active threads in a guild (or in some of
/// its channels) are replaced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThreadListSync {
    /// The guild.
    #[serde(deserialize_with = "sf")]
    pub guild_id: Snowflake,
    /// The parent channels whose threads are replaced; `None` for the whole guild.
    #[serde(default, deserialize_with = "opt_sf_list")]
    pub channel_ids: Option<Vec<Snowflake>>,
    /// The active threads the session can view.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub threads: Vec<Channel>,
    /// The account's memberships in those threads.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub members: Vec<ThreadMember>,
}

impl ThreadListSync {
    /// The threads with their memberships attached
    /// ([`Channel::set_thread_member`]), ready to store with
    /// [`Channel::replace_thread`]: a thread without one is not joined.
    pub fn threads_with_members(&self) -> Vec<Channel> {
        let mut threads = self.threads.clone();
        crate::api::threads::attach_members(&mut threads, &self.members);
        threads
    }
}

fn opt_sf_list<'de, D>(d: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(d)?;
    if v.is_array() {
        sf_list(v).map(Some).map_err(serde::de::Error::custom)
    } else {
        Ok(None)
    }
}

/// THREAD_MEMBERS_UPDATE: users joined or left a thread.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThreadMembersUpdate {
    /// The thread.
    #[serde(deserialize_with = "sf")]
    pub id: Snowflake,
    /// Its guild.
    #[serde(default, deserialize_with = "opt_sf")]
    pub guild_id: Option<Snowflake>,
    /// Thread members now (capped at 50).
    #[serde(default, deserialize_with = "opt_u64")]
    pub member_count: Option<u64>,
    /// Users who joined (with `member` and, raw, `presence`).
    #[serde(default, deserialize_with = "lenient_vec")]
    pub added_members: Vec<ThreadMember>,
    /// Users who left.
    #[serde(default, deserialize_with = "sf_list")]
    pub removed_member_ids: Vec<Snowflake>,
}

impl ThreadMembersUpdate {
    /// What the update means for the account `me`: `Some(member)` when it
    /// joined, `Some(None)` when it left, `None` when it is not concerned.
    pub fn membership_change(&self, me: &str) -> Option<Option<&ThreadMember>> {
        if let Some(m) = self
            .added_members
            .iter()
            .find(|m| m.user_id.as_deref() == Some(me))
        {
            return Some(Some(m));
        }
        self.removed_member_ids
            .iter()
            .any(|u| u == me)
            .then_some(None)
    }
}

/// One row of THREAD_MEMBER_LIST_UPDATE.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThreadMemberListItem {
    /// The user.
    #[serde(deserialize_with = "sf")]
    pub user_id: Snowflake,
    /// When they joined.
    #[serde(default)]
    pub join_timestamp: Option<String>,
    /// Thread member flags.
    #[serde(default, deserialize_with = "u64_or_zero")]
    pub flags: u64,
    /// The guild member, when still in the guild.
    #[serde(default, deserialize_with = "lenient_opt")]
    pub member: Option<GuildMember>,
    /// Presence (`offline` when not visibly online).
    #[serde(default, deserialize_with = "lenient_opt")]
    pub presence: Option<Presence>,
}

/// THREAD_MEMBER_LIST_UPDATE: the full member list of a thread subscribed
/// with [`crate::GatewayHandle::subscribe_threads`]. At most
/// [`MAX_THREAD_MEMBER_LIST`] rows are kept.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThreadMemberListUpdate {
    /// The guild.
    #[serde(deserialize_with = "sf")]
    pub guild_id: Snowflake,
    /// The thread.
    #[serde(deserialize_with = "sf")]
    pub thread_id: Snowflake,
    /// The members.
    #[serde(default, deserialize_with = "capped_members")]
    pub members: Vec<ThreadMemberListItem>,
}

fn capped_members<'de, D>(d: D) -> Result<Vec<ThreadMemberListItem>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let mut v: Vec<ThreadMemberListItem> = lenient_vec(d)?;
    v.truncate(MAX_THREAD_MEMBER_LIST);
    Ok(v)
}

/// One post in FORUM_UNREADS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForumUnread {
    /// The post.
    #[serde(deserialize_with = "sf")]
    pub thread_id: Snowflake,
    /// Messages after the requested ack, capped at 25.
    #[serde(default, deserialize_with = "opt_u64")]
    pub count: Option<u64>,
    /// The request had no valid ack for this post.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub missing: bool,
}

/// FORUM_UNREADS: the answer to [`crate::GatewayHandle::request_forum_unreads`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForumUnreads {
    /// The guild.
    #[serde(deserialize_with = "sf")]
    pub guild_id: Snowflake,
    /// The forum or media channel.
    #[serde(deserialize_with = "sf")]
    pub channel_id: Snowflake,
    /// One entry per requested post found.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub threads: Vec<ForumUnread>,
}

/// One post to count in [`crate::GatewayHandle::request_forum_unreads`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForumUnreadRequest {
    /// The post.
    pub thread_id: Snowflake,
    /// The newest message read in it, if any.
    pub ack_message_id: Option<Snowflake>,
}
