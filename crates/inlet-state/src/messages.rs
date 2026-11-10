//! Message windows: the most recent messages of a few channels, in memory only.
//!
//! A window holds at most [`MAX_MESSAGES_PER_CHANNEL`] messages of one
//! channel in ID order, and at most [`MAX_MESSAGE_WINDOWS`] channels have a
//! window; the least recently used one is evicted. A window is created by a
//! REST page ([`crate::AccountState::apply_messages`]) and kept current by
//! gateway events. Pages and live events are merged, never swapped: a
//! message that arrives while a page is on its way stays, and a message
//! deleted meanwhile does not come back (A2 from the v0 app review).

use std::collections::{BTreeMap, VecDeque};

use inlet_protocol::models::{parse_timestamp_ms, Emoji, Reaction};
use inlet_protocol::Message;
use serde_json::Value;

use crate::bounded::LruMap;
use crate::ids::{ChannelId, MessageId};
use crate::limits::{
    MAX_MESSAGES_PER_CHANNEL, MAX_MESSAGE_BYTES, MAX_MESSAGE_WINDOWS, MAX_REACTIONS_PER_MESSAGE,
    MAX_RECENTLY_DELETED, MAX_WINDOW_BYTES,
};

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

/// The loaded messages of one channel.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MessageWindow {
    messages: BTreeMap<MessageId, Message>,
    /// Approximate memory of the messages ([`approx_size`]).
    bytes: usize,
    has_more_before: bool,
    has_newest: bool,
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
    /// appended. `false` after a reconnect or after scrolling far back:
    /// load the newest page again.
    pub fn has_newest(&self) -> bool {
        self.has_newest
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

    /// Merges the newest page.
    fn merge_latest(&mut self, page: BTreeMap<MessageId, Message>, reached_start: bool) {
        let (Some(&p_old), Some(&p_new)) = (page.keys().next(), page.keys().next_back()) else {
            // The channel had no messages when the page was read; keep what
            // arrived live since then.
            if !self.has_newest {
                self.messages.clear();
                self.bytes = 0;
            }
            self.has_more_before = false;
            self.has_newest = true;
            return;
        };
        let fresh = self.has_newest;
        let contiguous = self.messages.contains_key(&p_old);
        let mut old = std::mem::take(&mut self.messages);
        let newer = old.split_off(&MessageId(p_new.0.saturating_add(1)));
        let mut in_range = old.split_off(&p_old);
        let older = old;
        let mut merged = BTreeMap::new();
        if contiguous {
            merged.extend(older);
        }
        for (id, m) in page {
            let keep_live = fresh
                && in_range
                    .get(&id)
                    .is_some_and(|live| edited_ms(live) >= edited_ms(&m));
            let chosen = match in_range.remove(&id) {
                Some(live) if keep_live => live,
                _ => m,
            };
            merged.insert(id, chosen);
        }
        // Messages in the page's range that the page lacks were deleted.
        if fresh {
            merged.extend(newer);
        }
        self.bytes = merged.values().map(approx_size).sum();
        self.messages = merged;
        self.has_more_before = if contiguous {
            self.has_more_before
        } else {
            !reached_start
        };
        self.has_newest = true;
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

    /// A live MESSAGE_CREATE. Returns whether the window changed.
    fn insert_live(&mut self, id: MessageId, m: &Message) -> bool {
        if !self.has_newest || self.was_deleted(id) || approx_size(m) > MAX_MESSAGE_BYTES {
            return false;
        }
        // An old message arriving late would leave a gap below the window.
        if self.has_more_before && self.oldest_id().is_some_and(|o| id < o) {
            return false;
        }
        let mut m = m.clone();
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
        let Some(old) = self.messages.get(&id) else {
            return false;
        };
        if edited_ms(m) < edited_ms(old) || old == m || approx_size(m) > MAX_MESSAGE_BYTES {
            return false;
        }
        let mut m = m.clone();
        carry_reactions(old, &mut m);
        self.put(id, m);
        self.trim_oldest();
        true
    }

    fn delete(&mut self, id: MessageId) -> bool {
        self.remember_deleted(id);
        self.take(id).is_some()
    }

    fn react(&mut self, id: MessageId, f: impl FnOnce(&mut Vec<Reaction>) -> bool) -> bool {
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
    fn value(v: &Value) -> usize {
        match v {
            Value::String(s) => 24 + s.len(),
            Value::Array(a) => 24 + a.iter().map(value).sum::<usize>(),
            Value::Object(o) => {
                48 + o
                    .iter()
                    .map(|(k, v)| 24 + k.len() + value(v))
                    .sum::<usize>()
            }
            _ => 16,
        }
    }
    fn map(o: &serde_json::Map<String, Value>) -> usize {
        o.iter().map(|(k, v)| 24 + k.len() + value(v)).sum()
    }
    fn user(u: &inlet_protocol::User) -> usize {
        128 + u.username.len() + u.global_name.as_ref().map_or(0, String::len) + map(&u.extra)
    }
    fn opt(s: &Option<String>) -> usize {
        s.as_ref().map_or(0, String::len)
    }
    let attachments: usize = m
        .attachments
        .iter()
        .map(|a| {
            160 + a.filename.len()
                + opt(&a.url)
                + opt(&a.proxy_url)
                + opt(&a.description)
                + map(&a.extra)
        })
        .sum();
    256 + m.content.len()
        + m.timestamp.len()
        + user(&m.author)
        + m.mentions.iter().map(user).sum::<usize>()
        + m.embeds.iter().map(value).sum::<usize>()
        + map(&m.extra)
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
        .filter(|m| approx_size(m) <= MAX_MESSAGE_BYTES)
        .filter_map(|m| Some((MessageId::parse(&m.id)?, m.clone())))
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

    /// Merges a REST page. Returns whether the window changed.
    pub(crate) fn apply_page(&mut self, channel: ChannelId, page: &MessagesPage) -> bool {
        let mut map = page_map(channel, &page.messages);
        let reached_start = page.reached_start();
        match page.before {
            None => {
                if !self.windows.contains_key(&channel) {
                    self.windows.insert(channel, MessageWindow::default());
                }
                self.windows.touch(&channel);
                let Some(w) = self.windows.get_mut(&channel) else {
                    return false;
                };
                map.retain(|id, _| !w.was_deleted(*id));
                let before = w.clone();
                w.merge_latest(map, reached_start);
                *w != before
            }
            Some(b) => {
                self.windows.touch(&channel);
                let Some(w) = self.windows.get_mut(&channel) else {
                    return false;
                };
                map.retain(|id, _| !w.was_deleted(*id));
                let before = w.clone();
                w.merge_older(b, map, reached_start) && *w != before
            }
        }
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
    /// but stop appending live messages until the newest page is loaded again.
    pub(crate) fn mark_stale(&mut self) {
        for w in self.windows.values_mut() {
            w.has_newest = false;
        }
    }

    pub(crate) fn clear(&mut self) {
        self.windows.clear();
    }
}
