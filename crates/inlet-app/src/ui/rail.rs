// SPDX-License-Identifier: AGPL-3.0-or-later
//! The guild rail on the far left: direct messages, then one entry per guild.

use gpui::{div, prelude::*, px, AnyElement, Context, FontWeight, SharedString};

use crate::app::Inlet;
use crate::assets::Icon;
use crate::theme;

/// Guild rail width.
pub const RAIL_WIDTH: f32 = 72.;

impl Inlet {
    pub(crate) fn render_guild_rail(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.selected_guild.clone();
        // One rail entry: a 4 px pill on the left (8 px at rest, 20 on hover, 40 when
        // selected) and a 44 px icon, a circle that becomes a rounded square.
        let item = |id: SharedString, content: AnyElement, active: bool| {
            let group = SharedString::from(format!("{id}-g"));
            div()
                .id(id)
                .group(group.clone())
                .relative()
                .w(px(RAIL_WIDTH))
                .h(px(48.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .child(
                    div()
                        .absolute()
                        .left_0()
                        .top(px(if active { 4. } else { 20. }))
                        .w(px(4.))
                        .h(px(if active { 40. } else { 8. }))
                        .rounded_r(px(4.))
                        .bg(theme::text_strong())
                        .when(!active, |d| {
                            d.opacity(0.).group_hover(group.clone(), |s| {
                                s.opacity(1.).top(px(14.)).h(px(20.))
                            })
                        }),
                )
                .child(
                    div()
                        .size(px(44.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(if active { 13. } else { 22. }))
                        .bg(if active {
                            theme::brand()
                        } else {
                            theme::guild_item()
                        })
                        .text_color(theme::text_strong())
                        .font_weight(FontWeight::MEDIUM)
                        .text_size(px(15.))
                        .group_hover(group, |s| s.rounded(px(13.)).bg(theme::brand()))
                        .child(content),
                )
        };
        div()
            .id("guild-rail")
            .w(px(RAIL_WIDTH))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(6.))
            .pt_2()
            .bg(theme::bg_app())
            .overflow_y_scroll()
            .child(
                item(
                    "home".into(),
                    Icon::ChatCircle
                        .el(px(22.), theme::text_strong())
                        .into_any_element(),
                    selected.is_none(),
                )
                .on_click(cx.listener(|this, _, _, cx| this.select_guild(None, cx))),
            )
            .child(
                div()
                    .w(px(32.))
                    .h(px(2.))
                    .flex_none()
                    .rounded_full()
                    .bg(theme::guild_item()),
            )
            .children(self.guilds.iter().map(|g| {
                let id = g.id.clone();
                let active = selected.as_deref() == Some(g.id.as_str());
                // The guild's icon when it has one, rounded like the entry.
                let icon = self.guild_icon_image(&g.id, 44., cx).map(|i| {
                    i.rounded(px(if active { 13. } else { 22. }))
                        .group_hover(format!("guild-{}-g", g.id), |s| s.rounded(px(13.)))
                        .into_any_element()
                });
                item(
                    format!("guild-{}", g.id).into(),
                    icon.unwrap_or_else(|| g.initials().into_any_element()),
                    active,
                )
                .on_click(
                    cx.listener(move |this, _, _, cx| this.select_guild(Some(id.clone()), cx)),
                )
            }))
    }
}
