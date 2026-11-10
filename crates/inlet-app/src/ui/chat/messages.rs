// SPDX-License-Identifier: AGPL-3.0-or-later
//! The message list's data. The open channel's messages live in the account state
//! ([`AccountState`], one per account, on the UI thread); this module turns its window
//! into [`Row`]s (date and unread dividers, messages, system lines, the channel's
//! welcome, messages being sent), keeps the converted messages the rows show, and
//! handles the messages domain's events.
//!
//! The list is updated with the smallest splice between the old and the new rows, so
//! the scroll position holds while messages arrive, change or older pages load above.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::NaiveDate;
use gpui::{App, AppContext, Context, FollowMode, ListScrollEvent, Task, Window};
use inlet_protocol as p;
use inlet_protocol::models::{channel_type, message_flags, message_reference_type, message_type};
use inlet_state::{
    AccountState, AckKind, Changes, ChannelId, GuildId, MessageId, MessagesPage, RoleId, UserId,
};
use serde_json::Value;

use super::time;
use crate::app::Inlet;
use crate::backend::composer;
use crate::backend::messages::parse::{self, SharedNames};
use crate::backend::messages::{Cmd, Ev, Outgoing};
use crate::model::{self, Message, Names};
use crate::ui::markdown::{self, Block};

/// How long a message that was jumped to stays highlighted.
const JUMP_HIGHLIGHT_MS: i64 = 2_000;
/// Shortest time between two requests for older messages from the UI.
pub const OLDER_PAGE_INTERVAL_MS: i64 = 1_000;
/// Newest pages asked for again in a row when live messages overflowed the state's
/// buffer during the load.
const MAX_NEWEST_RETRIES: u8 = 3;

/// One row of the message list.
#[derive(Clone, Debug, PartialEq)]
pub enum Row {
    /// The start of the channel ("Welcome to #general!").
    Welcome(WelcomeRow),
    /// A date separator; `new` when the first unread message is also the first of the day.
    Day {
        day: NaiveDate,
        new: bool,
    },
    /// The unread line with "NEW".
    Unread,
    Message(Box<MessageRow>),
    /// A system message (someone joined, a message was pinned, …) or a type Inlet does
    /// not know.
    System(Box<SystemRow>),
    /// A message of this account that is being sent or could not be sent.
    Outgoing(Box<OutgoingRow>),
}

impl Row {
    pub fn message_id(&self) -> Option<MessageId> {
        match self {
            Row::Message(m) => Some(m.id),
            Row::System(s) => Some(s.id),
            _ => None,
        }
    }
}

/// A converted message, compared by identity: two rows are equal when they show the
/// same conversion.
#[derive(Clone, Debug)]
pub struct Shown(pub Rc<Message>);

impl PartialEq for Shown {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

impl std::ops::Deref for Shown {
    type Target = Message;
    fn deref(&self) -> &Message {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tag {
    Bot,
    System,
    /// The author started the thread.
    Op,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MessageRow {
    pub id: MessageId,
    pub message: Shown,
    /// Same author shortly after the message above: no header.
    pub grouped: bool,
    /// Mentions the account, one of its roles, or everyone.
    pub mention: bool,
    /// Was just jumped to (from a reply).
    pub jumped: bool,
    /// Spoilers are shown.
    pub revealed: bool,
    /// The author's highest coloured role, `0xRRGGBB`.
    pub name_color: Option<u32>,
    pub tag: Option<Tag>,
    pub reply: Option<ReplyRow>,
    /// Time alone, for grouped and compact rows.
    pub clock: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReplyState {
    Loaded {
        author: String,
        snippet: String,
    },
    /// The message replied to was deleted.
    Deleted,
    /// The server did not send it.
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplyRow {
    /// The message replied to, for the jump.
    pub target: Option<MessageId>,
    pub state: ReplyState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SystemKind {
    Join,
    Pin,
    Call,
    Edit,
    RecipientAdd,
    RecipientRemove,
    Thread,
    Follow,
    /// A type Inlet does not know: a neutral line.
    Unknown,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SystemRow {
    pub id: MessageId,
    pub kind: SystemKind,
    pub text: String,
    pub time: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WelcomeRow {
    pub title: String,
    pub subtitle: String,
    /// A direct message (the icon is an @).
    pub private: bool,
}

#[derive(Clone, Debug)]
pub struct OutgoingRow {
    pub nonce: String,
    pub author: String,
    pub blocks: Rc<Vec<Block>>,
    pub failed: Option<String>,
}

impl PartialEq for OutgoingRow {
    fn eq(&self, other: &Self) -> bool {
        self.nonce == other.nonce
            && self.failed == other.failed
            && Rc::ptr_eq(&self.blocks, &other.blocks)
    }
}

/// An entry of the outbox: a message of this account on its way, or one that failed.
pub struct OutboxEntry {
    pub out: Outgoing,
    blocks: Rc<Vec<Block>>,
}

/// The request for older messages from the list: one at a time, at most one a second,
/// and only while older messages exist. Scrolling at the top never fetches in a loop.
#[derive(Default, Debug)]
pub struct OlderPager {
    in_flight: bool,
    last_ms: Option<i64>,
}

impl OlderPager {
    /// Returns `before` for a new request, or `None` when none may start now.
    pub fn request(
        &mut self,
        now_ms: i64,
        has_more_before: bool,
        loading: bool,
        oldest: Option<MessageId>,
    ) -> Option<MessageId> {
        if !has_more_before || loading || self.in_flight {
            return None;
        }
        if self
            .last_ms
            .is_some_and(|last| now_ms.saturating_sub(last) < OLDER_PAGE_INTERVAL_MS)
        {
            return None;
        }
        let before = oldest?;
        self.in_flight = true;
        self.last_ms = Some(now_ms);
        Some(before)
    }

    /// The page arrived or failed.
    pub fn done(&mut self) {
        self.in_flight = false;
    }
}

/// Converted messages of one channel, with the edit time each was converted from.
type Conversions = HashMap<MessageId, (Option<String>, Rc<Message>)>;

/// The message list's view state.
#[derive(Default)]
pub struct MessageView {
    pub rows: Vec<Row>,
    /// Converted messages per channel, with the edit time they were converted from.
    rendered: HashMap<ChannelId, Conversions>,
    outbox: Vec<OutboxEntry>,
    /// The account's read position when the channel was opened: the "NEW" line goes
    /// above the first message after it.
    unread_after: Option<(ChannelId, MessageId)>,
    /// Messages after the read position, and the time of the first, for the banner.
    pub unread_count: usize,
    pub unread_since_ms: i64,
    revealed: HashSet<MessageId>,
    jumped: Option<(MessageId, i64)>,
    pager: OlderPager,
    /// Compact mode: no avatars, the time before the name, no space between groups.
    pub compact: bool,
    /// The open channel's newest page has landed.
    loaded: bool,
    newest_retries: u8,
    ticker: Option<Task<()>>,
    scroll_hooked: bool,
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// The message's time in Unix milliseconds: its timestamp, or the ID's.
pub fn message_ms(m: &p::Message) -> i64 {
    p::models::parse_timestamp_ms(&m.timestamp)
        .or_else(|| MessageId::parse(&m.id).map(MessageId::timestamp_ms))
        .unwrap_or(0)
}

/// Default and reply messages; everything else is a system line.
fn is_regular(kind: i64) -> bool {
    kind == message_type::DEFAULT || kind == message_type::REPLY
}

fn is_reply(m: &p::Message) -> bool {
    m.kind == message_type::REPLY || m.extra.contains_key("referenced_message")
}

/// Fluxer's grouping: `next` goes under `prev`'s header when both are ordinary messages
/// from the same account (and the same webhook name), `next` is not a reply or a
/// published copy, both have the same "silent" flag, they are on the same day and at
/// most seven minutes apart. Grouping is by account ID, never by display name.
pub fn groups_with(prev: &p::Message, prev_ms: i64, next: &p::Message, next_ms: i64) -> bool {
    let silent = |m: &p::Message| m.flag_bits() & message_flags::SUPPRESS_NOTIFICATIONS;
    is_regular(prev.kind)
        && is_regular(next.kind)
        && !is_reply(next)
        && prev.author.id == next.author.id
        && prev.webhook_id() == next.webhook_id()
        && (next.webhook_id().is_none() || prev.author.username == next.author.username)
        && next.flag_bits() & message_flags::IS_CROSSPOST == 0
        && silent(prev) == silent(next)
        && next_ms >= prev_ms
        && next_ms - prev_ms <= model::GROUP_WINDOW_MS
        && time::day(prev_ms) == time::day(next_ms)
}

/// Whether the row is highlighted for the account: it is mentioned, one of its roles
/// is, or everyone is (unless the guild's settings suppress those).
pub fn mentions_me(state: &AccountState, ch: ChannelId, m: &p::Message) -> bool {
    let me = state.me_id();
    if m.mentions.iter().any(|u| UserId::parse(&u.id) == Some(me)) {
        return true;
    }
    let guild = state.channels().guild_of(ch);
    let settings = state.guild_settings().get(guild);
    if m.mention_everyone() && !(guild.is_some() && settings.suppress_everyone) {
        return true;
    }
    let Some(g) = guild else {
        return false;
    };
    if settings.suppress_roles {
        return false;
    }
    let roles = m.mention_roles();
    !roles.is_empty()
        && state
            .guild(g)
            .and_then(|g| g.members.me())
            .is_some_and(|mine| roles.iter().any(|r| mine.roles.contains(r)))
}

/// The colour of the author's highest role that has one.
fn name_color(state: &AccountState, guild: Option<GuildId>, m: &p::Message) -> Option<u32> {
    let g = guild?;
    let gs = state.guild(g)?;
    let author = UserId::parse(&m.author.id)?;
    let attached;
    let roles = match state.member(g, author) {
        Some(member) => &member.roles,
        None => {
            attached = m.member()?;
            &attached.roles
        }
    };
    roles
        .iter()
        .filter_map(|r| RoleId::parse(r))
        .filter_map(|r| gs.roles.get(r))
        .filter(|r| r.color & 0xFF_FF_FF != 0)
        .max_by_key(|r| (r.position, std::cmp::Reverse(RoleId::parse(&r.id))))
        .map(|r| (r.color & 0xFF_FF_FF) as u32)
}

fn tag(channel: Option<&p::Channel>, m: &p::Message, shown: &Message) -> Option<Tag> {
    if m.author.system {
        return Some(Tag::System);
    }
    if shown.author_bot || m.webhook_id().is_some() {
        return Some(Tag::Bot);
    }
    let thread = channel.filter(|c| {
        matches!(
            c.kind,
            channel_type::PUBLIC_THREAD
                | channel_type::PRIVATE_THREAD
                | channel_type::ANNOUNCEMENT_THREAD
        )
    })?;
    (thread.extra.get("owner_id").and_then(Value::as_str) == Some(m.author.id.as_str()))
        .then_some(Tag::Op)
}

fn reply_of(m: &p::Message, shown: &Message) -> Option<ReplyRow> {
    if !is_reply(m) {
        return None;
    }
    let reference = m.message_reference();
    if reference
        .as_ref()
        .is_some_and(|r| r.kind != message_reference_type::DEFAULT)
    {
        return None;
    }
    let target = reference
        .and_then(|r| r.message_id)
        .as_deref()
        .and_then(MessageId::parse);
    let state = match m.extra.get("referenced_message") {
        Some(Value::Null) => ReplyState::Deleted,
        Some(_) => match &shown.reply {
            Some(r) => ReplyState::Loaded {
                author: r.author.clone(),
                snippet: r.snippet.clone(),
            },
            None => ReplyState::Unknown,
        },
        None => ReplyState::Unknown,
    };
    Some(ReplyRow { target, state })
}

fn system_row(id: MessageId, m: &p::Message, shown: &Message) -> SystemRow {
    use message_type::*;
    let name = shown.author.clone();
    let target = m.mentions.first();
    let target_name = target
        .map(|u| model::clean_name(u.display_name()))
        .unwrap_or_default();
    let content = model::snippet(&m.content, 100);
    let (kind, text) = match m.kind {
        USER_JOIN => (SystemKind::Join, t!("system-join", name = name)),
        CHANNEL_PINNED_MESSAGE => (SystemKind::Pin, t!("system-pinned", name = name)),
        RECIPIENT_ADD => (
            SystemKind::RecipientAdd,
            t!("system-recipient-add", name = name, target = target_name),
        ),
        RECIPIENT_REMOVE if target.is_none_or(|u| u.id == m.author.id) => (
            SystemKind::RecipientRemove,
            t!("system-recipient-left", name = name),
        ),
        RECIPIENT_REMOVE => (
            SystemKind::RecipientRemove,
            t!("system-recipient-remove", name = name, target = target_name),
        ),
        CALL => (SystemKind::Call, t!("system-call", name = name)),
        CHANNEL_NAME_CHANGE => (
            SystemKind::Edit,
            t!("system-channel-name", name = name, channel = content),
        ),
        CHANNEL_ICON_CHANGE => (SystemKind::Edit, t!("system-channel-icon", name = name)),
        CHANNEL_FOLLOW_ADD => (
            SystemKind::Follow,
            t!("system-follow-add", name = name, channel = content),
        ),
        THREAD_CREATED => (
            SystemKind::Thread,
            t!("system-thread-created", name = name, thread = content),
        ),
        THREAD_STARTER_MESSAGE => (SystemKind::Thread, t!("system-thread-starter")),
        _ => (SystemKind::Unknown, t!("system-unknown")),
    };
    SystemRow {
        id,
        kind,
        text,
        time: shown.timestamp.clone(),
    }
}

fn welcome_row(state: &AccountState, ch: ChannelId) -> WelcomeRow {
    let channel = state.channel(ch);
    let kind = channel.map(|c| c.kind).unwrap_or(channel_type::GUILD_TEXT);
    let recipients = || {
        channel
            .map(|c| {
                c.recipients
                    .iter()
                    .map(|u| model::clean_name(u.display_name()))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default()
    };
    let name = channel
        .and_then(|c| c.name.as_deref())
        .filter(|n| !n.trim().is_empty())
        .map(model::clean_name);
    match kind {
        channel_type::DM => {
            let who = recipients();
            WelcomeRow {
                title: who.clone(),
                subtitle: t!("welcome-dm", name = who),
                private: true,
            }
        }
        channel_type::GROUP_DM => WelcomeRow {
            title: name.unwrap_or_else(recipients),
            subtitle: t!("welcome-group"),
            private: true,
        },
        _ => {
            let channel = name.unwrap_or_default();
            WelcomeRow {
                title: t!("welcome-channel-title", channel = channel),
                subtitle: t!("welcome-channel", channel = channel),
                private: false,
            }
        }
    }
}

/// Mention names for a message converted on the UI thread (rare: see `shown`).
fn names_for(state: &AccountState, ch: ChannelId, m: &p::Message) -> Names {
    let mut n = Names::default();
    for u in &m.mentions {
        n.users
            .insert(u.id.clone(), model::clean_name(u.display_name()));
    }
    if let Some(g) = state.channels().guild_of(ch) {
        if let Some(gs) = state.guild(g) {
            for r in gs.roles.as_slice() {
                n.roles.insert(r.id.clone(), model::clean_name(&r.name));
            }
        }
        for c in state.guild_channels(g) {
            if let Some(name) = &c.name {
                n.channels.insert(c.id.clone(), model::clean_name(name));
            }
        }
    }
    n
}

/// The smallest change from `old` to `new`: the range of `old` to replace and how many
/// rows replace it. `None` when nothing changed.
pub fn splice_range(old: &[Row], new: &[Row]) -> Option<(Range<usize>, usize)> {
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    if prefix == old.len() && prefix == new.len() {
        return None;
    }
    let room = old.len().min(new.len()) - prefix;
    let suffix = old
        .iter()
        .rev()
        .zip(new.iter().rev())
        .take(room)
        .take_while(|(a, b)| a == b)
        .count();
    Some((prefix..old.len() - suffix, new.len() - prefix - suffix))
}

/// Whether the demo scene asks for compact mode (demo builds only).
fn starts_compact() -> bool {
    #[cfg(feature = "demo")]
    {
        crate::demo::scenes::current() == crate::demo::scenes::Scene::ChatCompact
    }
    #[cfg(not(feature = "demo"))]
    {
        false
    }
}

impl MessageView {
    pub fn new() -> Self {
        Self {
            compact: starts_compact(),
            ..Self::default()
        }
    }

    /// Keeps converted messages that came with an event or a page. A conversion of an
    /// older edit never replaces a newer one, the rule the account state follows too.
    fn remember(&mut self, raw: &[&p::Message], rendered: Vec<Message>) {
        let edits: HashMap<&str, &Option<String>> = raw
            .iter()
            .map(|m| (m.id.as_str(), &m.edited_timestamp))
            .collect();
        for m in rendered {
            let (Some(ch), Some(id)) = (ChannelId::parse(&m.channel_id), MessageId::parse(&m.id))
            else {
                continue;
            };
            let edit = edits
                .get(m.id.as_str())
                .map(|e| (*e).clone())
                .unwrap_or_default();
            let map = self.rendered.entry(ch).or_default();
            match map.get(&id) {
                Some((old, _)) if *old > edit => {}
                _ => {
                    map.insert(id, (edit, Rc::new(m)));
                }
            }
        }
    }

    /// The conversion of `m`; converted here when it is missing or older than the
    /// stored message (an edit that only the state saw).
    fn shown(
        &mut self,
        state: &AccountState,
        ch: ChannelId,
        id: MessageId,
        m: &p::Message,
    ) -> Rc<Message> {
        let map = self.rendered.entry(ch).or_default();
        if let Some((edit, shown)) = map.get(&id) {
            if *edit == m.edited_timestamp {
                return shown.clone();
            }
        }
        let names: SharedNames = Arc::new(Mutex::new(names_for(state, ch, m)));
        let shown = Rc::new(parse::to_message(m, &names));
        map.insert(id, (m.edited_timestamp.clone(), shown.clone()));
        shown
    }

    /// Drops conversions of messages the state no longer holds.
    fn prune(&mut self, state: &AccountState, changes: &Changes) {
        self.rendered.retain(|ch, map| {
            if !changes.messages.contains(ch) {
                return true;
            }
            match state.messages(*ch) {
                // A loading window still has messages on their way in its buffer.
                Some(w) if w.is_loading() => true,
                Some(w) => {
                    map.retain(|id, _| w.get(*id).is_some());
                    true
                }
                None => false,
            }
        });
    }

    /// Builds the rows of channel `ch` from the account state.
    pub fn build_rows(&mut self, state: &AccountState, ch: ChannelId) -> Vec<Row> {
        let mut rows = Vec::new();
        self.unread_count = 0;
        self.unread_since_ms = 0;
        if self.jumped.is_some_and(|(_, until)| until <= now_ms()) {
            self.jumped = None;
        }
        if let Some(window) = state.messages(ch) {
            if !window.has_more_before() && !window.is_loading() {
                rows.push(Row::Welcome(welcome_row(state, ch)));
            }
            let channel = state.channel(ch);
            let guild = state.channels().guild_of(ch);
            let unread_after = self
                .unread_after
                .filter(|(c, _)| *c == ch)
                .map(|(_, id)| id);
            let mut prev: Option<(&p::Message, i64, Option<NaiveDate>)> = None;
            for m in window.iter() {
                let Some(id) = MessageId::parse(&m.id) else {
                    continue;
                };
                let shown = self.shown(state, ch, id, m);
                let ms = if shown.timestamp_ms > 0 {
                    shown.timestamp_ms
                } else {
                    message_ms(m)
                };
                let day = time::day(ms);
                let unread = unread_after.is_some_and(|a| id > a);
                let first_unread = unread && self.unread_count == 0;
                if unread {
                    if first_unread {
                        self.unread_since_ms = ms;
                    }
                    self.unread_count += 1;
                }
                let new_day = day.is_some() && prev.map(|p| p.2) != Some(day);
                match day {
                    Some(day) if new_day => rows.push(Row::Day {
                        day,
                        new: first_unread,
                    }),
                    _ if first_unread => rows.push(Row::Unread),
                    _ => {}
                }
                if !is_regular(m.kind) {
                    rows.push(Row::System(Box::new(system_row(id, m, &shown))));
                    prev = Some((m, ms, day));
                    continue;
                }
                let grouped =
                    !first_unread && prev.is_some_and(|(pm, pms, _)| groups_with(pm, pms, m, ms));
                rows.push(Row::Message(Box::new(MessageRow {
                    id,
                    grouped,
                    mention: mentions_me(state, ch, m),
                    jumped: self.jumped.is_some_and(|(j, _)| j == id),
                    revealed: self.revealed.contains(&id),
                    name_color: name_color(state, guild, m),
                    tag: tag(channel, m, &shown),
                    reply: reply_of(m, &shown),
                    clock: time::clock(ms),
                    message: Shown(shown),
                })));
                prev = Some((m, ms, day));
            }
        }
        let me = model::clean_name(state.me().display_name());
        for o in &self.outbox {
            if ChannelId::parse(&o.out.channel_id) == Some(ch) {
                rows.push(Row::Outgoing(Box::new(OutgoingRow {
                    nonce: o.out.nonce.clone(),
                    author: me.clone(),
                    blocks: o.blocks.clone(),
                    failed: o.out.failed.clone(),
                })));
            }
        }
        rows
    }

    /// Adds or updates an outbox entry.
    fn upsert_outgoing(&mut self, out: Outgoing) {
        match self.outbox.iter_mut().find(|o| o.out.nonce == out.nonce) {
            Some(entry) => entry.out = out,
            None => {
                let blocks = Rc::new(markdown::parse(&out.text));
                self.outbox.push(OutboxEntry { out, blocks });
            }
        }
    }
}

impl Inlet {
    pub(crate) fn on_messages_event(
        &mut self,
        event: Ev,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            Ev::Guilds(guilds) => self.on_guilds(guilds, cx),
            Ev::Channels { guild_id, channels } => self.on_channels(guild_id, channels, window, cx),
            Ev::Dispatch { dispatch, rendered } => self.on_dispatch(*dispatch, rendered, cx),
            Ev::Page {
                channel_id,
                page,
                rendered,
            } => self.on_page(channel_id, page, rendered, cx),
            Ev::PageFailed { channel_id, older } => {
                if older {
                    self.chat.view.pager.done();
                } else if let (Some(ch), Some(account)) =
                    (ChannelId::parse(&channel_id), self.account.clone())
                {
                    account.update(cx, |s, _| s.cancel_load(ch));
                }
            }
            Ev::MessageCreated(message) => self.chat.view.remember(&[], vec![message]),
            Ev::Outgoing(out) => {
                self.chat.view.upsert_outgoing(out);
                self.refresh_rows(cx);
            }
        }
    }

    fn open_channel_id(&self) -> Option<ChannelId> {
        self.selected_channel.as_deref().and_then(ChannelId::parse)
    }

    fn on_dispatch(
        &mut self,
        dispatch: p::Dispatch,
        rendered: Vec<Message>,
        cx: &mut Context<Self>,
    ) {
        if let p::Dispatch::Ready(ready) = &dispatch {
            match self.account.clone() {
                None => {
                    let state = AccountState::from_ready_at(ready, now_ms());
                    self.account = Some(cx.new(|_| state));
                    self.start_ticker(cx);
                }
                Some(account) => {
                    account.update(cx, |s, _| s.apply(&dispatch));
                }
            }
            // A first READY, or a new one after a reconnect: the open channel's window is
            // missing or stale, so load its newest page.
            self.chat.view.newest_retries = 0;
            self.load_open_channel(cx);
            return;
        }
        let Some(account) = self.account.clone() else {
            return;
        };
        let raw = match &dispatch {
            p::Dispatch::MessageCreate(m) | p::Dispatch::MessageUpdate(m) => vec![&**m],
            _ => Vec::new(),
        };
        self.chat.view.remember(&raw, rendered);
        let mut changes = account.update(cx, |s, _| s.apply(&dispatch));
        if let p::Dispatch::MessageCreate(m) = &dispatch {
            if let Some(nonce) = &m.nonce {
                let before = self.chat.view.outbox.len();
                self.chat.view.outbox.retain(|o| &o.out.nonce != nonce);
                if self.chat.view.outbox.len() != before {
                    self.refresh_rows(cx);
                }
            }
            // A message in the open channel while it shows the newest messages: read.
            let open = self.open_channel_id();
            let at_bottom = self.chat.message_list.is_following_tail() && self.chat.view.loaded;
            if let (true, Some(ch)) = (at_bottom, open) {
                if ChannelId::parse(&m.channel_id) == Some(ch) {
                    changes.merge(
                        account.update(cx, |s, _| s.mark_read(ch, None, AckKind::Automatic)),
                    );
                }
            }
        }
        self.after_changes(changes, cx);
    }

    fn on_page(
        &mut self,
        channel_id: String,
        page: MessagesPage,
        rendered: Vec<Message>,
        cx: &mut Context<Self>,
    ) {
        let (Some(ch), Some(account)) = (ChannelId::parse(&channel_id), self.account.clone())
        else {
            return;
        };
        let older = page.before.is_some();
        {
            let raw: Vec<&p::Message> = page.messages.iter().collect();
            self.chat.view.remember(&raw, rendered);
        }
        let mut changes = account.update(cx, |s, _| s.apply_messages(ch, page));
        if older {
            self.chat.view.pager.done();
        } else if self.open_channel_id() == Some(ch) {
            self.chat.view.loaded = true;
            // Opened and shown: the channel is read.
            changes.merge(account.update(cx, |s, _| s.mark_read(ch, None, AckKind::Explicit)));
            // Too many live messages arrived during the load: ask again (a few times).
            let incomplete = account
                .read(cx)
                .messages(ch)
                .is_some_and(|w| !w.has_newest() && !w.is_loading());
            if incomplete && self.chat.view.newest_retries < MAX_NEWEST_RETRIES {
                self.chat.view.newest_retries += 1;
                self.load_open_channel(cx);
            }
        }
        self.after_changes(changes, cx);
    }

    /// Re-renders what changed in the account state.
    fn after_changes(&mut self, changes: Changes, cx: &mut Context<Self>) {
        let Some(account) = self.account.clone() else {
            return;
        };
        self.chat.view.prune(account.read(cx), &changes);
        let Some(ch) = self.open_channel_id() else {
            return;
        };
        let guild = account.read(cx).channels().guild_of(ch);
        let relevant = changes.reset
            || changes.messages.contains(&ch)
            || changes.guilds
            || guild.is_some_and(|g| changes.member_guilds.contains(&g));
        if relevant {
            self.refresh_rows(cx);
        }
    }

    /// Rebuilds the rows of the open channel and splices the list.
    pub(crate) fn refresh_rows(&mut self, cx: &mut Context<Self>) {
        let new = match (self.open_channel_id(), self.account.clone()) {
            (Some(ch), Some(account)) => self.chat.view.build_rows(account.read(cx), ch),
            _ => Vec::new(),
        };
        let list = &self.chat.message_list;
        if self.chat.view.rows.is_empty() {
            list.reset(new.len());
            list.set_follow_mode(FollowMode::Tail);
        } else if let Some((range, count)) = splice_range(&self.chat.view.rows, &new) {
            list.splice(range, count);
        }
        self.chat.view.rows = new;
    }

    /// Shows a newly selected channel: an empty list until its messages are there.
    pub(crate) fn open_channel_messages(&mut self, cx: &mut Context<Self>) {
        self.clear_messages();
        self.load_open_channel(cx);
    }

    /// Empties the list (another guild or channel is about to show).
    pub(crate) fn clear_messages(&mut self) {
        let view = &mut self.chat.view;
        view.rows.clear();
        view.unread_after = None;
        view.revealed.clear();
        view.jumped = None;
        view.pager = OlderPager::default();
        view.loaded = false;
        view.newest_retries = 0;
        self.chat.message_list.reset(0);
        self.chat.message_list.set_follow_mode(FollowMode::Tail);
    }

    /// Signed out: no messages, no account state, nothing on its way.
    pub(crate) fn reset_messages(&mut self) {
        self.clear_messages();
        self.account = None;
        let compact = self.chat.view.compact;
        self.chat.view = MessageView {
            compact,
            ..MessageView::default()
        };
    }

    /// Opens the open channel's window in the account state and asks for its newest
    /// page. Before READY there is no state yet; READY loads the channel then.
    fn load_open_channel(&mut self, cx: &mut Context<Self>) {
        let (Some(channel_id), Some(account)) =
            (self.selected_channel.clone(), self.account.clone())
        else {
            return;
        };
        let Some(ch) = ChannelId::parse(&channel_id) else {
            return;
        };
        let (marker, changes) = account.update(cx, |s, _| {
            let marker = s
                .unread(ch)
                .unread
                .then(|| s.read_state(ch).and_then(|r| r.ack_message_id))
                .flatten();
            let changes = s.begin_load(ch);
            s.touch_channel(ch);
            (marker, changes)
        });
        let view = &mut self.chat.view;
        if view.unread_after.is_none_or(|(c, _)| c != ch) {
            view.unread_after = marker.map(|m| (ch, m));
        }
        view.loaded = false;
        self.backend.send(Cmd::OpenChannel { channel_id });
        self.after_changes(changes, cx);
        // An existing window shows right away; the page merges into it.
        self.refresh_rows(cx);
    }

    /// The list was scrolled to its top: ask for older messages, within the budget.
    pub(crate) fn load_older(&mut self, cx: &mut Context<Self>) {
        let (Some(channel_id), Some(ch), Some(account)) = (
            self.selected_channel.clone(),
            self.open_channel_id(),
            self.account.clone(),
        ) else {
            return;
        };
        let (has_more, loading, oldest) = account
            .read(cx)
            .messages(ch)
            .map(|w| (w.has_more_before(), w.is_loading(), w.oldest_id()))
            .unwrap_or((false, true, None));
        let view = &mut self.chat.view;
        let loaded = view.loaded;
        if let Some(before) = view
            .pager
            .request(now_ms(), has_more && loaded, loading, oldest)
        {
            self.backend.send(Cmd::LoadOlder {
                channel_id,
                before: before.to_string(),
            });
        }
    }

    /// Calls `load_older` when the person scrolls near the top of the list. The list
    /// reports only scrolling by the person, so loading never repeats by itself.
    pub(crate) fn hook_list_scroll(&mut self, cx: &mut Context<Self>) {
        if self.chat.view.scroll_hooked {
            return;
        }
        self.chat.view.scroll_hooked = true;
        let weak = cx.entity().downgrade();
        self.chat.message_list.set_scroll_handler(
            move |event: &ListScrollEvent, _window: &mut Window, cx: &mut App| {
                if event.visible_range.start <= 1 {
                    let _ = weak.update(cx, |this, cx| this.load_older(cx));
                }
            },
        );
    }

    /// Ticks the account state about once a second (typing, timeouts, load timeouts).
    fn start_ticker(&mut self, cx: &mut Context<Self>) {
        self.chat.view.ticker = Some(cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            let alive = this.update(cx, |this, cx| this.tick_account(cx));
            if alive.is_err() {
                break;
            }
        }));
    }

    fn tick_account(&mut self, cx: &mut Context<Self>) {
        let Some(account) = self.account.clone() else {
            return;
        };
        let changes = account.update(cx, |s, _| s.tick(now_ms()));
        let jump_over = self
            .chat
            .view
            .jumped
            .is_some_and(|(_, until)| until <= now_ms());
        if jump_over {
            self.refresh_rows(cx);
        }
        if !changes.is_empty() || jump_over {
            self.after_changes(changes, cx);
            cx.notify();
        }
    }

    /// Scrolls to a message on screen and highlights it for a moment.
    pub(crate) fn jump_to_message(&mut self, id: MessageId, cx: &mut Context<Self>) {
        match self
            .chat
            .view
            .rows
            .iter()
            .position(|r| r.message_id() == Some(id))
        {
            Some(ix) => {
                self.chat.view.jumped = Some((id, now_ms() + JUMP_HIGHLIGHT_MS));
                self.refresh_rows(cx);
                self.chat.message_list.pause_following_tail();
                self.chat.message_list.scroll_to_reveal_item(ix);
            }
            None => self.chat.notice = Some(t!("chat-reply-not-loaded")),
        }
        cx.notify();
    }

    /// Back to the newest messages, following new ones again.
    pub(crate) fn jump_to_present(&mut self, cx: &mut Context<Self>) {
        self.chat.message_list.scroll_to_end();
        self.chat.message_list.set_follow_mode(FollowMode::Tail);
        cx.notify();
    }

    /// "Mark as read" in the new-messages banner: the "NEW" line goes too.
    pub(crate) fn mark_open_channel_read(&mut self, cx: &mut Context<Self>) {
        self.chat.view.unread_after = None;
        if let (Some(ch), Some(account)) = (self.open_channel_id(), self.account.clone()) {
            let changes = account.update(cx, |s, _| s.mark_read(ch, None, AckKind::Explicit));
            self.after_changes(changes, cx);
        }
        self.refresh_rows(cx);
        cx.notify();
    }

    /// Shows the spoilers of a message.
    pub(crate) fn reveal(&mut self, id: MessageId, cx: &mut Context<Self>) {
        self.chat.view.revealed.insert(id);
        self.refresh_rows(cx);
        cx.notify();
    }

    /// "Show more": the whole text of a long message, parsed on demand.
    pub(crate) fn expand(&mut self, id: MessageId, cx: &mut Context<Self>) {
        let Some(ch) = self.open_channel_id() else {
            return;
        };
        if let Some((_, shown)) = self
            .chat
            .view
            .rendered
            .get_mut(&ch)
            .and_then(|m| m.get_mut(&id))
        {
            let mut full = (**shown).clone();
            full.blocks = markdown::parse(&full.content);
            if full.edited {
                markdown::mark_edited(&mut full.blocks);
            }
            full.truncated = false;
            *shown = Rc::new(full);
        }
        self.refresh_rows(cx);
        cx.notify();
    }

    /// Sends a failed message again.
    pub(crate) fn retry_outgoing(&mut self, nonce: &str, cx: &mut Context<Self>) {
        let view = &mut self.chat.view;
        if let Some(ix) = view.outbox.iter().position(|o| o.out.nonce == nonce) {
            let entry = view.outbox.remove(ix);
            self.backend.send(composer::Cmd::Send {
                channel_id: entry.out.channel_id,
                text: entry.out.text,
            });
            self.refresh_rows(cx);
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;

    fn local_ms(h: u32, min: u32, day: u32) -> i64 {
        chrono::Local
            .with_ymd_and_hms(2026, 10, day, h, min, 0)
            .earliest()
            .unwrap()
            .timestamp_millis()
    }

    fn raw(id: u64, author: &str, extra: Value) -> p::Message {
        let mut v = json!({
            "id": id.to_string(),
            "channel_id": "100",
            "author": {"id": author, "username": format!("u{author}")},
            "type": 0,
            "content": "hi",
            "timestamp": "2026-10-01T12:00:00Z"
        });
        if let (Some(obj), Some(more)) = (v.as_object_mut(), extra.as_object()) {
            for (k, val) in more {
                obj.insert(k.clone(), val.clone());
            }
        }
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn grouping_breaks_on_reply_author_day_and_seven_minutes() {
        let t0 = local_ms(12, 0, 1);
        let a = raw(1, "7", json!({}));
        let same = raw(2, "7", json!({}));
        assert!(groups_with(&a, t0, &same, t0 + 60_000));
        assert!(groups_with(&a, t0, &same, t0 + model::GROUP_WINDOW_MS));
        // Seven minutes is the edge.
        assert!(!groups_with(&a, t0, &same, t0 + model::GROUP_WINDOW_MS + 1));
        // Another account with the same display name.
        let other = raw(3, "8", json!({}));
        assert!(!groups_with(&a, t0, &other, t0 + 1_000));
        // A reply always starts a group.
        let reply = raw(
            4,
            "7",
            json!({"type": 19, "message_reference": {"message_id": "1"}, "referenced_message": null}),
        );
        assert!(!groups_with(&a, t0, &reply, t0 + 1_000));
        // Midnight between them.
        let late = local_ms(23, 58, 1);
        assert!(!groups_with(&a, late, &same, late + 4 * 60_000));
        // System messages neither group nor are grouped under.
        let join = raw(5, "7", json!({"type": 7}));
        assert!(!groups_with(&a, t0, &join, t0 + 1_000));
        assert!(!groups_with(&join, t0, &same, t0 + 1_000));
        // Webhooks group only under the same name.
        let hook_a = raw(6, "9", json!({"webhook_id": "50"}));
        let mut hook_b = raw(7, "9", json!({"webhook_id": "50"}));
        assert!(groups_with(&hook_a, t0, &hook_b, t0 + 1_000));
        hook_b.author.username = "other".into();
        assert!(!groups_with(&hook_a, t0, &hook_b, t0 + 1_000));
        // Out of order never groups.
        assert!(!groups_with(&a, t0, &same, t0 - 1));
    }

    fn state_with_roles(my_roles: &[&str], settings: Value) -> AccountState {
        let ready: p::Ready = serde_json::from_value(json!({
            "session_id": "s",
            "user": {"id": "10", "username": "me"},
            "guilds": [{
                "id": "20",
                "properties": {"id": "20", "name": "G", "owner_id": "99"},
                "channels": [{"id": "100", "type": 0, "guild_id": "20", "name": "general"}],
                "roles": [
                    {"id": "20", "name": "@everyone", "permissions": "1024", "position": 0},
                    {"id": "30", "name": "Mods", "color": 0xFF0000, "position": 2},
                    {"id": "31", "name": "Other", "color": 0x00FF00, "position": 1}
                ],
                "members": [{"user": {"id": "10"}, "roles": my_roles, "joined_at": "2026-01-01T00:00:00Z"}]
            }],
            "user_guild_settings": settings,
        }))
        .unwrap();
        AccountState::from_ready_at(&ready, 1)
    }

    fn page(state: &mut AccountState, ch: ChannelId, msgs: Vec<p::Message>) {
        let _ = state.begin_load(ch);
        let _ = state.apply_messages(ch, MessagesPage::latest(msgs, 50));
    }

    #[test]
    fn mention_of_my_role_highlights_the_row() {
        let ch = ChannelId(100);
        let mut state = state_with_roles(&["30"], json!([]));
        let role_ping = raw(1, "7", json!({"mention_roles": ["30"]}));
        let other_role = raw(2, "7", json!({"mention_roles": ["31"]}));
        let me = raw(
            3,
            "7",
            json!({"mentions": [{"id": "10", "username": "me"}]}),
        );
        let everyone = raw(4, "7", json!({"mention_everyone": true}));
        let plain = raw(5, "7", json!({}));
        assert!(mentions_me(&state, ch, &role_ping));
        assert!(!mentions_me(&state, ch, &other_role));
        assert!(mentions_me(&state, ch, &me));
        assert!(mentions_me(&state, ch, &everyone));
        assert!(!mentions_me(&state, ch, &plain));

        // In a window, the row carries the highlight.
        page(&mut state, ch, vec![role_ping, plain]);
        let mut view = MessageView::default();
        let flags: Vec<bool> = view
            .build_rows(&state, ch)
            .iter()
            .filter_map(|r| match r {
                Row::Message(m) => Some(m.mention),
                _ => None,
            })
            .collect();
        assert_eq!(flags, [true, false]);

        // The guild's "suppress role mentions" turns it off.
        let muted = state_with_roles(&["30"], json!([{"guild_id": "20", "suppress_roles": true}]));
        let ping = raw(1, "7", json!({"mention_roles": ["30"]}));
        assert!(!mentions_me(&muted, ch, &ping));
    }

    #[test]
    fn names_take_the_highest_coloured_role() {
        let state = state_with_roles(&[], json!([]));
        let m = raw(1, "7", json!({"member": {"roles": ["31", "30"]}}));
        assert_eq!(name_color(&state, Some(GuildId(20)), &m), Some(0xFF0000));
        let none = raw(2, "7", json!({}));
        assert_eq!(name_color(&state, Some(GuildId(20)), &none), None);
    }

    #[test]
    fn unknown_message_type_renders_a_neutral_line() {
        let ch = ChannelId(100);
        let mut state = state_with_roles(&[], json!([]));
        let unknown = raw(1, "7", json!({"type": 4242, "content": "<script>"}));
        let join = raw(2, "7", json!({"type": 7}));
        page(&mut state, ch, vec![unknown, join]);
        let mut view = MessageView::default();
        let rows = view.build_rows(&state, ch);
        let systems: Vec<&SystemRow> = rows
            .iter()
            .filter_map(|r| match r {
                Row::System(s) => Some(&**s),
                _ => None,
            })
            .collect();
        assert_eq!(systems.len(), 2);
        assert_eq!(systems[0].kind, SystemKind::Unknown);
        assert_eq!(systems[0].text, t!("system-unknown"));
        assert!(!systems[0].text.contains("<script>"));
        assert_eq!(systems[1].kind, SystemKind::Join);
        assert!(systems[1].text.contains("u7"));
        // The page reached the channel's start: the welcome comes first.
        assert!(matches!(rows[0], Row::Welcome(_)));
    }

    #[test]
    fn the_new_line_goes_above_the_first_unread_message() {
        let ch = ChannelId(100);
        let mut state = state_with_roles(&[], json!([]));
        page(
            &mut state,
            ch,
            (1..=4).map(|i| raw(i, "7", json!({}))).collect(),
        );
        let mut view = MessageView {
            unread_after: Some((ch, MessageId(2))),
            ..MessageView::default()
        };
        let rows = view.build_rows(&state, ch);
        let at = rows
            .iter()
            .position(|r| *r == Row::Unread)
            .expect("a NEW line");
        assert_eq!(rows[at + 1].message_id(), Some(MessageId(3)));
        // The message after the line shows its header again.
        match &rows[at + 1] {
            Row::Message(m) => assert!(!m.grouped),
            other => panic!("{other:?}"),
        }
        assert_eq!(view.unread_count, 2);
        // Building again gives equal rows: nothing to splice.
        let again = view.build_rows(&state, ch);
        assert_eq!(splice_range(&rows, &again), None);
    }

    #[test]
    fn splices_are_minimal() {
        let a = Row::Unread;
        let day = |d| Row::Day {
            day: NaiveDate::from_ymd_opt(2026, 10, d).unwrap(),
            new: false,
        };
        assert_eq!(
            splice_range(&[day(1), a.clone()], &[day(1), a.clone()]),
            None
        );
        // Appended at the end.
        assert_eq!(
            splice_range(&[day(1)], &[day(1), a.clone()]),
            Some((1..1, 1))
        );
        // Prepended (an older page).
        assert_eq!(splice_range(&[day(2)], &[day(1), day(2)]), Some((0..0, 1)));
        // One changed in the middle.
        assert_eq!(
            splice_range(&[day(1), day(2), day(3)], &[day(1), a.clone(), day(3)]),
            Some((1..2, 1))
        );
        // Removed.
        assert_eq!(
            splice_range(&[day(1), a.clone()], &[day(1)]),
            Some((1..2, 0))
        );
        assert_eq!(splice_range(&[], &[a]), Some((0..0, 1)));
    }

    #[test]
    fn scrolling_up_fetches_at_most_one_page_per_second() {
        // Scroll events every 16 ms for ten seconds at the top of a long channel; every
        // page answers at once.
        let mut pager = OlderPager::default();
        let mut requests = 0;
        for i in 0..625i64 {
            if pager
                .request(i * 16, true, false, Some(MessageId(1_000)))
                .is_some()
            {
                requests += 1;
                pager.done();
            }
        }
        assert!((10..=11).contains(&requests), "{requests} requests");

        // A slow server: one request at a time.
        let mut pager = OlderPager::default();
        assert!(pager.request(0, true, false, Some(MessageId(5))).is_some());
        assert!(pager
            .request(5_000, true, false, Some(MessageId(5)))
            .is_none());
        pager.done();
        assert!(pager
            .request(5_000, true, false, Some(MessageId(5)))
            .is_some());

        // At the start of the channel, or while the newest page loads: never.
        let mut pager = OlderPager::default();
        for i in 0..1_000i64 {
            let now = i * 1_000;
            assert!(pager
                .request(now, false, false, Some(MessageId(5)))
                .is_none());
            assert!(pager.request(now, true, true, Some(MessageId(5))).is_none());
        }
    }
}
