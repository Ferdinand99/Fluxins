//! Threads and forums: starting threads and forum posts, joining and
//! leaving, archiving, archived lists, thread search and forum post data.
//!
//! A thread is a [`Channel`] of type 10, 11 or 12 with a parent; a forum
//! post is a public thread in a forum (15) or media (16) channel. The
//! thread fields stay in [`Channel::extra`]; the helpers here read them
//! ([`Channel::thread_metadata`], [`Channel::applied_tags`],
//! [`Channel::available_tags`] …).
//!
//! # Membership and private threads
//!
//! Fluxer puts the account's own membership as `member` on a thread only in
//! READY/GUILD_CREATE `threads`, THREAD_CREATE, THREAD_LIST_SYNC
//! (`members`) and REST answers. THREAD_UPDATE never carries it. Keep the
//! membership on the stored thread with [`Channel::merge_thread_update`]
//! and [`Channel::set_thread_member`] (from THREAD_MEMBER_UPDATE,
//! THREAD_MEMBERS_UPDATE and THREAD_LIST_SYNC), because
//! [`crate::PermissionContext`] reads it to show private threads.
//!
//! # Listing a forum
//!
//! Fluxer's client lists forum posts with [`crate::Client::search_threads`]
//! (sorted by last message) plus the active threads from the gateway, and
//! fills in the first message and author with [`crate::Client::forum_post_data`].
//! Unread counts per post come from Request Forum Unreads
//! ([`crate::GatewayHandle::request_forum_unreads`]).

use std::collections::BTreeMap;
use std::time::Duration;

use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use super::SearchOutcome;
use crate::error::{Error, Result};
use crate::models::{
    channel_type, extra_field, extra_list, lenient_bool, lenient_vec, null_as_default, opt_sf,
    opt_u64, sf, sf_list, Channel, GuildMember, Message, OutgoingMessage, Snowflake,
};
use crate::rest::{enc_query, id};
use crate::Client;

/// Thread member flags ([`ThreadMember::flags`]). At most one of the three
/// notification flags is set; none means "follow the parent".
pub mod thread_member_flags {
    /// The user sent a message in the thread or created it.
    pub const HAS_INTERACTED: u64 = 1 << 0;
    /// Notify of every message.
    pub const ALL_MESSAGES: u64 = 1 << 1;
    /// Notify of mentions only.
    pub const ONLY_MENTIONS: u64 = 1 << 2;
    /// Notify of nothing.
    pub const NO_MESSAGES: u64 = 1 << 3;
}

/// Auto archive durations in minutes.
pub mod auto_archive {
    /// One hour.
    pub const ONE_HOUR: u32 = 60;
    /// One day.
    pub const ONE_DAY: u32 = 1440;
    /// Three days (the default).
    pub const THREE_DAYS: u32 = 4320;
    /// One week.
    pub const ONE_WEEK: u32 = 10080;
}

/// Channel flags of forum channels and posts ([`Channel::flag_bits`]).
pub mod forum_flags {
    /// The post is the pinned post of its forum.
    pub const PINNED: u64 = 1 << 1;
    /// A new post needs at least one tag.
    pub const REQUIRE_TAG: u64 = 1 << 4;
    /// Clients hide the media download options of the channel.
    pub const HIDE_MEDIA_DOWNLOAD_OPTIONS: u64 = 1 << 15;
}

/// Most tags on one forum post.
pub const MAX_APPLIED_TAGS: usize = 5;
/// Longest thread or post name.
pub const MAX_THREAD_NAME: usize = 100;

/// Archive and lock state of a thread (`thread_metadata`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ThreadMetadata {
    /// Archived.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub archived: bool,
    /// Auto archive duration in minutes.
    #[serde(default, deserialize_with = "opt_u64")]
    pub auto_archive_duration: Option<u64>,
    /// When the archive state or duration last changed (or creation).
    #[serde(default)]
    pub archive_timestamp: Option<String>,
    /// Only thread moderators can act in it.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub locked: bool,
    /// Members who are not moderators can add members (private threads).
    #[serde(default)]
    pub invitable: Option<bool>,
    /// When the thread was created.
    #[serde(default)]
    pub create_timestamp: Option<String>,
    /// Every other field.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// A thread member (the account's own membership, or an entry of a list).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ThreadMember {
    /// The thread (absent on the `member` of a thread in READY and GUILD_CREATE).
    #[serde(default, deserialize_with = "opt_sf")]
    pub id: Option<Snowflake>,
    /// The user (absent like `id`).
    #[serde(default, deserialize_with = "opt_sf")]
    pub user_id: Option<Snowflake>,
    /// The guild, only on THREAD_MEMBER_UPDATE.
    #[serde(default, deserialize_with = "opt_sf")]
    pub guild_id: Option<Snowflake>,
    /// When the user last joined.
    #[serde(default)]
    pub join_timestamp: Option<String>,
    /// [`thread_member_flags`].
    #[serde(default, deserialize_with = "crate::models::u64_or_zero")]
    pub flags: u64,
    /// The thread is muted (own membership only).
    #[serde(default, deserialize_with = "lenient_bool")]
    pub muted: bool,
    /// Mute expiry (own membership only), raw.
    #[serde(default)]
    pub mute_config: Option<Value>,
    /// The guild member, when sent.
    #[serde(default, deserialize_with = "crate::models::lenient_opt")]
    pub member: Option<GuildMember>,
    /// Every other field.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl ThreadMember {
    /// The notification level bits only.
    pub fn notification_flags(&self) -> u64 {
        use thread_member_flags::*;
        self.flags & (ALL_MESSAGES | ONLY_MENTIONS | NO_MESSAGES)
    }
}

/// A forum tag (`available_tags` on a forum channel).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForumTag {
    /// Tag ID.
    #[serde(deserialize_with = "sf")]
    pub id: Snowflake,
    /// Name.
    #[serde(default, deserialize_with = "null_as_default")]
    pub name: String,
    /// Only thread moderators can apply or remove it.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub moderated: bool,
    /// Custom emoji of the guild.
    #[serde(default, deserialize_with = "opt_sf")]
    pub emoji_id: Option<Snowflake>,
    /// A Unicode emoji.
    #[serde(default)]
    pub emoji_name: Option<String>,
}

impl Channel {
    /// Whether this is a forum (15) or media (16) channel.
    pub fn is_forum(&self) -> bool {
        matches!(
            self.kind,
            channel_type::GUILD_FORUM | channel_type::GUILD_MEDIA
        )
    }

    /// The archive and lock state of a thread.
    pub fn thread_metadata(&self) -> Option<ThreadMetadata> {
        extra_field(&self.extra, "thread_metadata")
    }

    /// The account's membership in this thread, when known.
    pub fn thread_member(&self) -> Option<ThreadMember> {
        extra_field(&self.extra, "member")
    }

    /// Whether the account joined this thread (as far as the stored channel says).
    pub fn is_thread_member(&self) -> bool {
        self.extra.get("member").is_some_and(Value::is_object)
    }

    /// Stores or clears the account's membership (from THREAD_MEMBER_UPDATE,
    /// THREAD_MEMBERS_UPDATE, THREAD_LIST_SYNC or a join or leave).
    pub fn set_thread_member(&mut self, member: Option<&ThreadMember>) {
        match member.and_then(|m| serde_json::to_value(m).ok()) {
            Some(v) => {
                self.extra.insert("member".into(), v);
            }
            None => {
                self.extra.remove("member");
            }
        }
    }

    /// Replaces this stored thread with a THREAD_UPDATE (or any thread
    /// object), keeping the account's membership when the update has none.
    pub fn merge_thread_update(&mut self, update: Channel) {
        let member = self.extra.remove("member");
        *self = update;
        if !self.is_thread_member() {
            if let Some(m) = member.filter(Value::is_object) {
                self.extra.insert("member".into(), m);
            }
        }
    }

    /// Forum tag IDs on a forum post.
    pub fn applied_tags(&self) -> Vec<Snowflake> {
        self.extra
            .get("applied_tags")
            .and_then(|v| sf_list(v.clone()).ok())
            .unwrap_or_default()
    }

    /// The tags posts in this forum can use.
    pub fn available_tags(&self) -> Vec<ForumTag> {
        extra_list(&self.extra, "available_tags")
    }

    /// Thread members (capped at 50 by Fluxer).
    pub fn member_count(&self) -> Option<u64> {
        self.extra.get("member_count").and_then(value_u64)
    }

    /// Messages in a thread (not counting deleted ones or a post's first message).
    pub fn message_count(&self) -> Option<u64> {
        self.extra.get("message_count").and_then(value_u64)
    }

    /// The user or webhook that created the thread.
    pub fn owner_id(&self) -> Option<Snowflake> {
        self.extra
            .get("owner_id")
            .and_then(|v| opt_sf(v.clone()).ok().flatten())
    }

    /// Whether THREAD_CREATE says the thread was just created (not that the account was added).
    pub fn newly_created(&self) -> bool {
        self.extra.get("newly_created") == Some(&Value::Bool(true))
    }
}

fn value_u64(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// A page of threads (archived lists, active threads).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ThreadList {
    /// Threads.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub threads: Vec<Channel>,
    /// The account's memberships in those threads.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub members: Vec<ThreadMember>,
    /// More pages follow.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub has_more: bool,
}

impl ThreadList {
    /// Puts each membership from `members` on its thread
    /// ([`Channel::set_thread_member`]), so the threads can be stored as they are.
    pub fn attach_members(&mut self) {
        attach_members(&mut self.threads, &self.members);
    }
}

pub(crate) fn attach_members(threads: &mut [Channel], members: &[ThreadMember]) {
    for m in members {
        if let Some(t) = threads
            .iter_mut()
            .find(|t| Some(t.id.as_str()) == m.id.as_deref())
        {
            t.set_thread_member(Some(m));
        }
    }
}

/// Results of [`Client::search_threads`].
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ThreadSearchResults {
    /// Matching threads.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub threads: Vec<Channel>,
    /// The account's memberships in those threads.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub members: Vec<ThreadMember>,
    /// More results follow (raise `offset`).
    #[serde(default, deserialize_with = "lenient_bool")]
    pub has_more: bool,
    /// Matching threads in total.
    #[serde(default, deserialize_with = "crate::models::u64_or_zero")]
    pub total_results: u64,
    /// The first message of each forum post returned.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub first_messages: Vec<Message>,
}

/// A thread to start in a text or announcement channel
/// ([`Client::start_thread`], [`Client::start_thread_from_message`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewThread {
    /// Name, 1-100 characters.
    pub name: String,
    /// [`channel_type::PUBLIC_THREAD`] (default), `PRIVATE_THREAD` or
    /// `ANNOUNCEMENT_THREAD`. Ignored when starting from a message.
    pub kind: i64,
    /// [`auto_archive`] minutes (default from the server: three days).
    pub auto_archive_duration: Option<u32>,
    /// Slowmode in seconds (0-21,600).
    pub rate_limit_per_user: Option<u32>,
    /// Private threads: members who are not moderators can add members.
    pub invitable: Option<bool>,
}

impl NewThread {
    /// A public thread with this name.
    pub fn named(name: impl Into<String>) -> Self {
        NewThread {
            name: name.into(),
            kind: channel_type::PUBLIC_THREAD,
            auto_archive_duration: None,
            rate_limit_per_user: None,
            invitable: None,
        }
    }

    fn check(&self) -> Result<()> {
        super::text(&self.name, MAX_THREAD_NAME, "thread name")?;
        check_archive(self.auto_archive_duration)?;
        check_slowmode(self.rate_limit_per_user)
    }
}

fn check_archive(minutes: Option<u32>) -> Result<()> {
    use auto_archive::*;
    match minutes {
        None | Some(ONE_HOUR | ONE_DAY | THREE_DAYS | ONE_WEEK) => Ok(()),
        Some(_) => Err(Error::InvalidInput(
            "auto archive duration: 60, 1440, 4320 or 10080 minutes".into(),
        )),
    }
}

fn check_slowmode(secs: Option<u32>) -> Result<()> {
    match secs {
        Some(s) if s > 21_600 => Err(Error::InvalidInput("slowmode: 0-21,600 seconds".into())),
        _ => Ok(()),
    }
}

/// A post to create in a forum or media channel ([`Client::create_forum_post`]).
#[derive(Debug, Clone, PartialEq)]
pub struct NewForumPost {
    /// Title, 1-100 characters.
    pub name: String,
    /// The first message. Its reply target, nonce and text-to-speech are not sent.
    pub message: OutgoingMessage,
    /// Forum tag IDs (at most 5; required when the forum has `REQUIRE_TAG`).
    pub applied_tags: Vec<Snowflake>,
    /// [`auto_archive`] minutes.
    pub auto_archive_duration: Option<u32>,
    /// Slowmode in seconds (0-21,600).
    pub rate_limit_per_user: Option<u32>,
}

impl NewForumPost {
    /// A post with a title and a text message.
    pub fn new(name: impl Into<String>, content: impl Into<String>) -> Self {
        NewForumPost {
            name: name.into(),
            message: OutgoingMessage::text(content),
            applied_tags: Vec::new(),
            auto_archive_duration: None,
            rate_limit_per_user: None,
        }
    }

    /// Adds forum tags.
    pub fn with_tags<I, S>(mut self, tags: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.applied_tags.extend(tags.into_iter().map(Into::into));
        self
    }

    fn to_json(&self) -> Result<Value> {
        super::text(&self.name, MAX_THREAD_NAME, "post title")?;
        super::id_list(&self.applied_tags, MAX_APPLIED_TAGS, "applied_tags")?;
        check_archive(self.auto_archive_duration)?;
        check_slowmode(self.rate_limit_per_user)?;
        let mut message = self.message.clone();
        message.reply_to = None;
        message.nonce = None;
        message.tts = false;
        let mut body = json!({
            "name": self.name,
            "message": message.to_json(None),
            "type": channel_type::PUBLIC_THREAD,
        });
        if !self.applied_tags.is_empty() {
            body["applied_tags"] = json!(self.applied_tags);
        }
        if let Some(a) = self.auto_archive_duration {
            body["auto_archive_duration"] = json!(a);
        }
        if let Some(r) = self.rate_limit_per_user {
            body["rate_limit_per_user"] = json!(r);
        }
        Ok(body)
    }
}

/// A forum post as Create returns it: the thread and its first message.
#[derive(Debug, Clone, PartialEq)]
pub struct ForumPost {
    /// The post (a public thread).
    pub thread: Channel,
    /// The first message (same ID as the post), when it parsed.
    pub message: Option<Message>,
}

/// Changes to a thread ([`Client::edit_thread`]). Fields left `None` stay.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThreadEdit {
    /// New name.
    pub name: Option<String>,
    /// Archive (`true`) or unarchive (`false`).
    pub archived: Option<bool>,
    /// Lock or unlock (moderators).
    pub locked: Option<bool>,
    /// [`auto_archive`] minutes.
    pub auto_archive_duration: Option<u32>,
    /// The complete set of forum tags of a post (at most 5).
    pub applied_tags: Option<Vec<Snowflake>>,
    /// Private threads: members can add members.
    pub invitable: Option<bool>,
    /// Slowmode in seconds (moderators).
    pub rate_limit_per_user: Option<u32>,
    /// Channel flags of a post (only [`forum_flags::PINNED`]; moderators).
    pub flags: Option<u64>,
}

impl ThreadEdit {
    fn to_json(&self) -> Result<Value> {
        let mut body = Map::new();
        if let Some(n) = &self.name {
            super::text(n, MAX_THREAD_NAME, "thread name")?;
            body.insert("name".into(), json!(n));
        }
        if let Some(a) = self.archived {
            body.insert("archived".into(), json!(a));
        }
        if let Some(l) = self.locked {
            body.insert("locked".into(), json!(l));
        }
        if let Some(d) = self.auto_archive_duration {
            check_archive(Some(d))?;
            body.insert("auto_archive_duration".into(), json!(d));
        }
        if let Some(tags) = &self.applied_tags {
            super::id_list(tags, MAX_APPLIED_TAGS, "applied_tags")?;
            body.insert("applied_tags".into(), json!(tags));
        }
        if let Some(i) = self.invitable {
            body.insert("invitable".into(), json!(i));
        }
        if let Some(r) = self.rate_limit_per_user {
            check_slowmode(Some(r))?;
            body.insert("rate_limit_per_user".into(), json!(r));
        }
        if let Some(f) = self.flags {
            body.insert("flags".into(), json!(f & forum_flags::PINNED));
        }
        if body.is_empty() {
            return Err(Error::InvalidInput("a thread edit without changes".into()));
        }
        Ok(Value::Object(body))
    }
}

/// Values for [`ThreadSearch::sort_by`].
pub mod thread_sort {
    /// Newest activity first (default).
    pub const LAST_MESSAGE_TIME: &str = "last_message_time";
    /// Most recently archived.
    pub const ARCHIVE_TIME: &str = "archive_time";
    /// Best match.
    pub const RELEVANCE: &str = "relevance";
    /// Newest thread.
    pub const CREATION_TIME: &str = "creation_time";
}

/// Filters for [`Client::search_threads`]. The default lists every thread
/// of the channel, newest activity first, 25 at a time.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThreadSearch {
    /// Text to match in thread names (at most 100 characters).
    pub name: Option<String>,
    /// Forum tag IDs (at most 20).
    pub tags: Vec<Snowflake>,
    /// `match_some` or `match_all` (default from the forum).
    pub tag_setting: Option<String>,
    /// Only archived (`true`) or only active (`false`) threads.
    pub archived: Option<bool>,
    /// A [`thread_sort`] value.
    pub sort_by: Option<String>,
    /// `asc` or `desc` (default).
    pub sort_order: Option<String>,
    /// Results per page, 1-25.
    pub limit: Option<u8>,
    /// Results to skip, 0-9,975.
    pub offset: Option<u32>,
    /// Only threads with an ID below this.
    pub max_id: Option<Snowflake>,
    /// Only threads with an ID above this.
    pub min_id: Option<Snowflake>,
}

impl ThreadSearch {
    fn to_query(&self) -> Result<String> {
        let mut q: Vec<String> = Vec::new();
        if let Some(n) = &self.name {
            super::text(n, 100, "thread search name")?;
            q.push(format!("name={}", enc_query(n)));
        }
        super::id_list(&self.tags, 20, "tag")?;
        for t in &self.tags {
            q.push(format!("tag={t}"));
        }
        if let Some(s) = &self.tag_setting {
            if !matches!(s.as_str(), "match_some" | "match_all") {
                return Err(Error::InvalidInput("tag_setting".into()));
            }
            q.push(format!("tag_setting={s}"));
        }
        if let Some(a) = self.archived {
            q.push(format!("archived={a}"));
        }
        if let Some(s) = &self.sort_by {
            use thread_sort::*;
            if ![LAST_MESSAGE_TIME, ARCHIVE_TIME, RELEVANCE, CREATION_TIME].contains(&s.as_str()) {
                return Err(Error::InvalidInput("thread sort_by".into()));
            }
            q.push(format!("sort_by={s}"));
        }
        if let Some(o) = &self.sort_order {
            if !matches!(o.as_str(), "asc" | "desc") {
                return Err(Error::InvalidInput("sort_order".into()));
            }
            q.push(format!("sort_order={o}"));
        }
        if let Some(l) = self.limit {
            q.push(format!("limit={}", l.clamp(1, 25)));
        }
        if let Some(o) = self.offset {
            q.push(format!("offset={}", o.min(9_975)));
        }
        if let Some(m) = &self.max_id {
            q.push(format!("max_id={}", id(m)?));
        }
        if let Some(m) = &self.min_id {
            q.push(format!("min_id={}", id(m)?));
        }
        Ok(q.join("&"))
    }
}

/// Owner and first message of a forum post ([`Client::forum_post_data`]).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ForumPostData {
    /// The guild member who created the post (none for webhooks or when they left).
    #[serde(default, deserialize_with = "crate::models::lenient_opt")]
    pub owner: Option<GuildMember>,
    /// The first message (none when it was deleted).
    #[serde(default, deserialize_with = "crate::models::lenient_opt")]
    pub first_message: Option<Message>,
}

/// The account's notification settings in a thread ([`Client::update_thread_settings`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThreadSettings {
    /// One of [`thread_member_flags`] `ALL_MESSAGES`, `ONLY_MENTIONS`,
    /// `NO_MESSAGES`, or 0 to follow the parent.
    pub flags: Option<u64>,
    /// Muted.
    pub muted: Option<bool>,
}

impl Client {
    /// `POST /channels/{channel}/messages/{message}/threads`: starts a public
    /// thread from a message (the thread gets the message's ID).
    pub async fn start_thread_from_message(
        &self,
        channel_id: &str,
        message_id: &str,
        thread: &NewThread,
    ) -> Result<Channel> {
        thread.check()?;
        let mut body = json!({ "name": thread.name });
        if let Some(a) = thread.auto_archive_duration {
            body["auto_archive_duration"] = json!(a);
        }
        if let Some(r) = thread.rate_limit_per_user {
            body["rate_limit_per_user"] = json!(r);
        }
        self.raw(
            Method::POST,
            &format!(
                "/channels/{}/messages/{}/threads",
                id(channel_id)?,
                id(message_id)?
            ),
            Some(&body),
        )
        .await?
        .json()
    }

    /// `POST /channels/{channel}/threads`: starts a thread without a source
    /// message in a text or announcement channel. For forums use
    /// [`Client::create_forum_post`].
    pub async fn start_thread(&self, channel_id: &str, thread: &NewThread) -> Result<Channel> {
        thread.check()?;
        if !matches!(
            thread.kind,
            channel_type::PUBLIC_THREAD
                | channel_type::PRIVATE_THREAD
                | channel_type::ANNOUNCEMENT_THREAD
        ) {
            return Err(Error::InvalidInput("thread type".into()));
        }
        let mut body = json!({ "name": thread.name, "type": thread.kind });
        if let Some(a) = thread.auto_archive_duration {
            body["auto_archive_duration"] = json!(a);
        }
        if let Some(r) = thread.rate_limit_per_user {
            body["rate_limit_per_user"] = json!(r);
        }
        if let Some(i) = thread.invitable {
            body["invitable"] = json!(i);
        }
        self.raw(
            Method::POST,
            &format!("/channels/{}/threads", id(channel_id)?),
            Some(&body),
        )
        .await?
        .json()
    }

    /// `POST /channels/{forum}/threads`: creates a post with tags in a forum
    /// or media channel. Posts with files are not supported here yet.
    pub async fn create_forum_post(
        &self,
        forum_id: &str,
        post: &NewForumPost,
    ) -> Result<ForumPost> {
        let body = post.to_json()?;
        let mut thread: Channel = self
            .raw(
                Method::POST,
                &format!("/channels/{}/threads", id(forum_id)?),
                Some(&body),
            )
            .await?
            .json()?;
        let message = extra_field(&thread.extra, "message");
        thread.extra.remove("message");
        Ok(ForumPost { thread, message })
    }

    /// `PATCH /channels/{thread}`: renames, archives, unarchives, locks or
    /// retags a thread. Returns the changed thread.
    pub async fn edit_thread(&self, thread_id: &str, edit: &ThreadEdit) -> Result<Channel> {
        let body = edit.to_json()?;
        self.raw(
            Method::PATCH,
            &format!("/channels/{}", id(thread_id)?),
            Some(&body),
        )
        .await?
        .json()
    }

    /// Archives (`true`) or unarchives a thread ([`Client::edit_thread`]).
    pub async fn set_thread_archived(&self, thread_id: &str, archived: bool) -> Result<Channel> {
        self.edit_thread(
            thread_id,
            &ThreadEdit {
                archived: Some(archived),
                ..ThreadEdit::default()
            },
        )
        .await
    }

    /// `PUT /channels/{thread}/thread-members/@me`: joins a thread.
    pub async fn join_thread(&self, thread_id: &str) -> Result<()> {
        self.raw(
            Method::PUT,
            &format!("/channels/{}/thread-members/@me", id(thread_id)?),
            None,
        )
        .await?;
        Ok(())
    }

    /// `DELETE /channels/{thread}/thread-members/@me`: leaves a thread.
    pub async fn leave_thread(&self, thread_id: &str) -> Result<()> {
        self.raw(
            Method::DELETE,
            &format!("/channels/{}/thread-members/@me", id(thread_id)?),
            None,
        )
        .await?;
        Ok(())
    }

    /// `PATCH /channels/{thread}/thread-members/@me/settings`: notification
    /// level and mute for a joined thread. `None` when nothing changed (204).
    pub async fn update_thread_settings(
        &self,
        thread_id: &str,
        settings: &ThreadSettings,
    ) -> Result<Option<ThreadMember>> {
        use thread_member_flags::*;
        let mut body = Map::new();
        if let Some(f) = settings.flags {
            if f & !(ALL_MESSAGES | ONLY_MENTIONS | NO_MESSAGES) != 0 || f.count_ones() > 1 {
                return Err(Error::InvalidInput(
                    "thread notification flags: one level or 0".into(),
                ));
            }
            body.insert("flags".into(), json!(f));
        }
        if let Some(m) = settings.muted {
            body.insert("muted".into(), json!(m));
        }
        let raw = self
            .raw(
                Method::PATCH,
                &format!("/channels/{}/thread-members/@me/settings", id(thread_id)?),
                Some(&Value::Object(body)),
            )
            .await?;
        if raw.body.is_empty() {
            return Ok(None);
        }
        raw.json().map(Some)
    }

    async fn thread_list(&self, path: String) -> Result<ThreadList> {
        let mut list: ThreadList = self.raw(Method::GET, &path, None).await?.json()?;
        list.attach_members();
        Ok(list)
    }

    /// `GET /channels/{channel}/threads/archived/public`: archived public and
    /// announcement threads, most recently archived first. `before` is an
    /// ISO 8601 time (the `archive_timestamp` of the last thread seen);
    /// `limit` 2-100. Memberships are attached to the threads.
    pub async fn public_archived_threads(
        &self,
        channel_id: &str,
        before: Option<&str>,
        limit: u8,
    ) -> Result<ThreadList> {
        self.thread_list(archived_path(
            &format!("/channels/{}/threads/archived/public", id(channel_id)?),
            before,
            limit,
            false,
        )?)
        .await
    }

    /// `GET /channels/{channel}/threads/archived/private`: archived private
    /// threads (thread moderators only). Like [`Client::public_archived_threads`].
    pub async fn private_archived_threads(
        &self,
        channel_id: &str,
        before: Option<&str>,
        limit: u8,
    ) -> Result<ThreadList> {
        self.thread_list(archived_path(
            &format!("/channels/{}/threads/archived/private", id(channel_id)?),
            before,
            limit,
            false,
        )?)
        .await
    }

    /// `GET /channels/{channel}/users/@me/threads/archived/private`: archived
    /// private threads the account joined. `before` is a thread ID here.
    pub async fn joined_private_archived_threads(
        &self,
        channel_id: &str,
        before: Option<&str>,
        limit: u8,
    ) -> Result<ThreadList> {
        self.thread_list(archived_path(
            &format!(
                "/channels/{}/users/@me/threads/archived/private",
                id(channel_id)?
            ),
            before,
            limit,
            true,
        )?)
        .await
    }

    /// `GET /channels/{channel}/threads/search`: threads of a channel or forum
    /// by name and tag. Paced with message search (one search per second per
    /// account). While the instance builds its index the answer is
    /// [`SearchOutcome::Indexing`]; the crate does not retry.
    pub async fn search_threads(
        &self,
        channel_id: &str,
        search: &ThreadSearch,
    ) -> Result<SearchOutcome<ThreadSearchResults>> {
        let query = search.to_query()?;
        let mut path = format!("/channels/{}/threads/search", id(channel_id)?);
        if !query.is_empty() {
            path.push('?');
            path.push_str(&query);
        }
        self.search_slot().await?;
        let raw = self.raw(Method::GET, &path, None).await?;
        let value = raw.value();
        if raw.status == 202
            || value.get("code").and_then(Value::as_str) == Some("SEARCH_INDEX_NOT_READY")
        {
            let retry_after = value
                .get("retry_after")
                .and_then(Value::as_f64)
                .map(|s| crate::ratelimit::clamp_server_secs(s, MAX_INDEX_WAIT));
            return Ok(SearchOutcome::Indexing { retry_after });
        }
        let mut results: ThreadSearchResults = raw.json()?;
        attach_members(&mut results.threads, &results.members);
        Ok(SearchOutcome::Ready(results))
    }

    /// `POST /channels/{forum}/post-data`: the owner and first message of up
    /// to 100 posts, keyed by post ID.
    pub async fn forum_post_data(
        &self,
        forum_id: &str,
        thread_ids: &[&str],
    ) -> Result<BTreeMap<Snowflake, ForumPostData>> {
        if thread_ids.is_empty() || thread_ids.len() > 100 {
            return Err(Error::InvalidInput("post data: 1-100 thread IDs".into()));
        }
        for t in thread_ids {
            id(t)?;
        }
        let raw = self
            .raw(
                Method::POST,
                &format!("/channels/{}/post-data", id(forum_id)?),
                Some(&json!({ "thread_ids": thread_ids })),
            )
            .await?;
        let mut out = BTreeMap::new();
        if let Some(Value::Object(map)) = raw.value().get_mut("threads").map(Value::take) {
            for (k, v) in map {
                match ForumPostData::deserialize(v) {
                    Ok(d) => {
                        out.insert(k, d);
                    }
                    Err(e) => tracing::warn!(error = %e, "post data did not parse"),
                }
            }
        }
        Ok(out)
    }
}

/// Longest "index not ready" wait honoured.
const MAX_INDEX_WAIT: Duration = Duration::from_secs(300);

fn archived_path(
    base: &str,
    before: Option<&str>,
    limit: u8,
    before_is_id: bool,
) -> Result<String> {
    let mut path = format!("{base}?limit={}", limit.clamp(2, 100));
    if let Some(b) = before {
        path.push_str("&before=");
        if before_is_id {
            path.push_str(id(b)?);
        } else {
            if b.is_empty()
                || b.len() > 40
                || !b
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b':' | b'.' | b'-' | b'+'))
            {
                return Err(Error::InvalidInput("before: an ISO 8601 time".into()));
            }
            path.push_str(&enc_query(b));
        }
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_search_query_and_checks() {
        let q = ThreadSearch {
            name: Some("bug report".into()),
            tags: vec!["1".into(), "2".into()],
            archived: Some(false),
            sort_by: Some(thread_sort::CREATION_TIME.into()),
            limit: Some(200),
            ..ThreadSearch::default()
        }
        .to_query()
        .unwrap();
        assert_eq!(
            q,
            "name=bug+report&tag=1&tag=2&archived=false&sort_by=creation_time&limit=25"
        );
        let bad = ThreadSearch {
            tags: vec!["..".into()],
            ..ThreadSearch::default()
        };
        assert!(matches!(bad.to_query(), Err(Error::InvalidInput(_))));
        assert!(
            archived_path("/x", Some("2026-01-01T00:00:00.000Z"), 1, false)
                .unwrap()
                .ends_with("?limit=2&before=2026-01-01T00%3A00%3A00.000Z")
        );
        assert!(archived_path("/x", Some("a&b=c"), 50, false).is_err());
        assert!(archived_path("/x", Some("../1"), 50, true).is_err());
    }

    #[test]
    fn membership_survives_a_thread_update() {
        let mut stored: Channel = serde_json::from_value(json!({
            "id": "5", "type": 12, "guild_id": "1", "parent_id": "2",
            "member": {"join_timestamp": "2026-01-01T00:00:00Z", "flags": 1}
        }))
        .unwrap();
        assert!(stored.is_thread_member());
        let update: Channel = serde_json::from_value(json!({
            "id": "5", "type": 12, "guild_id": "1", "parent_id": "2", "name": "renamed",
            "thread_metadata": {"archived": true, "locked": null, "auto_archive_duration": "60"}
        }))
        .unwrap();
        stored.merge_thread_update(update);
        assert!(stored.is_thread_member());
        assert_eq!(stored.name.as_deref(), Some("renamed"));
        let meta = stored.thread_metadata().unwrap();
        assert!(meta.archived && !meta.locked);
        assert_eq!(meta.auto_archive_duration, Some(60));
        stored.set_thread_member(None);
        assert!(!stored.is_thread_member());
        stored.set_thread_member(Some(&ThreadMember {
            flags: thread_member_flags::ONLY_MENTIONS | thread_member_flags::HAS_INTERACTED,
            ..ThreadMember::default()
        }));
        assert_eq!(
            stored.thread_member().unwrap().notification_flags(),
            thread_member_flags::ONLY_MENTIONS
        );
    }
}
