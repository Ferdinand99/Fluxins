// SPDX-License-Identifier: AGPL-3.0-or-later
//! A person's roles in a guild, as small chips with the role's colour, highest role
//! first (the everyone role is never listed).

use gpui::{div, prelude::*, px, AnyElement, App, FontWeight, Hsla};
use inlet_state::{GuildId, RoleId};

use crate::app::Inlet;
use crate::{model, theme};

/// Roles shown at most.
const MAX_SHOWN: usize = 40;

impl Inlet {
    /// The chips of `roles` (IDs) in `guild`; roles the account state does not know
    /// are left out.
    pub(super) fn role_chips(&self, guild: GuildId, roles: &[String], cx: &App) -> AnyElement {
        let mut known: Vec<(i64, String, Option<Hsla>)> = Vec::new();
        if let Some(gs) = self.account.as_ref().and_then(|a| a.read(cx).guild(guild)) {
            for id in roles.iter().filter_map(|r| RoleId::parse(r)) {
                let Some(role) = gs.roles.get(id) else {
                    continue;
                };
                if role.id == guild.to_string() {
                    continue;
                }
                let color = (role.color & 0xFF_FF_FF != 0)
                    .then(|| gpui::rgb((role.color & 0xFF_FF_FF) as u32).into());
                known.push((role.position, model::clean_name(&role.name), color));
            }
        }
        known.sort_by_key(|k| std::cmp::Reverse(k.0));
        known.truncate(MAX_SHOWN);
        if known.is_empty() {
            return div()
                .text_size(px(13.))
                .text_color(theme::text_tertiary())
                .child(t!("profile-no-roles"))
                .into_any_element();
        }
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap(px(4.))
            .children(known.into_iter().map(|(_, name, color)| {
                div()
                    .h(px(22.))
                    .px(px(6.))
                    .rounded(px(4.))
                    .bg(theme::guild_item())
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(4.))
                    .child(
                        div()
                            .size(px(10.))
                            .flex_none()
                            .rounded_full()
                            .bg(color.unwrap_or_else(theme::text_tertiary)),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme::text())
                            .child(name),
                    )
            }))
            .into_any_element()
    }
}
