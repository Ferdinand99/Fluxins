// SPDX-License-Identifier: AGPL-3.0-or-later
//! The line above a reply: a 2 px spine from the avatar column up and over to a small
//! avatar, the replied-to author's name and the first words of their message (14 px,
//! one line). A click jumps to that message when it is loaded.

use gpui::{div, prelude::*, px, AnyElement, Context, FontWeight, SharedString};

use super::message::AVATAR_COLUMN;
use super::messages::{ReplyRow, ReplyState};
use super::system_message::ChatIcon;
use crate::app::Inlet;
use crate::model::Guild;
use crate::theme;

/// Height of the reply line (`1.125rem`).
const LINE_HEIGHT: f32 = 18.;

pub fn render(id: &str, reply: &ReplyRow, compact: bool, cx: &mut Context<Inlet>) -> AnyElement {
    let muted = theme::text_muted();
    let content =
        match &reply.state {
            ReplyState::Loaded { author, snippet } => div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .min_w_0()
                .child(
                    div()
                        .size(px(16.))
                        .flex_none()
                        .rounded_full()
                        .bg(theme::guild_item())
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(8.))
                        .text_color(muted)
                        .child(
                            Guild {
                                id: String::new(),
                                name: author.clone(),
                            }
                            .initials(),
                        ),
                )
                .child(
                    div()
                        .flex_none()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(muted.opacity(0.64))
                        .child(format!("@{author}")),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_color(muted)
                        .hover(|s| s.text_color(theme::text()))
                        .child(snippet.clone()),
                ),
            ReplyState::Deleted | ReplyState::Unknown => div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .child(ChatIcon::Warning.el(px(14.), muted))
                .child(div().italic().text_color(muted).child(
                    if reply.state == ReplyState::Deleted {
                        t!("reply-deleted")
                    } else {
                        t!("reply-unknown")
                    },
                )),
        };
    let target = reply.target;
    let lead = if compact {
        div().w(px(64.)).flex_none()
    } else {
        // The spine: from the middle of the avatar column, up and over to the preview.
        div()
            .w(px(AVATAR_COLUMN))
            .h(px(LINE_HEIGHT))
            .flex_none()
            .relative()
            .child(
                div()
                    .absolute()
                    .left(px(AVATAR_COLUMN / 2. - 1.))
                    .top(px(LINE_HEIGHT / 2.))
                    .w(px(AVATAR_COLUMN / 2. + 9.))
                    .h(px(LINE_HEIGHT / 2. + 2.))
                    .border_t_2()
                    .border_l_2()
                    .rounded_tl(px(6.))
                    .border_color(theme::reply_spine()),
            )
    };
    div()
        .id(SharedString::from(id.to_string()))
        .h(px(LINE_HEIGHT + 4.))
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .text_size(px(14.))
        .line_height(px(LINE_HEIGHT))
        .when(target.is_some(), |d| d.cursor_pointer())
        .child(lead)
        .child(content.flex_1())
        .on_click(cx.listener(move |this, _, _, cx| {
            if let Some(target) = target {
                this.jump_to_message(target, cx);
            }
        }))
        .into_any_element()
}
