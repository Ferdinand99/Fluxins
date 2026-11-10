// SPDX-License-Identifier: AGPL-3.0-or-later
//! The frame of a modal dialog: a dimmed backdrop over the whole window that blocks
//! clicks, with a card in the middle.

use gpui::{div, prelude::*, px, rgba, AnyElement, Div, ElementId, Stateful};

use crate::theme;

/// A modal with `children` stacked in a card `width` pixels wide.
pub fn modal(
    id: impl Into<ElementId>,
    width: f32,
    children: impl IntoIterator<Item = AnyElement>,
) -> Stateful<Div> {
    div()
        .id(id)
        .occlude()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(rgba(0x000000a0))
        .child(
            div()
                .w(px(width))
                .p_6()
                .rounded_lg()
                .bg(theme::bg_chat())
                .flex()
                .flex_col()
                .gap_3()
                .children(children),
        )
}
