// SPDX-License-Identifier: AGPL-3.0-or-later
//! Threads, forum and media channels.
//!
//! - `panel`: a thread (or forum post) opens in a side panel next to the chat, with its
//!   own message list (the chat's [`MessageView`](crate::ui::chat::messages::MessageView))
//!   and its own composer (a second [`ComposerView`]).
//! - `list`: the thread list of a text channel (the header button): active threads from
//!   the account state, archived ones a page at a time.
//! - `forum`, `forum_post`, `media_grid`, `tags`: a forum or media channel replaces the
//!   chat with its posts, sorted and filtered by tag, as a list or a grid.
//!
//! What the account may see comes from the account state, which fails closed: a private
//! thread without membership is never listed or opened, and the panel closes the moment
//! the account loses access. Thread events for a guild are subscribed to (op 14) only
//! once the person opens a thread list, a thread or a forum there.

mod forum;
mod forum_post;
mod list;
mod media_grid;
pub mod panel;
mod tags;

use std::collections::HashMap;

use gpui::{div, prelude::*, px, AnyElement, Context, Entity, Subscription, Window};
use inlet_state::{Changes, ChannelId};

use crate::app::Inlet;
use crate::backend::messages::{self, threads as backend};
use crate::theme;
use crate::ui::composer::ComposerView;

pub use panel::Panel;

/// The thread panel's width (Fluxer's default; it can be 360 to 900 there).
pub const PANEL_WIDTH: f32 = 450.;

/// Icons of this domain (Phosphor, fill).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadIcon {
    /// Threads (`chats`).
    Chats,
    /// Close (`x`).
    Close,
}

impl ThreadIcon {
    fn data(self) -> &'static [u8] {
        match self {
            ThreadIcon::Chats => include_bytes!("../../../assets/icons/chats.svg"),
            ThreadIcon::Close => include_bytes!("../../../assets/icons/x.svg"),
        }
    }

    pub fn el(self, size: gpui::Pixels, color: gpui::Hsla) -> gpui::Svg {
        gpui::svg()
            .data(self.data())
            .size(size)
            .flex_none()
            .text_color(color)
    }
}

/// "1 message", "3 messages" (`new`: "3 new messages").
fn count_text(n: u64, new: bool) -> String {
    match (n, new) {
        (1, false) => t!("threads-message-one"),
        (1, true) => t!("threads-new-message-one"),
        (n, false) => t!("threads-messages", count = n),
        (n, true) => t!("threads-new-messages", count = n),
    }
}

/// This domain's view state.
#[derive(Default)]
pub struct State {
    /// The thread open in the side panel.
    pub panel: Option<Panel>,
    /// The side panel's composer: a second composer, made when a thread first opens.
    pub composer: Option<Entity<ComposerView>>,
    composer_events: Option<Subscription>,
    /// The instance's message length limit, once known.
    max_len: Option<usize>,
    /// Nonces of `/msg` sends from the panel's composer (their failures go back to it).
    dm_nonces: Vec<String>,
    /// The thread list of the open channel.
    pub browser: Option<list::Browser>,
    /// The open forum or media channel.
    pub forum: Option<forum::ForumView>,
    /// Newest-page requests of the panel waiting for their answer, so every
    /// `begin_load` meets its answer even after the panel closed.
    loads: HashMap<ChannelId, u64>,
    /// Demo screenshots: the scene's thread or list was opened once.
    #[cfg(feature = "demo")]
    demo_done: bool,
}

impl State {
    /// Signed out, or another account shown: nothing of the old account stays on
    /// screen or on its way. The panel's composer goes too (its text was saved with the
    /// old account's drafts); the next thread opens a new one.
    pub fn reset(&mut self) {
        self.panel = None;
        self.composer = None;
        self.composer_events = None;
        self.browser = None;
        self.forum = None;
        self.loads.clear();
        self.dm_nonces.clear();
        self.max_len = None;
    }
}

/// The threads button in the channel header (see `ui/chat/header_buttons.rs`): for a
/// text or announcement channel of a guild, it opens the thread list.
pub fn header_button(this: &Inlet, cx: &mut Context<Inlet>) -> Option<AnyElement> {
    let open = this.selected_channel.as_ref()?;
    this.selected_guild.as_ref()?;
    let channel = this.channels.iter().find(|c| &c.id == open)?;
    if !matches!(
        channel.kind,
        crate::model::ChannelKind::Text | crate::model::ChannelKind::Announcement
    ) {
        return None;
    }
    let shown = this.threads.browser.is_some();
    let color = if shown {
        theme::text_strong()
    } else {
        theme::text_muted()
    };
    Some(
        div()
            .id("threads-button")
            .flex_none()
            .p(px(6.))
            .rounded(px(6.))
            .cursor_pointer()
            .when(shown, |d| d.bg(theme::selected_bg()))
            .hover(|s| s.bg(theme::hover_bg()))
            .child(ThreadIcon::Chats.el(px(24.), color))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_thread_list(cx)))
            .into_any_element(),
    )
}

impl Inlet {
    /// The chat (or the open forum), with the thread panel beside it and the thread list
    /// over its top right corner.
    pub(crate) fn render_center(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let main = if self.forum_is_open() {
            self.render_forum(cx).into_any_element()
        } else {
            self.render_chat(cx).into_any_element()
        };
        div()
            .relative()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_row()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .overflow_hidden()
                    .child(main),
            )
            .children(self.render_thread_panel(cx))
            .children(self.render_thread_list(cx))
    }

    /// Whether the thread list or the thread panel is open (Escape closes them).
    pub(crate) fn threads_open(&self) -> bool {
        self.threads.browser.is_some() || self.threads.panel.is_some()
    }

    /// Escape: the thread list closes first, then the thread panel.
    pub(crate) fn close_threads_on_escape(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.threads.browser.take().is_some() {
            cx.notify();
            return;
        }
        self.close_thread_panel(window, cx);
    }

    /// Another guild or channel is about to show (`channel` is the new one, `None` for
    /// another guild): the thread list closes, and the panel and the forum view close
    /// unless they belong to it.
    pub(crate) fn threads_leave_channel(&mut self, channel: Option<&str>, cx: &mut Context<Self>) {
        let channel = channel.and_then(ChannelId::parse);
        self.threads.browser = None;
        if self
            .threads
            .panel
            .as_ref()
            .is_some_and(|p| Some(p.parent) != channel)
        {
            self.drop_thread_panel(cx);
        }
        if self
            .threads
            .forum
            .as_ref()
            .is_some_and(|f| Some(f.forum) != channel)
        {
            self.close_forum();
        }
        cx.notify();
    }

    /// Answers from the backend's thread domain.
    pub(crate) fn on_thread_event(
        &mut self,
        event: backend::Ev,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let active = window.is_window_active();
        match event {
            backend::Ev::Page {
                thread_id,
                request,
                page,
                rendered,
            } => self.on_panel_page(thread_id, request, page, rendered, active, cx),
            backend::Ev::PageFailed {
                thread_id,
                request,
                older,
            } => self.on_panel_page_failed(thread_id, request, older, cx),
            backend::Ev::Archived {
                channel_id,
                request,
                result,
            } => self.on_archived_page(channel_id, request, result),
            backend::Ev::ForumPage {
                forum_id,
                request,
                result,
            } => self.on_forum_page(forum_id, request, result, cx),
            backend::Ev::PostData { forum_id, previews } => self.on_post_data(forum_id, previews),
        }
        cx.notify();
    }

    /// After the account state changed: the panel follows its thread (and closes when
    /// the account lost access), and the forum view checks its forum.
    pub(crate) fn threads_after_changes(
        &mut self,
        changes: &Changes,
        members: bool,
        cx: &mut Context<Self>,
    ) {
        self.panel_after_changes(changes, members, cx);
        self.forum_after_changes(changes, cx);
    }

    /// Thread events and messages for the open guild, with the member list of the
    /// panel's thread. The backend skips a subscription equal to the last one.
    pub(crate) fn subscribe_threads(&self) {
        let Some(guild_id) = self.selected_guild.clone() else {
            return;
        };
        let member_lists = self
            .threads
            .panel
            .as_ref()
            .filter(|p| p.guild.to_string() == guild_id)
            .map(|p| vec![p.thread.to_string()])
            .unwrap_or_default();
        self.backend
            .send(messages::Cmd::Thread(backend::Cmd::Subscribe {
                guild_id,
                member_lists,
            }));
    }

    /// Once per frame, with the window: the keyboard never stays in a composer that is
    /// not on screen (the panel's after the panel closed, the chat's under a forum), so
    /// nothing typed can go to a channel nobody sees.
    pub(crate) fn threads_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use gpui::Focusable;
        let panel_hidden = self.threads.panel.is_none()
            && self
                .threads
                .composer
                .as_ref()
                .is_some_and(|v| v.focus_handle(cx).is_focused(window));
        let chat_hidden =
            self.forum_is_open() && self.composer.view.focus_handle(cx).is_focused(window);
        if panel_hidden || chat_hidden {
            self.focus_center(window, cx);
        }
        #[cfg(feature = "demo")]
        self.demo_threads(window, cx);
    }

    /// The keyboard goes to the chat's composer, or to the window under a forum.
    pub(crate) fn focus_center(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use gpui::Focusable;
        if self.forum_is_open() {
            window.focus(&self.focus, cx);
        } else {
            let focus = self.composer.view.focus_handle(cx);
            window.focus(&focus, cx);
        }
    }

    /// Demo scenes open the thread panel or the thread list once `#general` shows its
    /// messages, so the screenshots show them without a pointer.
    #[cfg(feature = "demo")]
    pub(crate) fn demo_threads(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use crate::demo::scenes::{current, Scene};
        if self.threads.demo_done {
            return;
        }
        let scene = current();
        if !matches!(scene, Scene::ThreadPanel | Scene::ThreadList) {
            self.threads.demo_done = true;
            return;
        }
        let general = crate::demo::snowflake("c-general");
        if self.selected_channel.as_deref() != Some(general.as_str())
            || self.chat.view.rows.is_empty()
        {
            return;
        }
        self.threads.demo_done = true;
        if scene == Scene::ThreadList {
            self.toggle_thread_list(cx);
        } else if let Some(thread) =
            ChannelId::parse(&crate::demo::snowflake(crate::demo::threads::THREAD))
        {
            self.open_thread(thread, window, cx);
        }
    }
}
