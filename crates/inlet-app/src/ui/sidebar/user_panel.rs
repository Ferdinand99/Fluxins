// SPDX-License-Identifier: AGPL-3.0-or-later
//! The floating user panel under the rail and channel list.

use gpui::{div, prelude::*, px, Context, FontWeight};

use crate::app::Inlet;
use crate::assets::Icon;
use crate::model::Guild;
use crate::theme;
use crate::ui::notifications::badge::rail_badge;

impl Inlet {
    /// The floating user panel under the rail and channel list.
    pub(crate) fn render_user_panel(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let initials = Guild {
            id: String::new(),
            name: self.username.clone(),
        }
        .initials();
        let icon_button = |id: &'static str, icon: Icon| {
            div()
                .id(id)
                .size(px(32.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.))
                .cursor_pointer()
                .hover(|s| s.bg(theme::hover_bg()))
                .child(icon.el(px(20.), theme::text_muted()))
        };
        // Mentions waiting in the other signed-in accounts, on the avatar.
        let elsewhere = self.switcher.background_mentions();
        div().flex_none().p(px(6.)).bg(theme::bg_app()).child(
            div()
                .h(px(58.))
                .px_3()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .rounded(px(8.))
                .bg(theme::panel_bg())
                .border_1()
                .border_color(theme::ring())
                .child(
                    // Avatar and name open the account switcher, as in Fluxer.
                    div()
                        .id("user-area")
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.switcher.open = true;
                            cx.notify();
                        }))
                        .child(
                            div()
                                .relative()
                                .size(px(32.))
                                .flex_none()
                                .rounded_full()
                                .bg(theme::brand())
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_size(px(12.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme::text_strong())
                                .child(initials)
                                .when(elsewhere > 0, |d| {
                                    d.child(rail_badge(elsewhere, theme::panel_bg()))
                                }),
                        )
                        .child(self.render_user_name()),
                )
                .child(
                    icon_button("settings", Icon::Gear)
                        .on_click(cx.listener(|this, _, _, cx| this.open_settings(None, cx))),
                )
                .child(
                    icon_button("about", Icon::Info).on_click(cx.listener(|this, _, _, cx| {
                        this.show_about = true;
                        cx.notify();
                    })),
                )
                .child(
                    icon_button("sign-out", Icon::SignOut).on_click(cx.listener(Self::sign_out)),
                ),
        )
    }

    /// The name and instance in the user panel.
    fn render_user_name(&self) -> impl IntoElement {
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .overflow_hidden()
            .whitespace_nowrap()
            .child(
                div()
                    .text_size(px(14.))
                    .line_height(px(18.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme::text_strong())
                    .text_ellipsis()
                    .child(self.username.clone()),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .line_height(px(14.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme::text_muted())
                    .text_ellipsis()
                    .child(self.instance.clone()),
            )
    }
}
