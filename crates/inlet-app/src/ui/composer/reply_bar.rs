// SPDX-License-Identifier: AGPL-3.0-or-later
//! The reply bar above the box: who is being replied to, the "mention on/off" switch
//! (whether the author is notified, sent as `replied_user`), and a close button. Escape
//! closes it too.

use gpui::{div, prelude::*, px, AnyElement, Context, Focusable, FontWeight, Window};

use super::editor::ComposerView;
use crate::theme;

/// The message the next send replies to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reply {
    pub message_id: String,
    /// The author's name, cleaned.
    pub author: String,
    /// Notify the author (Fluxer's default).
    pub mention: bool,
}

impl ComposerView {
    /// Starts a reply to a message in the open channel and puts the caret in the box.
    #[allow(dead_code)] // called by the message actions
    pub fn start_reply(
        &mut self,
        message_id: String,
        author: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.edit = None;
        self.reply = Some(Reply {
            message_id,
            author,
            mention: true,
        });
        window.focus(&self.editor.focus_handle(cx), cx);
        cx.notify();
    }

    pub(super) fn render_reply_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let reply = self.reply.as_ref()?;
        let mention = reply.mention;
        Some(
            div()
                .mx(px(6.))
                .h(px(32.))
                .px_4()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .rounded_t(px(8.))
                .bg(theme::panel_bg())
                .border_1()
                .border_b_0()
                .border_color(theme::ring())
                .text_size(px(14.))
                .text_color(theme::text_muted())
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_1()
                        .child(t!("composer-replying-to"))
                        .child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme::text_strong())
                                .child(reply.author.clone()),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_3()
                        .child(
                            div()
                                .id("reply-mention")
                                .cursor_pointer()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(if mention {
                                    theme::link()
                                } else {
                                    theme::text_tertiary()
                                })
                                .child(if mention {
                                    t!("composer-reply-mention-on")
                                } else {
                                    t!("composer-reply-mention-off")
                                })
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(r) = &mut this.reply {
                                        r.mention = !r.mention;
                                    }
                                    cx.notify();
                                })),
                        )
                        .child(
                            div()
                                .id("reply-close")
                                .cursor_pointer()
                                .text_color(theme::text_tertiary())
                                .hover(|s| s.text_color(theme::text_strong()))
                                .child(t!("composer-reply-close"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.reply = None;
                                    cx.notify();
                                })),
                        ),
                )
                .into_any_element(),
        )
    }
}
