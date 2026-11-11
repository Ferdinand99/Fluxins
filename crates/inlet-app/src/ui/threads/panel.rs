// SPDX-License-Identifier: AGPL-3.0-or-later
//! The thread side panel: a thread or forum post next to the chat, with its own message
//! list and its own composer.
//!
//! - The list is the chat's own [`MessageView`] with a second [`ListState`]: the same
//!   rows, conversions, pending sends and read rules, drawn by the same row renderer.
//! - The composer is a second [`ComposerView`] sharing the drafts with the chat's
//!   (drafts are kept per channel, so the thread's draft and the channel's never mix).
//!   Its sends show as pending rows in the panel, never in the chat.
//! - Reading follows the chat's rules: the panel's thread is marked read only while the
//!   chat is on screen with no layer over it, the window has focus, and the panel's
//!   list shows its newest messages at the bottom. Acks go through the same gate.
//! - Access fails closed: the panel opens only on a thread the account can view, and
//!   closes as soon as the account state says it no longer can (left a private thread,
//!   lost the channel).
//! - Escape closes the panel (after the composer's own reply or edit), before anything
//!   would be marked read.

use gpui::{
    div, list, prelude::*, px, AnyElement, App, Context, Entity, FollowMode, FontWeight,
    ListAlignment, ListScrollEvent, ListState, Window,
};
use inlet_protocol as p;
use inlet_state::{AccountState, AckKind, Changes, ChannelId, GuildId, MessageId, MessagesPage};

use super::{ThreadIcon, PANEL_WIDTH};
use crate::app::Inlet;
use crate::backend::composer::{Cmd as ComposerCmd, Ev as ComposerEv, Outgoing};
use crate::backend::messages::{self, threads as backend, Pending};
use crate::model::{self, Message};
use crate::theme;
use crate::ui::chat::messages::{splice_range, MessageView};
use crate::ui::composer::{begin_send, edit_last, ChannelContext, ComposerEvent, ComposerView};
use crate::ui::HEADER_HEIGHT;

/// The message length limit until the instance's own is known (as the chat's composer).
const DEFAULT_MAX_LEN: usize = 2000;
/// `/msg` sends of the panel's composer remembered for their failure.
const MAX_DM_NONCES: usize = 8;

/// The thread open in the side panel.
pub struct Panel {
    pub thread: ChannelId,
    pub guild: GuildId,
    /// The channel, forum or media channel the thread belongs to.
    pub parent: ChannelId,
    /// Rows and read rules, as in the chat.
    pub view: MessageView,
    /// The panel's own list window.
    pub list: ListState,
    scroll_hooked: bool,
}

impl Panel {
    pub fn new(thread: ChannelId, guild: GuildId, parent: ChannelId, compact: bool) -> Self {
        let list = ListState::new(0, ListAlignment::Bottom, px(800.));
        list.set_follow_mode(FollowMode::Tail);
        let mut view = MessageView::new();
        view.compact = compact;
        Panel {
            thread,
            guild,
            parent,
            view,
            list,
            scroll_hooked: false,
        }
    }
}

/// Where a thread may open: its guild and parent, when the account can view it. `None`
/// for anything else (unknown, not a thread, hidden, a private thread the account is not
/// in): fails closed.
pub fn can_open(state: &AccountState, thread: ChannelId) -> Option<(GuildId, ChannelId)> {
    let t = state.channel(thread)?;
    if !t.is_thread() {
        return None;
    }
    let guild = state.channels().guild_of(thread)?;
    let parent = state.channels().parent_of(thread)?;
    state
        .permissions(guild, thread)
        .can_view()
        .then_some((guild, parent))
}

/// The panel composer's channel: the thread, its guild and slowmode, "Message <name>".
pub fn thread_context(state: &AccountState, thread: ChannelId) -> ChannelContext {
    let t = state.channel(thread);
    let name = model::clean_name(t.and_then(|t| t.name.as_deref()).unwrap_or_default());
    ChannelContext {
        channel_id: thread.to_string(),
        guild_id: state.channels().guild_of(thread).map(|g| g.to_string()),
        placeholder: t!("threads-composer-placeholder", thread = name),
        slowmode_secs: t
            .and_then(|t| t.rate_limit_per_user)
            .filter(|s| (1..=21_600).contains(s))
            .map_or(0, |s| s as u32),
    }
}

/// A send from the panel's composer: its pending row goes in the panel's list, and the
/// returned command sends it. `None` for a send to anything but the panel's thread.
pub fn send_from_panel(panel: &mut Panel, message: Outgoing) -> Option<ComposerCmd> {
    (message.channel_id == panel.thread.to_string()).then(|| begin_send(&mut panel.view, message))
}

/// The second composer: the same drafts as the chat's composer.
pub fn new_thread_composer(
    shared: std::rc::Rc<std::cell::RefCell<crate::ui::composer::Shared>>,
    max_len: usize,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ComposerView> {
    cx.new(|cx| ComposerView::new(shared, max_len, window, cx))
}

impl Inlet {
    /// Opens a thread in the side panel (in place of the one there). Does nothing but
    /// say so when the account cannot view it.
    pub(crate) fn open_thread(
        &mut self,
        thread: ChannelId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(account) = self.account.clone() else {
            return;
        };
        let Some((guild, parent)) = can_open(account.read(cx), thread) else {
            self.chat.notice = Some(t!("threads-unavailable"));
            cx.notify();
            return;
        };
        self.threads.browser = None;
        if self
            .threads
            .panel
            .as_ref()
            .is_some_and(|p| p.thread == thread)
        {
            return;
        }
        let compact = self.chat.view.compact;
        self.threads.panel = Some(Panel::new(thread, guild, parent, compact));
        let composer = self.thread_composer(window, cx);
        let ctx = thread_context(account.read(cx), thread);
        let weak = account.downgrade();
        composer.update(cx, |v, cx| {
            v.set_channel(ctx, window, cx);
            v.set_account(Some(weak), cx);
        });
        self.load_panel_newest(cx);
        self.subscribe_threads();
        cx.notify();
    }

    /// The panel's composer, made on first use with the chat composer's drafts.
    fn thread_composer(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ComposerView> {
        if let Some(view) = &self.threads.composer {
            return view.clone();
        }
        let max_len = self.threads.max_len.unwrap_or(DEFAULT_MAX_LEN);
        let view = new_thread_composer(self.composer.shared.clone(), max_len, window, cx);
        self.threads.composer_events = Some(cx.subscribe_in(
            &view,
            window,
            |this, _, event: &ComposerEvent, window, cx| {
                this.on_panel_composer_event(event.clone(), window, cx)
            },
        ));
        self.threads.composer = Some(view.clone());
        view
    }

    /// The close button or Escape: the panel goes and the chat's composer has the keyboard.
    pub(crate) fn close_thread_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.threads.panel.is_none() {
            return;
        }
        self.drop_thread_panel(cx);
        self.focus_center(window, cx);
    }

    /// Closes the panel without moving the keyboard.
    pub(crate) fn drop_thread_panel(&mut self, cx: &mut Context<Self>) {
        if self.threads.panel.take().is_some() {
            self.subscribe_threads();
            cx.notify();
        }
    }

    /// Opens the thread's window in the account state and asks for its newest page.
    fn load_panel_newest(&mut self, cx: &mut Context<Self>) {
        let (Some(account), Some(panel)) = (self.account.clone(), self.threads.panel.as_mut())
        else {
            return;
        };
        let ch = panel.thread;
        let changes = account.update(cx, |s, _| {
            let c = s.begin_load(ch);
            s.touch_channel(ch);
            c
        });
        let request = panel.view.begin_newest(ch);
        self.threads.loads.insert(ch, request);
        self.backend
            .send(messages::Cmd::Thread(backend::Cmd::OpenThread {
                thread_id: ch.to_string(),
                request,
            }));
        self.after_changes(changes, false, cx);
        self.refresh_panel_rows(cx);
    }

    /// The person scrolled to the top of the panel: the page before, within the budget.
    fn load_panel_older(&mut self, cx: &mut Context<Self>) {
        let (Some(account), Some(panel)) = (self.account.clone(), self.threads.panel.as_mut())
        else {
            return;
        };
        let ch = panel.thread;
        if let Some((before, request)) = panel.view.older_request(account.read(cx), ch) {
            self.backend
                .send(messages::Cmd::Thread(backend::Cmd::LoadOlder {
                    thread_id: ch.to_string(),
                    before: before.to_string(),
                    request,
                }));
        }
    }

    pub(super) fn on_panel_page(
        &mut self,
        thread_id: String,
        request: u64,
        page: MessagesPage,
        rendered: Vec<Message>,
        window_active: bool,
        cx: &mut Context<Self>,
    ) {
        let (Some(ch), Some(account)) = (ChannelId::parse(&thread_id), self.account.clone()) else {
            return;
        };
        let older = page.before.is_some();
        let open = self.threads.panel.as_ref().is_some_and(|p| p.thread == ch);
        if older {
            // An older page is only for the panel that asked.
            let Some(panel) = self.threads.panel.as_mut().filter(|_| open) else {
                return;
            };
            panel.view.older_done(request);
        } else {
            // Every newest page pairs a `begin_load`, open or not; a replaced one is dropped.
            if self.threads.loads.get(&ch) != Some(&request) {
                return;
            }
            self.threads.loads.remove(&ch);
        }
        let raw = page.messages.clone();
        let mut changes = account.update(cx, |s, _| s.apply_messages(ch, page));
        if let Some(panel) = self.threads.panel.as_mut().filter(|_| open) {
            panel.view.remember(account.read(cx), &raw, rendered);
            if !older && panel.view.accept_newest(ch, request) {
                panel.view.set_loaded();
                if self.panel_may_read(window_active, cx) {
                    changes.merge(self.read_channel(ch, AckKind::Explicit, false, cx));
                }
            }
        }
        self.after_changes(changes, false, cx);
        self.refresh_panel_rows(cx);
    }

    pub(super) fn on_panel_page_failed(
        &mut self,
        thread_id: String,
        request: u64,
        older: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(ch) = ChannelId::parse(&thread_id) else {
            return;
        };
        if older {
            if let Some(panel) = self.threads.panel.as_mut().filter(|p| p.thread == ch) {
                panel.view.older_done(request);
            }
            return;
        }
        if self.threads.loads.get(&ch) != Some(&request) {
            return;
        }
        self.threads.loads.remove(&ch);
        if let Some(panel) = self.threads.panel.as_mut().filter(|p| p.thread == ch) {
            panel.view.newest_failed(ch, request);
        }
        if let Some(account) = self.account.clone() {
            account.update(cx, |s, _| s.cancel_load(ch));
        }
        self.refresh_panel_rows(cx);
    }

    /// Converted messages of a gateway event that belong to the panel's thread go to its
    /// list; the rest is returned for the chat.
    pub(crate) fn panel_take_rendered(
        &mut self,
        dispatch: &p::Dispatch,
        rendered: Vec<Message>,
        cx: &mut Context<Self>,
    ) -> Vec<Message> {
        let (Some(account), Some(panel)) = (self.account.clone(), self.threads.panel.as_mut())
        else {
            return rendered;
        };
        let thread = panel.thread.to_string();
        let (mine, rest): (Vec<Message>, Vec<Message>) =
            rendered.into_iter().partition(|m| m.channel_id == thread);
        if !mine.is_empty() {
            let raw: Vec<&p::Message> = match dispatch {
                p::Dispatch::MessageCreate(m) | p::Dispatch::MessageUpdate(m) => vec![&**m],
                _ => Vec::new(),
            };
            panel.view.remember(account.read(cx), raw, mine);
        }
        rest
    }

    /// A gateway event, after the state took it: a message in the panel's thread clears
    /// its pending row, counts while the list misses its newest messages, and is read
    /// when the panel is watched.
    pub(crate) fn panel_on_dispatch(
        &mut self,
        dispatch: &p::Dispatch,
        window_active: bool,
        changes: &mut Changes,
        cx: &mut Context<Self>,
    ) {
        let p::Dispatch::MessageCreate(m) = dispatch else {
            return;
        };
        let (Some(account), Some(panel)) = (self.account.clone(), self.threads.panel.as_mut())
        else {
            return;
        };
        let ch = panel.thread;
        if ChannelId::parse(&m.channel_id) != Some(ch) {
            return;
        }
        if let Some(nonce) = &m.nonce {
            panel.view.sent(nonce);
        }
        panel.view.note_live(account.read(cx), ch);
        if self.panel_may_read(window_active, cx) {
            changes.merge(self.read_channel(ch, AckKind::Automatic, false, cx));
        }
        self.refresh_panel_rows(cx);
    }

    /// Re-renders the panel when its thread's messages, names or members changed; closes
    /// it when the account can no longer view the thread; loads its newest page again
    /// after a new READY or when its window was evicted.
    pub(super) fn panel_after_changes(
        &mut self,
        changes: &Changes,
        members: bool,
        cx: &mut Context<Self>,
    ) {
        let (Some(account), Some(panel)) = (self.account.clone(), self.threads.panel.as_mut())
        else {
            return;
        };
        let ch = panel.thread;
        panel.view.prune(account.read(cx), changes);
        if can_open(account.read(cx), ch).is_none() {
            self.threads.panel = None;
            self.chat.notice = Some(t!("threads-unavailable"));
            self.subscribe_threads();
            cx.notify();
            return;
        }
        let window_gone =
            account.read(cx).messages(ch).is_none() && !self.threads.loads.contains_key(&ch);
        if changes.reset || window_gone {
            self.load_panel_newest(cx);
            return;
        }
        let relevant = changes.messages.contains(&ch)
            || changes.guilds
            || changes.channels.contains(&ch)
            || (members && changes.member_guilds.contains(&panel.guild));
        if relevant {
            self.refresh_panel_rows(cx);
        }
    }

    /// Whether the panel's thread may be marked read now: the chat is on screen with no
    /// layer over it, the window has focus, and the panel shows its complete newest page
    /// at the bottom.
    fn panel_may_read(&self, window_active: bool, cx: &App) -> bool {
        let (Some(account), Some(panel)) = (self.account.as_ref(), self.threads.panel.as_ref())
        else {
            return false;
        };
        self.chat_is_visible()
            && !self.layer_open()
            && panel.view.may_mark_read(
                account.read(cx),
                panel.thread,
                panel.list.is_following_tail(),
                window_active,
            )
    }

    /// Once a second (the chat's tick): the panel's thread is read if it is watched.
    pub(crate) fn tick_thread_panel(&mut self, cx: &mut Context<Self>) {
        let Some(ch) = self.threads.panel.as_ref().map(|p| p.thread) else {
            return;
        };
        if !self.panel_may_read(self.notifications.window_active, cx) {
            return;
        }
        let changes = self.read_channel(ch, AckKind::Automatic, false, cx);
        if !changes.is_empty() {
            self.after_changes(changes, false, cx);
            cx.notify();
        }
    }

    /// Rebuilds the panel's rows and splices its list.
    pub(crate) fn refresh_panel_rows(&mut self, cx: &mut Context<Self>) {
        let (Some(account), Some(panel)) = (self.account.clone(), self.threads.panel.as_mut())
        else {
            return;
        };
        let new = panel.view.build_rows(account.read(cx), panel.thread);
        if panel.view.rows.is_empty() {
            panel.list.reset(new.len());
            panel.list.set_follow_mode(FollowMode::Tail);
        } else if let Some((range, count)) = splice_range(&panel.view.rows, &new) {
            panel.list.splice(range, count);
        }
        panel.view.rows = new;
    }

    /// A pending row of the panel's thread goes in the panel. Returns the row when it is
    /// the chat's.
    pub(crate) fn panel_pending(
        &mut self,
        out: Pending,
        cx: &mut Context<Self>,
    ) -> Option<Pending> {
        let panel =
            self.threads.panel.as_mut().filter(|p| {
                p.thread.to_string() == out.channel_id || p.view.has_pending(&out.nonce)
            });
        let Some(panel) = panel else {
            return Some(out);
        };
        panel.view.upsert_pending(out);
        self.refresh_panel_rows(cx);
        None
    }

    /// The server took the message with this nonce.
    pub(crate) fn panel_sent(&mut self, nonce: &str, cx: &mut Context<Self>) {
        if let Some(panel) = self.threads.panel.as_mut() {
            if panel.view.sent(nonce) {
                self.refresh_panel_rows(cx);
                cx.notify();
            }
        }
    }

    /// Retry on a failed row of the panel; whether the row was the panel's.
    pub(crate) fn panel_retry(&mut self, nonce: &str, cx: &mut Context<Self>) -> bool {
        let Some(panel) = self
            .threads
            .panel
            .as_mut()
            .filter(|p| p.view.has_pending(nonce))
        else {
            return false;
        };
        if let Some(cmd) = panel.view.retry(nonce) {
            if let messages::Cmd::Resend { channel_id, .. } = &cmd {
                if let Some(view) = &self.threads.composer {
                    let channel_id = channel_id.clone();
                    view.update(cx, |v, cx| v.on_resend(&channel_id, cx));
                }
            }
            self.backend.send(cmd);
            self.refresh_panel_rows(cx);
        }
        cx.notify();
        true
    }

    /// The spoilers of a message are shown, in the panel too.
    pub(crate) fn panel_reveal(&mut self, id: MessageId, cx: &mut Context<Self>) {
        if let Some(panel) = self.threads.panel.as_mut() {
            if panel.view.reveal(id) {
                self.refresh_panel_rows(cx);
            }
        }
    }

    /// The instance's message length limit is known.
    pub(crate) fn panel_session(
        &mut self,
        max_len: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.threads.max_len = Some(max_len);
        if let Some(view) = self.threads.composer.clone() {
            // The chat's composer keeps the drafts file; this one only takes the limit.
            view.update(cx, |v, cx| v.set_session(max_len, None, window, cx));
        }
    }

    /// The composer backend's answers that concern the panel's composer go to it. Returns
    /// the event when the chat's composer should have it too (or instead).
    pub(crate) fn panel_composer_event(
        &mut self,
        event: ComposerEv,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<ComposerEv> {
        let Some(view) = self.threads.composer.clone() else {
            return Some(event);
        };
        match event {
            ComposerEv::SlowmodeWait { channel_id, secs } => {
                view.update(cx, |v, cx| v.on_slowmode_wait(&channel_id, secs, cx));
                Some(ComposerEv::SlowmodeWait { channel_id, secs })
            }
            ComposerEv::Members { guild_id, members } => {
                let (g, m) = (guild_id.clone(), members.clone());
                view.update(cx, |v, cx| v.on_members(g, m, cx));
                Some(ComposerEv::Members { guild_id, members })
            }
            ComposerEv::Edited { message_id } => {
                view.update(cx, |v, cx| v.on_edited(&message_id, window, cx));
                Some(ComposerEv::Edited { message_id })
            }
            ComposerEv::EditFailed { message_id, reason } => {
                let shown = view.update(cx, |v, cx| v.on_edit_failed(&message_id, &reason, cx));
                (!shown).then_some(ComposerEv::EditFailed { message_id, reason })
            }
            ComposerEv::DirectMessageFailed { nonce, reason } => {
                if let Some(ix) = self.threads.dm_nonces.iter().position(|n| *n == nonce) {
                    self.threads.dm_nonces.remove(ix);
                    view.update(cx, |v, cx| {
                        v.on_direct_message_failed(&nonce, reason, window, cx)
                    });
                    None
                } else {
                    Some(ComposerEv::DirectMessageFailed { nonce, reason })
                }
            }
            other => Some(other),
        }
    }

    /// What the panel's composer asks of the app.
    fn on_panel_composer_event(
        &mut self,
        event: ComposerEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            ComposerEvent::Send(message) => {
                let Some(panel) = self.threads.panel.as_mut() else {
                    return;
                };
                if let Some(cmd) = send_from_panel(panel, message) {
                    self.refresh_panel_rows(cx);
                    self.backend.send(cmd);
                }
            }
            ComposerEvent::Edit {
                channel_id,
                message_id,
                content,
                mentions,
            } => self.backend.send(ComposerCmd::Edit {
                channel_id,
                message_id,
                content,
                mentions,
            }),
            ComposerEvent::Typing { channel_id } => {
                self.backend.send(ComposerCmd::Typing { channel_id })
            }
            ComposerEvent::SearchMembers {
                guild_id,
                query,
                nonce,
            } => self.backend.send(ComposerCmd::SearchMembers {
                guild_id,
                query,
                nonce,
            }),
            ComposerEvent::SetNick { guild_id, nick } => {
                self.backend.send(ComposerCmd::SetNick { guild_id, nick })
            }
            ComposerEvent::DirectMessage { user_id, message } => {
                if self.threads.dm_nonces.len() >= MAX_DM_NONCES {
                    self.threads.dm_nonces.remove(0);
                }
                self.threads.dm_nonces.push(message.nonce.clone());
                self.backend
                    .send(ComposerCmd::DirectMessage { user_id, message })
            }
            ComposerEvent::EditLast { channel_id } => {
                let thread = self.threads.panel.as_ref().map(|p| p.thread);
                let (Some(account), Some(view), Some(thread)) =
                    (self.account.clone(), self.threads.composer.clone(), thread)
                else {
                    return;
                };
                if thread.to_string() == channel_id {
                    edit_last(&view, &account, thread, window, cx);
                }
            }
        }
    }

    /// Follows scrolling in the panel: older messages at the top, reading at the bottom.
    /// The handler runs while gpui holds the list borrowed, so it only defers; nothing
    /// touches the list inside it.
    fn hook_panel_scroll(&mut self, cx: &mut Context<Self>) {
        let Some(panel) = self.threads.panel.as_mut() else {
            return;
        };
        if panel.scroll_hooked {
            return;
        }
        panel.scroll_hooked = true;
        let thread = panel.thread;
        let weak = cx.entity().downgrade();
        panel.list.set_scroll_handler(
            move |event: &ListScrollEvent, window: &mut Window, cx: &mut App| {
                let at_top = event.visible_range.start <= 1;
                let at_bottom = event.visible_range.end >= event.count;
                if !at_top && !at_bottom {
                    return;
                }
                let active = window.is_window_active();
                let weak = weak.clone();
                cx.defer(move |cx| {
                    let _ = weak.update(cx, |this, cx| {
                        if this.threads.panel.as_ref().map(|p| p.thread) != Some(thread) {
                            return;
                        }
                        if at_top {
                            this.load_panel_older(cx);
                        }
                        if at_bottom && this.panel_may_read(active, cx) {
                            let changes = this.read_channel(thread, AckKind::Automatic, false, cx);
                            this.after_changes(changes, false, cx);
                        }
                        cx.notify();
                    });
                });
            },
        );
    }

    fn render_panel_row(&mut self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let row = self
            .threads
            .panel
            .as_ref()
            .and_then(|p| Some((p.view.rows.get(ix)?.clone(), p.view.compact)));
        match row {
            Some((row, compact)) => self.render_row_of(row, compact, cx),
            None => div().into_any_element(),
        }
    }

    /// The panel: a header with the thread's name and a close button, its messages and
    /// its composer.
    pub(super) fn render_thread_panel(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.hook_panel_scroll(cx);
        let panel = self.threads.panel.as_ref()?;
        let state = self.account.as_ref()?.read(cx);
        let thread = state.channel(panel.thread)?;
        let title = model::clean_name(thread.name.as_deref().unwrap_or_default());
        // A forum post shows no thread icon (as in Fluxer).
        let post = state
            .channel(panel.parent)
            .is_some_and(|parent| parent.is_forum());
        let list_state = panel.list.clone();
        let composer = self.threads.composer.clone();
        Some(
            div()
                .id("thread-panel")
                .w(px(PANEL_WIDTH))
                .h_full()
                .flex_none()
                .flex()
                .flex_col()
                .border_l_1()
                .border_color(theme::divider())
                .bg(theme::bg_chat())
                .child(
                    div()
                        .h(px(HEADER_HEIGHT))
                        .flex_none()
                        .px_4()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .border_b_1()
                        .border_color(theme::divider())
                        .when(!post, |d| {
                            d.child(ThreadIcon::Chats.el(px(24.), theme::text_muted()))
                        })
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .font_weight(FontWeight::MEDIUM)
                                .line_height(px(24.))
                                .text_color(theme::text_strong())
                                .child(title),
                        )
                        .child(
                            div()
                                .id("thread-panel-close")
                                .flex_none()
                                .p(px(6.))
                                .rounded(px(6.))
                                .cursor_pointer()
                                .hover(|s| s.bg(theme::hover_bg()))
                                .child(ThreadIcon::Close.el(px(20.), theme::text_muted()))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.close_thread_panel(window, cx)
                                })),
                        ),
                )
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .w_full()
                        .overflow_hidden()
                        .flex()
                        .flex_col()
                        .child(
                            list(
                                list_state,
                                cx.processor(|this, ix: usize, _window, cx| {
                                    this.render_panel_row(ix, cx)
                                }),
                            )
                            .flex_1()
                            .w_full(),
                        ),
                )
                .children(composer)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::chat::messages::Row;
    use crate::ui::composer::drafts::Draft;
    use crate::ui::composer::{bind_keys, Shared};
    use gpui::TestAppContext;
    use std::cell::RefCell;
    use std::rc::Rc;

    const GUILD: &str = "100";
    const GENERAL: &str = "301";
    const THREAD: &str = "501";
    const PRIVATE: &str = "502";

    fn state() -> AccountState {
        let ready: p::Ready = serde_json::from_value(serde_json::json!({
            "session_id": "s",
            "user": {"id": "10", "username": "me", "mfa_enabled": false},
            "guilds": [{
                "id": GUILD,
                "properties": {"id": GUILD, "name": "g", "owner_id": "1"},
                "roles": [{"id": GUILD, "name": "@everyone", "position": 0,
                           "permissions": "274877975552"}],
                "channels": [{"id": GENERAL, "type": 0, "name": "general", "position": 0}],
                "threads": [
                    {"id": THREAD, "type": 11, "parent_id": GENERAL, "name": "release plans",
                     "rate_limit_per_user": 30},
                    {"id": PRIVATE, "type": 12, "parent_id": GENERAL, "name": "secret"}
                ],
                "members": [{"user": {"id": "10", "username": "me"}, "roles": [],
                             "joined_at": "2026-01-01T00:00:00Z"}]
            }],
            "private_channels": [], "users": [], "read_states": [], "user_guild_settings": []
        }))
        .unwrap();
        AccountState::from_ready_at(&ready, 1_800_000_000_000)
    }

    struct Host {
        main: Entity<ComposerView>,
        thread: Entity<ComposerView>,
        events: Vec<(bool, ComposerEvent)>,
    }

    impl Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .w(px(900.))
                .flex()
                .flex_row()
                .child(div().flex_1().child(self.main.clone()))
                .child(div().w(px(PANEL_WIDTH)).child(self.thread.clone()))
        }
    }

    fn draft(shared: &Rc<RefCell<Shared>>, channel: &str) -> Option<String> {
        shared.borrow().drafts.get(channel).map(|d| d.wire.clone())
    }

    fn put_draft(shared: &Rc<RefCell<Shared>>, channel: &str, text: &str) {
        shared.borrow_mut().drafts.put(
            channel,
            Some(Draft {
                wire: text.into(),
                labels: Vec::new(),
                seq: 0,
            }),
        );
    }

    #[gpui::test]
    fn thread_opens_in_a_side_panel_with_its_own_composer(cx: &mut TestAppContext) {
        let state = state();
        let thread = ChannelId::parse(THREAD).unwrap();
        // A private thread without membership never opens: fails closed.
        assert_eq!(can_open(&state, ChannelId::parse(PRIVATE).unwrap()), None);
        assert_eq!(
            can_open(&state, ChannelId::parse(GENERAL).unwrap()),
            None,
            "no thread"
        );
        let (guild, parent) = can_open(&state, thread).expect("a public thread opens");
        assert_eq!(parent.to_string(), GENERAL);
        let mut panel = Panel::new(thread, guild, parent, false);

        // The chat's composer and the panel's, sharing the drafts, in one window with the
        // app's key bindings.
        cx.update(crate::ui::notifications::bind_keys);
        cx.update(bind_keys);
        let shared: Rc<RefCell<Shared>> = Rc::default();
        let ctx = thread_context(&state, thread);
        assert_eq!(ctx.channel_id, THREAD);
        assert_eq!(ctx.guild_id.as_deref(), Some(GUILD));
        assert_eq!(ctx.slowmode_secs, 30, "the thread's own slowmode");
        let (host, cx) = cx.add_window_view(|window, cx| {
            let main = cx.new(|cx| ComposerView::new(shared.clone(), 2000, window, cx));
            let thread = new_thread_composer(shared.clone(), 2000, window, cx);
            for (is_thread, view) in [(false, &main), (true, &thread)] {
                cx.subscribe(view, move |host: &mut Host, _, e: &ComposerEvent, _| {
                    host.events.push((is_thread, e.clone()))
                })
                .detach();
            }
            Host {
                main,
                thread,
                events: Vec::new(),
            }
        });
        let (main, thread_view) = host.read_with(cx, |h, _| (h.main.clone(), h.thread.clone()));
        // Text in both boxes: the chat's draft, and the thread's (as drafts the composers
        // bring back when they open their channel).
        put_draft(&shared, GENERAL, "a draft for the channel");
        put_draft(&shared, THREAD, "hello thread");
        main.update_in(cx, |v, window, cx| {
            v.set_channel(
                ChannelContext {
                    channel_id: GENERAL.into(),
                    guild_id: Some(GUILD.into()),
                    placeholder: "#general".into(),
                    slowmode_secs: 0,
                },
                window,
                cx,
            )
        });
        // Opening the thread focuses the panel's composer; Enter sends from it alone.
        thread_view.update_in(cx, |v, window, cx| v.set_channel(ctx.clone(), window, cx));
        cx.simulate_keystrokes("enter");

        let sends: Vec<(bool, Outgoing)> = host.read_with(cx, |h, _| {
            h.events
                .iter()
                .filter_map(|(t, e)| match e {
                    ComposerEvent::Send(o) => Some((*t, o.clone())),
                    _ => None,
                })
                .collect()
        });
        assert_eq!(sends.len(), 1, "one send, from the focused composer only");
        let (from_thread, out) = sends[0].clone();
        assert!(from_thread, "the panel's composer sent it");
        assert_eq!(out.channel_id, THREAD);
        assert_eq!(out.content, "hello thread");
        assert_eq!(
            draft(&shared, THREAD),
            None,
            "the sent text is no draft any more"
        );
        assert_eq!(
            draft(&shared, GENERAL).as_deref(),
            Some("a draft for the channel"),
            "the chat's draft is untouched"
        );

        // The send shows as a pending row in the panel's own list, never in the chat's.
        let mut chat = MessageView::new();
        let cmd = send_from_panel(&mut panel, out.clone()).expect("the panel's thread");
        assert!(matches!(cmd, ComposerCmd::Send(ref o) if o.channel_id == THREAD));
        let rows = panel.view.build_rows(&state, thread);
        assert!(rows
            .iter()
            .any(|r| matches!(r, Row::Pending(p) if p.nonce == out.nonce)));
        let general = ChannelId::parse(GENERAL).unwrap();
        assert!(!chat
            .build_rows(&state, general)
            .iter()
            .any(|r| matches!(r, Row::Pending(_))));
        // A send for another channel is not the panel's.
        let mut other = out.clone();
        other.channel_id = GENERAL.into();
        assert!(send_from_panel(&mut panel, other).is_none());
    }
}
