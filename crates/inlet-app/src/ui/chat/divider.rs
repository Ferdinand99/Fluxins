// SPDX-License-Identifier: AGPL-3.0-or-later
//! Dividers in the message list: the date separator and the unread line with "NEW".
//! When the first unread message is also the first of a day, the date separator turns
//! red and carries the "NEW" mark, as in Fluxer.

use chrono::NaiveDate;
use gpui::{div, prelude::*, px, AnyElement, FontWeight};

use super::message::{pill, GROUP_SPACING};
use crate::model;
use crate::theme;

/// The "NEW" mark at the right end of the line.
fn new_mark() -> gpui::Div {
    pill(t!("chat-new-mark"), theme::danger())
        .px(px(8.))
        .rounded(px(3.))
        .font_weight(FontWeight::BOLD)
}

fn line(color: gpui::Hsla) -> gpui::Div {
    div().flex_1().h(px(2.)).bg(color)
}

/// The date separator: a 20 px row with the date in the middle of a 2 px line.
pub fn render_day(day: NaiveDate, new: bool) -> AnyElement {
    let (line_color, text_color) = if new {
        (theme::danger().opacity(0.4), theme::danger())
    } else {
        (theme::separator_line(), theme::text_tertiary())
    };
    div()
        .w_full()
        .pt(px(GROUP_SPACING))
        .px(px(20.))
        .child(
            div()
                .h(px(20.))
                .flex()
                .flex_row()
                .items_center()
                .child(line(line_color))
                .child(
                    div()
                        .px(px(12.))
                        .text_size(px(12.))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(text_color)
                        .child(model::day_label(day)),
                )
                .child(line(line_color))
                .when(new, |d| d.child(new_mark())),
        )
        .into_any_element()
}

/// The unread line between two messages of the same day.
pub fn render_unread() -> AnyElement {
    div()
        .w_full()
        .h(px(GROUP_SPACING))
        .pl_4()
        .pr(px(20.))
        .flex()
        .flex_row()
        .items_center()
        .child(line(theme::danger().opacity(0.4)))
        .child(new_mark())
        .into_any_element()
}
