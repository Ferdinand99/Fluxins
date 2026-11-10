//! Message search (`POST /search/messages`) with Fluxer's filters.
//!
//! Build a [`MessageSearch`] for a channel, a guild or the DMs, add filters
//! (text, author, mentions, "has", dates, pinned …) and call
//! [`crate::Client::search_messages`]. Searches of one account start at least
//! one second apart ([`crate::coalesce::SEARCH_INTERVAL`]); debounce typing in
//! the search box in the UI as well.

use reqwest::Method;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use super::threads::{attach_members, ThreadMember};
use super::SearchOutcome;
use crate::error::{Error, Result};
use crate::models::{lenient_vec, snowflake_at_ms, u64_or_zero, Channel, Message, Snowflake};
use crate::Client;

/// A snowflake (not `@me`) for the search body.
fn sf_id(value: &str) -> Result<&str> {
    if crate::media::is_snowflake(value) {
        Ok(value)
    } else {
        Err(Error::InvalidInput(
            "message search: an ID is not a snowflake".into(),
        ))
    }
}

/// Values for [`MessageSearch::scope`].
pub mod search_scope {
    /// The guild in `guild_id`, or the channel in `channel_id` (default).
    pub const CURRENT: &str = "current";
    /// Open DMs and group DMs.
    pub const OPEN_DMS: &str = "open_dms";
    /// Every DM and group DM.
    pub const ALL_DMS: &str = "all_dms";
    /// Every guild.
    pub const ALL_GUILDS: &str = "all_guilds";
    /// Every guild and every DM.
    pub const ALL: &str = "all";
    /// Every guild and the open DMs.
    pub const OPEN_DMS_AND_ALL_GUILDS: &str = "open_dms_and_all_guilds";
}

/// Values for [`MessageSearch::has`] and [`MessageSearch::exclude_has`].
pub mod search_has {
    /// An image attachment.
    pub const IMAGE: &str = "image";
    /// An audio attachment.
    pub const SOUND: &str = "sound";
    /// A video attachment.
    pub const VIDEO: &str = "video";
    /// Any attachment.
    pub const FILE: &str = "file";
    /// A sticker.
    pub const STICKER: &str = "sticker";
    /// An embed.
    pub const EMBED: &str = "embed";
    /// An http(s) link in the text.
    pub const LINK: &str = "link";
    /// A forward.
    pub const SNAPSHOT: &str = "snapshot";
}

/// Values for [`MessageSearch::author_types`].
pub mod search_author_type {
    /// A user account.
    pub const USER: &str = "user";
    /// A bot.
    pub const BOT: &str = "bot";
    /// A webhook.
    pub const WEBHOOK: &str = "webhook";
}

const SCOPES: &[&str] = &[
    search_scope::CURRENT,
    search_scope::OPEN_DMS,
    search_scope::ALL_DMS,
    search_scope::ALL_GUILDS,
    search_scope::ALL,
    search_scope::OPEN_DMS_AND_ALL_GUILDS,
];
const HAS: &[&str] = &[
    search_has::IMAGE,
    search_has::SOUND,
    search_has::VIDEO,
    search_has::FILE,
    search_has::STICKER,
    search_has::EMBED,
    search_has::LINK,
    search_has::SNAPSHOT,
];
const AUTHOR_TYPES: &[&str] = &[
    search_author_type::USER,
    search_author_type::BOT,
    search_author_type::WEBHOOK,
];

/// A message search. Start with [`MessageSearch::in_guild`],
/// [`MessageSearch::in_channel`] or [`MessageSearch::in_scope`] and add filters.
/// Every list is checked against Fluxer's limits before anything is sent
/// ([`Error::InvalidInput`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MessageSearch {
    /// A [`search_scope`] value (default `current`).
    pub scope: Option<String>,
    /// The guild to search (`context_guild_id`).
    pub guild_id: Option<Snowflake>,
    /// The channel to search (`context_channel_id`).
    pub channel_id: Option<Snowflake>,
    /// Narrow a guild or wide scope to these channels (at most 500).
    pub channel_ids: Vec<Snowflake>,
    /// Channels to leave out (at most 500).
    pub exclude_channel_ids: Vec<Snowflake>,
    /// Free text, 1-1,024 characters; every word must match.
    pub content: Option<String>,
    /// Phrases that must match exactly (at most 10).
    pub exact_phrases: Vec<String>,
    /// Authors, any of which matches ("from:"; at most 100).
    pub author_ids: Vec<Snowflake>,
    /// Authors to leave out (at most 100).
    pub exclude_author_ids: Vec<Snowflake>,
    /// [`search_author_type`] values, any of which matches (at most 20).
    pub author_types: Vec<String>,
    /// Users who must all be mentioned ("mentions:"; at most 100).
    pub mentions: Vec<Snowflake>,
    /// Only messages with (or without) an everyone mention.
    pub mention_everyone: Option<bool>,
    /// Only pinned (or unpinned) messages.
    pub pinned: Option<bool>,
    /// [`search_has`] values that must all be present (at most 20).
    pub has: Vec<String>,
    /// [`search_has`] values that must not be present (at most 20).
    pub exclude_has: Vec<String>,
    /// Link hostnames that must all be present (at most 100).
    pub link_hostnames: Vec<String>,
    /// File names that must all be present (at most 100).
    pub attachment_filenames: Vec<String>,
    /// File extensions without the dot, lower case (at most 50).
    pub attachment_extensions: Vec<String>,
    /// Only messages after this ID ("after:").
    pub min_id: Option<Snowflake>,
    /// Only messages before this ID ("before:").
    pub max_id: Option<Snowflake>,
    /// `timestamp` (default) or `relevance`.
    pub sort_by: Option<String>,
    /// `desc` (newest first, default) or `asc`.
    pub sort_order: Option<String>,
    /// Page, 1-400.
    pub page: Option<u16>,
    /// Results per page, 1-25.
    pub hits_per_page: Option<u8>,
    /// Allow results from age-restricted channels (ignored under 18).
    pub include_nsfw: bool,
}

impl MessageSearch {
    /// Search one guild.
    pub fn in_guild(guild_id: impl Into<String>) -> Self {
        MessageSearch {
            guild_id: Some(guild_id.into()),
            ..Default::default()
        }
    }

    /// Search one channel (a guild channel, DM or group DM).
    pub fn in_channel(channel_id: impl Into<String>) -> Self {
        MessageSearch {
            channel_id: Some(channel_id.into()),
            ..Default::default()
        }
    }

    /// Search a wider [`search_scope`] (DMs, every guild …).
    pub fn in_scope(scope: &str) -> Self {
        MessageSearch {
            scope: Some(scope.to_owned()),
            ..Default::default()
        }
    }

    /// Free text.
    pub fn text(mut self, content: impl Into<String>) -> Self {
        self.content = Some(content.into());
        self
    }

    /// From this author ("from:").
    pub fn from_author(mut self, user_id: impl Into<String>) -> Self {
        self.author_ids.push(user_id.into());
        self
    }

    /// Mentions this user ("mentions:").
    pub fn mentioning(mut self, user_id: impl Into<String>) -> Self {
        self.mentions.push(user_id.into());
        self
    }

    /// Has this kind of content ("has:", a [`search_has`] value).
    pub fn with(mut self, has: &str) -> Self {
        self.has.push(has.to_owned());
        self
    }

    /// In this channel of the guild ("in:").
    pub fn in_channels(mut self, channel_id: impl Into<String>) -> Self {
        self.channel_ids.push(channel_id.into());
        self
    }

    /// Sent before this Unix time in milliseconds ("before:").
    pub fn before_time(mut self, unix_ms: u64) -> Self {
        self.max_id = Some(snowflake_at_ms(unix_ms));
        self
    }

    /// Sent after this Unix time in milliseconds ("after:").
    pub fn after_time(mut self, unix_ms: u64) -> Self {
        self.min_id = Some(snowflake_at_ms(unix_ms));
        self
    }

    /// Only pinned messages.
    pub fn pinned_only(mut self) -> Self {
        self.pinned = Some(true);
        self
    }

    /// Oldest first.
    pub fn oldest_first(mut self) -> Self {
        self.sort_order = Some("asc".into());
        self
    }

    /// This page (1-400).
    pub fn page(mut self, page: u16) -> Self {
        self.page = Some(page);
        self
    }

    /// The request body, after checking every field against Fluxer's limits.
    pub fn to_json(&self) -> Result<Value> {
        let bad = |what: &str| Err(Error::InvalidInput(format!("message search: {what}")));
        let mut body = Map::new();
        let scope = self.scope.as_deref().unwrap_or(search_scope::CURRENT);
        if !SCOPES.contains(&scope) {
            return bad("unknown scope");
        }
        if scope == search_scope::CURRENT && self.guild_id.is_none() && self.channel_id.is_none() {
            return bad("the current scope needs a guild or a channel");
        }
        if self.scope.is_some() {
            body.insert("scope".into(), json!(scope));
        }
        if let Some(g) = &self.guild_id {
            body.insert("context_guild_id".into(), json!(sf_id(g)?));
        }
        if let Some(c) = &self.channel_id {
            body.insert("context_channel_id".into(), json!(sf_id(c)?));
        }
        let mut ids = |key: &str, list: &[String], max: usize| -> Result<()> {
            if list.len() > max {
                return Err(Error::InvalidInput(format!("{key}: at most {max} IDs")));
            }
            for i in list {
                sf_id(i)?;
            }
            if !list.is_empty() {
                body.insert(key.into(), json!(list));
            }
            Ok(())
        };
        ids("channel_ids", &self.channel_ids, 500)?;
        ids("exclude_channel_id", &self.exclude_channel_ids, 500)?;
        ids("author_id", &self.author_ids, 100)?;
        ids("exclude_author_id", &self.exclude_author_ids, 100)?;
        ids("mentions", &self.mentions, 100)?;
        if let Some(c) = &self.content {
            super::text(c, 1024, "content")?;
            body.insert("content".into(), json!(c));
        }
        let mut texts = |key: &str, list: &[String], max: usize, len: usize| -> Result<()> {
            if list.len() > max {
                return Err(Error::InvalidInput(format!("{key}: at most {max}")));
            }
            for t in list {
                super::text(t, len, key)?;
            }
            if !list.is_empty() {
                body.insert(key.into(), json!(list));
            }
            Ok(())
        };
        texts("exact_phrases", &self.exact_phrases, 10, 1024)?;
        texts("link_hostname", &self.link_hostnames, 100, 255)?;
        texts("attachment_filename", &self.attachment_filenames, 100, 1024)?;
        texts("attachment_extension", &self.attachment_extensions, 50, 32)?;
        let mut kinds = |key: &str, list: &[String], allowed: &[&str]| -> Result<()> {
            if list.len() > 20 {
                return Err(Error::InvalidInput(format!("{key}: at most 20")));
            }
            if list.iter().any(|k| !allowed.contains(&k.as_str())) {
                return Err(Error::InvalidInput(format!("{key}: unknown value")));
            }
            if !list.is_empty() {
                body.insert(key.into(), json!(list));
            }
            Ok(())
        };
        kinds("author_type", &self.author_types, AUTHOR_TYPES)?;
        kinds("has", &self.has, HAS)?;
        kinds("exclude_has", &self.exclude_has, HAS)?;
        if let Some(m) = self.mention_everyone {
            body.insert("mention_everyone".into(), json!(m));
        }
        if let Some(p) = self.pinned {
            body.insert("pinned".into(), json!(p));
        }
        if let Some(m) = &self.min_id {
            body.insert("min_id".into(), json!(sf_id(m)?));
        }
        if let Some(m) = &self.max_id {
            body.insert("max_id".into(), json!(sf_id(m)?));
        }
        if let Some(s) = &self.sort_by {
            if !matches!(s.as_str(), "timestamp" | "relevance") {
                return bad("sort_by");
            }
            body.insert("sort_by".into(), json!(s));
        }
        if let Some(o) = &self.sort_order {
            if !matches!(o.as_str(), "asc" | "desc") {
                return bad("sort_order");
            }
            body.insert("sort_order".into(), json!(o));
        }
        if let Some(p) = self.page {
            body.insert("page".into(), json!(p.clamp(1, 400)));
        }
        if let Some(h) = self.hits_per_page {
            body.insert("hits_per_page".into(), json!(h.clamp(1, 25)));
        }
        if self.include_nsfw {
            body.insert("include_nsfw".into(), json!(true));
        }
        Ok(Value::Object(body))
    }
}

/// One page of [`crate::Client::search_messages`].
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct MessageSearchResults {
    /// Matching messages (without `referenced_message`).
    #[serde(default, deserialize_with = "lenient_vec")]
    pub messages: Vec<Message>,
    /// The channels of those messages.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub channels: Vec<Channel>,
    /// Matches in total.
    #[serde(default, deserialize_with = "u64_or_zero")]
    pub total: u64,
    /// Page size applied.
    #[serde(default, deserialize_with = "u64_or_zero")]
    pub hits_per_page: u64,
    /// Page applied.
    #[serde(default, deserialize_with = "u64_or_zero")]
    pub page: u64,
    /// Threads that hold a returned message (wide searches), with the
    /// account's memberships attached.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub threads: Vec<Channel>,
    /// The account's memberships in those threads.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub members: Vec<ThreadMember>,
}

impl MessageSearchResults {
    /// Whether another page follows this one.
    pub fn has_more(&self) -> bool {
        let per = self.hits_per_page.max(1);
        self.page < 400 && self.page.saturating_mul(per) < self.total
    }
}

impl Client {
    /// `POST /search/messages`. At most one search per second per account
    /// (shared with [`Client::search_threads`]); a search whose turn is more
    /// than three seconds away returns [`Error::RateLimited`] without a
    /// request. While a channel is being indexed the answer is
    /// [`SearchOutcome::Indexing`]; search again later (the crate does not retry).
    pub async fn search_messages(
        &self,
        search: &MessageSearch,
    ) -> Result<SearchOutcome<MessageSearchResults>> {
        let body = search.to_json()?;
        self.search_slot().await?;
        let raw = self
            .raw(Method::POST, "/search/messages", Some(&body))
            .await?;
        let value = raw.value();
        if value.get("indexing") == Some(&Value::Bool(true)) {
            return Ok(SearchOutcome::Indexing { retry_after: None });
        }
        let mut results: MessageSearchResults = raw.json()?;
        attach_members(&mut results.threads, &results.members);
        Ok(SearchOutcome::Ready(results))
    }
}
