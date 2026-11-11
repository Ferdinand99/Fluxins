// SPDX-License-Identifier: AGPL-3.0-or-later
//! The Notifications tab: the global desktop switches (notifications, the message in
//! them, the taskbar flash, the sound) and a test notification. Which messages notify
//! per server and channel is the account's setting on the server, changed with the bell
//! in the channel header (`ui/overlays/notification_settings.rs`); this tab only points
//! there. The values live in the `Alerts` global.

use gpui::{div, prelude::*, px, AnyElement, Context, FontWeight, SharedString};

use super::super::{SettingsTab, SettingsView};
use super::desktop::{heading, note, toggle};
use crate::theme;
use crate::ui::notifications::alerts::{self, Alerts};
use crate::ui::widgets::button;

pub struct NotificationsTab;

impl SettingsTab for NotificationsTab {
    fn id(&self) -> &'static str {
        "notifications"
    }

    fn title(&self) -> SharedString {
        t!("settings-notifications-title").into()
    }

    fn render(&mut self, cx: &mut Context<SettingsView>) -> AnyElement {
        let prefs = cx
            .try_global::<Alerts>()
            .map(|a| a.prefs().clone())
            .unwrap_or_default();
        let enabled = prefs.enabled;

        div()
            .flex()
            .flex_col()
            .max_w(px(640.))
            .child(
                div()
                    .text_size(px(20.))
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme::text_strong())
                    .child(t!("settings-notifications-title")),
            )
            .child(heading(t!("settings-notifications-desktop")))
            .child(
                toggle(
                    "settings-notifications-enabled",
                    t!("settings-notifications-enabled"),
                    enabled,
                    true,
                )
                .on_click(cx.listener(move |_, _, _, cx| {
                    alerts::change_prefs(cx, |p| p.enabled = !enabled);
                    cx.notify();
                })),
            )
            .child(
                toggle(
                    "settings-notifications-preview",
                    t!("settings-notifications-preview"),
                    prefs.preview,
                    enabled,
                )
                .on_click(cx.listener(move |_, _, _, cx| {
                    if enabled {
                        alerts::change_prefs(cx, |p| p.preview = !p.preview);
                        cx.notify();
                    }
                })),
            )
            .child(note(t!("settings-notifications-preview-note")))
            .child(
                toggle(
                    "settings-notifications-flash",
                    t!("settings-notifications-flash"),
                    prefs.flash,
                    enabled,
                )
                .on_click(cx.listener(move |_, _, _, cx| {
                    if enabled {
                        alerts::change_prefs(cx, |p| p.flash = !p.flash);
                        cx.notify();
                    }
                })),
            )
            .child(heading(t!("settings-notifications-sounds")))
            .child(
                toggle(
                    "settings-notifications-sound",
                    t!("settings-notifications-sound"),
                    prefs.sound,
                    true,
                )
                .on_click(cx.listener(|_, _, _, cx| {
                    alerts::change_prefs(cx, |p| p.sound = !p.sound);
                    cx.notify();
                })),
            )
            .child(heading(t!("settings-notifications-rules")))
            .child(note(t!("settings-notifications-rules-note")).pl_0())
            .child(note(t!("settings-notifications-dnd-note")).pl_0().pt_2())
            .child(
                note(t!("settings-notifications-every-account"))
                    .pl_0()
                    .pt_2(),
            )
            .child(
                div().flex().flex_row().pt_4().child(
                    button(
                        "settings-notifications-test",
                        t!("settings-notifications-test"),
                        false,
                    )
                    .on_click(cx.listener(|_, _, _, cx| {
                        alerts::send_test_notification(cx);
                    })),
                ),
            )
            .into_any_element()
    }
}
