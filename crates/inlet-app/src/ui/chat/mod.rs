// SPDX-License-Identifier: AGPL-3.0-or-later
//! The chat column: channel header, error and notice bars, the message list (with the
//! new-messages banner and "Jump to present" over it) and the composer.

mod action_bar;
mod context_menu;
mod divider;
mod embed;
mod header;
pub mod header_buttons;
mod jump;
mod message;
pub mod messages;
pub mod pins;
mod reactions;
mod reactors;
mod reply_preview;
mod system_message;
pub mod time;
pub mod typing;
mod welcome;

use gpui::{div, list, prelude::*, px, Context, FollowMode, ListAlignment, ListState, Window};

use crate::app::Inlet;
use crate::backend::accounts::Cmd;
use crate::theme;
use crate::ui::widgets::button;
pub use context_menu::bind_keys;
#[cfg(test)]
pub(crate) use context_menu::unread_mark;
use messages::MessageView;

pub struct ChatState {
    /// The rows of the open channel and what they are built from.
    pub view: MessageView,
    pub message_list: ListState,
    /// Connection state for the status line.
    pub status: String,
    pub error: Option<String>,
    pub notice: Option<String>,
    /// The gateway stopped and is not retried automatically.
    pub gateway_down: Option<String>,
    /// A channel mention was clicked; opened on the next render (needs the window).
    pub pending_channel_jump: Option<String>,
    /// The message actions: the open menu or dialog, reaction clicks, who reacted.
    pub actions: context_menu::ActionsState,
}

impl ChatState {
    pub fn new() -> Self {
        let message_list = ListState::new(0, ListAlignment::Bottom, px(800.));
        message_list.set_follow_mode(FollowMode::Tail);
        Self {
            view: MessageView::new(),
            message_list,
            status: String::new(),
            error: None,
            notice: None,
            gateway_down: None,
            pending_channel_jump: None,
            actions: Default::default(),
        }
    }
}

impl Inlet {
    /// Opens the channel of a clicked channel mention, if it is in the list on screen.
    pub(crate) fn take_channel_jump(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(channel_id) = self.chat.pending_channel_jump.take() {
            if self
                .channels
                .iter()
                .any(|c| c.id == channel_id && c.is_messageable())
            {
                self.select_channel(channel_id, window, cx);
            } else {
                self.chat.notice = Some(t!("chat-channel-not-on-screen"));
            }
        }
    }

    pub(crate) fn render_chat(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        self.hook_list_scroll(cx);
        let has_channel = self.selected_channel.is_some();
        let bar = |id: &'static str, text: String, bg| {
            div()
                .id(id)
                .px_4()
                .py_2()
                .bg(bg)
                .text_color(theme::text_strong())
                .cursor_pointer()
                .child(t!("chat-dismissable", text = text))
        };
        div()
            .flex_1()
            // Narrower than its content beside the thread panel: the header and the
            // rows shrink with it instead of being cut off.
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(theme::bg_chat())
            .child(self.render_chat_header(cx))
            .children(self.chat.error.clone().map(|e| {
                bar("error", e, theme::danger()).on_click(cx.listener(|this, _, _, cx| {
                    this.chat.error = None;
                    cx.notify();
                }))
            }))
            .children(self.chat.gateway_down.clone().map(|message| {
                div()
                    .px_4()
                    .py_2()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .bg(theme::selected_bg())
                    .text_color(theme::text_strong())
                    .child(t!("chat-disconnected", message = message))
                    .child(
                        button("reconnect", t!("chat-reconnect"), true).on_click(cx.listener(
                            |this, _, _, cx| {
                                this.chat.gateway_down = None;
                                this.chat.status = t!("status-reconnecting");
                                this.backend
                                    .send(Cmd::Reconnect(this.switcher.active.clone()));
                                cx.notify();
                            },
                        )),
                    )
            }))
            .children(self.chat.notice.clone().map(|n| {
                bar("notice", n, theme::selected_bg()).on_click(cx.listener(|this, _, _, cx| {
                    this.chat.notice = None;
                    cx.notify();
                }))
            }))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .flex()
                    .flex_col()
                    .child(
                        list(
                            self.chat.message_list.clone(),
                            cx.processor(|this, ix: usize, _window, cx| this.render_row(ix, cx)),
                        )
                        .flex_1()
                        .w_full(),
                    )
                    .children(self.render_new_messages_bar(cx))
                    .children(self.render_jump_to_present(cx)),
            )
            .when(has_channel, |d| d.child(self.render_composer()))
    }
}
