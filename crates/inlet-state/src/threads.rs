//! Threads, forum posts and the account's membership in them.
//!
//! Threads are channels: they live in [`crate::channels::Channels`] next to
//! their parent, count against the guild's channel cap, and get permissions
//! like any channel ([`crate::AccountState::permissions`]). This module
//! applies the THREAD_* events with Fluxer's membership rules, and holds the
//! small, bounded helpers the app uses for forums and archived lists.
//!
//! # Membership (fails closed)
//!
//! The account's own membership is the `member` object on a stored thread,
//! and the permission calculator reads it to show private threads. Fluxer
//! sends it only on authoritative sources (READY, GUILD_CREATE,
//! THREAD_CREATE, THREAD_LIST_SYNC, REST): there a missing `member` means
//! "not a member", so those replace the stored thread as a whole (as
//! [`inlet_protocol::Channel::replace_thread`] does). THREAD_UPDATE never
//! carries it and keeps the stored one
//! ([`inlet_protocol::Channel::merge_thread_update`]). THREAD_MEMBER_UPDATE
//! and THREAD_MEMBERS_UPDATE set or clear it. Every change recomputes the
//! thread's permissions, so a private thread the account left is hidden at
//! once. A new READY rebuilds every thread from its guild's `threads`.
//!
//! # Bounds
//!
//! Threads count against [`crate::limits::MAX_CHANNELS_PER_GUILD`], and at
//! most [`crate::limits::MAX_THREADS_PER_GUILD`] of them are stored per
//! guild. Forum unread counts from op 28 keep at most [`MAX_POST_UNREADS`]
//! posts. The app's paged lists ([`ThreadPages`]) keep at most
//! [`MAX_LISTED_THREADS`].

use std::collections::BTreeSet;

use inlet_protocol::api::threads::{forum_flags, ThreadMember};
use inlet_protocol::gateway::events::ForumUnreads;
use inlet_protocol::{Channel, Dispatch, ForumUnreadRequest};
use serde_json::Value;

use crate::account::AccountState;
use crate::bounded::LruMap;
use crate::ids::{ChannelId, GuildId, MessageId, UserId};
use crate::limits::{MAX_CHANNELS_PER_GUILD, MAX_THREADS_PER_GUILD};
use crate::Changes;

/// Forum posts whose op 28 unread count is kept.
pub const MAX_POST_UNREADS: usize = 2_000;
/// Threads one paged list of the app ([`ThreadPages`]) keeps; past it, no more pages.
pub const MAX_LISTED_THREADS: usize = 500;
/// Posts in one op 28 request (Fluxer's `FORUM_UNREADS_MAX_THREADS`).
pub const MAX_FORUM_UNREAD_POSTS: usize = 40;

/// What op 28 said about one forum post.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PostUnread {
    /// The post's ack when the count came; a newer ack makes it stale.
    ack: MessageId,
    count: u32,
}

/// Thread state that is not a channel: forum unread counts.
#[derive(Debug, Clone, PartialEq)]
pub struct Threads {
    post_unreads: LruMap<ChannelId, PostUnread>,
}

impl Default for Threads {
    fn default() -> Self {
        Threads {
            post_unreads: LruMap::new(MAX_POST_UNREADS),
        }
    }
}

impl Threads {
    /// Applies an event that is not a thread event: nothing changes. Thread
    /// events go through [`crate::AccountState::apply`].
    pub fn apply(&mut self, _event: &Dispatch) -> Changes {
        Changes::default()
    }
}

/// How a thread object is stored.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    /// READY, GUILD_CREATE, THREAD_CREATE, THREAD_LIST_SYNC or REST: taken
    /// as it is; a missing `member` clears the stored membership.
    Authoritative,
    /// THREAD_UPDATE: keeps the stored membership.
    Update,
}

/// Whether a thread is archived.
pub fn is_archived(t: &Channel) -> bool {
    t.thread_metadata().is_some_and(|m| m.archived)
}

/// Whether a forum post is its forum's pinned post.
pub fn is_pinned_post(t: &Channel) -> bool {
    t.flag_bits() & forum_flags::PINNED != 0
}

/// Fluxer's `lastActivityId`: the newest message, or the thread itself.
pub fn last_activity(t: &Channel) -> u64 {
    let id = ChannelId::parse(&t.id).map_or(0, |c| c.0);
    MessageId::parse_opt(t.last_message_id.as_deref()).map_or(id, |m| m.0.max(id))
}

impl AccountState {
    /// THREAD_* and FORUM_UNREADS (called from [`AccountState::apply`]).
    pub(crate) fn apply_thread(&mut self, d: &Dispatch, c: &mut Changes) {
        match d {
            Dispatch::ThreadCreate(t) => self.put_thread(t, Source::Authoritative, c),
            Dispatch::ThreadUpdate(t) => self.put_thread(t, Source::Update, c),
            Dispatch::ThreadDelete(t) => {
                if let Some(id) = ChannelId::parse(&t.id) {
                    if self.is_thread_id(id) {
                        self.drop_thread(id, false, c);
                    }
                }
            }
            Dispatch::ThreadListSync(s) => {
                let Some(g) = GuildId::parse(&s.guild_id) else {
                    return;
                };
                let scope: Option<BTreeSet<ChannelId>> = s
                    .channel_ids
                    .as_ref()
                    .map(|ids| ids.iter().filter_map(|i| ChannelId::parse(i)).collect());
                let threads = s.threads_with_members();
                let listed: BTreeSet<ChannelId> = threads
                    .iter()
                    .filter_map(|t| ChannelId::parse(&t.id))
                    .collect();
                // Active threads in the scope that the list no longer has are gone;
                // archived ones the app stored from REST stay.
                let stale: Vec<ChannelId> = self
                    .guild_threads(g)
                    .filter(|(id, t)| {
                        !listed.contains(id)
                            && !is_archived(t)
                            && self
                                .channels
                                .parent_of(*id)
                                .is_some_and(|p| scope.as_ref().is_none_or(|s| s.contains(&p)))
                    })
                    .map(|(id, _)| id)
                    .collect();
                for id in stale {
                    self.drop_thread(id, true, c);
                }
                for t in &threads {
                    if GuildId::parse_opt(t.guild_id.as_deref()).is_none_or(|tg| tg == g) {
                        let mut t = t.clone();
                        t.guild_id = Some(g.to_string());
                        self.put_thread(&t, Source::Authoritative, c);
                    }
                }
            }
            Dispatch::ThreadMemberUpdate(m) => {
                let mine = m
                    .user_id
                    .as_deref()
                    .is_none_or(|u| UserId::parse(u) == Some(self.me_id));
                if let (true, Some(id)) = (mine, m.id.as_deref().and_then(ChannelId::parse)) {
                    self.set_membership(id, Some(m), c);
                }
            }
            Dispatch::ThreadMembersUpdate(u) => {
                let Some(id) = ChannelId::parse(&u.id) else {
                    return;
                };
                if !self.is_thread_id(id) {
                    return;
                }
                if let (Some(n), Some(t)) = (u.member_count, self.channels.get_mut(id)) {
                    if t.member_count() != Some(n.min(50)) {
                        t.extra
                            .insert("member_count".into(), Value::from(n.min(50)));
                        c.channel(id);
                    }
                }
                let me = self.me_id.to_string();
                if let Some(change) = u.membership_change(&me) {
                    self.set_membership(id, change, c);
                }
            }
            Dispatch::ForumUnreads(f) => self.forum_unreads(f),
            _ => {}
        }
    }

    /// The threads of a guild's `threads` list (READY and GUILD_CREATE),
    /// stored after the guild's channels. A thread needs a parent among
    /// `fresh` (the channels the guild keeps); its ID joins `fresh`.
    pub(crate) fn ingest_threads(
        &mut self,
        g: GuildId,
        list: Option<&Value>,
        fresh: &mut BTreeSet<ChannelId>,
        c: &mut Changes,
    ) {
        let Some(Value::Array(items)) = list else {
            return;
        };
        let mut kept = 0;
        for item in items {
            if kept >= MAX_THREADS_PER_GUILD || fresh.len() >= MAX_CHANNELS_PER_GUILD {
                break;
            }
            let Ok(mut t) = serde_json::from_value::<Channel>(item.clone()) else {
                continue;
            };
            let Some(id) = ChannelId::parse(&t.id) else {
                continue;
            };
            let parent_here = ChannelId::parse_opt(t.parent_id.as_deref())
                .is_some_and(|p| fresh.contains(&p) && !self.is_thread_id(p));
            if !t.is_thread() || !parent_here || fresh.contains(&id) {
                continue;
            }
            t.guild_id = Some(g.to_string());
            fresh.insert(id);
            kept += 1;
            self.store_channel(id, t, Some(g), c);
        }
    }

    /// Stores a thread from REST (an archived thread, or a forum post the
    /// app is about to open). It is authoritative: a missing `member` means
    /// the account is not a member. Ignored when its guild or its parent is
    /// not known, or the guild holds too many threads.
    pub fn upsert_thread(&mut self, t: &Channel) -> Changes {
        let mut c = Changes::default();
        self.put_thread(t, Source::Authoritative, &mut c);
        c
    }

    /// The stored threads under `parent` that the account can view: active
    /// ones first, then archived ones, newest activity first in each.
    pub fn threads_of(&self, parent: ChannelId) -> Vec<&Channel> {
        let Some(g) = self.channels.guild_of(parent) else {
            return Vec::new();
        };
        let mut v: Vec<&Channel> = self
            .guild_threads(g)
            .filter(|(id, _)| self.channels.parent_of(*id) == Some(parent))
            .filter(|(id, _)| self.permissions(g, *id).can_view())
            .map(|(_, t)| t)
            .collect();
        v.sort_by_key(|t| (is_archived(t), std::cmp::Reverse(last_activity(t))));
        v
    }

    /// A channel's ack, also when the channel is not stored right now (READY
    /// read states of posts the account has not opened wait in an archive).
    pub fn ack_of(&self, ch: ChannelId) -> Option<MessageId> {
        self.read_states.ack_of(ch)
    }

    /// Messages in a forum post after the account's ack, as op 28 counted
    /// them. 0 when the account joined the post (its read state counts
    /// then), the ack moved since, or nothing was counted.
    pub fn forum_post_unread(&self, post: &Channel) -> u32 {
        let Some(id) = ChannelId::parse(&post.id) else {
            return 0;
        };
        if post.is_thread_member() {
            return 0;
        }
        match (self.threads.post_unreads.get(&id), self.ack_of(id)) {
            (Some(u), Some(ack)) if u.ack == ack => u.count,
            _ => 0,
        }
    }

    fn forum_unreads(&mut self, f: &ForumUnreads) {
        for entry in &f.threads {
            let Some(id) = ChannelId::parse(&entry.thread_id) else {
                continue;
            };
            let count = u32::try_from(entry.count.unwrap_or(0)).unwrap_or(u32::MAX);
            match self.ack_of(id) {
                Some(ack) if count > 0 && !entry.missing => {
                    self.threads
                        .post_unreads
                        .insert(id, PostUnread { ack, count });
                }
                _ => {
                    self.threads.post_unreads.remove(&id);
                }
            }
        }
    }

    fn is_thread_id(&self, id: ChannelId) -> bool {
        self.channels.get(id).is_some_and(Channel::is_thread)
    }

    fn guild_threads(&self, g: GuildId) -> impl Iterator<Item = (ChannelId, &Channel)> {
        self.guilds
            .get(&g)
            .into_iter()
            .flat_map(|guild| guild.channels.iter().copied())
            .filter_map(|id| self.channels.get(id).map(|t| (id, t)))
            .filter(|(_, t)| t.is_thread())
    }

    /// Stores a thread. It must name a known guild (or keep the one it has)
    /// and a parent channel in that guild that is not a thread; anything
    /// else is dropped. Its permissions are recomputed.
    fn put_thread(&mut self, t: &Channel, src: Source, c: &mut Changes) {
        let Some(id) = ChannelId::parse(&t.id) else {
            return;
        };
        if !t.is_thread() {
            return;
        }
        let known = self.channels.guild_of(id);
        let sent = GuildId::parse_opt(t.guild_id.as_deref());
        if t.guild_id.is_some() && sent.is_none() {
            return;
        }
        let g = match (known, sent) {
            (Some(k), Some(s)) if k != s => return,
            (Some(k), _) => k,
            // A stored DM never becomes a thread.
            (None, Some(_)) if self.channels.get(id).is_some() => return,
            (None, Some(s)) => s,
            (None, None) => return,
        };
        if known.is_some() && !self.is_thread_id(id) {
            return;
        }
        let Some(parent) = ChannelId::parse_opt(t.parent_id.as_deref()) else {
            return;
        };
        if self.channels.guild_of(parent) != Some(g) || self.is_thread_id(parent) {
            return;
        }
        let Some(guild) = self.guilds.get(&g) else {
            return;
        };
        if guild.unavailable {
            return;
        }
        if !guild.channels.contains(&id)
            && (guild.channels.len() >= MAX_CHANNELS_PER_GUILD
                || self.guild_threads(g).count() >= MAX_THREADS_PER_GUILD)
        {
            return;
        }
        let mut stored = match (src, self.channels.get(id)) {
            (Source::Update, Some(old)) => {
                let mut merged = old.clone();
                merged.merge_thread_update(t.clone());
                merged
            }
            // Authoritative, or nothing stored to keep a membership from.
            _ => t.clone(),
        };
        stored.guild_id = Some(g.to_string());
        if let Some(guild) = self.guilds.get_mut(&g) {
            guild.channels.insert(id);
        }
        self.store_channel(id, stored, Some(g), c);
        self.recompute_ids(g, &[id], c);
    }

    /// Sets (`Some`) or clears the account's membership in a stored thread.
    fn set_membership(&mut self, id: ChannelId, m: Option<&ThreadMember>, c: &mut Changes) {
        let Some(g) = self.channels.guild_of(id) else {
            return;
        };
        let Some(t) = self.channels.get_mut(id) else {
            return;
        };
        if !t.is_thread() {
            return;
        }
        let before = t.extra.get("member").cloned();
        t.set_thread_member(m);
        if t.extra.get("member") != before.as_ref() {
            c.channel(id);
            // Membership decides whether the thread counts towards the guild's pill.
            c.read(id, Some(g));
            self.recompute_ids(g, &[id], c);
        }
    }

    /// Removes a stored thread with its window, and its read state unless `keep_ack`.
    fn drop_thread(&mut self, id: ChannelId, keep_ack: bool, c: &mut Changes) {
        let g = self.channels.guild_of(id);
        self.forget_channel(id, keep_ack, c);
        if let Some(g) = g {
            if let Some(guild) = self.guilds.get_mut(&g) {
                guild.channels.remove(&id);
            }
            self.recompute_ids(g, &[id], c);
        }
    }
}

// ----- forums -----

/// How a forum's posts are ordered (Fluxer's `ForumSortOrderTypes`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ForumSort {
    /// Newest message (or post) first.
    #[default]
    LatestActivity,
    /// Newest post first.
    CreationTime,
}

impl ForumSort {
    /// The forum's own default (`default_sort_order`).
    pub fn default_of(forum: &Channel) -> ForumSort {
        match forum
            .extra
            .get("default_sort_order")
            .and_then(Value::as_i64)
        {
            Some(1) => ForumSort::CreationTime,
            _ => ForumSort::LatestActivity,
        }
    }

    /// The `sort_by` of a thread search in this order.
    pub fn search_key(self) -> &'static str {
        use inlet_protocol::api::threads::thread_sort;
        match self {
            ForumSort::LatestActivity => thread_sort::LAST_MESSAGE_TIME,
            ForumSort::CreationTime => thread_sort::CREATION_TIME,
        }
    }

    fn key(self, t: &Channel) -> u64 {
        match self {
            ForumSort::LatestActivity => last_activity(t),
            ForumSort::CreationTime => ChannelId::parse(&t.id).map_or(0, |c| c.0),
        }
    }
}

/// Whether a post needs one or all of the picked tags (`default_tag_setting`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TagMatch {
    /// At least one of them (`match_some`).
    #[default]
    Some,
    /// All of them (`match_all`).
    All,
}

impl TagMatch {
    /// The forum's own setting.
    pub fn default_of(forum: &Channel) -> TagMatch {
        match forum
            .extra
            .get("default_tag_setting")
            .and_then(Value::as_str)
        {
            Some("match_all") => TagMatch::All,
            _ => TagMatch::Some,
        }
    }

    /// The `tag_setting` of a thread search.
    pub fn search_key(self) -> &'static str {
        match self {
            TagMatch::Some => "match_some",
            TagMatch::All => "match_all",
        }
    }
}

/// Fluxer's `matchesTagFilter`: no picked tags lets every post through.
pub fn matches_tags(applied: &[String], picked: &[String], how: TagMatch) -> bool {
    if picked.is_empty() {
        return true;
    }
    match how {
        TagMatch::All => picked.iter().all(|t| applied.contains(t)),
        TagMatch::Some => picked.iter().any(|t| applied.contains(t)),
    }
}

/// A forum's posts as the forum view lists them.
#[derive(Debug, Default, PartialEq)]
pub struct ForumPostList<'a> {
    /// The pinned post, when it is active and passes the filter.
    pub pinned: Option<&'a Channel>,
    /// Active posts.
    pub active: Vec<&'a Channel>,
    /// Archived posts.
    pub archived: Vec<&'a Channel>,
}

/// Fluxer's `buildForumPostList`: the active posts the account knows and the
/// posts of the paged search, without repeats, filtered by the picked tags,
/// with the pinned post apart and both groups newest first in `sort` order.
pub fn forum_post_list<'a>(
    known: &[&'a Channel],
    extra: &'a [Channel],
    sort: ForumSort,
    tags: &[String],
    how: TagMatch,
) -> ForumPostList<'a> {
    let mut seen = BTreeSet::new();
    let mut out = ForumPostList::default();
    for post in known.iter().copied().chain(extra.iter()) {
        if !seen.insert(post.id.as_str()) {
            continue;
        }
        if !matches_tags(&post.applied_tags(), tags, how) {
            continue;
        }
        let archived = is_archived(post);
        if is_pinned_post(post) && !archived && out.pinned.is_none() {
            out.pinned = Some(post);
            continue;
        }
        if archived {
            out.archived.push(post);
        } else {
            out.active.push(post);
        }
    }
    let by = |t: &&Channel| std::cmp::Reverse(sort.key(t));
    out.active.sort_by_key(by);
    out.archived.sort_by_key(by);
    out
}

/// Op 28 (Request Forum Unreads) for one open forum view: at most one
/// request per opening, for up to [`MAX_FORUM_UNREAD_POSTS`] posts the
/// account has not joined (joined posts have their own read state) and has
/// read before (op 28 counts after an ack). The protocol paces what is sent
/// (at most 4 per 5 s per account).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ForumUnreadRequests {
    open: Option<ChannelId>,
    sent: bool,
}

impl ForumUnreadRequests {
    /// The forum view opened (or opened again): one request may go.
    pub fn open(&mut self, forum: ChannelId) {
        self.open = Some(forum);
        self.sent = false;
    }

    /// The forum view closed.
    pub fn close(&mut self) {
        self.open = None;
        self.sent = false;
    }

    /// The request for this opening, the first time there is something to
    /// ask; `None` after that, for a forum that is not open, or with
    /// nothing to ask.
    pub fn take(
        &mut self,
        state: &AccountState,
        forum: ChannelId,
        posts: &[&Channel],
    ) -> Option<Vec<ForumUnreadRequest>> {
        if self.sent || self.open != Some(forum) {
            return None;
        }
        let mut seen = BTreeSet::new();
        let ask: Vec<ForumUnreadRequest> = posts
            .iter()
            .filter(|p| !p.is_thread_member())
            .filter(|p| ChannelId::parse_opt(p.parent_id.as_deref()) == Some(forum))
            .filter_map(|p| {
                let id = ChannelId::parse(&p.id)?;
                let ack = state.ack_of(id)?;
                seen.insert(id).then(|| ForumUnreadRequest {
                    thread_id: id.to_string(),
                    ack_message_id: Some(ack.to_string()),
                })
            })
            .take(MAX_FORUM_UNREAD_POSTS)
            .collect();
        if ask.is_empty() {
            return None;
        }
        self.sent = true;
        Some(ask)
    }
}

// ----- paged lists -----

/// Where the next page of a [`ThreadPages`] list starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageBy {
    /// `before` the `archive_timestamp` of the last thread (archived lists,
    /// most recently archived first).
    ArchiveTime,
    /// After the threads already listed (thread search).
    Offset,
}

/// The next page to ask for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageRequest {
    /// Pairs the answer with the request; other answers are dropped.
    pub request: u64,
    /// [`PageBy::ArchiveTime`]: the time to page before (`None` for the first page).
    pub before: Option<String>,
    /// [`PageBy::Offset`]: results to skip.
    pub offset: u32,
}

/// A list of threads loaded a page at a time when the person asks (the
/// archived threads of a channel, the archived posts of a forum): one page
/// in flight at a time, no repeats, at most [`MAX_LISTED_THREADS`] threads,
/// and a page that does not move the cursor ends the list.
#[derive(Debug, Clone, PartialEq)]
pub struct ThreadPages {
    by: PageBy,
    threads: Vec<Channel>,
    has_more: bool,
    in_flight: Option<u64>,
    answered: bool,
    next_request: u64,
    before: Option<String>,
    offset: u32,
}

impl ThreadPages {
    /// An empty list that pages `by`, numbering its requests after `last_request`
    /// (the newest request of a list it replaces), so an answer meant for that list
    /// never pairs with this one.
    pub fn continuing(by: PageBy, last_request: u64) -> Self {
        let mut pages = ThreadPages::new(by);
        pages.next_request = last_request;
        pages
    }

    /// The newest request this list made.
    pub fn last_request(&self) -> u64 {
        self.next_request
    }

    /// An empty list that pages `by`.
    pub fn new(by: PageBy) -> Self {
        ThreadPages {
            by,
            threads: Vec::new(),
            has_more: true,
            in_flight: None,
            answered: false,
            next_request: 0,
            before: None,
            offset: 0,
        }
    }

    /// The threads loaded so far, in the order they came.
    pub fn threads(&self) -> &[Channel] {
        &self.threads
    }

    /// Whether another page may follow.
    pub fn has_more(&self) -> bool {
        self.has_more
    }

    /// Whether a page is on its way.
    pub fn is_loading(&self) -> bool {
        self.in_flight.is_some()
    }

    /// Whether a page has landed since the list (re)started.
    pub fn is_loaded(&self) -> bool {
        self.answered
    }

    /// Starts over (another filter or order): the list empties, and the
    /// answer to a request on its way is dropped.
    pub fn reset(&mut self) {
        let next = self.next_request;
        *self = ThreadPages::new(self.by);
        self.next_request = next;
    }

    /// The next page to ask for; `None` while one is on its way, when there
    /// are no more, or when the list is full.
    pub fn next_page(&mut self) -> Option<PageRequest> {
        if self.in_flight.is_some() || !self.has_more {
            return None;
        }
        if self.threads.len() >= MAX_LISTED_THREADS {
            self.has_more = false;
            return None;
        }
        self.next_request += 1;
        self.in_flight = Some(self.next_request);
        Some(PageRequest {
            request: self.next_request,
            before: self.before.clone(),
            offset: self.offset,
        })
    }

    /// A page arrived. Returns whether it was the one waited for; an answer
    /// to another request changes nothing.
    pub fn on_page(&mut self, request: u64, page: Vec<Channel>, has_more: bool) -> bool {
        if self.in_flight != Some(request) {
            return false;
        }
        self.in_flight = None;
        self.answered = true;
        let came = page.len();
        let mut last_time = None;
        for t in page {
            if self.threads.len() >= MAX_LISTED_THREADS {
                break;
            }
            if let Some(ts) = t.thread_metadata().and_then(|m| m.archive_timestamp) {
                last_time = Some(ts);
            }
            if !self.threads.iter().any(|o| o.id == t.id) {
                self.threads.push(t);
            }
        }
        let moved = match self.by {
            PageBy::ArchiveTime => match last_time {
                Some(ts) if self.before.as_deref() != Some(ts.as_str()) => {
                    self.before = Some(ts);
                    true
                }
                _ => false,
            },
            PageBy::Offset => {
                self.offset = self
                    .offset
                    .saturating_add(u32::try_from(came).unwrap_or(u32::MAX));
                came > 0
            }
        };
        self.has_more = has_more && moved && self.threads.len() < MAX_LISTED_THREADS;
        true
    }

    /// The request failed: the same page may be asked for again. Returns whether it
    /// was the one waited for (a failure of another request changes nothing).
    pub fn on_failed(&mut self, request: u64) -> bool {
        if self.in_flight == Some(request) {
            self.in_flight = None;
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn thread(id: u64, last: Option<u64>, tags: &[&str], extra: Value) -> Channel {
        let mut v = json!({
            "id": id.to_string(), "type": 11, "guild_id": "1", "parent_id": "2",
            "applied_tags": tags,
        });
        if let Some(l) = last {
            v["last_message_id"] = json!(l.to_string());
        }
        if let (Some(o), Some(more)) = (v.as_object_mut(), extra.as_object()) {
            for (k, x) in more {
                o.insert(k.clone(), x.clone());
            }
        }
        serde_json::from_value(v).unwrap()
    }

    fn ids(list: &[&Channel]) -> Vec<String> {
        list.iter().map(|t| t.id.clone()).collect()
    }

    #[test]
    fn forum_sort_and_tag_filter() {
        let archived = json!({"thread_metadata": {"archived": true}});
        // 10: the oldest post with the newest message; 30: the newest post, no
        // messages; 20: tagged 5 and 6; 15: the pinned post.
        let a = thread(10, Some(100), &["5"], json!({}));
        let b = thread(20, Some(50), &["5", "6"], json!({}));
        let c = thread(30, None, &["6"], json!({}));
        let pinned = thread(15, Some(16), &[], json!({"flags": 2}));
        let searched = vec![
            thread(5, Some(7), &["5"], archived.clone()),
            thread(8, Some(9), &["6"], archived),
            // The same post from the search as from the gateway is listed once.
            thread(20, Some(50), &["5", "6"], json!({})),
        ];
        let known = [&a, &b, &c, &pinned];
        let list = |sort, tags: &[&str], how| {
            let tags: Vec<String> = tags.iter().map(|t| (*t).to_owned()).collect();
            let l = forum_post_list(&known, &searched, sort, &tags, how);
            (
                l.pinned.map(|p| p.id.clone()),
                ids(&l.active),
                ids(&l.archived),
            )
        };

        let (pin, active, old) = list(ForumSort::LatestActivity, &[], TagMatch::Some);
        assert_eq!(pin.as_deref(), Some("15"));
        assert_eq!(active, ["10", "20", "30"], "newest message first");
        assert_eq!(old, ["8", "5"]);

        let (_, active, _) = list(ForumSort::CreationTime, &[], TagMatch::Some);
        assert_eq!(active, ["30", "20", "10"], "newest post first");

        let (pin, active, old) = list(ForumSort::LatestActivity, &["5"], TagMatch::Some);
        assert_eq!(active, ["10", "20"]);
        assert_eq!(old, ["5"]);
        assert_eq!(pin, None, "the untagged pinned post is filtered out too");

        let (_, active, _) = list(ForumSort::LatestActivity, &["5", "6"], TagMatch::Some);
        assert_eq!(active, ["10", "20", "30"], "any picked tag");
        let (_, active, old) = list(ForumSort::LatestActivity, &["5", "6"], TagMatch::All);
        assert_eq!(active, ["20"], "every picked tag");
        assert!(old.is_empty());

        // The forum's own defaults.
        let forum: Channel = serde_json::from_value(json!({
            "id": "2", "type": 15, "default_sort_order": 1, "default_tag_setting": "match_all"
        }))
        .unwrap();
        assert_eq!(ForumSort::default_of(&forum), ForumSort::CreationTime);
        assert_eq!(TagMatch::default_of(&forum), TagMatch::All);
        assert_eq!(ForumSort::LatestActivity.search_key(), "last_message_time");
        assert_eq!(ForumSort::CreationTime.search_key(), "creation_time");
        assert_eq!(TagMatch::Some.search_key(), "match_some");
    }

    #[test]
    fn archived_threads_are_paged() {
        let at = |id: u64, ts: &str| {
            thread(
                id,
                None,
                &[],
                json!({"thread_metadata": {"archived": true, "archive_timestamp": ts}}),
            )
        };
        let mut pages = ThreadPages::new(PageBy::ArchiveTime);
        assert!(!pages.is_loaded());
        let first = pages.next_page().expect("the first page");
        assert_eq!(first.before, None);
        assert!(pages.next_page().is_none(), "one page at a time");
        assert!(pages.is_loading());
        assert!(pages.on_page(
            first.request,
            vec![at(9, "2026-01-09T00:00:00Z"), at(8, "2026-01-08T00:00:00Z")],
            true
        ));
        assert!(pages.is_loaded() && !pages.is_loading());
        let second = pages.next_page().expect("more pages");
        assert_eq!(second.before.as_deref(), Some("2026-01-08T00:00:00Z"));
        // An answer to an old request changes nothing.
        assert!(!pages.on_page(first.request, vec![at(1, "2026-01-01T00:00:00Z")], true));
        // A failure frees the slot; the same page is asked for again. A failure of
        // another request changes nothing.
        assert!(!pages.on_failed(first.request));
        assert!(pages.on_failed(second.request));
        let again = pages.next_page().expect("a retry");
        assert_eq!(again.before.as_deref(), Some("2026-01-08T00:00:00Z"));
        // A thread already listed is not listed twice.
        assert!(pages.on_page(
            again.request,
            vec![at(8, "2026-01-08T00:00:00Z"), at(7, "2026-01-07T00:00:00Z")],
            false
        ));
        let listed: Vec<&str> = pages.threads().iter().map(|t| t.id.as_str()).collect();
        assert_eq!(listed, ["9", "8", "7"]);
        assert!(
            !pages.has_more() && pages.next_page().is_none(),
            "the last page"
        );

        // A page that does not move the cursor ends the list, so it never loops.
        let mut stuck = ThreadPages::new(PageBy::ArchiveTime);
        let r = stuck.next_page().unwrap();
        stuck.on_page(r.request, vec![thread(3, None, &[], json!({}))], true);
        assert!(stuck.next_page().is_none());

        // Search pages go by offset; a reset drops the answer on its way.
        let mut search = ThreadPages::new(PageBy::Offset);
        let r = search.next_page().unwrap();
        assert_eq!(r.offset, 0);
        search.on_page(r.request, (1..=25).map(|i| at(i, "x")).collect(), true);
        let r2 = search.next_page().unwrap();
        assert_eq!(r2.offset, 25);
        search.reset();
        assert!(!search.on_page(r2.request, vec![at(99, "y")], true));
        assert!(search.threads().is_empty() && !search.is_loaded());
        assert_eq!(search.next_page().unwrap().offset, 0);

        // A list that replaces another numbers its requests after the old one's.
        let next = ThreadPages::continuing(PageBy::Offset, search.last_request())
            .next_page()
            .unwrap();
        assert!(next.request > r2.request);

        // Bounded: never more than the cap, and no page past it.
        let mut big = ThreadPages::new(PageBy::Offset);
        let mut n = 0;
        while let Some(r) = big.next_page() {
            let page = (0..100).map(|i| at(1_000 + n * 100 + i, "z")).collect();
            big.on_page(r.request, page, true);
            n += 1;
            assert!(n < 20, "paging stops");
        }
        assert_eq!(big.threads().len(), MAX_LISTED_THREADS);
    }
}
