// SPDX-License-Identifier: AGPL-3.0-or-later
//! The start of a channel: an 80 px icon circle, "Welcome to #channel!" (30 px, 600)
//! and a muted line under it (18 px). Shown above the oldest message once the list
//! reaches the channel's beginning.

use gpui::{div, prelude::*, px, AnyElement, FontWeight};

use super::messages::WelcomeRow;
use crate::assets::Icon;
use crate::theme;

pub fn render(w: &WelcomeRow) -> AnyElement {
    let icon = if w.private { Icon::At } else { Icon::Hash };
    div()
        .w_full()
        .px_4()
        .pt(px(120.))
        .pb_2()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .size(px(80.))
                .rounded_full()
                .bg(theme::guild_item())
                .flex()
                .items_center()
                .justify_center()
                .child(icon.el(px(44.), theme::text_strong())),
        )
        .child(
            div()
                .text_size(px(30.))
                .line_height(px(38.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme::text_strong())
                .child(w.title.clone()),
        )
        .child(
            div()
                .text_size(px(18.))
                .line_height(px(24.))
                .text_color(theme::text_muted())
                .child(w.subtitle.clone()),
        )
        .into_any_element()
}
