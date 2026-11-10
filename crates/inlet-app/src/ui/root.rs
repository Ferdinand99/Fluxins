// SPDX-License-Identifier: AGPL-3.0-or-later
//! The window's root view: the current screen, the main layout and the overlays.

use gpui::{div, prelude::*, px, Context, Window};

use crate::app::{Inlet, Screen};
use crate::text_input::{FocusNext, FocusPrev};
use crate::{about, assets, theme};

impl Render for Inlet {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.media_frame(window, cx);
        self.take_channel_jump(window, cx);
        let root = div()
            .relative()
            .size_full()
            .track_focus(&self.focus)
            .on_action(|_: &FocusNext, window, cx| window.focus_next(cx))
            .on_action(|_: &FocusPrev, window, cx| window.focus_prev(cx))
            .text_color(theme::text())
            .text_size(px(16.))
            .font_family(assets::ui_font());
        let root = self.notification_keys(root, cx);
        if self.show_about {
            return root
                .child(about::render(self.show_licences, cx))
                .into_any_element();
        }
        let content = match self.screen.clone() {
            Screen::Starting { instance } => self.render_starting(instance, cx).into_any_element(),
            Screen::Offline {
                instance,
                message,
                retry_in_secs,
            } => self
                .render_offline(&instance, &message, retry_in_secs, cx)
                .into_any_element(),
            Screen::Fatal => self.render_fatal(cx).into_any_element(),
            Screen::Confirm {
                display_name,
                account,
                instance,
                pasted,
            } => self
                .render_confirm(&display_name, &account, &instance, pasted, cx)
                .into_any_element(),
            Screen::Finishing => self.render_finishing().into_any_element(),
            Screen::SignIn => self.render_sign_in(cx).into_any_element(),
            Screen::Main => self.render_main(cx).into_any_element(),
        };
        let overlays = self.render_overlays(cx);
        root.child(content).children(overlays).into_any_element()
    }
}

impl Inlet {
    /// Signed in: rail, channel list and user panel on the left, the chat on the right.
    fn render_main(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_row()
            .child(
                div()
                    .h_full()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .flex()
                            .flex_row()
                            .child(self.render_guild_rail(cx))
                            .child(self.render_channel_list(cx)),
                    )
                    .child(self.render_user_panel(cx)),
            )
            .child(self.render_chat(cx))
    }
}
