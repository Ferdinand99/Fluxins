// SPDX-License-Identifier: AGPL-3.0-or-later
//! The header above the channel list: the guild name, or "Direct messages".

use gpui::{div, prelude::*, px, FontWeight};

use crate::app::Inlet;
use crate::theme;
use crate::ui::HEADER_HEIGHT;

impl Inlet {
    pub(super) fn render_guild_header(&self) -> impl IntoElement {
        let title = match &self.selected_guild {
            None => t!("sidebar-direct-messages"),
            Some(id) => self
                .guilds
                .iter()
                .find(|g| &g.id == id)
                .map(|g| g.name.clone())
                .unwrap_or_default(),
        };
        div()
            .h(px(HEADER_HEIGHT))
            .flex_none()
            .px_4()
            .flex()
            .items_center()
            .border_b_1()
            .border_color(theme::divider())
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(theme::text_strong())
            .whitespace_nowrap()
            .overflow_hidden()
            .text_ellipsis()
            .child(title)
    }
}
