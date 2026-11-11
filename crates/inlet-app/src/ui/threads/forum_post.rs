// SPDX-License-Identifier: AGPL-3.0-or-later
//! One forum post in the list: its title, tags, the start of its first message, its
//! message count and unread count, and whether it is new or archived. A click opens the
//! post in the thread panel.

use gpui::{div, prelude::*, px, AnyElement, Context, FontWeight, SharedString};
use inlet_protocol as p;
use inlet_protocol::api::threads::ForumTag;
use inlet_state::threads::is_archived;

use super::{tags, ThreadIcon};
use crate::app::Inlet;
use crate::model;
use crate::theme;

impl Inlet {
    pub(super) fn post_card(
        &self,
        post: &p::Channel,
        forum_tags: &[ForumTag],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let title = model::clean_name(post.name.as_deref().unwrap_or_default());
        let preview = self
            .threads
            .forum
            .as_ref()
            .and_then(|f| f.preview(&post.id))
            .cloned();
        let unread = self
            .account
            .as_ref()
            .map_or(0, |a| a.read(cx).forum_post_unread(post));
        let new = self.is_new_post(post, cx);
        let open = self
            .threads
            .panel
            .as_ref()
            .is_some_and(|p| p.thread.to_string() == post.id);
        let count = post.message_count().unwrap_or(0);
        let archived = is_archived(post);
        let target = post.clone();
        div()
            .id(SharedString::from(format!("post-{}", post.id)))
            .flex_none()
            .w_full()
            .p_3()
            .rounded(px(8.))
            .flex()
            .flex_col()
            .gap_1()
            .cursor_pointer()
            .bg(if open {
                theme::selected_bg()
            } else {
                theme::panel_bg()
            })
            .border_1()
            .border_color(theme::accent_border())
            .hover(|s| s.bg(theme::hover_bg()))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .when(new, |d| {
                        d.child(
                            div()
                                .flex_none()
                                .px(px(5.))
                                .rounded(px(4.))
                                .bg(theme::brand())
                                .text_color(theme::on_brand())
                                .text_size(px(10.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(t!("forum-new")),
                        )
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_size(px(16.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::text_strong())
                            .child(title),
                    ),
            )
            .children(tags::post_tags(&post.applied_tags(), forum_tags))
            .children(
                preview
                    .filter(|p| !p.snippet.is_empty() || !p.author.is_empty())
                    .map(|p| {
                        div()
                            .flex()
                            .flex_row()
                            .gap_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_size(px(14.))
                            .child(
                                div()
                                    .flex_none()
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(theme::text_secondary())
                                    .child(format!("{}:", p.author)),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .text_ellipsis()
                                    .overflow_hidden()
                                    .text_color(theme::text_muted())
                                    .child(p.snippet),
                            )
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .text_size(px(12.))
                    .text_color(theme::text_muted())
                    .child(ThreadIcon::Chats.el(px(14.), theme::text_muted()))
                    .child(super::count_text(count, false))
                    .when(unread > 0, |d| {
                        d.child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme::text_strong())
                                .child(super::count_text(u64::from(unread), true)),
                        )
                    })
                    .when(archived, |d| d.child(t!("threads-archived-tag"))),
            )
            .on_click(
                cx.listener(move |this, _, window, cx| this.open_post(target.clone(), window, cx)),
            )
            .into_any_element()
    }
}
