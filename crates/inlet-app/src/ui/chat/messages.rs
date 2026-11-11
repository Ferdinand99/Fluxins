// SPDX-License-Identifier: AGPL-3.0-or-later
//! The message list's data. The open channel's messages live in the account state
//! ([`AccountState`], one per account, on the UI thread); this module turns its window
//! into [`Row`]s (date and unread dividers, messages, system lines, the channel's
//! welcome, messages being sent), keeps the converted messages the rows show, and
//! handles the messages domain's events.
//!
//! The list is updated with the smallest splice between the old and the new rows, so
//! the scroll position holds while messages arrive, change or older pages load above.
//!
//! Loads are paired: every `begin_load` gets exactly one `apply_messages` or
//! `cancel_load`, because the backend answers every page request exactly once (see
//! `backend::messages::fetch::PageAnswer`) and requests carry an ID, so an answer to a
//! request that a newer one replaced is recognised.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Range;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::NaiveDate;
use gpui::{App, AppContext, Context, FollowMode, ListScrollEvent, SharedString, Task, Window};
use inlet_protocol as p;
use inlet_protocol::models::{channel_type, message_flags, message_reference_type, message_type};
use inlet_state::{
    limits, AccountState, AckKind, Changes, ChannelId, GuildId, MessageId, MessagesPage, RoleId,
    UserId,
};
use serde_json::Value;

use super::action_bar::RowMeta;
use super::time;
use crate::app::Inlet;
use crate::backend::messages::outbox::{is_own_copy, Sent};
use crate::backend::messages::parse::{self, SharedNames};
use crate::backend::messages::{Cmd, Ev, Pending};
use crate::model::{self, Message, Names};
use crate::ui::markdown::{self, Block};

/// How long a message that was jumped to stays highlighted (Fluxer's highlight
/// transition, `--message-transition-highlight`).
pub const JUMP_HIGHLIGHT_MS: i64 = 320;
/// Shortest time between two requests for older messages from the UI.
pub const OLDER_PAGE_INTERVAL_MS: i64 = 1_000;
/// Shortest time between two reloads of the newest page that the person asks for
/// ("Jump to present", reaching the bottom of a list that lost its newest messages).
const NEWEST_RELOAD_INTERVAL_MS: i64 = 1_000;
/// The new-messages banner says "50+" past this many.
const BANNER_MAX_COUNTED: usize = 50;
/// Newest pages asked for again in a row when live messages overflowed the state's
/// buffer during the load.
const MAX_NEWEST_RETRIES: u8 = 3;
/// Channels whose converted messages are kept (the open one and the last few opened),
/// while they have a message window.
pub const RECENT_CHANNELS: usize = 5;
/// Converted messages kept per channel: a full window and a full live buffer.
pub const MAX_CONVERSIONS_PER_CHANNEL: usize =
    limits::MAX_MESSAGES_PER_CHANNEL + limits::MAX_PENDING_LIVE;

/// One row of the message list.
#[derive(Clone, Debug, PartialEq)]
pub enum Row {
    /// The start of the channel ("Welcome to #general!").
    Welcome(Rc<WelcomeRow>),
    /// A date separator; `new` when the first unread message is also the first of the day.
    Day {
        day: NaiveDate,
        new: bool,
    },
    /// The unread line with "NEW".
    Unread,
    Message(Rc<MessageRow>),
    /// A system message (someone joined, a message was pinned, …) or a type Inlet does
    /// not know.
    System(Rc<SystemRow>),
    /// A message of this account that is being sent or could not be sent.
    Pending(Rc<PendingRow>),
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
    pub clock: SharedString,
    /// What the message actions need: own, pinned, hidden embeds, reactions.
    pub meta: RowMeta,
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
pub struct PendingRow {
    pub nonce: String,
    pub author: String,
    pub blocks: Rc<Vec<Block>>,
    pub failed: Option<String>,
    /// Under an own message or another pending one: no header.
    pub grouped: bool,
}

impl PartialEq for PendingRow {
    fn eq(&self, other: &Self) -> bool {
        self.nonce == other.nonce
            && self.failed == other.failed
            && self.grouped == other.grouped
            && Rc::ptr_eq(&self.blocks, &other.blocks)
    }
}

/// An entry of the outbox: a message of this account on its way, or one that failed.
pub struct OutboxEntry {
    pub out: Pending,
    blocks: Rc<Vec<Block>>,
    /// When the row was first shown, in Unix milliseconds (the send's time).
    queued_ms: u64,
}

/// The request for older messages from the list: one at a time, at most one a second,
/// and only while older messages exist. Scrolling at the top never fetches in a loop.
#[derive(Default, Debug)]
pub struct OlderPager {
    /// The request waiting for its answer.
    in_flight: Option<u64>,
    last_ms: Option<i64>,
}

impl OlderPager {
    /// Returns `before` for request `request`, or `None` when none may start now.
    pub fn request(
        &mut self,
        now_ms: i64,
        has_more_before: bool,
        loading: bool,
        oldest: Option<MessageId>,
        request: u64,
    ) -> Option<MessageId> {
        if !has_more_before || loading || self.in_flight.is_some() {
            return None;
        }
        if self
            .last_ms
            .is_some_and(|last| now_ms.saturating_sub(last) < OLDER_PAGE_INTERVAL_MS)
        {
            return None;
        }
        let before = oldest?;
        self.in_flight = Some(request);
        self.last_ms = Some(now_ms);
        Some(before)
    }

    /// The answer to `request` came (a page or a failure). Answers to other requests
    /// change nothing.
    pub fn done(&mut self, request: u64) {
        if self.in_flight == Some(request) {
            self.in_flight = None;
        }
    }
}

/// The edit time of each raw message, by ID, that came with a batch of conversions.
type Edits = HashMap<String, Option<String>>;

fn edits_of<'a>(raw: impl IntoIterator<Item = &'a p::Message>) -> Edits {
    raw.into_iter()
        .map(|m| (m.id.clone(), m.edited_timestamp.clone()))
        .collect()
}

/// A converted message and what the rows need of it, worked out once.
#[derive(Clone, Debug)]
struct Converted {
    /// The edit time it was converted from.
    edit: Option<String>,
    shown: Rc<Message>,
    ms: i64,
    day: Option<NaiveDate>,
    clock: SharedString,
}

impl Converted {
    fn new(edit: Option<String>, m: Message) -> Self {
        let ms = if m.timestamp_ms > 0 {
            m.timestamp_ms
        } else {
            MessageId::parse(&m.id).map_or(0, MessageId::timestamp_ms)
        };
        Converted {
            edit,
            shown: Rc::new(m),
            ms,
            day: time::day(ms),
            clock: time::clock(ms).into(),
        }
    }
}

/// The converted messages of the open channel and the few opened before it, only
/// while the account state holds a window for them. Bounded by
/// [`RECENT_CHANNELS`] × [`MAX_CONVERSIONS_PER_CHANNEL`]; everything else the backend
/// converts (live messages in other channels) is dropped at once.
#[derive(Default)]
struct Conversions {
    by_channel: HashMap<ChannelId, HashMap<MessageId, Converted>>,
    /// Most recently opened first.
    recent: VecDeque<ChannelId>,
}

impl Conversions {
    /// The channel was opened: it is kept first.
    fn touch(&mut self, ch: ChannelId) {
        self.recent.retain(|c| *c != ch);
        self.recent.push_front(ch);
        while self.recent.len() > RECENT_CHANNELS {
            if let Some(old) = self.recent.pop_back() {
                self.by_channel.remove(&old);
            }
        }
    }

    fn keeps(&self, state: &AccountState, ch: ChannelId) -> bool {
        self.recent.contains(&ch) && state.messages(ch).is_some()
    }

    /// Keeps the conversions of messages the state holds (or buffers while a window
    /// loads or misses its newest messages), after the state took the event or page.
    /// A conversion of an older edit never replaces a newer one.
    fn remember(&mut self, state: &AccountState, edits: &Edits, rendered: Vec<Message>) {
        for m in rendered {
            let (Some(ch), Some(id)) = (ChannelId::parse(&m.channel_id), MessageId::parse(&m.id))
            else {
                continue;
            };
            if !self.keeps(state, ch) {
                continue;
            }
            let Some(w) = state.messages(ch) else {
                continue;
            };
            let buffering = w.is_loading() || !w.has_newest();
            if w.get(id).is_none() && !buffering {
                continue;
            }
            let edit = edits.get(&m.id).cloned().flatten();
            let map = self.by_channel.entry(ch).or_default();
            if map.get(&id).is_some_and(|old| old.edit > edit) {
                continue;
            }
            map.insert(id, Converted::new(edit, m));
            if map.len() > MAX_CONVERSIONS_PER_CHANNEL {
                map.retain(|id, _| w.get(*id).is_some());
            }
        }
    }

    /// Drops conversions the state no longer needs.
    fn prune(&mut self, state: &AccountState, changes: &Changes) {
        let recent = &self.recent;
        self.by_channel.retain(|ch, map| {
            let Some(w) = state.messages(*ch).filter(|_| recent.contains(ch)) else {
                return false;
            };
            if changes.messages.contains(ch) && !w.is_loading() && w.has_newest() {
                map.retain(|id, _| w.get(*id).is_some());
            }
            true
        });
    }

    /// The conversion of `m`; converted here when it is missing or older than the
    /// stored message (an edit only the state saw, or a channel opened long ago).
    fn get(
        &mut self,
        state: &AccountState,
        ch: ChannelId,
        id: MessageId,
        m: &p::Message,
    ) -> Converted {
        let map = self.by_channel.entry(ch).or_default();
        if let Some(c) = map.get(&id) {
            if c.edit == m.edited_timestamp {
                return c.clone();
            }
        }
        let names: SharedNames = Arc::new(Mutex::new(names_for(state, ch, m)));
        let c = Converted::new(m.edited_timestamp.clone(), parse::to_message(m, &names));
        map.insert(id, c.clone());
        c
    }

    fn get_mut(&mut self, ch: ChannelId, id: MessageId) -> Option<&mut Converted> {
        self.by_channel.get_mut(&ch)?.get_mut(&id)
    }

    /// How many conversions are kept, over all channels.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.by_channel.values().map(HashMap::len).sum()
    }
}

/// What "Jump to present" does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JumpAction {
    /// The window has the newest messages: scroll to them.
    Scroll,
    /// The window lost its newest messages (scrolled far back, or a load that could
    /// not keep up): load the newest page, which replaces it.
    LoadNewest,
}

/// The message list's view state.
#[derive(Default)]
pub struct MessageView {
    pub rows: Vec<Row>,
    conversions: Conversions,
    outbox: Vec<OutboxEntry>,
    /// The account's read position when the channel was opened: the "NEW" line goes
    /// above the first message after it.
    unread_after: Option<(ChannelId, MessageId)>,
    /// Messages after the read position, for the banner.
    pub unread_count: usize,
    /// The banner's text, worked out with the rows.
    pub banner: Option<String>,
    revealed: HashSet<MessageId>,
    jumped: Option<(MessageId, i64)>,
    pager: OlderPager,
    /// Newest-page requests waiting for their answer, per channel.
    loading: HashMap<ChannelId, u64>,
    next_request: u64,
    /// Compact mode: no avatars, the time before the name, no space between groups.
    pub compact: bool,
    /// The open channel's newest page has landed.
    loaded: bool,
    newest_retries: u8,
    last_reload_ms: Option<i64>,
    /// The open channel's window lost its newest messages (see [`JumpAction`]).
    pub detached: bool,
    /// Live messages that arrived while detached.
    pub detached_new: usize,
    ticker: Option<Task<()>>,
    /// Ends the jump highlight on time.
    jump_timer: Option<Task<()>>,
    scroll_hooked: bool,
    /// The person scrolled the "NEW" line out of view above (the banner shows).
    pub divider_above: bool,
    /// The person scrolled away from the newest messages ("Jump to present" shows).
    pub scrolled_away: bool,
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
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
#[cfg(test)]
pub fn groups_with(prev: &p::Message, prev_ms: i64, next: &p::Message, next_ms: i64) -> bool {
    groups_on(
        prev,
        prev_ms,
        time::day(prev_ms),
        next,
        next_ms,
        time::day(next_ms),
    )
}

/// [`groups_with`] with the local days already worked out.
fn groups_on(
    prev: &p::Message,
    prev_ms: i64,
    prev_day: Option<NaiveDate>,
    next: &p::Message,
    next_ms: i64,
    next_day: Option<NaiveDate>,
) -> bool {
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
        && prev_day == next_day
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

/// Mention names for a message converted on the UI thread (rare: see `Conversions::get`).
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

/// Whether the window of `ch` misses its newest messages and is not loading them.
fn is_detached(state: &AccountState, ch: ChannelId) -> bool {
    state
        .messages(ch)
        .is_some_and(|w| !w.has_newest() && !w.is_loading())
}

impl MessageView {
    pub fn new() -> Self {
        Self {
            compact: starts_compact(),
            ..Self::default()
        }
    }

    fn request_id(&mut self) -> u64 {
        self.next_request += 1;
        self.next_request
    }

    /// Whether the newest page answering `request` for `ch` is still wanted: it is the
    /// current request (which it ends), or no request is waiting. A page for a request
    /// that a newer one replaced is not.
    pub fn accept_newest(&mut self, ch: ChannelId, request: u64) -> bool {
        match self.loading.get(&ch) {
            Some(current) if *current == request => {
                self.loading.remove(&ch);
                true
            }
            Some(_) => false,
            None => true,
        }
    }

    /// What "Jump to present" should do for channel `ch`.
    pub fn jump_action(&self, state: &AccountState, ch: ChannelId) -> JumpAction {
        if is_detached(state, ch) {
            JumpAction::LoadNewest
        } else {
            JumpAction::Scroll
        }
    }

    /// A view whose channel's newest page has landed, for tests of the read rules.
    #[cfg(test)]
    pub fn loaded_for_tests() -> Self {
        Self {
            loaded: true,
            ..Self::default()
        }
    }

    /// Whether the open channel's newest page has landed.
    pub fn is_loaded(&self) -> bool {
        self.loaded
    }

    /// Highlights message `id` as jumped to from `now_ms` for [`JUMP_HIGHLIGHT_MS`].
    pub fn highlight(&mut self, id: MessageId, now_ms: i64) {
        self.jumped = Some((id, now_ms + JUMP_HIGHLIGHT_MS));
    }

    /// Keeps the converted messages of a page applied outside this view (the page
    /// around a message that a jump loads), so the rows need no conversion here.
    pub fn remember_page(
        &mut self,
        state: &AccountState,
        messages: &[p::Message],
        rendered: Vec<Message>,
    ) {
        self.conversions
            .remember(state, &edits_of(messages), rendered);
    }

    /// Whether channel `ch` may be marked read now: its newest page landed, its window
    /// has the newest messages, the list shows the bottom and the window has focus.
    pub fn may_mark_read(
        &self,
        state: &AccountState,
        ch: ChannelId,
        at_bottom: bool,
        window_active: bool,
    ) -> bool {
        self.loaded
            && at_bottom
            && window_active
            && state.messages(ch).is_some_and(|w| w.has_newest())
    }

    /// A live message in the open channel `ch`, after the state took it: counted for
    /// the "new messages" pill while the window misses its newest messages.
    pub fn note_live(&mut self, state: &AccountState, ch: ChannelId) {
        if is_detached(state, ch) {
            self.detached_new += 1;
        }
    }

    /// Builds the rows of channel `ch` from the account state.
    pub fn build_rows(&mut self, state: &AccountState, ch: ChannelId) -> Vec<Row> {
        let mut rows = Vec::new();
        self.unread_count = 0;
        let mut unread_since_ms = 0;
        if self.jumped.is_some_and(|(_, until)| until <= now_ms()) {
            self.jumped = None;
        }
        self.detached = is_detached(state, ch);
        if !self.detached {
            self.detached_new = 0;
        }
        let me = state.me_id();
        // Whether the last row is this account's message (pending rows group under it).
        let mut last_mine = false;
        if let Some(window) = state.messages(ch) {
            if !window.has_more_before() && !window.is_loading() {
                rows.push(Row::Welcome(Rc::new(welcome_row(state, ch))));
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
                let c = self.conversions.get(state, ch, id, m);
                let (ms, day) = (c.ms, c.day);
                let unread = unread_after.is_some_and(|a| id > a);
                let first_unread = unread && self.unread_count == 0;
                if unread {
                    if first_unread {
                        unread_since_ms = ms;
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
                last_mine = UserId::parse(&m.author.id) == Some(me) && is_regular(m.kind);
                if !is_regular(m.kind) {
                    rows.push(Row::System(Rc::new(system_row(id, m, &c.shown))));
                    prev = Some((m, ms, day));
                    continue;
                }
                let grouped = !first_unread
                    && prev.is_some_and(|(pm, pms, pday)| groups_on(pm, pms, pday, m, ms, day));
                rows.push(Row::Message(Rc::new(MessageRow {
                    id,
                    grouped,
                    mention: mentions_me(state, ch, m),
                    jumped: self.jumped.is_some_and(|(j, _)| j == id),
                    revealed: self.revealed.contains(&id),
                    name_color: name_color(state, guild, m),
                    tag: tag(channel, m, &c.shown),
                    reply: reply_of(m, &c.shown),
                    clock: c.clock.clone(),
                    meta: RowMeta::of(me, m),
                    message: Shown(c.shown.clone()),
                })));
                prev = Some((m, ms, day));
            }
        }
        let author = model::clean_name(state.me().display_name());
        for o in &self.outbox {
            if ChannelId::parse(&o.out.channel_id) == Some(ch) {
                rows.push(Row::Pending(Rc::new(PendingRow {
                    nonce: o.out.nonce.clone(),
                    author: author.clone(),
                    blocks: o.blocks.clone(),
                    failed: o.out.failed.clone(),
                    grouped: last_mine,
                })));
                last_mine = true;
            }
        }
        self.banner = match self.unread_count {
            0 => None,
            1 => Some(t!("chat-new-message", since = time::full(unread_since_ms))),
            n if n > BANNER_MAX_COUNTED => Some(t!(
                "chat-new-messages-many",
                count = BANNER_MAX_COUNTED,
                since = time::full(unread_since_ms)
            )),
            n => Some(t!(
                "chat-new-messages",
                count = n,
                since = time::full(unread_since_ms)
            )),
        };
        rows
    }

    /// "Mark as unread": the "NEW" line goes above the first message after `after`.
    pub fn mark_unread_after(&mut self, ch: ChannelId, after: MessageId) {
        self.unread_after = Some((ch, after));
        self.divider_above = false;
    }

    /// Adds or updates an outbox entry: a message being sent, or one that failed.
    pub fn upsert_pending(&mut self, out: Pending) {
        match self.outbox.iter_mut().find(|o| o.out.nonce == out.nonce) {
            Some(entry) => entry.out = out,
            None => {
                let blocks = Rc::new(markdown::parse(&out.text));
                self.outbox.push(OutboxEntry {
                    out,
                    blocks,
                    queued_ms: u64::try_from(now_ms()).unwrap_or(0),
                });
            }
        }
    }

    /// Rows of `channel_id` whose message a page already has. Fluxer's lists carry no
    /// nonce, so a row goes when the page has a message of `me` with the same wire text
    /// and reply that is not older than the row ([`is_own_copy`]). Each message clears at most
    /// one row. Whether any went.
    pub fn clear_arrived(&mut self, me: &str, channel_id: &str, messages: &[p::Message]) -> bool {
        let before = self.outbox.len();
        for m in messages {
            if let Some(ix) = self.outbox.iter().position(|o| {
                o.out.channel_id == channel_id
                    && is_own_copy(
                        m,
                        &Sent {
                            me,
                            content: &o.out.text,
                            reply_to: o.out.reply_to.as_deref(),
                            attachments: 0,
                            sent_ms: o.queued_ms,
                        },
                    )
            }) {
                self.outbox.remove(ix);
            }
        }
        self.outbox.len() != before
    }

    /// The server took the message with this nonce: its row goes. Whether one went.
    pub fn sent(&mut self, nonce: &str) -> bool {
        let before = self.outbox.len();
        self.outbox.retain(|o| o.out.nonce != nonce);
        self.outbox.len() != before
    }

    /// Retry of a failed row: it shows "sending" again, and the command sends it with
    /// its first nonce.
    pub fn retry(&mut self, nonce: &str) -> Option<Cmd> {
        let entry = self
            .outbox
            .iter_mut()
            .find(|o| o.out.nonce == nonce && o.out.failed.is_some())?;
        entry.out.failed = None;
        Some(Cmd::Resend {
            channel_id: entry.out.channel_id.clone(),
            nonce: entry.out.nonce.clone(),
            text: entry.out.text.clone(),
        })
    }
}

/// Events that change members, and so maybe a name colour on screen.
fn changes_members(d: &p::Dispatch) -> bool {
    matches!(
        d,
        p::Dispatch::GuildMemberAdd(_)
            | p::Dispatch::GuildMemberUpdate(_)
            | p::Dispatch::GuildMembersChunk(_)
            | p::Dispatch::GuildMemberListUpdate(_)
    )
}

impl Inlet {
    pub(crate) fn on_messages_event(
        &mut self,
        event: Ev,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let active = window.is_window_active();
        match event {
            Ev::Guilds(guilds) => self.on_guilds(guilds, cx),
            Ev::Channels { guild_id, channels } => self.on_channels(guild_id, channels, window, cx),
            Ev::Dispatch { dispatch, rendered } => {
                self.on_dispatch(*dispatch, rendered, active, cx)
            }
            Ev::Page {
                channel_id,
                request,
                page,
                rendered,
            } => self.on_page(channel_id, request, page, rendered, active, cx),
            Ev::PageFailed {
                channel_id,
                request,
                older,
            } => self.on_page_failed(channel_id, request, older, cx),
            Ev::Pending(out) => {
                self.chat.view.upsert_pending(out);
                self.refresh_rows(cx);
            }
            Ev::Sent { nonce } => {
                if self.chat.view.sent(&nonce) {
                    self.refresh_rows(cx);
                    cx.notify();
                }
            }
            Ev::Action(ev) => self.on_action_event(ev, cx),
            Ev::Inbox(ev) => self.on_inbox_event(ev, cx),
        }
    }

    pub(crate) fn open_channel_id(&self) -> Option<ChannelId> {
        self.selected_channel.as_deref().and_then(ChannelId::parse)
    }

    fn on_dispatch(
        &mut self,
        dispatch: p::Dispatch,
        rendered: Vec<Message>,
        window_active: bool,
        cx: &mut Context<Self>,
    ) {
        if self.settings_update_is_stale(&dispatch) {
            return;
        }
        if let p::Dispatch::Ready(ready) = &dispatch {
            let mut changes = match self.account.clone() {
                None => {
                    let state = AccountState::from_ready_at(ready, now_ms());
                    self.account = Some(cx.new(|_| state));
                    self.start_ticker(cx);
                    let mut c = Changes::default();
                    c.reset = true;
                    c
                }
                Some(account) => account.update(cx, |s, _| s.apply(&dispatch)),
            };
            self.settings_applied(&dispatch, &mut changes, cx);
            self.after_changes(changes, false, cx);
            // A first READY, or a new one after a reconnect: the open channel's window is
            // missing or stale, so load its newest page.
            self.chat.view.newest_retries = 0;
            self.load_open_channel(cx);
            self.inbox_on_dispatch(&dispatch, &[], cx);
            return;
        }
        let Some(account) = self.account.clone() else {
            return;
        };
        let mut changes = account.update(cx, |s, _| s.apply(&dispatch));
        self.settings_applied(&dispatch, &mut changes, cx);
        self.note_saved(&dispatch);
        self.inbox_on_dispatch(&dispatch, &rendered, cx);
        let edits = match &dispatch {
            p::Dispatch::MessageCreate(m) | p::Dispatch::MessageUpdate(m) => edits_of([&**m]),
            _ => Edits::new(),
        };
        self.chat
            .view
            .conversions
            .remember(account.read(cx), &edits, rendered);
        if let p::Dispatch::MessageCreate(m) = &dispatch {
            if let Some(nonce) = &m.nonce {
                let before = self.chat.view.outbox.len();
                self.chat.view.outbox.retain(|o| &o.out.nonce != nonce);
                if self.chat.view.outbox.len() != before {
                    self.refresh_rows(cx);
                }
            }
            let open = self.open_channel_id();
            if let Some(ch) = open.filter(|ch| ChannelId::parse(&m.channel_id) == Some(*ch)) {
                let counted = self.chat.view.detached_new;
                self.chat.view.note_live(account.read(cx), ch);
                if self.chat.view.detached_new != counted {
                    cx.notify();
                }
                // A message in the open channel while it shows the newest messages and
                // has focus: read.
                let at_bottom = self.chat.message_list.is_following_tail();
                if self.chat.view.may_mark_read(
                    account.read(cx),
                    ch,
                    at_bottom,
                    window_active && self.chat_is_visible(),
                ) {
                    changes.merge(self.read_channel(ch, AckKind::Automatic, false, cx));
                }
            }
        }
        self.after_changes(changes, changes_members(&dispatch), cx);
    }

    fn on_page(
        &mut self,
        channel_id: String,
        request: u64,
        page: MessagesPage,
        rendered: Vec<Message>,
        window_active: bool,
        cx: &mut Context<Self>,
    ) {
        let (Some(ch), Some(account)) = (ChannelId::parse(&channel_id), self.account.clone())
        else {
            return;
        };
        let older = page.before.is_some();
        if older {
            self.chat.view.pager.done(request);
        } else if !self.chat.view.accept_newest(ch, request) {
            // A page a newer request replaced: its snapshot may be older than what the
            // state knows (from before a reconnect). The newer answer pairs the load.
            return;
        }
        if !older {
            // Messages of this account that the page already has are no longer pending
            // (one that arrived although its send looked failed).
            let me = account.read(cx).me_id().to_string();
            self.chat
                .view
                .clear_arrived(&me, &channel_id, &page.messages);
        }
        let edits = edits_of(&page.messages);
        let mut changes = account.update(cx, |s, _| s.apply_messages(ch, page));
        self.chat
            .view
            .conversions
            .remember(account.read(cx), &edits, rendered);
        if !older && self.open_channel_id() == Some(ch) {
            self.chat.view.loaded = true;
            let complete = account
                .read(cx)
                .messages(ch)
                .is_some_and(|w| w.has_newest());
            if complete {
                self.chat.view.newest_retries = 0;
                // Opened and shown at the bottom with focus: the channel is read.
                let at_bottom = self.chat.message_list.is_following_tail();
                if self.chat.view.may_mark_read(
                    account.read(cx),
                    ch,
                    at_bottom,
                    window_active && self.chat_is_visible(),
                ) {
                    changes.merge(self.read_channel(ch, AckKind::Explicit, false, cx));
                }
            } else if is_detached(account.read(cx), ch)
                && self.chat.view.newest_retries < MAX_NEWEST_RETRIES
            {
                // Too many live messages arrived during the load: ask again.
                self.chat.view.newest_retries += 1;
                self.load_open_channel(cx);
            }
        }
        self.after_changes(changes, false, cx);
        if !older && self.open_channel_id() == Some(ch) {
            // A jump to a message waits for this page.
            self.inbox_after_newest_page(cx);
        }
    }

    fn on_page_failed(
        &mut self,
        channel_id: String,
        request: u64,
        older: bool,
        cx: &mut Context<Self>,
    ) {
        if older {
            self.chat.view.pager.done(request);
            return;
        }
        let Some(ch) = ChannelId::parse(&channel_id) else {
            return;
        };
        // An answer to a request a newer one replaced: the newer one pairs the load.
        if self.chat.view.loading.get(&ch) != Some(&request) {
            return;
        }
        self.chat.view.loading.remove(&ch);
        if let Some(account) = self.account.clone() {
            account.update(cx, |s, _| s.cancel_load(ch));
            if self.open_channel_id() == Some(ch) {
                self.refresh_rows(cx);
            }
        }
    }

    /// Re-renders what changed in the account state.
    pub(crate) fn after_changes(
        &mut self,
        changes: Changes,
        members: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(account) = self.account.clone() else {
            return;
        };
        // The sidebar and the rail read their badges from the same state.
        self.notifications
            .refresh_badges(account.read(cx), &changes);
        self.chat.view.conversions.prune(account.read(cx), &changes);
        let Some(ch) = self.open_channel_id() else {
            return;
        };
        let guild = account.read(cx).channels().guild_of(ch);
        let relevant = changes.reset
            || changes.messages.contains(&ch)
            || changes.guilds
            || (members && guild.is_some_and(|g| changes.member_guilds.contains(&g)));
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
        if let Some(ch) = self.open_channel_id() {
            self.chat.view.conversions.touch(ch);
        }
        self.load_open_channel(cx);
    }

    /// Empties the list (another guild or channel is about to show).
    pub(crate) fn clear_messages(&mut self) {
        let view = &mut self.chat.view;
        view.rows.clear();
        view.unread_after = None;
        view.revealed.clear();
        view.jumped = None;
        view.loaded = false;
        view.newest_retries = 0;
        view.detached = false;
        view.detached_new = 0;
        view.divider_above = false;
        view.scrolled_away = false;
        self.chat.message_list.reset(0);
        self.chat.message_list.set_follow_mode(FollowMode::Tail);
    }

    /// Signed out: no messages, no account state, nothing on its way.
    pub(crate) fn reset_messages(&mut self) {
        self.clear_messages();
        self.account = None;
        self.chat.actions.clear();
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
        view.conversions.touch(ch);
        if view.unread_after.is_none_or(|(c, _)| c != ch) {
            view.unread_after = marker.map(|m| (ch, m));
        }
        view.loaded = false;
        view.detached_new = 0;
        let request = view.request_id();
        view.loading.insert(ch, request);
        self.backend.send(Cmd::OpenChannel {
            channel_id,
            request,
        });
        self.after_changes(changes, false, cx);
        // An existing window shows right away; the page merges into it.
        self.refresh_rows(cx);
    }

    /// Loads the newest page again when the person asks for it, at most once a second.
    fn reload_newest(&mut self, cx: &mut Context<Self>) {
        let now = now_ms();
        let view = &mut self.chat.view;
        if view
            .last_reload_ms
            .is_some_and(|last| now.saturating_sub(last) < NEWEST_RELOAD_INTERVAL_MS)
        {
            return;
        }
        view.last_reload_ms = Some(now);
        view.newest_retries = 0;
        self.load_open_channel(cx);
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
        let request = view.next_request + 1;
        if let Some(before) =
            view.pager
                .request(now_ms(), has_more && loaded, loading, oldest, request)
        {
            view.next_request = request;
            self.backend.send(Cmd::LoadOlder {
                channel_id,
                before: before.to_string(),
                request,
            });
        }
    }

    /// Follows the person's scrolling: asks for older messages near the top, loads the
    /// newest page again at the bottom of a list that lost it, and notes whether the
    /// "NEW" line and the newest messages are out of view (for the banner and "Jump to
    /// present"). The list reports only scrolling by the person, so loading never
    /// repeats by itself.
    ///
    /// The handler runs while gpui holds the list's state borrowed for the scroll: anything
    /// that touches `message_list` in it (`splice`, `reset`, `scroll_to_end`, even
    /// `is_following_tail`) would panic. So the handler only notes plain view fields;
    /// loading pages and reading the channel (which can rebuild rows and splice the list)
    /// are deferred until the scroll is done and the borrow is released.
    pub(crate) fn hook_list_scroll(&mut self, cx: &mut Context<Self>) {
        if self.chat.view.scroll_hooked {
            return;
        }
        self.chat.view.scroll_hooked = true;
        let weak = cx.entity().downgrade();
        self.chat.message_list.set_scroll_handler(
            move |event: &ListScrollEvent, window: &mut Window, cx: &mut App| {
                let window_active = window.is_window_active();
                let range = event.visible_range.clone();
                let following = event.is_following_tail;
                let count = event.count;
                let at_top = range.start <= 1;
                let at_bottom = range.end >= count;
                // Inside the list's borrow: plain fields of the view only.
                let _ = weak.update(cx, |this, cx| {
                    let view = &mut this.chat.view;
                    view.divider_above = view
                        .rows
                        .iter()
                        .position(|r| matches!(r, Row::Unread | Row::Day { new: true, .. }))
                        .is_some_and(|ix| ix < range.start);
                    view.scrolled_away = !following && range.end < count;
                    cx.notify();
                });
                if !at_top && !at_bottom {
                    return;
                }
                // After the scroll: the list is free again.
                let weak = weak.clone();
                cx.defer(move |cx| {
                    let _ = weak.update(cx, |this, cx| {
                        if at_top {
                            this.load_older(cx);
                        } else if this.chat.view.detached {
                            this.reload_newest(cx);
                        }
                        // Back at the bottom with focus: the channel is read. The tick
                        // does the same once a second, for the ways to the bottom that
                        // report nothing here.
                        if at_bottom {
                            this.read_if_watched(window_active, following, cx);
                        }
                        cx.notify();
                    });
                });
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
        self.tick_notifications(cx);
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
            self.after_changes(changes, false, cx);
            cx.notify();
        } else if markdown::refresh_due(now_ms()) {
            // A relative timestamp on screen shows a new label.
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
                self.chat.view.highlight(id, now_ms());
                self.refresh_rows(cx);
                self.chat.message_list.pause_following_tail();
                self.chat.message_list.scroll_to_reveal_item(ix);
                // The highlight ends on time, not at the next tick.
                self.chat.view.jump_timer = Some(cx.spawn(async move |this, cx| {
                    let ms = u64::try_from(JUMP_HIGHLIGHT_MS).unwrap_or(0);
                    cx.background_executor()
                        .timer(Duration::from_millis(ms))
                        .await;
                    let _ = this.update(cx, |this, cx| {
                        this.refresh_rows(cx);
                        cx.notify();
                    });
                }));
            }
            None => self.chat.notice = Some(t!("chat-reply-not-loaded")),
        }
        cx.notify();
    }

    /// Back to the newest messages, following new ones again. A window that lost its
    /// newest messages loads them; the page replaces it.
    pub(crate) fn jump_to_present(&mut self, cx: &mut Context<Self>) {
        let action = match (self.open_channel_id(), self.account.clone()) {
            (Some(ch), Some(account)) => self.chat.view.jump_action(account.read(cx), ch),
            _ => JumpAction::Scroll,
        };
        if action == JumpAction::LoadNewest {
            self.reload_newest(cx);
        }
        self.chat.view.scrolled_away = false;
        self.chat.message_list.scroll_to_end();
        self.chat.message_list.set_follow_mode(FollowMode::Tail);
        cx.notify();
    }

    /// "Mark as read" in the new-messages banner: the "NEW" line goes too.
    pub(crate) fn mark_open_channel_read(&mut self, cx: &mut Context<Self>) {
        self.chat.view.unread_after = None;
        self.chat.view.divider_above = false;
        if let Some(ch) = self.open_channel_id() {
            let changes = self.read_channel(ch, AckKind::Explicit, true, cx);
            self.after_changes(changes, false, cx);
        }
        self.refresh_rows(cx);
        cx.notify();
    }

    /// The open channel was read some other way ("mark as read" for a server): its "NEW"
    /// line and banner go.
    pub(crate) fn clear_unread_marker(&mut self, cx: &mut Context<Self>) {
        self.chat.view.unread_after = None;
        self.chat.view.divider_above = false;
        self.refresh_rows(cx);
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
        if let Some(c) = self.chat.view.conversions.get_mut(ch, id) {
            let mut full = (*c.shown).clone();
            full.blocks = markdown::parse(&full.content);
            if full.edited {
                markdown::mark_edited(&mut full.blocks);
            }
            full.truncated = false;
            c.shown = Rc::new(full);
        }
        self.refresh_rows(cx);
        cx.notify();
    }

    /// Sends a failed message again, with the nonce it had: the row shows "sending"
    /// until the gateway's MESSAGE_CREATE with that nonce replaces it.
    pub(crate) fn retry_pending(&mut self, nonce: &str, cx: &mut Context<Self>) {
        if let Some(cmd) = self.chat.view.retry(nonce) {
            if let Cmd::Resend { channel_id, .. } = &cmd {
                let channel_id = channel_id.clone();
                self.composer_resent(&channel_id, cx);
            }
            self.backend.send(cmd);
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

    fn raw_in(channel: &str, id: u64, author: &str, extra: Value) -> p::Message {
        let mut v = json!({
            "id": id.to_string(),
            "channel_id": channel,
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

    fn raw(id: u64, author: &str, extra: Value) -> p::Message {
        raw_in("100", id, author, extra)
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

    fn state_with(my_roles: &[&str], settings: Value, extra_channels: u64) -> AccountState {
        let mut channels =
            vec![json!({"id": "100", "type": 0, "guild_id": "20", "name": "general"})];
        for i in 0..extra_channels {
            channels.push(
                json!({"id": (10_000 + i).to_string(), "type": 0, "guild_id": "20", "name": "c"}),
            );
        }
        let ready: p::Ready = serde_json::from_value(json!({
            "session_id": "s",
            "user": {"id": "10", "username": "me"},
            "guilds": [{
                "id": "20",
                "properties": {"id": "20", "name": "G", "owner_id": "99"},
                "channels": channels,
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

    fn state_with_roles(my_roles: &[&str], settings: Value) -> AccountState {
        state_with(my_roles, settings, 0)
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
        assert!(view.banner.is_some());
        // Building again gives equal rows: nothing to splice.
        let again = view.build_rows(&state, ch);
        assert_eq!(splice_range(&rows, &again), None);
    }

    #[test]
    fn a_newest_page_for_a_replaced_request_is_dropped() {
        let ch = ChannelId(100);
        let mut view = MessageView::default();
        // No request waiting (the demo, or a page after a failure): taken.
        assert!(view.accept_newest(ch, 1));
        // Request 2, then request 3 after a reconnect: the late page for 2 is dropped,
        // the page for 3 is taken and ends the load.
        view.loading.insert(ch, 2);
        view.loading.insert(ch, 3);
        assert!(!view.accept_newest(ch, 2));
        assert!(view.accept_newest(ch, 3));
        assert!(!view.loading.contains_key(&ch));
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
            let id = i as u64;
            if pager
                .request(i * 16, true, false, Some(MessageId(1_000)), id)
                .is_some()
            {
                requests += 1;
                pager.done(id);
            }
        }
        assert!((10..=11).contains(&requests), "{requests} requests");

        // A slow server: one request at a time; only its own answer frees the pager.
        let mut pager = OlderPager::default();
        assert!(pager
            .request(0, true, false, Some(MessageId(5)), 1)
            .is_some());
        assert!(pager
            .request(5_000, true, false, Some(MessageId(5)), 2)
            .is_none());
        pager.done(7);
        assert!(pager
            .request(5_000, true, false, Some(MessageId(5)), 2)
            .is_none());
        pager.done(1);
        assert!(pager
            .request(5_000, true, false, Some(MessageId(5)), 2)
            .is_some());

        // At the start of the channel, or while the newest page loads: never.
        let mut pager = OlderPager::default();
        for i in 0..1_000i64 {
            let now = i * 1_000;
            assert!(pager
                .request(now, false, false, Some(MessageId(5)), 1)
                .is_none());
            assert!(pager
                .request(now, true, true, Some(MessageId(5)), 1)
                .is_none());
        }
    }

    #[test]
    fn conversions_stay_bounded_for_channels_without_a_window() {
        let mut state = state_with_roles(&[], json!([]));
        let mut view = MessageView::default();
        // Live messages in 1 000 channels nobody opened (most unknown to the state).
        for ch in 0..1_000u64 {
            let m = raw_in(&(10_000 + ch).to_string(), 1_000 + ch, "7", json!({}));
            let rendered = parse::to_message(&m, &SharedNames::default());
            let _ = state.apply(&p::Dispatch::MessageCreate(Box::new(m.clone())));
            view.conversions
                .remember(&state, &edits_of([&m]), vec![rendered]);
        }
        assert_eq!(view.conversions.len(), 0);

        // Opening 20 channels in turn keeps the conversions of the last few only.
        let mut state = state_with(&[], json!([]), 20);
        for i in 0..20u64 {
            let ch = ChannelId(10_000 + i);
            view.conversions.touch(ch);
            let msgs: Vec<p::Message> = (1..=10)
                .map(|n| raw_in(&ch.to_string(), i * 100 + n, "7", json!({})))
                .collect();
            let rendered = msgs
                .iter()
                .map(|m| parse::to_message(m, &SharedNames::default()))
                .collect();
            page(&mut state, ch, msgs.clone());
            view.conversions
                .remember(&state, &edits_of(&msgs), rendered);
        }
        assert_eq!(view.conversions.by_channel.len(), RECENT_CHANNELS);
        assert_eq!(view.conversions.len(), RECENT_CHANNELS * 10);
        // Live messages in an opened channel whose window was dropped are not kept.
        let gone = ChannelId(10_019);
        let _ = state.drop_message_windows();
        let m = raw_in(&gone.to_string(), 99_999, "7", json!({}));
        let rendered = parse::to_message(&m, &SharedNames::default());
        view.conversions
            .remember(&state, &edits_of([&m]), vec![rendered]);
        view.conversions.prune(&state, &Changes::default());
        assert_eq!(view.conversions.len(), 0);
    }

    #[test]
    fn a_window_without_its_newest_messages_is_reloaded_not_marked_read() {
        let ch = ChannelId(100);
        let mut state = state_with_roles(&[], json!([]));
        let mut view = MessageView::default();
        // The newest page, then eleven older pages of 50: the window keeps 500 and drops
        // the newest.
        let newest: Vec<p::Message> = (10_000..10_050).map(|i| raw(i, "7", json!({}))).collect();
        page(&mut state, ch, newest);
        view.loaded = true;
        assert_eq!(view.jump_action(&state, ch), JumpAction::Scroll);
        assert!(view.may_mark_read(&state, ch, true, true));
        for p_ix in 0..11u64 {
            let oldest = state.messages(ch).and_then(|w| w.oldest_id()).unwrap();
            let msgs: Vec<p::Message> = (0..50)
                .map(|i| raw(oldest.0 - 1 - i, "7", json!({})))
                .collect();
            let _ = state.apply_messages(ch, MessagesPage::before(oldest, msgs, 50));
            let _ = p_ix;
        }
        assert!(!state.messages(ch).unwrap().has_newest());
        assert_eq!(view.jump_action(&state, ch), JumpAction::LoadNewest);
        // Not read while the newest messages are missing, even at the bottom.
        assert!(!view.may_mark_read(&state, ch, true, true));
        // Live messages are buffered by the state and counted for the pill.
        let live = raw(20_000, "7", json!({}));
        let _ = state.apply(&p::Dispatch::MessageCreate(Box::new(live)));
        view.note_live(&state, ch);
        let _ = view.build_rows(&state, ch);
        assert!(view.detached);
        assert_eq!(view.detached_new, 1);
        // Loading the newest page replaces the window.
        let fresh: Vec<p::Message> = (19_960..=20_000).map(|i| raw(i, "7", json!({}))).collect();
        page(&mut state, ch, fresh);
        assert!(state.messages(ch).unwrap().has_newest());
        assert_eq!(view.jump_action(&state, ch), JumpAction::Scroll);
        let _ = view.build_rows(&state, ch);
        assert!(!view.detached);
        assert_eq!(view.detached_new, 0);
    }
}
