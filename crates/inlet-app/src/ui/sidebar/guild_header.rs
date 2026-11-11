// SPDX-License-Identifier: AGPL-3.0-or-later
//! The header above the channel list: the guild's banner when it has one, then the
//! guild name, or "Direct messages".

use gpui::{div, prelude::*, px, Context, FontWeight};

use crate::app::Inlet;
use crate::theme;
use crate::ui::HEADER_HEIGHT;

impl Inlet {
    pub(super) fn render_guild_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // The DM list's header is the quick switcher bar (see `dm_list`).
        if self.selected_guild.is_none() {
            return div().flex_none().child(self.render_dm_header(cx));
        }
        let title = match &self.selected_guild {
            None => t!("sidebar-direct-messages"),
            Some(id) => self
                .guilds
                .iter()
                .find(|g| &g.id == id)
                .map(|g| g.name.clone())
                .unwrap_or_default(),
        };
        // Deviation: a 120 px banner with the name below it, not Fluxer's 16:9 banner
        // with the name over it (see `ui/media/avatar.rs`).
        let banner = self.guild_banner(super::SIDEBAR_WIDTH, cx);
        let name = div()
            .debug_selector(|| "guild-header".into())
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
            .child(title);
        div()
            .flex_none()
            .flex()
            .flex_col()
            .children(banner)
            .child(name)
    }
}
