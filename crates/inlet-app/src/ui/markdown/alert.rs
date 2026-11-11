// SPDX-License-Identifier: AGPL-3.0-or-later
//! Alerts (`> [!NOTE]`, `[!TIP]`, `[!IMPORTANT]`, `[!WARNING]`, `[!CAUTION]`), drawn like
//! Fluxer's `.alert`: a 4 px bar in the alert's colour, a title with its Phosphor icon
//! (regular weight, as Fluxer's `AlertRenderer`) and the content in the message colour.

use fluxer_markdown_parser::ast::AlertType;
use gpui::{div, prelude::*, px, svg, AnyElement, FontWeight, Hsla};

use super::blocks::{render_seq, Depth, Draw};
use super::{AlertKind, Block};
use crate::theme;

pub(super) fn kind(t: &AlertType) -> AlertKind {
    match t {
        AlertType::Note => AlertKind::Note,
        AlertType::Tip => AlertKind::Tip,
        AlertType::Important => AlertKind::Important,
        AlertType::Warning => AlertKind::Warning,
        AlertType::Caution => AlertKind::Caution,
    }
}

fn look(kind: AlertKind) -> (Hsla, &'static [u8], String) {
    match kind {
        AlertKind::Note => (
            theme::alert_note(),
            include_bytes!("../../../assets/icons/markdown/info.svg"),
            t!("markdown-alert-note"),
        ),
        AlertKind::Tip => (
            theme::alert_tip(),
            include_bytes!("../../../assets/icons/markdown/lightbulb-filament.svg"),
            t!("markdown-alert-tip"),
        ),
        AlertKind::Important => (
            theme::alert_important(),
            include_bytes!("../../../assets/icons/markdown/warning.svg"),
            t!("markdown-alert-important"),
        ),
        AlertKind::Warning => (
            theme::alert_warning(),
            include_bytes!("../../../assets/icons/markdown/warning-octagon.svg"),
            t!("markdown-alert-warning"),
        ),
        AlertKind::Caution => (
            theme::alert_caution(),
            include_bytes!("../../../assets/icons/markdown/warning-circle.svg"),
            t!("markdown-alert-caution"),
        ),
    }
}

/// Draws an alert: padding 6 px 12 px 6 px 16 px, 6 px corners, title 15 px semibold.
pub(super) fn render(
    id: &str,
    kind: AlertKind,
    children: &[Block],
    d: &Draw,
    depth: Depth,
) -> AnyElement {
    let (color, icon, title) = look(kind);
    div()
        .relative()
        .pl(px(16.))
        .pr(px(12.))
        .py(px(6.))
        .rounded(px(6.))
        .child(
            div()
                .absolute()
                .left_0()
                .top_0()
                .bottom_0()
                .w(px(4.))
                .rounded(px(4.))
                .bg(color),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.))
                .mb(px(4.))
                .text_size(px(15.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(color)
                .child(svg().data(icon).size(px(15.)).flex_none().text_color(color))
                .child(title),
        )
        .child(
            div()
                .mt(px(4.))
                .text_color(theme::text())
                .child(render_seq(id, children, d, depth)),
        )
        .into_any_element()
}
