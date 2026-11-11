// SPDX-License-Identifier: AGPL-3.0-or-later
//! The channel header: icon, name, topic, the header buttons and the status line.

use gpui::{div, prelude::*, px, Context, FontWeight};

use super::header_buttons;
use crate::app::Inlet;
use crate::theme;
use crate::ui::sidebar::channel_list::channel_icon;
use crate::ui::HEADER_HEIGHT;

impl Inlet {
    pub(crate) fn render_chat_header(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let open = self
            .selected_channel
            .as_ref()
            .and_then(|id| self.channels.iter().find(|c| &c.id == id));
        let title = open.map(|c| c.name.clone()).unwrap_or_default();
        let title_icon = open.map(|c| channel_icon(c.kind));
        let topic = self
            .selected_channel
            .as_ref()
            .and_then(|id| self.channels.iter().find(|c| &c.id == id))
            .and_then(|c| c.topic.clone());
        let buttons = header_buttons::render(self, cx);
        div()
            .h(px(HEADER_HEIGHT))
            .flex_none()
            .px_4()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .border_b_1()
            .border_color(theme::divider())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .children(title_icon.map(|i| i.el(px(24.), theme::text_muted())))
                    .child(
                        div()
                            .flex_none()
                            .font_weight(FontWeight::MEDIUM)
                            .line_height(px(24.))
                            .text_color(theme::text_strong())
                            .child(title),
                    )
                    .children(topic.map(|t| {
                        div()
                            .min_w_0()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .w(px(1.))
                                    .h(px(20.))
                                    .flex_none()
                                    .bg(theme::accent_border()),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .text_size(px(13.))
                                    .line_height(px(18.))
                                    .text_color(theme::text_tertiary())
                                    .child(t),
                            )
                    })),
            )
            // Nothing yet: each button's domain fills its slot.
            .children(buttons)
            .child(
                div()
                    .flex_none()
                    .pl_3()
                    .text_size(px(12.))
                    .text_color(theme::text_muted())
                    .child(self.chat.status.clone()),
            )
    }
}
