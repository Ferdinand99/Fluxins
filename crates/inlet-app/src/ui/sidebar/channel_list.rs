// SPDX-License-Identifier: AGPL-3.0-or-later
//! The channel list of a guild: categories that fold, and channel rows.

use gpui::{div, prelude::*, px, AnyElement, Context, FontWeight, SharedString};

use crate::app::Inlet;
use crate::assets::Icon;
use crate::model::{Channel, ChannelKind};
use crate::theme;
use crate::ui::notifications::badge::{mention_badge, RowMarks};

pub fn channel_icon(kind: ChannelKind) -> Icon {
    match kind {
        ChannelKind::Text => Icon::Hash,
        ChannelKind::Announcement => Icon::Megaphone,
        ChannelKind::Voice => Icon::Speaker,
        ChannelKind::Dm | ChannelKind::GroupDm => Icon::At,
        _ => Icon::Chats,
    }
}

pub fn channel_label(c: &Channel) -> String {
    match c.kind {
        ChannelKind::Dm | ChannelKind::GroupDm => format!("@{}", c.name),
        ChannelKind::Text | ChannelKind::Announcement => format!("#{}", c.name),
        _ => c.name.clone(),
    }
}

impl Inlet {
    /// The rows of a guild's channel list.
    pub(super) fn channel_rows(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let selected = self.selected_channel.clone();
        let mut previous_was_channel = false;
        self.channels
            .iter()
            .filter(|c| {
                // Folded categories hide their channels, except the open one.
                c.kind == ChannelKind::Category
                    || c.parent_id
                        .as_ref()
                        .is_none_or(|p| !self.sidebar.collapsed.contains(p))
                    || selected.as_deref() == Some(c.id.as_str())
            })
            .map(|c| {
                if c.kind == ChannelKind::Category {
                    let folded = self.sidebar.collapsed.contains(&c.id);
                    let cat = c.id.clone();
                    let spaced = std::mem::replace(&mut previous_was_channel, false);
                    // A muted category is drawn at half strength, as a muted channel is.
                    let dimmed = self.badge(&c.id).dimmed;
                    return div()
                        .id(SharedString::from(format!("ch-{}", c.id)))
                        .flex_none()
                        .when(spaced, |d| d.mt_4())
                        .when(dimmed, |d| d.opacity(0.5))
                        .h(px(32.))
                        .ml_2()
                        .px_2()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_1()
                        .cursor_pointer()
                        .text_size(px(14.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme::text_channel())
                        .hover(|s| s.text_color(theme::text_strong()))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .child(c.name.clone()),
                        )
                        .child(
                            if folded {
                                Icon::CaretRight
                            } else {
                                Icon::CaretDown
                            }
                            .el(px(12.), theme::text_channel()),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if !this.sidebar.collapsed.remove(&cat) {
                                this.sidebar.collapsed.insert(cat.clone());
                            }
                            cx.notify();
                        }))
                        .into_any_element();
                }
                previous_was_channel = true;
                let active = selected.as_deref() == Some(c.id.as_str());
                self.channel_row(c, active, cx)
            })
            .collect()
    }

    /// One channel (or direct message) row.
    pub(super) fn channel_row(
        &self,
        c: &Channel,
        active: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let messageable = c.is_messageable();
        let id = c.id.clone();
        let badge = self.badge(&c.id);
        let marks = RowMarks::of(badge, active);
        let color = if active {
            theme::text_strong()
        } else if marks.highlight {
            theme::text_secondary()
        } else {
            theme::text_channel()
        };
        let row = div()
            .id(SharedString::from(format!("ch-{}", c.id)))
            // Rows must not shrink: a scrolling flex column shrinks its children
            // when the list is taller than the view, and `overflow_hidden` lets
            // a row shrink to nothing.
            .flex_none()
            .h(px(32.))
            .ml_2()
            .px_2()
            .rounded(px(6.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.))
            .whitespace_nowrap()
            .overflow_hidden()
            .font_weight(FontWeight::MEDIUM)
            .text_color(color)
            .when(active, |d| d.bg(theme::selected_bg()))
            .when(messageable, |d| {
                d.cursor_pointer()
                    .hover(|s| s.bg(theme::hover_bg()).text_color(theme::text()))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.select_channel(id.clone(), window, cx)
                    }))
            })
            .child(channel_icon(c.kind).el(px(20.), color))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(c.name.clone()),
            )
            .when(marks.count, |d| d.child(mention_badge(badge.mentions)));
        // The unread pill sits 4 px left of the row, outside its clipped box, so the row
        // goes in a frame that carries it (and the half-strength look of a muted channel).
        div()
            .relative()
            .flex_none()
            .flex()
            .flex_col()
            .when(marks.dimmed, |d| d.opacity(0.5))
            .when(marks.pill, |d| {
                d.child(
                    div()
                        .absolute()
                        .left(px(4.))
                        .top(px(12.))
                        .w(px(8.))
                        .h(px(8.))
                        .rounded_r(px(4.))
                        .bg(theme::text_strong())
                        .when(marks.pill_faded, |p| p.opacity(0.4)),
                )
            })
            .child(row)
            .into_any_element()
    }
}
