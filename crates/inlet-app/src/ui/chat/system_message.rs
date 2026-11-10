// SPDX-License-Identifier: AGPL-3.0-or-later
//! System messages (someone joined, a message was pinned, a call, a renamed channel, a
//! new thread, …) as one line with an icon in the avatar column, and a neutral line for
//! types Inlet does not know. The text is built from the catalogue with cleaned names;
//! the message's own content is never parsed as markdown here.

use gpui::{div, prelude::*, px, svg, AnyElement, FontWeight, Hsla, Pixels, Svg};

use super::message::{AVATAR_COLUMN, GROUP_SPACING};
use super::messages::{SystemKind, SystemRow};
use crate::assets::Icon;
use crate::theme;

/// Icons of the message list that the rest of the app does not use (Phosphor, MIT,
/// fill weight, like [`Icon`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatIcon {
    ArrowRight,
    ArrowDown,
    PushPin,
    Phone,
    Pencil,
    UserPlus,
    UserMinus,
    Warning,
}

impl ChatIcon {
    fn data(self) -> &'static [u8] {
        match self {
            ChatIcon::ArrowRight => include_bytes!("../../../assets/icons/arrow-right.svg"),
            ChatIcon::ArrowDown => include_bytes!("../../../assets/icons/arrow-down.svg"),
            ChatIcon::PushPin => include_bytes!("../../../assets/icons/push-pin.svg"),
            ChatIcon::Phone => include_bytes!("../../../assets/icons/phone.svg"),
            ChatIcon::Pencil => include_bytes!("../../../assets/icons/pencil-simple.svg"),
            ChatIcon::UserPlus => include_bytes!("../../../assets/icons/user-plus.svg"),
            ChatIcon::UserMinus => include_bytes!("../../../assets/icons/user-minus.svg"),
            ChatIcon::Warning => include_bytes!("../../../assets/icons/warning-circle.svg"),
        }
    }

    /// The icon as an element, `size` square, drawn in `color`.
    pub fn el(self, size: Pixels, color: Hsla) -> Svg {
        svg()
            .data(self.data())
            .size(size)
            .flex_none()
            .text_color(color)
    }
}

pub fn render(row: &SystemRow, compact: bool) -> AnyElement {
    let size = px(18.);
    let color = theme::text_tertiary();
    let icon = match row.kind {
        SystemKind::Join => ChatIcon::ArrowRight.el(size, color).into_any_element(),
        SystemKind::Pin => ChatIcon::PushPin.el(size, color).into_any_element(),
        SystemKind::Call => ChatIcon::Phone.el(size, color).into_any_element(),
        SystemKind::Edit => ChatIcon::Pencil.el(size, color).into_any_element(),
        SystemKind::RecipientAdd => ChatIcon::UserPlus.el(size, color).into_any_element(),
        SystemKind::RecipientRemove => ChatIcon::UserMinus.el(size, color).into_any_element(),
        SystemKind::Thread => Icon::Chats.el(size, color).into_any_element(),
        SystemKind::Follow => Icon::Megaphone.el(size, color).into_any_element(),
        SystemKind::Unknown => Icon::Info.el(size, color).into_any_element(),
    };
    div()
        .w_full()
        .when(!compact, |d| d.pt(px(GROUP_SPACING)))
        .child(
            div()
                .w_full()
                .pl_4()
                .pr_4()
                .py(px(2.))
                .flex()
                .flex_row()
                .items_center()
                .gap_3()
                .line_height(px(22.))
                .hover(|s| s.bg(theme::hover_bg()))
                .child(
                    div()
                        .w(px(AVATAR_COLUMN))
                        .flex_none()
                        .flex()
                        .justify_center()
                        .child(icon),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .items_baseline()
                        .gap_2()
                        .child(
                            div()
                                .text_color(theme::text_tertiary())
                                .when(row.kind == SystemKind::Unknown, |d| d.italic())
                                .child(row.text.clone()),
                        )
                        .child(
                            div()
                                .text_size(px(12.))
                                .font_weight(FontWeight::NORMAL)
                                .text_color(theme::timestamp())
                                .child(row.time.clone()),
                        ),
                ),
        )
        .into_any_element()
}
