// SPDX-License-Identifier: AGPL-3.0-or-later
//! One message in the list: date separator, avatar, header, reply line, body.

use std::rc::Rc;

use gpui::{div, prelude::*, px, AnyElement, Context, FontWeight, SharedString};

use super::embed::render_embed;
use crate::app::Inlet;
use crate::model::{self, Guild};
use crate::theme;
use crate::ui::markdown::{self, RenderCtx};

/// Avatar column of the message grid (16 | 40 | 12 | text).
pub const AVATAR_COLUMN: f32 = 40.;

impl Inlet {
    pub(super) fn render_message(&mut self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(shown) = self.chat.messages.get(ix) else {
            return div().into_any_element();
        };
        let m = &shown.message;
        let weak = cx.entity().downgrade();
        let message_id = m.id.clone();
        let ctx = RenderCtx {
            revealed: shown.revealed,
            on_link: Rc::new({
                let weak = weak.clone();
                move |url, masked, _window, cx| {
                    let url = url.to_string();
                    let _ = weak.update(cx, |this, cx| this.open_link(&url, masked, cx));
                }
            }),
            on_reveal: Rc::new({
                let weak = weak.clone();
                let message_id = message_id.clone();
                move |_window, cx| {
                    let _ = weak.update(cx, |this, cx| {
                        if let Some(ix) = this
                            .chat
                            .messages
                            .iter()
                            .position(|s| s.message.id == message_id)
                        {
                            this.chat.messages[ix].revealed = true;
                            this.chat.message_list.splice(ix..ix + 1, 1);
                            cx.notify();
                        }
                    });
                }
            }),
        };
        let mut body = div().flex().flex_col().gap_1().child(markdown::render(
            &format!("m{}", m.id),
            &m.blocks,
            &ctx,
        ));
        if m.truncated {
            let message_id = message_id.clone();
            body = body.child(
                div()
                    .id(SharedString::from(format!("more-{}", m.id)))
                    .text_color(theme::link())
                    .cursor_pointer()
                    .child(t!("message-show-more"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(ix) = this
                            .chat
                            .messages
                            .iter()
                            .position(|s| s.message.id == message_id)
                        {
                            let msg = &mut this.chat.messages[ix].message;
                            msg.blocks = markdown::parse(&msg.content);
                            msg.truncated = false;
                            this.chat.message_list.splice(ix..ix + 1, 1);
                            cx.notify();
                        }
                    })),
            );
        }
        for (i, e) in m.embeds.iter().enumerate() {
            body = body.child(render_embed(&format!("m{}-e{i}", m.id), e, &ctx, cx));
        }
        if m.is_empty() {
            body = body.child(
                div()
                    .text_size(px(14.))
                    .text_color(theme::text_muted())
                    .child(t!("message-no-text")),
            );
        }
        for (i, a) in m.attachments.iter().enumerate() {
            let url = a.url.clone();
            body = body.child(
                div()
                    .id(SharedString::from(format!("att-{}-{i}", m.id)))
                    .text_color(theme::link())
                    .cursor_pointer()
                    .child(t!(
                        "message-attachment",
                        filename = a.filename,
                        host = markdown::link_target_label(&a.url)
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| this.open_link(&url, false, cx))),
            );
        }
        let header = (!shown.grouped).then(|| {
            div()
                .flex()
                .flex_row()
                .items_baseline()
                .gap_2()
                .child(
                    div()
                        .font_weight(FontWeight::MEDIUM)
                        .line_height(px(22.))
                        .text_color(theme::text_strong())
                        .child(m.author.clone()),
                )
                .when(m.author_bot, |d| {
                    d.child(
                        div()
                            .px(px(5.6))
                            .py(px(2.))
                            .rounded(px(4.))
                            .bg(theme::brand())
                            .text_size(px(10.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(gpui::white())
                            .child(t!("message-bot-tag")),
                    )
                })
                .when(m.author_username != m.author, |d| {
                    d.child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme::text_muted())
                            .child(format!("@{}", m.author_username)),
                    )
                })
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme::timestamp())
                        .child(m.timestamp.clone()),
                )
        });
        let reply = m.reply.as_ref().map(|r| {
            div()
                .flex()
                .flex_row()
                .gap_1()
                .text_size(px(13.))
                .text_color(theme::text_muted())
                .whitespace_nowrap()
                .overflow_hidden()
                .child("\u{21B1}")
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(r.author.clone()),
                )
                .child(r.snippet.clone())
        });
        let new_day = match m.day() {
            Some(day) => {
                ix == 0 || self.chat.messages.get(ix - 1).and_then(|p| p.message.day()) != Some(day)
            }
            None => false,
        };
        let separator = m.day().filter(|_| new_day).map(|day| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .px_4()
                .pt_4()
                .child(div().flex_1().h(px(2.)).bg(theme::separator_line()))
                .child(
                    div()
                        .text_size(px(12.))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme::text_tertiary())
                        .child(model::day_label(day)),
                )
                .child(div().flex_1().h(px(2.)).bg(theme::separator_line()))
        });
        // Avatar slot: initials until avatars arrive (next milestone).
        let avatar = (!shown.grouped).then(|| {
            div()
                .size(px(AVATAR_COLUMN))
                .rounded_full()
                .bg(theme::guild_item())
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(14.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme::text_muted())
                .child(
                    Guild {
                        id: String::new(),
                        name: m.author.clone(),
                    }
                    .initials(),
                )
        });
        div()
            .w_full()
            .flex()
            .flex_col()
            .children(separator)
            .child(
                div()
                    .w_full()
                    .when(!shown.grouped, |d| d.mt_4())
                    .pl_4()
                    .pr_4()
                    .py(px(2.))
                    .flex()
                    .flex_row()
                    .gap_3()
                    .text_color(theme::text())
                    .line_height(px(22.))
                    .when(m.mentions_me, |d| {
                        d.bg(theme::mention_me_bg())
                            .border_l_2()
                            .border_color(theme::mention_me_bar())
                    })
                    .when(!m.mentions_me, |d| d.hover(|s| s.bg(theme::hover_bg())))
                    .child(div().w(px(AVATAR_COLUMN)).flex_none().children(avatar))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .children(reply)
                            .children(header)
                            .child(body),
                    ),
            )
            .into_any_element()
    }
}
