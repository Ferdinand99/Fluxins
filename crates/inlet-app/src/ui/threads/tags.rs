// SPDX-License-Identifier: AGPL-3.0-or-later
//! Forum tags: the pills on a post, and the filter bar of the forum view.
//!
//! Tag names come from the server and are cleaned like any name. A tag's custom emoji
//! is not drawn (its Unicode emoji is).

use gpui::{div, prelude::*, px, AnyElement, Context, SharedString};
use inlet_protocol::api::threads::ForumTag;

use crate::app::Inlet;
use crate::model;
use crate::theme;

/// What a tag shows: its Unicode emoji, then its name.
pub fn label(tag: &ForumTag) -> String {
    let name = model::clean_name(&tag.name);
    match tag.emoji_name.as_deref().map(str::trim) {
        Some(e) if !e.is_empty() && tag.emoji_id.is_none() => {
            format!("{} {name}", e.chars().take(8).collect::<String>())
        }
        _ => name,
    }
}

/// A small pill with a tag's label.
pub fn pill(tag: &ForumTag, picked: bool) -> gpui::Div {
    div()
        .flex_none()
        .px_2()
        .py(px(1.))
        .rounded(px(12.))
        .text_size(px(12.))
        .line_height(px(18.))
        .whitespace_nowrap()
        .border_1()
        .border_color(if picked {
            theme::brand()
        } else {
            theme::accent_border()
        })
        .bg(if picked {
            theme::selected_bg()
        } else {
            theme::bg_textarea()
        })
        .text_color(if picked {
            theme::text_strong()
        } else {
            theme::text_secondary()
        })
        .child(label(tag))
}

/// The tags of a post, in the forum's order; tags the forum no longer has are left out.
pub fn post_tags(applied: &[String], tags: &[ForumTag]) -> Option<AnyElement> {
    let shown: Vec<&ForumTag> = tags.iter().filter(|t| applied.contains(&t.id)).collect();
    if shown.is_empty() {
        return None;
    }
    Some(
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap_1()
            .children(shown.into_iter().map(|t| pill(t, false)))
            .into_any_element(),
    )
}

/// The forum's tags as filter buttons; a click picks or drops one.
pub fn filter_bar(
    _this: &Inlet,
    tags: &[ForumTag],
    picked: &[String],
    cx: &mut Context<Inlet>,
) -> Vec<AnyElement> {
    tags.iter()
        .map(|tag| {
            let on = picked.contains(&tag.id);
            let id = tag.id.clone();
            div()
                .id(SharedString::from(format!("forum-tag-{}", tag.id)))
                .cursor_pointer()
                .hover(|s| s.opacity(0.85))
                .child(pill(tag, on))
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_forum_tag(id.clone(), cx)))
                .into_any_element()
        })
        .collect()
}
