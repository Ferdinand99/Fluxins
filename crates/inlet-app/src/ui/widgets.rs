// SPDX-License-Identifier: AGPL-3.0-or-later
//! Small shared widgets: buttons and labelled form fields.

use gpui::{div, prelude::*, px, Entity, FontWeight, SharedString};

use crate::text_input::TextInput;
use crate::theme;

pub fn field(label: impl AsRef<str>, input: Entity<TextInput>) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_size(px(12.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme::text_muted())
                .child(label.as_ref().to_uppercase()),
        )
        .child(input)
}

pub fn button(
    id: &'static str,
    label: impl Into<SharedString>,
    primary: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_4()
        .py_2()
        .rounded_md()
        .flex()
        .justify_center()
        .cursor_pointer()
        .text_color(theme::text_strong())
        .bg(if primary {
            theme::brand()
        } else {
            theme::bg_textarea()
        })
        .hover(|s| s.opacity(0.85))
        .child(label.into())
}
