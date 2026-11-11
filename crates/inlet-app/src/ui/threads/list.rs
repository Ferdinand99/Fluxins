// SPDX-License-Identifier: AGPL-3.0-or-later
//! The thread list of a text or announcement channel, opened from the header: the
//! active threads the account state knows (and the account can view), then the archived
//! public ones, a page at a time when the person asks for more.
//!
//! Budget: one page of archived threads when the list opens, then one per click on
//! "Load more", one at a time (`inlet_state::ThreadPages`).
//!
// Deviation: archived private threads (Fluxer lists them for moderators and members) are
// not listed yet; they open from the active list while they are active.

use gpui::{div, prelude::*, px, AnyElement, Context, FontWeight, SharedString};
use inlet_protocol as p;
use inlet_state::threads::{is_archived, last_activity};
use inlet_state::{ChannelId, PageBy, ThreadPages};

use super::ThreadIcon;
use crate::app::Inlet;
use crate::backend::messages::{self, threads as backend};
use crate::model;
use crate::theme;
use crate::ui::HEADER_HEIGHT;

/// Width of the thread list.
const LIST_WIDTH: f32 = 420.;
/// Its greatest height.
const LIST_MAX_HEIGHT: f32 = 520.;

/// The open thread list.
pub struct Browser {
    /// The channel whose threads are listed.
    pub channel: ChannelId,
    pub archived: ThreadPages,
    /// Why the last archived page did not come.
    pub error: Option<String>,
}

impl Inlet {
    /// The header button: opens the list for the open channel, or closes it.
    pub(super) fn toggle_thread_list(&mut self, cx: &mut Context<Self>) {
        if self.threads.browser.take().is_some() {
            cx.notify();
            return;
        }
        let Some(channel) = self.open_channel_id() else {
            return;
        };
        self.threads.browser = Some(Browser {
            channel,
            archived: ThreadPages::continuing(PageBy::ArchiveTime, self.threads.list_requests),
            error: None,
        });
        self.subscribe_threads();
        self.load_archived_page();
        cx.notify();
    }

    /// The next page of archived threads, if one may go now.
    fn load_archived_page(&mut self) {
        let Some(b) = self.threads.browser.as_mut() else {
            return;
        };
        let Some(next) = b.archived.next_page() else {
            return;
        };
        self.threads.list_requests = next.request;
        b.error = None;
        self.backend
            .send(messages::Cmd::Thread(backend::Cmd::Archived {
                channel_id: b.channel.to_string(),
                before: next.before,
                request: next.request,
            }));
    }

    pub(super) fn on_archived_page(
        &mut self,
        channel_id: String,
        request: u64,
        result: Result<(Vec<p::Channel>, bool), String>,
    ) {
        let Some(b) = self
            .threads
            .browser
            .as_mut()
            .filter(|b| b.channel.to_string() == channel_id)
        else {
            return;
        };
        match result {
            Ok((threads, has_more)) => {
                b.archived.on_page(request, threads, has_more);
            }
            Err(reason) => {
                if b.archived.on_failed(request) {
                    b.error = Some(t!("threads-error-list", error = reason));
                }
            }
        }
    }

    /// A thread of the list was clicked: an archived one is stored first (its membership
    /// as REST gave it), then it opens in the side panel if the account can view it.
    fn open_listed_thread(
        &mut self,
        thread: p::Channel,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = ChannelId::parse(&thread.id) else {
            return;
        };
        if let Some(account) = self.account.clone() {
            if account.read(cx).channel(id).is_none() {
                let changes = account.update(cx, |s, _| s.upsert_thread(&thread));
                self.after_changes(changes, false, cx);
            }
        }
        self.open_thread(id, window, cx);
    }

    fn thread_row(&self, t: &p::Channel, cx: &mut Context<Self>) -> AnyElement {
        let name = model::clean_name(t.name.as_deref().unwrap_or_default());
        let count = t.message_count().unwrap_or(0);
        let open = self
            .threads
            .panel
            .as_ref()
            .is_some_and(|p| p.thread.to_string() == t.id);
        let thread = t.clone();
        div()
            .id(SharedString::from(format!("thread-row-{}", t.id)))
            .flex_none()
            .px_3()
            .py_2()
            .rounded(px(6.))
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .cursor_pointer()
            .when(open, |d| d.bg(theme::selected_bg()))
            .hover(|s| s.bg(theme::hover_bg()))
            .child(ThreadIcon::Chats.el(px(18.), theme::text_muted()))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme::text_strong())
                            .child(name),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme::text_muted())
                            .child(super::count_text(count, false)),
                    ),
            )
            .when(is_archived(t), |d| {
                d.child(
                    div()
                        .flex_none()
                        .text_size(px(12.))
                        .text_color(theme::text_muted())
                        .child(t!("threads-archived-tag")),
                )
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_listed_thread(thread.clone(), window, cx)
            }))
            .into_any_element()
    }

    fn section_title(text: String) -> AnyElement {
        div()
            .px_3()
            .pt_3()
            .pb_1()
            .text_size(px(12.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(theme::text_muted())
            .child(text.to_uppercase())
            .into_any_element()
    }

    fn quiet_line(text: String) -> AnyElement {
        div()
            .px_3()
            .py_2()
            .text_size(px(14.))
            .text_color(theme::text_muted())
            .child(text)
            .into_any_element()
    }

    /// The thread list over the chat's top right corner.
    pub(super) fn render_thread_list(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let b = self.threads.browser.as_ref()?;
        let state = self.account.as_ref()?.read(cx);
        let mut active: Vec<p::Channel> = state
            .threads_of(b.channel)
            .into_iter()
            .filter(|t| !is_archived(t))
            .cloned()
            .collect();
        active.sort_by_key(|t| std::cmp::Reverse(last_activity(t)));
        let archived: Vec<p::Channel> = b
            .archived
            .threads()
            .iter()
            .filter(|t| !active.iter().any(|a| a.id == t.id))
            .cloned()
            .collect();
        let (loading, more, loaded, error) = (
            b.archived.is_loading(),
            b.archived.has_more(),
            b.archived.is_loaded(),
            b.error.clone(),
        );
        let mut rows: Vec<AnyElement> = vec![Self::section_title(t!("threads-active"))];
        if active.is_empty() {
            rows.push(Self::quiet_line(t!("threads-none-active")));
        }
        for t in &active {
            rows.push(self.thread_row(t, cx));
        }
        rows.push(Self::section_title(t!("threads-archived")));
        for t in &archived {
            rows.push(self.thread_row(t, cx));
        }
        if loaded && archived.is_empty() && !more {
            rows.push(Self::quiet_line(t!("threads-none-archived")));
        }
        if let Some(e) = error {
            rows.push(Self::quiet_line(e));
        }
        if loading {
            rows.push(Self::quiet_line(t!("threads-loading")));
        } else if more {
            let label = if loaded {
                t!("threads-load-more")
            } else {
                t!("threads-retry")
            };
            rows.push(
                div()
                    .id("threads-load-more")
                    .mx_3()
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
                        this.load_archived_page();
                        cx.notify();
                    }))
                    .into_any_element(),
            );
        }
        Some(
            div()
                .absolute()
                .top(px(HEADER_HEIGHT - 4.))
                .right(px(16.))
                .w(px(LIST_WIDTH))
                .max_h(px(LIST_MAX_HEIGHT))
                .flex()
                .flex_col()
                .rounded(px(8.))
                .bg(theme::floating_bg())
                .border_1()
                .border_color(theme::accent_border())
                .shadow(vec![gpui::BoxShadow {
                    color: theme::floating_shadow(),
                    offset: gpui::point(px(0.), px(4.)),
                    blur_radius: px(12.),
                    spread_radius: px(0.),
                    inset: false,
                }])
                .occlude()
                .child(
                    div()
                        .px_3()
                        .py_2()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .border_b_1()
                        .border_color(theme::divider())
                        .child(ThreadIcon::Chats.el(px(20.), theme::text_muted()))
                        .child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme::text_strong())
                                .child(t!("threads-browser-title")),
                        ),
                )
                .child(
                    div()
                        .id("threads-list")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .p_1()
                        .flex()
                        .flex_col()
                        .children(rows),
                )
                .into_any_element(),
        )
    }
}
