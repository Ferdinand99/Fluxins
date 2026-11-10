// SPDX-License-Identifier: AGPL-3.0-or-later
//! What floats over the message list: the new-messages banner at the top (while the
//! "NEW" line is scrolled out of view above) and the "Jump to present" pill at the
//! bottom (while the newest messages are scrolled out of view below).

use gpui::{div, prelude::*, px, AnyElement, Context, FontWeight};

use super::messages::Row;
use super::system_message::ChatIcon;
use super::time;
use crate::app::Inlet;
use crate::theme;

/// The banner says "50+" past this many.
const MAX_COUNTED: usize = 50;

impl Inlet {
    pub(super) fn render_new_messages_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let view = &self.chat.view;
        if view.unread_count == 0 {
            return None;
        }
        let divider = view
            .rows
            .iter()
            .position(|r| matches!(r, Row::Unread | Row::Day { new: true, .. }))?;
        if self.chat.message_list.item_is_above_viewport(divider) != Some(true) {
            return None;
        }
        let text = if view.unread_count > MAX_COUNTED {
            t!(
                "chat-new-messages-many",
                count = MAX_COUNTED,
                since = time::full(view.unread_since_ms)
            )
        } else {
            t!(
                "chat-new-messages",
                count = view.unread_count,
                since = time::full(view.unread_since_ms)
            )
        };
        Some(
            div()
                .absolute()
                .top_0()
                .left(px(8.))
                .right(px(8.))
                .h(px(40.))
                .px_4()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_2()
                .rounded_b(px(8.))
                .bg(theme::brand())
                .text_size(px(14.))
                .text_color(theme::on_brand())
                .child(
                    div()
                        .id("new-messages")
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_weight(FontWeight::MEDIUM)
                        .cursor_pointer()
                        .child(text)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.chat.message_list.pause_following_tail();
                            this.chat.message_list.scroll_to_reveal_item(divider);
                            cx.notify();
                        })),
                )
                .child(
                    div()
                        .id("mark-read")
                        .flex_none()
                        .font_weight(FontWeight::SEMIBOLD)
                        .cursor_pointer()
                        .child(t!("chat-mark-as-read"))
                        .on_click(cx.listener(|this, _, _, cx| this.mark_open_channel_read(cx))),
                )
                .into_any_element(),
        )
    }

    pub(super) fn render_jump_to_present(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let count = self.chat.view.rows.len();
        if count == 0 || self.chat.message_list.is_following_tail() {
            return None;
        }
        if self.chat.message_list.item_is_below_viewport(count - 1) != Some(true) {
            return None;
        }
        Some(
            div()
                .absolute()
                .bottom(px(16.))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .id("jump-to-present")
                        .min_h(px(44.))
                        .px_4()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .rounded_full()
                        .bg(theme::bg_textarea())
                        .border_1()
                        .border_color(theme::accent_border().opacity(0.8))
                        .shadow_lg()
                        .text_size(px(14.))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme::text())
                        .cursor_pointer()
                        .hover(|s| s.bg(theme::guild_item()))
                        .child(ChatIcon::ArrowDown.el(px(16.), theme::text()))
                        .child(t!("chat-jump-to-present"))
                        .on_click(cx.listener(|this, _, _, cx| this.jump_to_present(cx))),
                )
                .into_any_element(),
        )
    }
}
