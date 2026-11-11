// SPDX-License-Identifier: AGPL-3.0-or-later
//! One row of the message list: a message (cozy or compact), a divider, a system line,
//! the channel's welcome or a message on its way.
//!
//! Two slots: [`Inlet::attachments_slot`] (images and files under the text, drawn by the
//! media views) and [`Inlet::actions_slot`] (the hover bar with reactions, reply, edit…,
//! drawn by `action_bar.rs`). The reaction bubbles go under the attachments.

use std::rc::Rc;

use gpui::{
    div, prelude::*, px, AnyElement, App, Context, Div, FontWeight, Hsla, MouseButton,
    MouseDownEvent, SharedString, Stateful, Window,
};
use inlet_state::MessageId;

use super::embed::render_embed;
use super::messages::{MessageRow, PendingRow, Row, Tag};
use super::{divider, reply_preview, system_message, welcome};
use crate::app::Inlet;
use crate::theme;
use crate::ui::markdown::{self, RenderCtx};

/// Avatar column of the message grid (16 | 40 | 12 | text).
pub const AVATAR_COLUMN: f32 = 40.;
/// Space above the first message of a group (`--message-group-spacing`, cozy only).
pub const GROUP_SPACING: f32 = 16.;
/// Width of the time column in compact mode.
const COMPACT_TIME_COLUMN: f32 = 64.;
/// Group name for hover styles of a message row.
const ROW_GROUP: &str = "message-row";

impl Inlet {
    pub(super) fn render_row(&mut self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.chat.view.rows.get(ix).cloned() else {
            return div().into_any_element();
        };
        let compact = self.chat.view.compact;
        match row {
            Row::Welcome(w) => welcome::render(&w),
            Row::Day { day, new } => divider::render_day(day, new),
            Row::Unread => divider::render_unread(),
            Row::System(s) => system_message::render(&s, compact),
            Row::Message(m) => self.render_message(&m, compact, cx),
            Row::Pending(o) => self.render_pending(&o, compact, cx),
        }
    }

    fn render_ctx(&self, row: &MessageRow, cx: &mut Context<Self>) -> RenderCtx {
        let weak = cx.entity().downgrade();
        let id = row.id;
        RenderCtx {
            revealed: row.revealed,
            names: self.mention_names(&row.message, cx),
            jumbo: !self.chat.view.compact,
            off_thread: Some(self.markdown_off_thread(cx)),
            on_link: Rc::new({
                let weak = weak.clone();
                move |url, masked, _window, cx| {
                    let url = url.to_string();
                    let _ = weak.update(cx, |this, cx| this.open_link(&url, masked, cx));
                }
            }),
            on_reveal: Rc::new(move |_window, cx| {
                let _ = weak.update(cx, |this, cx| this.reveal(id, cx));
            }),
        }
    }

    /// Text, "Show more", embeds and attachments of a message.
    fn message_body(&self, row: &MessageRow, cx: &mut Context<Self>) -> gpui::Div {
        let m = &row.message;
        let ctx = self.render_ctx(row, cx);
        let mut body = div()
            .flex()
            .flex_col()
            .gap_1()
            .min_w_0()
            .child(markdown::render(&format!("m{}", m.id), &m.blocks, &ctx));
        if m.truncated {
            let id = row.id;
            body = body.child(
                div()
                    .id(SharedString::from(format!("more-{}", m.id)))
                    .text_color(theme::link())
                    .cursor_pointer()
                    .child(t!("message-show-more"))
                    .on_click(cx.listener(move |this, _, _, cx| this.expand(id, cx))),
            );
        }
        // Links the text hides in spoilers hide their embeds too, until shown. The links
        // are worked out when the message is converted, not here. Embeds hidden with
        // "Suppress Embeds" are not drawn at all.
        let hidden: &[String] = if row.revealed || row.meta.embeds_hidden {
            &[]
        } else {
            &m.spoiler_links
        };
        if !row.meta.embeds_hidden {
            for (i, e) in m.embeds.iter().enumerate() {
                let id = format!("m{}-e{i}", m.id);
                body = body.child(render_embed(self, &id, e, hidden, &ctx, cx));
            }
        }
        if m.is_empty() {
            body = body.child(
                div()
                    .text_size(px(14.))
                    .text_color(theme::text_muted())
                    .child(t!("message-no-text")),
            );
        }
        body.children(self.attachments_slot(row, cx))
            .children(self.reactions_slot(row, cx))
    }

    /// Images and files under the text: previews, video stills, file cards (see
    /// `ui/media`).
    pub(super) fn attachments_slot(
        &self,
        row: &MessageRow,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let m = &row.message;
        self.render_attachments(&m.channel_id, &m.id, &m.attachments, cx)
    }

    /// The hover bar of a message (react, reply, edit, more), while the row is under the
    /// pointer; it is drawn over the row's top right corner.
    pub(super) fn actions_slot(
        &self,
        row: &MessageRow,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        self.render_action_bar(row, cx)
    }

    fn render_message(
        &self,
        row: &MessageRow,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let m = &row.message;
        let reply = row
            .reply
            .as_ref()
            .map(|r| reply_preview::render(&format!("reply-{}", m.id), r, compact, cx));
        let name = div()
            .flex_none()
            .font_weight(FontWeight::MEDIUM)
            .line_height(px(22.))
            .text_color(
                row.name_color
                    .map(|c| gpui::rgb(c).into())
                    .unwrap_or(theme::text_strong()),
            )
            .child(m.author.clone());
        let tag = row.tag.map(render_tag);
        let body = self.message_body(row, cx);
        let main = if compact {
            div()
                .flex()
                .flex_row()
                .items_start()
                .gap_2()
                .child(
                    div()
                        .w(px(COMPACT_TIME_COLUMN))
                        .flex_none()
                        .text_size(px(11.))
                        .font_weight(FontWeight::MEDIUM)
                        .line_height(px(22.))
                        .text_color(theme::timestamp())
                        .child(row.clock.clone()),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_none()
                        .items_center()
                        .gap_1()
                        .children(tag)
                        .child(name),
                )
                .child(body.flex_1())
        } else {
            let header = (!row.grouped).then(|| {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .child(name)
                    .children(tag)
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
            // Grouped rows show their time in the avatar column under the pointer.
            let side: AnyElement = if row.grouped {
                div()
                    .w_full()
                    .flex()
                    .justify_end()
                    .text_size(px(11.))
                    .font_weight(FontWeight::MEDIUM)
                    .line_height(px(22.))
                    .text_color(theme::timestamp())
                    .opacity(0.)
                    .group_hover(ROW_GROUP, |s| s.opacity(1.))
                    .child(row.clock.clone())
                    .into_any_element()
            } else {
                let playing = self.media.hovered_message.as_deref() == Some(m.id.as_str());
                self.user_avatar(
                    m.author_avatar.as_deref(),
                    &m.author,
                    AVATAR_COLUMN,
                    playing,
                    cx,
                )
            };
            div()
                .flex()
                .flex_row()
                .gap_3()
                .child(div().w(px(AVATAR_COLUMN)).flex_none().child(side))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .children(header)
                        .child(body),
                )
        };
        let (bar, bg) = if row.jumped {
            (Some(theme::jumped_bar()), Some(theme::jumped_bg()))
        } else if row.mention {
            (Some(theme::mention_me_bar()), Some(theme::mention_me_bg()))
        } else {
            (None, None)
        };
        // Only the row under the pointer has a bar, so no hidden bar takes clicks.
        let actions = self
            .actions_slot(row, cx)
            .map(|a| div().absolute().top(px(-16.)).right(px(14.)).child(a));
        let (id, channel) = (row.id, inlet_state::ChannelId::parse(&m.channel_id));
        let starts_group = !row.grouped || row.reply.is_some();
        // An animated avatar plays while the pointer is over its message.
        let animated_avatar = !compact
            && !row.grouped
            && m.author_avatar
                .as_ref()
                .is_some_and(|a| a.hash.starts_with("a_"));
        let hover_id = m.id.clone();
        let on_hover = cx.listener(move |this, hovered: &bool, _, cx| {
            let animated = animated_avatar.then_some(hover_id.as_str());
            let changed = note_row_hover(
                &mut this.chat.actions,
                &mut this.media.hovered_message,
                id,
                animated,
                *hovered,
            );
            if changed {
                cx.notify();
            }
        });
        div()
            .w_full()
            .when(starts_group && !compact, |d| d.pt(px(GROUP_SPACING)))
            .child(
                row_frame(SharedString::from(format!("row-{}", m.id)), on_hover)
                    .relative()
                    .w_full()
                    .pl_4()
                    .pr_4()
                    .py(px(2.))
                    .flex()
                    .flex_col()
                    .text_color(theme::text())
                    .line_height(px(22.))
                    .when_some(bg, |d, bg| d.bg(bg))
                    .when(bg.is_none(), |d| d.hover(|s| s.bg(theme::hover_bg())))
                    .when_some(channel, |d, ch| {
                        d.on_mouse_down(
                            MouseButton::Right,
                            cx.listener(move |this, e: &MouseDownEvent, window, cx| {
                                this.open_message_menu(ch, id, e.position, window, cx)
                            }),
                        )
                    })
                    // The bar is drawn over the row's edge, so the content does not move.
                    .children(bar.map(|bar| {
                        div()
                            .absolute()
                            .left_0()
                            .top_0()
                            .bottom_0()
                            .w(px(2.))
                            .bg(bar)
                    }))
                    .children(reply)
                    .child(main)
                    .children(actions),
            )
            .into_any_element()
    }

    /// A message of this account on its way (dimmed) or failed (red, with "Retry"),
    /// drawn like the account's other messages and grouped under them.
    fn render_pending(
        &self,
        row: &PendingRow,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let weak = cx.entity().downgrade();
        let ctx = RenderCtx {
            revealed: true,
            names: self.mention_names_in_open_channel(&row.blocks, cx),
            jumbo: !compact,
            off_thread: Some(self.markdown_off_thread(cx)),
            on_link: Rc::new(move |url, masked, _window, cx| {
                let url = url.to_string();
                let _ = weak.update(cx, |this, cx| this.open_link(&url, masked, cx));
            }),
            on_reveal: Rc::new(|_, _| {}),
        };
        let text = div()
            .min_w_0()
            .when(row.failed.is_none(), |d| d.opacity(0.5))
            .when(row.failed.is_some(), |d| d.text_color(theme::danger()))
            .child(markdown::render(
                &format!("out-{}", row.nonce),
                &row.blocks,
                &ctx,
            ));
        let failed = row.failed.clone().map(|reason| {
            let nonce = row.nonce.clone();
            div()
                .flex()
                .flex_row()
                .gap_2()
                .text_size(px(14.))
                .text_color(theme::danger())
                .child(t!("message-send-failed", reason = reason))
                .child(
                    div()
                        .id(SharedString::from(format!("retry-{}", row.nonce)))
                        .text_color(theme::link())
                        .cursor_pointer()
                        .child(t!("message-retry"))
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.retry_pending(&nonce, cx)),
                        ),
                )
        });
        let name = || {
            div()
                .flex_none()
                .font_weight(FontWeight::MEDIUM)
                .line_height(px(22.))
                .text_color(theme::text_strong())
                .child(row.author.clone())
        };
        // The files of a message with attachments, by name (the composer shows the
        // progress).
        let files = (!row.files.is_empty()).then(|| {
            div()
                .min_w_0()
                .text_size(px(14.))
                .text_color(theme::text_secondary())
                .when(row.failed.is_none(), |d| d.opacity(0.5))
                .child(t!("composer-pending-files", names = row.files.join(", ")))
        });
        let body = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .child(text)
            .children(files)
            .children(failed);
        let main = if compact {
            div()
                .flex()
                .flex_row()
                .items_start()
                .gap_2()
                .child(div().w(px(COMPACT_TIME_COLUMN)).flex_none())
                .child(name())
                .child(body)
        } else {
            let side = (!row.grouped).then(|| {
                let mine = self.my_avatar(cx);
                self.user_avatar(mine.as_ref(), &row.author, AVATAR_COLUMN, false, cx)
            });
            div()
                .flex()
                .flex_row()
                .gap_3()
                .child(div().w(px(AVATAR_COLUMN)).flex_none().children(side))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .when(!row.grouped, |d| d.child(name()))
                        .child(body),
                )
        };
        div()
            .w_full()
            .when(!row.grouped && !compact, |d| d.pt(px(GROUP_SPACING)))
            .child(
                div()
                    .w_full()
                    .pl_4()
                    .pr_4()
                    .py(px(2.))
                    .line_height(px(22.))
                    .text_color(theme::text())
                    .child(main),
            )
            .into_any_element()
    }
}

/// The interactive frame of a message row: its ID, its hover group and its one hover
/// listener. gpui keeps a single `on_hover` per element (a second one replaces the
/// first, and debug builds assert), so everything that follows the pointer over a row
/// (the action bar, an animated avatar) goes through this one listener
/// ([`note_row_hover`]).
pub(super) fn row_frame(
    id: SharedString,
    on_hover: impl Fn(&bool, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    div().id(id).group(ROW_GROUP).on_hover(on_hover)
}

/// The pointer entered (`hovered`) or left message row `id`: the action bar follows it,
/// and so does an animated avatar (`animated`: the row's message ID when its avatar is
/// animated). Only the row's own leave clears what it set: another row may have taken
/// over already (hover changes come in paint order). Whether anything changed.
pub(super) fn note_row_hover(
    actions: &mut super::context_menu::ActionsState,
    playing: &mut Option<String>,
    id: MessageId,
    animated: Option<&str>,
    hovered: bool,
) -> bool {
    let mut changed = actions.hover_row(id, hovered);
    if let Some(message) = animated {
        let before = playing.clone();
        if hovered {
            *playing = Some(message.to_string());
        } else if playing.as_deref() == Some(message) {
            *playing = None;
        }
        changed |= before != *playing;
    }
    changed
}

/// The BOT, SYSTEM or OP tag after a name.
fn render_tag(tag: Tag) -> gpui::Div {
    let label = match tag {
        Tag::Bot => t!("message-bot-tag"),
        Tag::System => t!("message-system-tag"),
        Tag::Op => t!("message-op-tag"),
    };
    pill(label, theme::brand())
}

/// A small filled label (tags, "NEW").
pub(super) fn pill(label: String, bg: Hsla) -> gpui::Div {
    div()
        .flex_none()
        .px(px(5.6))
        .min_h(px(15.))
        .flex()
        .items_center()
        .rounded(px(4.))
        .bg(bg)
        .text_size(px(10.))
        .line_height(px(15.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme::on_brand())
        .child(label)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::chat::context_menu::ActionsState;
    use gpui::{point, Modifiers, TestAppContext};

    struct Host {
        actions: ActionsState,
        playing: Option<String>,
    }

    impl Render for Host {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            // A row whose avatar is animated, built like the list builds it.
            div().size_full().child(
                row_frame(
                    "row-1".into(),
                    cx.listener(|h, hovered: &bool, _, cx| {
                        let (actions, playing) = (&mut h.actions, &mut h.playing);
                        if note_row_hover(actions, playing, MessageId(1), Some("1"), *hovered) {
                            cx.notify();
                        }
                    }),
                )
                .w(px(200.))
                .h(px(40.)),
            )
        }
    }

    /// The row's one hover listener shows the action bar and plays the animated avatar
    /// together. With a second `on_hover` on the row, gpui keeps only the last one (and
    /// debug builds, which tests are, assert when the row is drawn).
    #[gpui::test]
    fn hover_shows_the_bar_and_plays_the_animated_avatar(cx: &mut TestAppContext) {
        let (host, cx) = cx.add_window_view(|_, _| Host {
            actions: ActionsState::default(),
            playing: None,
        });
        cx.simulate_mouse_move(point(px(20.), px(20.)), None, Modifiers::none());
        host.read_with(cx, |h, _| {
            assert!(h.actions.shows_bar(MessageId(1)), "the bar shows");
            assert_eq!(h.playing.as_deref(), Some("1"), "the avatar plays");
        });
        cx.simulate_mouse_move(point(px(20.), px(300.)), None, Modifiers::none());
        host.read_with(cx, |h, _| {
            assert!(!h.actions.shows_bar(MessageId(1)));
            assert_eq!(h.playing, None);
        });
    }

    #[test]
    fn the_message_row_registers_one_hover_listener() {
        let source = include_str!("message.rs");
        let code = &source[..source.find("#[cfg(test)]").unwrap_or(source.len())];
        assert_eq!(code.matches(".on_hover(").count(), 1, "only in `row_frame`");
    }
}
