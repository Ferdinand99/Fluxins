// SPDX-License-Identifier: AGPL-3.0-or-later
//! A forum or media channel in place of the chat: its posts, sorted by latest activity or
//! creation, filtered by tags, as a list (forum) or a grid (media). A post opens in the
//! thread panel.
//!
//! Where the posts come from (as Fluxer's client): the active posts the account state
//! knows from the gateway, and the archived ones from a thread search, a page at a time.
//! Changing the order or the tags starts the search over.
//!
//! Budget per opening: one search page (more on "Load more" or a new order or filter,
//! one at a time), one request for the first messages of the active posts that lack one,
//! and at most one op 28 for the posts' unread counts. Reading the forum (its new posts)
//! goes through the chat's gate, only while it is on screen with focus.

use std::collections::{HashMap, HashSet, VecDeque};

use gpui::{div, prelude::*, px, AnyElement, Context, FontWeight, Window};
use inlet_protocol as p;
use inlet_protocol::api::threads::ForumTag;
use inlet_state::threads::is_archived;
use inlet_state::{
    forum_post_list, AckKind, Changes, ChannelId, ForumSort, ForumUnreadRequests, GuildId,
    MessageId, PageBy, TagMatch, ThreadPages,
};

use crate::app::Inlet;
use crate::backend::messages;
use crate::backend::messages::threads::{self as backend, PostPreview, POST_DATA_MAX};
use crate::theme;

/// First-message previews kept per forum view (oldest dropped first).
const MAX_PREVIEWS: usize = 600;

/// The open forum or media channel.
pub struct ForumView {
    pub forum: ChannelId,
    pub guild: GuildId,
    /// A media channel: posts as a grid.
    pub media: bool,
    pub sort: ForumSort,
    /// Picked tag IDs, and whether a post needs one or all of them.
    pub tags: Vec<String>,
    pub how: TagMatch,
    /// Archived posts from the search.
    pub pages: ThreadPages,
    pub error: Option<String>,
    /// First messages by post ID.
    previews: HashMap<String, PostPreview>,
    preview_order: VecDeque<String>,
    /// Posts whose first message was asked for.
    asked: HashSet<String>,
    op28: ForumUnreadRequests,
    /// The forum's ack when it opened: posts after it are new.
    pub seen_until: Option<MessageId>,
}

impl ForumView {
    pub fn preview(&self, post: &str) -> Option<&PostPreview> {
        self.previews.get(post)
    }

    fn keep_previews(&mut self, previews: Vec<(String, PostPreview)>) {
        for (id, preview) in previews {
            if self.previews.insert(id.clone(), preview).is_none() {
                self.preview_order.push_back(id);
            }
            while self.previews.len() > MAX_PREVIEWS {
                match self.preview_order.pop_front() {
                    Some(old) => {
                        self.previews.remove(&old);
                        self.asked.remove(&old);
                    }
                    None => break,
                }
            }
        }
    }
}

impl Inlet {
    /// Whether the selected channel is a forum or media channel: the forum view shows
    /// in place of the chat even when it could not open (no account state yet, or no
    /// access), so the chat's composer never shows under a forum.
    pub(crate) fn forum_selected(&self) -> bool {
        self.selected_channel
            .as_ref()
            .is_some_and(|id| self.channels.iter().any(|c| &c.id == id && c.is_forum()))
    }

    /// Whether the forum view is up (its channel is the selected one).
    pub(crate) fn forum_is_open(&self) -> bool {
        self.threads
            .forum
            .as_ref()
            .is_some_and(|f| self.selected_channel.as_deref() == Some(f.forum.to_string().as_str()))
    }

    /// Opens `channel_id` in the forum view if it is a forum or media channel the
    /// account can view; anything else closes it. Returns whether it opened.
    pub(crate) fn open_forum(
        &mut self,
        channel_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.threads_leave_channel(Some(channel_id), cx);
        let is_forum = self
            .channels
            .iter()
            .any(|c| c.id == channel_id && c.is_forum());
        if !is_forum {
            return false;
        }
        // The chat's list empties; a forum has no messages of its own. The keyboard
        // leaves the chat's composer, which is not on screen.
        self.clear_messages();
        window.focus(&self.focus, cx);
        let (Some(account), Some(forum)) = (self.account.clone(), ChannelId::parse(channel_id))
        else {
            return true;
        };
        if self
            .threads
            .forum
            .as_ref()
            .is_some_and(|f| f.forum == forum)
        {
            return true;
        }
        let state = account.read(cx);
        let (Some(guild), Some(channel)) = (state.channels().guild_of(forum), state.channel(forum))
        else {
            return true;
        };
        if !state.permissions(guild, forum).can_view() {
            return true;
        }
        let media = channel.kind == p::models::channel_type::GUILD_MEDIA;
        let mut op28 = ForumUnreadRequests::default();
        op28.open(forum);
        self.threads.forum = Some(ForumView {
            forum,
            guild,
            media,
            sort: ForumSort::default_of(channel),
            tags: Vec::new(),
            how: TagMatch::default_of(channel),
            pages: ThreadPages::continuing(PageBy::Offset, self.threads.list_requests),
            error: None,
            previews: HashMap::new(),
            preview_order: VecDeque::new(),
            asked: HashSet::new(),
            op28,
            seen_until: state.ack_of(forum),
        });
        self.subscribe_threads();
        self.load_forum_page();
        self.ask_forum_extras(cx);
        self.read_forum(true, cx);
        true
    }

    /// The forum view goes (another channel, guild or sign-out).
    pub(super) fn close_forum(&mut self) {
        if let Some(mut f) = self.threads.forum.take() {
            f.op28.close();
        }
    }

    /// The next page of archived posts, if one may go now.
    fn load_forum_page(&mut self) {
        let Some(f) = self.threads.forum.as_mut() else {
            return;
        };
        let Some(next) = f.pages.next_page() else {
            return;
        };
        self.threads.list_requests = next.request;
        f.error = None;
        self.backend
            .send(messages::Cmd::Thread(backend::Cmd::ForumPage {
                forum_id: f.forum.to_string(),
                sort: f.sort.search_key(),
                tags: f.tags.clone(),
                tag_setting: f.how.search_key(),
                offset: next.offset,
                request: next.request,
            }));
    }

    /// Another order or tag filter: the archived list starts over.
    fn restart_forum_list(&mut self, cx: &mut Context<Self>) {
        if let Some(f) = self.threads.forum.as_mut() {
            f.pages.reset();
        }
        self.load_forum_page();
        cx.notify();
    }

    pub(super) fn set_forum_sort(&mut self, sort: ForumSort, cx: &mut Context<Self>) {
        let Some(f) = self.threads.forum.as_mut() else {
            return;
        };
        if f.sort != sort {
            f.sort = sort;
            self.restart_forum_list(cx);
        }
    }

    pub(super) fn toggle_forum_tag(&mut self, tag: String, cx: &mut Context<Self>) {
        let Some(f) = self.threads.forum.as_mut() else {
            return;
        };
        match f.tags.iter().position(|t| *t == tag) {
            Some(ix) => {
                f.tags.remove(ix);
            }
            None => f.tags.push(tag),
        }
        self.restart_forum_list(cx);
    }

    pub(super) fn on_forum_page(
        &mut self,
        forum_id: String,
        request: u64,
        result: Result<backend::ForumPage, String>,
        cx: &mut Context<Self>,
    ) {
        let Some(f) = self
            .threads
            .forum
            .as_mut()
            .filter(|f| f.forum.to_string() == forum_id)
        else {
            return;
        };
        match result {
            Ok(page) => {
                // Only public posts of this forum, whatever the server sent.
                let forum = f.forum;
                let posts: Vec<p::Channel> = page
                    .posts
                    .into_iter()
                    .filter(|t| {
                        t.kind == p::models::channel_type::PUBLIC_THREAD
                            && ChannelId::parse_opt(t.parent_id.as_deref()) == Some(forum)
                    })
                    .collect();
                if f.pages.on_page(request, posts, page.has_more) {
                    f.keep_previews(page.previews);
                }
            }
            Err(reason) => {
                if f.pages.on_failed(request) {
                    f.error = Some(reason);
                }
            }
        }
        self.ask_forum_extras(cx);
    }

    pub(super) fn on_post_data(&mut self, forum_id: String, previews: Vec<(String, PostPreview)>) {
        if let Some(f) = self
            .threads
            .forum
            .as_mut()
            .filter(|f| f.forum.to_string() == forum_id)
        {
            f.keep_previews(previews);
        }
    }

    /// For the posts on hand: the first messages that are missing (never twice per
    /// post), and op 28 once for this opening.
    fn ask_forum_extras(&mut self, cx: &mut Context<Self>) {
        let (Some(account), Some(f)) = (self.account.clone(), self.threads.forum.as_mut()) else {
            return;
        };
        let state = account.read(cx);
        let known = state.threads_of(f.forum);
        let list = forum_post_list(&known, f.pages.threads(), f.sort, &[], f.how);
        let posts: Vec<&p::Channel> = list
            .pinned
            .into_iter()
            .chain(list.active)
            .chain(list.archived)
            .collect();
        let missing: Vec<String> = posts
            .iter()
            .filter(|p| !f.previews.contains_key(&p.id) && !f.asked.contains(&p.id))
            .map(|p| p.id.clone())
            .take(POST_DATA_MAX)
            .collect();
        let unreads = f.op28.take(state, f.forum, &posts);
        let (forum_id, guild_id) = (f.forum.to_string(), f.guild.to_string());
        if !missing.is_empty() {
            f.asked.extend(missing.iter().cloned());
            self.backend
                .send(messages::Cmd::Thread(backend::Cmd::PostData {
                    forum_id: forum_id.clone(),
                    thread_ids: missing,
                }));
        }
        if let Some(posts) = unreads {
            self.backend
                .send(messages::Cmd::Thread(backend::Cmd::ForumUnreads {
                    guild_id,
                    forum_id,
                    posts,
                }));
        }
    }

    /// The forum's new posts are read while it is on screen with focus (Fluxer's
    /// `useForumViewing`), through the chat's gate.
    pub(super) fn read_forum(&mut self, at_open: bool, cx: &mut Context<Self>) {
        let Some(forum) = self.threads.forum.as_ref().map(|f| f.forum) else {
            return;
        };
        if !self.forum_is_open() || !self.notifications.window_active || self.layer_open() {
            return;
        }
        let kind = if at_open {
            AckKind::Explicit
        } else {
            AckKind::Automatic
        };
        let changes = self.read_channel(forum, kind, false, cx);
        if !changes.is_empty() {
            self.after_changes(changes, false, cx);
        }
    }

    /// The forum's channel or its posts changed: it closes when the account can no
    /// longer view it; new active posts may need their first messages.
    pub(super) fn forum_after_changes(&mut self, changes: &Changes, cx: &mut Context<Self>) {
        let (Some(account), Some(f)) = (self.account.clone(), self.threads.forum.as_ref()) else {
            return;
        };
        let (forum, guild) = (f.forum, f.guild);
        if !account.read(cx).permissions(guild, forum).can_view() {
            self.close_forum();
            self.chat.notice = Some(t!("threads-unavailable"));
            return;
        }
        if changes.reset {
            // A new session: op 28 may go once more (one sent while the gateway was
            // down was dropped).
            if let Some(f) = self.threads.forum.as_mut() {
                f.op28.open(forum);
            }
        }
        if !changes.channels.is_empty() || changes.reset {
            self.ask_forum_extras(cx);
        }
        if changes.read_state_channels.contains(&forum) {
            self.read_forum(false, cx);
        }
    }

    /// The forum view: the channel header, the order and tag bar, then the posts.
    pub(super) fn render_forum(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let header = self.render_chat_header(cx);
        let body = self.render_forum_body(cx);
        div()
            .flex_1()
            .h_full()
            .min_w_0()
            .flex()
            .flex_col()
            .bg(theme::bg_chat())
            .child(header)
            .children(self.chat.notice.clone().map(|n| {
                div()
                    .id("forum-notice")
                    .px_4()
                    .py_2()
                    .bg(theme::selected_bg())
                    .text_color(theme::text_strong())
                    .cursor_pointer()
                    .child(t!("chat-dismissable", text = n))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.chat.notice = None;
                        cx.notify();
                    }))
            }))
            .child(body)
    }

    fn render_forum_body(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let (Some(account), Some(f)) = (self.account.clone(), self.threads.forum.as_ref()) else {
            return div().into_any_element();
        };
        let state = account.read(cx);
        let Some(channel) = state.channel(f.forum) else {
            return div().into_any_element();
        };
        let tags: Vec<ForumTag> = channel.available_tags();
        let known = state.threads_of(f.forum);
        let list = forum_post_list(&known, f.pages.threads(), f.sort, &f.tags, f.how);
        let pinned = list.pinned.cloned();
        let active: Vec<p::Channel> = list.active.into_iter().cloned().collect();
        let archived: Vec<p::Channel> = list.archived.into_iter().cloned().collect();
        let (media, sort, picked) = (f.media, f.sort, f.tags.clone());
        let (loading, more, loaded, error) = (
            f.pages.is_loading(),
            f.pages.has_more(),
            f.pages.is_loaded(),
            f.error.clone(),
        );
        let toolbar = div()
            .px_4()
            .py_2()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap_2()
            .border_b_1()
            .border_color(theme::divider())
            .child(
                div()
                    .text_size(px(12.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::text_muted())
                    .child(t!("forum-sort-label").to_uppercase()),
            )
            .child(self.sort_button(ForumSort::LatestActivity, sort, cx))
            .child(self.sort_button(ForumSort::CreationTime, sort, cx))
            .children(super::tags::filter_bar(self, &tags, &picked, cx));
        let mut items: Vec<AnyElement> = Vec::new();
        let posts: Vec<p::Channel> = pinned.into_iter().chain(active).collect();
        if media {
            items.push(self.media_grid(&posts, &tags, cx));
        } else {
            for post in &posts {
                items.push(self.post_card(post, &tags, cx));
            }
        }
        if posts.is_empty() && archived.is_empty() && (loaded || error.is_some()) && !more {
            items.push(quiet(t!("forum-no-posts")));
        }
        if !archived.is_empty() {
            items.push(
                div()
                    .pt_4()
                    .pb_1()
                    .text_size(px(12.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::text_muted())
                    .child(t!("forum-archived-posts").to_uppercase())
                    .into_any_element(),
            );
            if media {
                items.push(self.media_grid(&archived, &tags, cx));
            } else {
                for post in &archived {
                    items.push(self.post_card(post, &tags, cx));
                }
            }
        }
        if let Some(e) = error {
            items.push(quiet(t!("threads-error-list", error = e)));
        }
        if loading {
            items.push(quiet(t!("threads-loading")));
        } else if more {
            let label = if loaded {
                t!("threads-load-more")
            } else {
                t!("threads-retry")
            };
            items.push(
                div()
                    .id("forum-load-more")
                    .my_2()
                    .px_3()
                    .py_1()
                    .rounded(px(6.))
                    .flex()
                    .justify_center()
                    .cursor_pointer()
                    .bg(theme::bg_textarea())
                    .hover(|s| s.opacity(0.85))
                    .text_color(theme::text_strong())
                    .child(label)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.load_forum_page();
                        cx.notify();
                    }))
                    .into_any_element(),
            );
        }
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(toolbar)
            .child(
                div()
                    .id("forum-posts")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_4()
                    .py_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .children(items),
            )
            .into_any_element()
    }

    fn sort_button(
        &self,
        sort: ForumSort,
        current: ForumSort,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let label = match sort {
            ForumSort::LatestActivity => t!("forum-sort-latest"),
            ForumSort::CreationTime => t!("forum-sort-created"),
        };
        let id = match sort {
            ForumSort::LatestActivity => "forum-sort-latest",
            ForumSort::CreationTime => "forum-sort-created",
        };
        let on = sort == current;
        div()
            .id(id)
            .px_2()
            .py(px(2.))
            .rounded(px(6.))
            .text_size(px(13.))
            .cursor_pointer()
            .text_color(if on {
                theme::text_strong()
            } else {
                theme::text_muted()
            })
            .when(on, |d| d.bg(theme::selected_bg()))
            .hover(|s| s.bg(theme::hover_bg()))
            .child(label)
            .on_click(cx.listener(move |this, _, _, cx| this.set_forum_sort(sort, cx)))
            .into_any_element()
    }

    /// A post was clicked: it is stored (as the search gave it) and opens in the panel.
    pub(super) fn open_post(
        &mut self,
        post: p::Channel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = ChannelId::parse(&post.id) else {
            return;
        };
        if let Some(account) = self.account.clone() {
            if account.read(cx).channel(id).is_none() {
                let changes = account.update(cx, |s, _| s.upsert_thread(&post));
                self.after_changes(changes, false, cx);
            }
        }
        self.open_thread(id, window, cx);
    }

    /// Whether a post came after the forum was last read (and is not the account's own).
    pub(super) fn is_new_post(&self, post: &p::Channel, cx: &gpui::App) -> bool {
        let Some(f) = self.threads.forum.as_ref() else {
            return false;
        };
        let me = self
            .account
            .as_ref()
            .map(|a| a.read(cx).me_id().to_string());
        if post.owner_id().is_some_and(|o| Some(o) == me) || is_archived(post) {
            return false;
        }
        let id = ChannelId::parse(&post.id).map(|c| c.0);
        matches!((id, f.seen_until), (Some(id), Some(seen)) if id > seen.0)
    }
}

fn quiet(text: String) -> AnyElement {
    div()
        .py_2()
        .text_size(px(14.))
        .text_color(theme::text_muted())
        .child(text)
        .into_any_element()
}
