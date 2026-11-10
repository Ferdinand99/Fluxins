// SPDX-License-Identifier: AGPL-3.0-or-later
//! What floats over the message list: the new-messages banner at the top (while the
//! "NEW" line is scrolled out of view above) and the "Jump to present" pill at the
//! bottom (while the newest messages are scrolled out of view below).

use gpui::{div, prelude::*, px, AnyElement, Context, FontWeight};

use super::messages::Row;
use super::system_message::ChatIcon;
use crate::app::Inlet;
use crate::theme;

impl Inlet {
    pub(super) fn render_new_messages_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let view = &self.chat.view;
        if !view.divider_above {
            return None;
        }
        let text = view.banner.clone()?;
        let divider = view
            .rows
            .iter()
            .position(|r| matches!(r, Row::Unread | Row::Day { new: true, .. }))?;
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
                            this.chat.view.divider_above = false;
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
        let view = &self.chat.view;
        // Also while the list lost its newest messages, even at its bottom.
        if view.rows.is_empty() || !(view.scrolled_away || view.detached) {
            return None;
        }
        let label = if view.detached_new == 1 {
            t!("chat-jump-new-message")
        } else if view.detached_new > 1 {
            t!("chat-jump-new-messages", count = view.detached_new)
        } else {
            t!("chat-jump-to-present")
        };
        Some(
            div()
                .absolute()
                .bottom(px(42.))
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
                        .child(label)
                        .on_click(cx.listener(|this, _, _, cx| this.jump_to_present(cx))),
                )
                .into_any_element(),
        )
    }
}
