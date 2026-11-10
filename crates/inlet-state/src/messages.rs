//! Message windows: the most recent messages of a few channels, in memory only.
//!
//! A window holds at most [`MAX_MESSAGES_PER_CHANNEL`] messages of one
//! channel in ID order, and at most [`MAX_MESSAGE_WINDOWS`] channels have a
//! window; the least recently used one is evicted. Pages and live events are
//! merged, never swapped (A2 from the v0 app review):
//!
//! - The app opens a window with [`crate::AccountState::begin_load`] before
//!   it asks for the newest page. Until that page lands the window is
//!   *loading*: live creates and edits wait in a small buffer, deletes are
//!   remembered.
//! - After a new READY every window is *stale* and behaves the same way until
//!   its newest page is loaded again.
//! - When the page lands, buffered messages newer than the page are added,
//!   buffered edits win when they are newer, and remembered deletes stay
//!   deleted. If the buffer overflowed, the window stays loading
//!   ([`MessageWindow::has_newest`] is `false`): load the newest page again.

use std::collections::{BTreeMap, VecDeque};

use inlet_protocol::models::{parse_timestamp_ms, Emoji, Reaction};
use inlet_protocol::Message;
use serde_json::{Map, Value};

use crate::bounded::LruMap;
use crate::ids::{ChannelId, MessageId};
use crate::limits::{
    MAX_MESSAGES_PER_CHANNEL, MAX_MESSAGE_BYTES, MAX_MESSAGE_WINDOWS, MAX_PENDING_LIVE,
    MAX_REACTIONS_PER_MESSAGE, MAX_RECENTLY_DELETED, MAX_WINDOW_BYTES,
};
use crate::size::{map_size, opt_len, value_size};

/// One page from `GET /channels/{id}/messages`, as the protocol crate's
/// `Client::messages` returns it (any order; newest first from the API).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MessagesPage {
    /// The messages.
    pub messages: Vec<Message>,
    /// The `before` the page was asked with; `None` for the newest page.
    pub before: Option<MessageId>,
    /// The `limit` it was asked with (1-100). Fewer messages than this
    /// means the start of the channel was reached.
    pub limit: u8,
}

impl MessagesPage {
    /// The newest messages of a channel.
    pub fn latest(messages: Vec<Message>, limit: u8) -> Self {
        MessagesPage {
            messages,
            before: None,
            limit,
        }
    }

    /// Messages older than `before`.
    pub fn before(before: MessageId, messages: Vec<Message>, limit: u8) -> Self {
        MessagesPage {
            messages,
            before: Some(before),
            limit,
        }
    }

    fn reached_start(&self) -> bool {
        self.messages.len() < usize::from(self.limit.max(1))
    }
}

/// The key a message over [`MAX_MESSAGE_BYTES`] carries in `extra` after it
/// was cut down to a stub (no content, embeds, attachments or mentions).
pub const OVERSIZED_KEY: &str = "inlet_oversized";

/// Whether a stored message is a stub for an oversized message.
pub fn is_oversized(m: &Message) -> bool {
    m.extra.get(OVERSIZED_KEY) == Some(&Value::Bool(true))
}

/// A copy fit for storage: the author trimmed, and a message over the byte
/// cap replaced by a stub, so the window keeps its place without a gap.
fn prepare(m: &Message) -> Message {
    let mut m = m.clone();
    crate::size::cap_user(&mut m.author);
    if approx_size(&m) > MAX_MESSAGE_BYTES {
        m.content.clear();
        m.embeds.clear();
        m.attachments.clear();
        m.mentions.clear();
        m.flags = None;
        m.nonce = None;
        crate::size::truncate(&mut m.timestamp, 64);
        m.edited_timestamp = None;
        m.extra = Map::new();
        m.extra.insert(OVERSIZED_KEY.into(), Value::Bool(true));
    }
    m
}

/// The loaded messages of one channel.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MessageWindow {
    messages: BTreeMap<MessageId, Message>,
    /// Approximate memory of the messages ([`approx_size`]).
    bytes: usize,
    has_more_before: bool,
    has_newest: bool,
    /// Live creates and edits while the window waits for its newest page.
    pending: BTreeMap<MessageId, Message>,
    pending_overflow: bool,
    recently_deleted: VecDeque<MessageId>,
}

impl MessageWindow {
    /// Messages, oldest first.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &Message> {
        self.messages.values()
    }

    /// One message.
    pub fn get(&self, id: MessageId) -> Option<&Message> {
        self.messages.get(&id)
    }

    /// Number of messages.
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    /// Whether the window is empty.
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// The oldest loaded message; load older ones with `before` set to it.
    pub fn oldest_id(&self) -> Option<MessageId> {
        self.messages.keys().next().copied()
    }

    /// The newest loaded message.
    pub fn newest_id(&self) -> Option<MessageId> {
        self.messages.keys().next_back().copied()
    }

    /// Whether older messages exist on the server.
    pub fn has_more_before(&self) -> bool {
        self.has_more_before
    }

    /// Whether the window reaches the newest message, so live messages are
    /// appended. `false` while loading, after a reconnect, after scrolling
    /// far back, or when the live buffer overflowed: load the newest page.
    pub fn has_newest(&self) -> bool {
        self.has_newest
    }

    /// Live messages waiting for the newest page.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Approximate memory the messages take, in bytes.
    pub fn approx_bytes(&self) -> usize {
        self.bytes
    }

    fn put(&mut self, id: MessageId, m: Message) {
        self.bytes = self.bytes.saturating_add(approx_size(&m));
        if let Some(old) = self.messages.insert(id, m) {
            self.bytes = self.bytes.saturating_sub(approx_size(&old));
        }
    }

    fn take(&mut self, id: MessageId) -> Option<Message> {
        let m = self.messages.remove(&id)?;
        self.bytes = self.bytes.saturating_sub(approx_size(&m));
        Some(m)
    }

    fn over_budget(&self) -> bool {
        self.messages.len() > MAX_MESSAGES_PER_CHANNEL
            || (self.bytes > MAX_WINDOW_BYTES && self.messages.len() > 1)
    }

    fn was_deleted(&self, id: MessageId) -> bool {
        self.recently_deleted.contains(&id)
    }

    fn remember_deleted(&mut self, id: MessageId) {
        if self.was_deleted(id) {
            return;
        }
        if self.recently_deleted.len() >= MAX_RECENTLY_DELETED {
            self.recently_deleted.pop_front();
        }
        self.recently_deleted.push_back(id);
    }

    /// Drops the oldest messages beyond the caps.
    fn trim_oldest(&mut self) {
        while self.over_budget() {
            let Some(id) = self.oldest_id() else { break };
            self.take(id);
            self.has_more_before = true;
        }
    }

    /// Drops the newest messages beyond the caps (after loading older pages).
    fn trim_newest(&mut self) {
        while self.over_budget() {
            let Some(id) = self.newest_id() else { break };
            self.take(id);
            self.has_newest = false;
        }
    }

    /// Buffers a live create or edit while the window is loading.
    fn buffer(&mut self, id: MessageId, mut m: Message) {
        if let Some(old) = self.pending.get(&id) {
            if edited_ms(&m) < edited_ms(old) {
                return;
            }
            carry_reactions(old, &mut m);
        }
        self.pending.insert(id, m);
        if self.pending.len() > MAX_PENDING_LIVE {
            self.pending.pop_first();
            self.pending_overflow = true;
        }
    }

    /// Merges the newest page and the live buffer.
    fn merge_latest(&mut self, page: BTreeMap<MessageId, Message>, reached_start: bool) {
        let pending = std::mem::take(&mut self.pending);
        let overflow = std::mem::take(&mut self.pending_overflow);
        let fresh = self.has_newest;
        let mut merged = BTreeMap::new();
        let mut has_more_before = !reached_start;
        let page_newest = page.keys().next_back().copied();
        match (page.keys().next().copied(), page_newest) {
            (Some(p_old), Some(p_new)) => {
                let contiguous = self.messages.contains_key(&p_old);
                let mut old = std::mem::take(&mut self.messages);
                let newer = old.split_off(&MessageId(p_new.0.saturating_add(1)));
                let mut in_range = old.split_off(&p_old);
                if contiguous {
                    merged.extend(old);
                    has_more_before = self.has_more_before;
                }
                for (id, m) in page {
                    let chosen = match in_range.remove(&id) {
                        Some(live) if fresh && edited_ms(&live) >= edited_ms(&m) => live,
                        _ => m,
                    };
                    merged.insert(id, chosen);
                }
                // Messages in the page's range that the page lacks were deleted.
                if fresh {
                    merged.extend(newer);
                }
            }
            _ => {
                // The channel had no messages when the page was read; keep
                // what arrived live since then.
                if fresh {
                    merged = std::mem::take(&mut self.messages);
                }
                has_more_before = false;
            }
        }
        for (id, mut m) in pending {
            if self.was_deleted(id) {
                continue;
            }
            match merged.get(&id) {
                Some(cur) => {
                    if edited_ms(&m) >= edited_ms(cur) {
                        carry_reactions(cur, &mut m);
                        merged.insert(id, m);
                    }
                }
                // A create newer than the page; after an overflow the
                // buffer has a hole, so wait for the next page instead.
                None if !overflow && page_newest.is_none_or(|n| id > n) => {
                    merged.insert(id, m);
                }
                None => {}
            }
        }
        self.bytes = merged.values().map(approx_size).sum();
        self.messages = merged;
        self.has_more_before = has_more_before;
        self.has_newest = !overflow;
        self.trim_oldest();
    }

    /// Merges a page of messages older than `before`. Returns `false` when
    /// the page no longer fits the window (it was replaced meanwhile).
    fn merge_older(
        &mut self,
        before: MessageId,
        page: BTreeMap<MessageId, Message>,
        reached_start: bool,
    ) -> bool {
        if !self.messages.contains_key(&before) {
            return false;
        }
        let at_oldest = self.oldest_id() == Some(before);
        for (id, m) in page.range(..before) {
            if !self.messages.contains_key(id) {
                self.put(*id, m.clone());
            }
        }
        if at_oldest {
            self.has_more_before = !reached_start;
        }
        self.trim_newest();
        true
    }

    /// A live MESSAGE_CREATE. Returns whether the visible messages changed.
    fn insert_live(&mut self, id: MessageId, m: &Message) -> bool {
        if self.was_deleted(id) {
            return false;
        }
        let mut m = prepare(m);
        if !self.has_newest {
            self.buffer(id, m);
            return false;
        }
        // An old message arriving late would leave a gap below the window.
        if self.has_more_before && self.oldest_id().is_some_and(|o| id < o) {
            return false;
        }
        if let Some(old) = self.messages.get(&id) {
            if old == &m {
                return false;
            }
            carry_reactions(old, &mut m);
        }
        self.put(id, m);
        self.trim_oldest();
        true
    }

    /// A MESSAGE_UPDATE. Edits that are older than the stored one are ignored.
    fn update(&mut self, id: MessageId, m: &Message) -> bool {
        if self.was_deleted(id) {
            return false;
        }
        let mut m = prepare(m);
        if !self.has_newest {
            self.buffer(id, m.clone());
        }
        let Some(old) = self.messages.get(&id) else {
            return false;
        };
        if edited_ms(&m) < edited_ms(old) || old == &m {
            return false;
        }
        carry_reactions(old, &mut m);
        self.put(id, m);
        self.trim_oldest();
        true
    }

    fn delete(&mut self, id: MessageId) -> bool {
        self.remember_deleted(id);
        self.pending.remove(&id);
        self.take(id).is_some()
    }

    fn react(&mut self, id: MessageId, f: impl Fn(&mut Vec<Reaction>) -> bool) -> bool {
        if let Some(p) = self.pending.get_mut(&id) {
            let mut reactions = p.reactions();
            if f(&mut reactions) {
                let value = serde_json::to_value(&reactions).unwrap_or(Value::Array(Vec::new()));
                p.extra.insert("reactions".into(), value);
            }
        }
        let Some(m) = self.messages.get(&id) else {
            return false;
        };
        let mut reactions = m.reactions();
        if !f(&mut reactions) {
            return false;
        }
        let mut m = m.clone();
        let value = serde_json::to_value(&reactions).unwrap_or(Value::Array(Vec::new()));
        m.extra.insert("reactions".into(), value);
        self.put(id, m);
        true
    }
}

/// A cheap estimate of the memory a message takes: its text, plus every
/// string and number in its JSON parts. Used to cap a window's size, so a
/// server sending huge messages cannot fill memory.
pub fn approx_size(m: &Message) -> usize {
    fn user(u: &inlet_protocol::User) -> usize {
        128 + u.id.len()
            + u.username.len()
            + u.discriminator.len()
            + opt_len(&u.global_name)
            + opt_len(&u.avatar)
            + u.flags.as_ref().map_or(0, value_size)
            + map_size(&u.extra)
    }
    let attachments: usize = m
        .attachments
        .iter()
        .map(|a| {
            160 + a.id.len()
                + a.filename.len()
                + opt_len(&a.content_type)
                + opt_len(&a.url)
                + opt_len(&a.proxy_url)
                + opt_len(&a.description)
                + map_size(&a.extra)
        })
        .sum();
    256 + m.id.len()
        + m.channel_id.len()
        + opt_len(&m.guild_id)
        + m.content.len()
        + m.timestamp.len()
        + opt_len(&m.edited_timestamp)
        + opt_len(&m.nonce)
        + m.flags.as_ref().map_or(0, value_size)
        + user(&m.author)
        + m.mentions.iter().map(user).sum::<usize>()
        + m.embeds.iter().map(value_size).sum::<usize>()
        + map_size(&m.extra)
        + attachments
}

/// The edit time in milliseconds; an unedited message is 0.
fn edited_ms(m: &Message) -> i64 {
    m.edited_timestamp
        .as_deref()
        .and_then(parse_timestamp_ms)
        .unwrap_or(0)
}

/// Keeps the stored reactions when an update does not carry any.
fn carry_reactions(old: &Message, new: &mut Message) {
    if !new.extra.contains_key("reactions") {
        if let Some(r) = old.extra.get("reactions") {
            new.extra.insert("reactions".into(), r.clone());
        }
    }
}

/// The message windows of one account.
#[derive(Debug, Clone, PartialEq)]
pub struct MessageWindows {
    windows: LruMap<ChannelId, MessageWindow>,
}

impl Default for MessageWindows {
    fn default() -> Self {
        MessageWindows {
            windows: LruMap::new(MAX_MESSAGE_WINDOWS),
        }
    }
}

/// Parses a page: messages of `channel` with a snowflake ID, by ID.
fn page_map(channel: ChannelId, messages: &[Message]) -> BTreeMap<MessageId, Message> {
    messages
        .iter()
        .filter(|m| ChannelId::parse(&m.channel_id) == Some(channel))
        .filter_map(|m| Some((MessageId::parse(&m.id)?, prepare(m))))
        .collect()
}

impl MessageWindows {
    /// A channel's window.
    pub fn get(&self, channel: ChannelId) -> Option<&MessageWindow> {
        self.windows.get(&channel)
    }

    /// Channels with a window, in ID order.
    pub fn channels(&self) -> impl Iterator<Item = ChannelId> + '_ {
        self.windows.iter().map(|(k, _)| *k)
    }

    /// Number of windows.
    pub fn len(&self) -> usize {
        self.windows.len()
    }

    /// Whether there are no windows.
    pub fn is_empty(&self) -> bool {
        self.windows.len() == 0
    }

    /// Marks the channel as just used, so it is evicted last.
    pub(crate) fn touch(&mut self, channel: ChannelId) {
        self.windows.touch(&channel);
    }

    /// Opens a loading window (or keeps the existing one) before the newest
    /// page is requested. Returns a channel whose window was evicted.
    pub(crate) fn begin_load(&mut self, channel: ChannelId) -> Option<ChannelId> {
        if self.windows.contains_key(&channel) {
            self.windows.touch(&channel);
            return None;
        }
        self.windows.insert(channel, MessageWindow::default())
    }

    /// Merges a REST page. Returns whether the window changed, and a channel
    /// whose window was evicted to make room.
    pub(crate) fn apply_page(
        &mut self,
        channel: ChannelId,
        page: &MessagesPage,
    ) -> (bool, Option<ChannelId>) {
        let mut map = page_map(channel, &page.messages);
        let reached_start = page.reached_start();
        let mut evicted = None;
        if page.before.is_none() {
            evicted = self.begin_load(channel);
        } else {
            self.windows.touch(&channel);
        }
        let Some(w) = self.windows.get_mut(&channel) else {
            return (false, evicted);
        };
        map.retain(|id, _| !w.was_deleted(*id));
        let before = w.clone();
        let fits = match page.before {
            None => {
                w.merge_latest(map, reached_start);
                true
            }
            Some(b) => w.merge_older(b, map, reached_start),
        };
        (fits && *w != before, evicted)
    }

    pub(crate) fn insert_live(&mut self, channel: ChannelId, id: MessageId, m: &Message) -> bool {
        self.windows
            .get_mut(&channel)
            .is_some_and(|w| w.insert_live(id, m))
    }

    pub(crate) fn update(&mut self, channel: ChannelId, id: MessageId, m: &Message) -> bool {
        self.windows
            .get_mut(&channel)
            .is_some_and(|w| w.update(id, m))
    }

    pub(crate) fn delete(&mut self, channel: ChannelId, id: MessageId) -> bool {
        self.windows.get_mut(&channel).is_some_and(|w| w.delete(id))
    }

    /// Adds one reaction. `me` says whether the account reacted.
    pub(crate) fn add_reaction(
        &mut self,
        channel: ChannelId,
        id: MessageId,
        emoji: &Emoji,
        me: bool,
    ) -> bool {
        let Some(w) = self.windows.get_mut(&channel) else {
            return false;
        };
        w.react(id, |rs| {
            if let Some(r) = rs.iter_mut().find(|r| r.emoji.same_reaction(emoji)) {
                // The account's own reaction is idempotent; others' cannot be told apart.
                if me && r.me {
                    return false;
                }
                r.count = r.count.saturating_add(1);
                r.me |= me;
                if emoji.animated && !r.emoji.animated {
                    r.emoji.animated = true;
                }
                return true;
            }
            if rs.len() >= MAX_REACTIONS_PER_MESSAGE {
                return false;
            }
            rs.push(Reaction {
                emoji: emoji.clone(),
                count: 1,
                me,
            });
            true
        })
    }

    /// Removes one reaction.
    pub(crate) fn remove_reaction(
        &mut self,
        channel: ChannelId,
        id: MessageId,
        emoji: &Emoji,
        me: bool,
    ) -> bool {
        let Some(w) = self.windows.get_mut(&channel) else {
            return false;
        };
        w.react(id, |rs| {
            let Some(i) = rs.iter().position(|r| r.emoji.same_reaction(emoji)) else {
                return false;
            };
            if me && !rs[i].me {
                return false;
            }
            rs[i].count = rs[i].count.saturating_sub(1);
            if me {
                rs[i].me = false;
            }
            if rs[i].count == 0 {
                rs.remove(i);
            }
            true
        })
    }

    /// Removes every reaction, or every reaction with one emoji.
    pub(crate) fn clear_reactions(
        &mut self,
        channel: ChannelId,
        id: MessageId,
        emoji: Option<&Emoji>,
    ) -> bool {
        let Some(w) = self.windows.get_mut(&channel) else {
            return false;
        };
        w.react(id, |rs| {
            let before = rs.len();
            match emoji {
                Some(e) => rs.retain(|r| !r.emoji.same_reaction(e)),
                None => rs.clear(),
            }
            rs.len() != before
        })
    }

    pub(crate) fn remove(&mut self, channel: ChannelId) -> bool {
        self.windows.remove(&channel).is_some()
    }

    pub(crate) fn retain(&mut self, mut keep: impl FnMut(ChannelId) -> bool) {
        self.windows.retain(|k, _| keep(*k));
    }

    /// After a new READY the windows may have missed messages: keep them,
    /// but buffer live messages until the newest page is loaded again.
    pub(crate) fn mark_stale(&mut self) {
        for w in self.windows.values_mut() {
            w.has_newest = false;
        }
    }

    pub(crate) fn clear(&mut self) {
        self.windows.clear();
    }
}
