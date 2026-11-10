// SPDX-License-Identifier: AGPL-3.0-or-later
//! The channel list of a guild: categories that fold, and channel rows.

use gpui::{div, prelude::*, px, AnyElement, Context, FontWeight, SharedString};

use crate::app::Inlet;
use crate::assets::Icon;
use crate::model::{Channel, ChannelKind};
use crate::theme;

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
                    return div()
                        .id(SharedString::from(format!("ch-{}", c.id)))
                        .flex_none()
                        .when(spaced, |d| d.mt_4())
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
        let color = if active {
            theme::text_strong()
        } else {
            theme::text_channel()
        };
        div()
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
            .into_any_element()
    }
}
