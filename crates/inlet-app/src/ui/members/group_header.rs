// SPDX-License-Identifier: AGPL-3.0-or-later
//! A group header of the member list: `{name}—{count}`, 14 px / 600 / 16 px in the
//! muted text colour, 32 px high (12 px above the text, 4 px below), as Fluxer's
//! `MemberListGroupHeader`. Names are never upper-cased.

use gpui::{div, prelude::*, px, AnyElement, App, FontWeight};
use inlet_state::{GuildId, RoleId};

use super::HEADER_HEIGHT;
use crate::app::Inlet;
use crate::{model, theme};

/// Characters of a role name shown in a header.
const MAX_LABEL_CHARS: usize = 100;

pub fn render(label: &str, count: u64) -> AnyElement {
    div()
        .h(px(HEADER_HEIGHT))
        .pt(px(12.))
        .pb(px(4.))
        .px(px(8.))
        .text_size(px(14.))
        .line_height(px(16.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme::text_muted())
        .truncate()
        .child(t!("members-group", name = label, count = count))
        .into_any_element()
}

impl Inlet {
    /// The name of a group: the hoisted role's name, "Online" or "Offline". A role the
    /// account state does not know is shown as "Online" (hoisted groups hold online
    /// members only).
    pub(super) fn group_label(&self, guild: GuildId, id: &str, cx: &App) -> String {
        match id {
            "offline" => return t!("members-offline"),
            "online" => return t!("members-online"),
            _ => {}
        }
        let role = RoleId::parse(id).and_then(|r| {
            let state = self.account.as_ref()?.read(cx);
            let name = state.guild(guild)?.roles.get(r)?.name.clone();
            Some(name)
        });
        match role.map(|n| model::clean_name(&n)) {
            Some(n) if !n.trim().is_empty() => n.chars().take(MAX_LABEL_CHARS).collect(),
            _ => t!("members-online"),
        }
    }
}
