// SPDX-License-Identifier: AGPL-3.0-or-later
//! The Desktop tab: closing to the tray, starting at login (hidden or not) and zoom.
//! The values live in `desktop::Desktop`; this tab only shows and changes them.

use gpui::{div, prelude::*, px, AnyElement, Context, Div, FontWeight, SharedString, Stateful};

use super::super::{SettingsTab, SettingsView};
use crate::assets::Icon;
use crate::desktop::{self, Desktop};
use crate::theme;
use crate::ui::widgets::button;

pub struct DesktopTab;

/// A row with a check box. A row that cannot be changed right now is dimmed and does
/// not react.
fn toggle(id: &'static str, label: String, on: bool, enabled: bool) -> Stateful<Div> {
    let mark = if on {
        Icon::CheckSquare
            .el(px(20.), theme::brand())
            .into_any_element()
    } else {
        div()
            .size(px(20.))
            .flex_none()
            .rounded(px(4.))
            .border_2()
            .border_color(theme::text_muted())
            .into_any_element()
    };
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .py_2()
        .when(enabled, |d| d.cursor_pointer())
        .when(!enabled, |d| d.opacity(0.5))
        .child(mark)
        .child(div().text_color(theme::text_strong()).child(label))
}

fn note(text: String) -> Div {
    div()
        .pl(px(32.))
        .text_size(px(13.))
        .text_color(theme::text_muted())
        .child(text)
}

fn heading(text: String) -> Div {
    div()
        .pt_4()
        .pb_1()
        .text_size(px(12.))
        .font_weight(FontWeight::BOLD)
        .text_color(theme::text_muted())
        .child(text)
}

impl SettingsTab for DesktopTab {
    fn id(&self) -> &'static str {
        "desktop"
    }

    fn title(&self) -> SharedString {
        t!("settings-desktop-title").into()
    }

    fn render(&mut self, cx: &mut Context<SettingsView>) -> AnyElement {
        let Some(d) = cx.try_global::<Desktop>() else {
            return div().into_any_element();
        };
        let close_to_tray = d.prefs.close_to_tray;
        let tray = d.tray_shown();
        let autostart = d.autostart;
        let hidden = d.prefs.start_minimized;
        let zoom = d.prefs.zoom;
        let error = d.autostart_error.clone();

        let mut col = div()
            .flex()
            .flex_col()
            .max_w(px(640.))
            .child(
                div()
                    .text_size(px(20.))
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme::text_strong())
                    .child(t!("settings-desktop-title")),
            )
            .child(heading(t!("settings-desktop-window")))
            .child(
                toggle(
                    "settings-close-to-tray",
                    t!("settings-desktop-close-to-tray"),
                    close_to_tray,
                    true,
                )
                .on_click(cx.listener(move |_, _, _, cx| {
                    desktop::set_close_to_tray(!close_to_tray, cx);
                    cx.notify();
                })),
            );
        if !tray {
            col = col.child(note(t!("settings-desktop-no-tray")));
        }

        col = col.child(heading(t!("settings-desktop-startup"))).child(
            toggle(
                "settings-autostart",
                t!("settings-desktop-autostart"),
                autostart == Some(true),
                autostart.is_some(),
            )
            .on_click(cx.listener(move |_, _, _, cx| {
                if let Some(on) = autostart {
                    desktop::set_autostart(!on, hidden, cx);
                    cx.notify();
                }
            })),
        );
        if autostart.is_none() {
            col = col.child(note(t!("settings-desktop-autostart-unknown")));
        }
        col = col.child(
            toggle(
                "settings-start-hidden",
                t!("settings-desktop-start-hidden"),
                hidden,
                autostart == Some(true),
            )
            .on_click(cx.listener(move |_, _, _, cx| {
                if autostart == Some(true) {
                    desktop::set_autostart(true, !hidden, cx);
                    cx.notify();
                }
            })),
        );
        if let Some(error) = error {
            col = col.child(note(t!("settings-desktop-autostart-failed", error = error)));
        }

        let percent = (zoom * 100.0).round() as u32;
        col = col.child(heading(t!("settings-desktop-zoom"))).child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .py_2()
                .child(
                    button("settings-zoom-out", "\u{2212}", false).on_click(cx.listener(
                        |_, _, _, cx| {
                            desktop::zoom_by(-1, cx);
                            cx.notify();
                        },
                    )),
                )
                .child(
                    div()
                        .w(px(64.))
                        .flex()
                        .justify_center()
                        .text_color(theme::text_strong())
                        .child(t!("settings-desktop-zoom-value", percent = percent)),
                )
                .child(button("settings-zoom-in", "+", false).on_click(cx.listener(
                    |_, _, _, cx| {
                        desktop::zoom_by(1, cx);
                        cx.notify();
                    },
                )))
                .child(
                    button(
                        "settings-zoom-reset",
                        t!("settings-desktop-zoom-reset"),
                        false,
                    )
                    .on_click(cx.listener(|_, _, _, cx| {
                        desktop::set_zoom(1.0, cx);
                        cx.notify();
                    })),
                ),
        );
        col = col.child(note(t!("settings-desktop-zoom-hint")));
        col.into_any_element()
    }
}
