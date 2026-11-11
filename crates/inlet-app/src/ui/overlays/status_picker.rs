// SPDX-License-Identifier: AGPL-3.0-or-later
//! The status picker: Fluxer's user area menu, opened from the user panel. It sets the
//! custom status (an emoji, a text and when it clears) and the status (online, idle, do
//! not disturb, invisible), and leads to the account switcher.
//!
//! A layer (see `desktop::OpenLayers`): while it is the topmost one it has the keyboard,
//! Escape closes it first, and nothing behind it is marked read by Escape. A click
//! outside closes it. A change shows at once and is saved on the account; when the save
//! fails, the saved status shows again (see `backend/social/presence`).

use std::time::Duration;

use gpui::{
    div, prelude::*, px, AnyElement, Context, Entity, Focusable, FontWeight, MouseButton,
    MouseDownEvent, SharedString, Subscription, Window,
};
use inlet_protocol::models::CustomStatus;
use inlet_state::presence::{CUSTOM_STATUS_TEXT_LIMIT, STATUSES};

use crate::app::{Inlet, Screen};
use crate::backend::social::presence::Cmd;
use crate::desktop::Layer;
use crate::text_input::{InputEvent, TextInput};
use crate::theme;
use crate::ui::social::presence::{custom_status_line, emoji, status_dot, status_label};
use crate::ui::widgets::button;

/// Width of the picker card.
const WIDTH: f32 = 264.;
/// The emoji offered for a custom status.
// Deviation: Fluxer opens its full emoji picker here; Inlet offers a short row.
pub const EMOJI: [&str; 8] = [
    "\u{1F600}",
    "\u{1F3A7}",
    "\u{1F4BB}",
    "\u{1F319}",
    "\u{1F355}",
    "\u{1F3AE}",
    "\u{1F4DA}",
    "\u{2708}\u{FE0F}",
];

/// When a custom status clears.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClearAfter {
    Never,
    ThirtyMinutes,
    OneHour,
    FourHours,
    OneDay,
}

impl ClearAfter {
    const ALL: [ClearAfter; 5] = [
        ClearAfter::Never,
        ClearAfter::ThirtyMinutes,
        ClearAfter::OneHour,
        ClearAfter::FourHours,
        ClearAfter::OneDay,
    ];

    fn duration(self) -> Option<Duration> {
        let minutes = match self {
            ClearAfter::Never => return None,
            ClearAfter::ThirtyMinutes => 30,
            ClearAfter::OneHour => 60,
            ClearAfter::FourHours => 4 * 60,
            ClearAfter::OneDay => 24 * 60,
        };
        Some(Duration::from_secs(minutes * 60))
    }

    fn label(self) -> String {
        match self {
            ClearAfter::Never => t!("presence-clear-never"),
            ClearAfter::ThirtyMinutes => t!("presence-clear-30m"),
            ClearAfter::OneHour => t!("presence-clear-1h"),
            ClearAfter::FourHours => t!("presence-clear-4h"),
            ClearAfter::OneDay => t!("presence-clear-24h"),
        }
    }
}

/// The picker while it is open.
#[derive(Default)]
pub struct Picker {
    editor: Option<CustomEditor>,
}

/// The custom status editor inside the picker.
struct CustomEditor {
    input: Entity<TextInput>,
    emoji: Option<String>,
    clear: ClearAfter,
    _submit: Subscription,
}

/// The custom status to save from the editor's parts, or `None` to clear it. `now_ms`
/// is the time the expiry counts from.
pub fn custom_from(
    text: &str,
    emoji: Option<&str>,
    clear: ClearAfter,
    now_ms: i64,
) -> Option<CustomStatus> {
    let text: String = text.trim().chars().take(CUSTOM_STATUS_TEXT_LIMIT).collect();
    if text.is_empty() && emoji.is_none() {
        return None;
    }
    let expires_at = clear.duration().and_then(|d| {
        let at = now_ms.saturating_add(d.as_millis() as i64);
        chrono::DateTime::from_timestamp_millis(at)
            .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
    });
    Some(CustomStatus {
        text: (!text.is_empty()).then_some(text),
        expires_at,
        emoji_id: None,
        emoji_name: emoji.map(str::to_owned),
        emoji_animated: false,
    })
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

impl Inlet {
    /// Whether the picker is open (it is a layer only over the main screen).
    pub(crate) fn status_picker_open(&self) -> bool {
        self.social.picker.is_some() && self.screen == Screen::Main
    }

    pub(crate) fn open_status_picker(&mut self, cx: &mut Context<Self>) {
        self.social.picker = Some(Picker::default());
        cx.notify();
    }

    fn close_status_picker(&mut self, cx: &mut Context<Self>) {
        self.social.picker = None;
        cx.notify();
    }

    /// Remembers the saved status before a change shown ahead of the server.
    fn remember_before_save(&mut self, cx: &mut Context<Self>) {
        if let Some(account) = &self.account {
            let own = account.read(cx).presences().own();
            self.social.presence.before_save =
                Some((own.status().to_owned(), own.custom().cloned()));
        }
    }

    /// A status was picked: shown at once, saved on the account, and sent as this
    /// session's presence.
    pub(crate) fn pick_status(&mut self, status: &str, cx: &mut Context<Self>) {
        self.remember_before_save(cx);
        if let Some(account) = self.account.clone() {
            account.update(cx, |s, _| s.set_own_status(Some(status), None));
        }
        self.backend.send(Cmd::SaveStatus {
            status: status.to_owned(),
        });
        self.sync_presence(cx);
        self.close_status_picker(cx);
    }

    /// Saves the custom status from the editor (an empty one clears it).
    fn save_custom_status(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.social.picker.as_ref().and_then(|p| p.editor.as_ref()) else {
            return;
        };
        let text = editor.input.read(cx).text().to_owned();
        let custom = custom_from(&text, editor.emoji.as_deref(), editor.clear, now_ms());
        self.set_custom_status(custom, cx);
    }

    fn set_custom_status(&mut self, custom: Option<CustomStatus>, cx: &mut Context<Self>) {
        self.remember_before_save(cx);
        if let Some(account) = self.account.clone() {
            let shown = custom.clone();
            account.update(cx, |s, _| s.set_own_status(None, Some(shown)));
        }
        self.backend.send(Cmd::SaveCustomStatus { custom });
        self.close_status_picker(cx);
    }

    /// A save failed: the saved status shows again.
    pub(crate) fn status_save_failed(&mut self, cx: &mut Context<Self>) {
        let (Some((status, custom)), Some(account)) = (
            self.social.presence.before_save.take(),
            self.account.clone(),
        ) else {
            return;
        };
        account.update(cx, |s, _| s.set_own_status(Some(&status), Some(custom)));
        self.sync_presence(cx);
        cx.notify();
    }

    fn open_custom_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let current = self
            .account
            .as_ref()
            .and_then(|a| a.read(cx).presences().own().custom_at(now_ms()).cloned());
        let text = current
            .as_ref()
            .and_then(|c| c.text.clone())
            .unwrap_or_default();
        let emoji = current
            .as_ref()
            .and_then(|c| c.emoji_name.clone())
            .filter(|_| current.as_ref().is_some_and(|c| c.emoji_id.is_none()));
        let input = cx.new(|cx| {
            TextInput::new(t!("presence-custom-placeholder"), cx)
                .max_chars(CUSTOM_STATUS_TEXT_LIMIT)
                .with_text(text)
        });
        let submit = cx.subscribe(&input, |this, _, _: &InputEvent, cx| {
            this.save_custom_status(cx)
        });
        input.focus_handle(cx).focus(window, cx);
        if let Some(picker) = self.social.picker.as_mut() {
            picker.editor = Some(CustomEditor {
                input,
                emoji,
                clear: ClearAfter::Never,
                _submit: submit,
            });
        }
        cx.notify();
    }

    /// Demo screenshots (CI): the status picker scene opens the picker once the
    /// account's status is in.
    #[cfg(feature = "demo")]
    pub(crate) fn demo_status_picker(&mut self, cx: &mut Context<Self>) {
        use crate::demo::scenes::{current, Scene};
        use std::sync::atomic::{AtomicBool, Ordering};
        static DONE: AtomicBool = AtomicBool::new(false);
        if std::env::var_os("INLET_DEMO").is_none()
            || current() != Scene::StatusPicker
            || self.account.is_none()
            || self.screen != Screen::Main
            || DONE.swap(true, Ordering::Relaxed)
        {
            return;
        }
        self.social.picker = Some(Picker::default());
        cx.notify();
    }

    /// The picker: a click-catcher over the window, with the card over the user panel.
    pub(crate) fn render_status_picker(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let editing = self
            .social
            .picker
            .as_ref()
            .is_some_and(|p| p.editor.is_some());
        let body = if editing {
            self.render_custom_editor(cx)
        } else {
            self.render_status_menu(cx)
        };
        let card = div()
            .id("status-picker")
            .occlude()
            .absolute()
            .left(px(8.))
            .bottom(px(76.))
            .w(px(WIDTH))
            .p(px(8.))
            .flex()
            .flex_col()
            .gap(px(2.))
            .rounded(px(8.))
            .bg(theme::panel_bg())
            .border_1()
            .border_color(theme::accent_border())
            .shadow_lg()
            .text_size(px(14.))
            .text_color(theme::text())
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(body);
        let frame = div()
            .id("status-picker-frame")
            // A click outside only closes the picker; it reaches nothing behind it.
            .occlude()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _, cx| this.close_status_picker(cx)),
            )
            .child(card);
        if self.is_top_layer(Layer::StatusPicker) {
            self.layer_keys(frame, cx).into_any_element()
        } else {
            frame.into_any_element()
        }
    }

    fn render_status_menu(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let now = now_ms();
        let custom = self
            .account
            .as_ref()
            .and_then(|a| a.read(cx).presences().own().custom_at(now).cloned());
        let chosen = self.own_status(cx);
        let custom_row = div()
            .id("status-custom")
            .h(px(36.))
            .px_2()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .rounded(px(6.))
            .cursor_pointer()
            .hover(|s| s.bg(theme::hover_bg()))
            .on_click(cx.listener(|this, _, window, cx| this.open_custom_editor(window, cx)))
            .child(match &custom {
                Some(c) => div()
                    .flex_1()
                    .min_w_0()
                    .child(custom_status_line(c, 14.))
                    .into_any_element(),
                None => div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .text_color(theme::text_muted())
                    .child(emoji("\u{1F642}", 18.))
                    .child(t!("presence-custom-set"))
                    .into_any_element(),
            })
            .when(custom.is_some(), |d| {
                d.child(
                    div()
                        .id("status-custom-clear")
                        .flex_none()
                        .px_1()
                        .rounded(px(4.))
                        .text_size(px(12.))
                        .text_color(theme::text_tertiary())
                        .hover(|s| s.text_color(theme::text_strong()))
                        .child(t!("presence-custom-clear"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            cx.stop_propagation();
                            this.set_custom_status(None, cx);
                        })),
                )
            });
        let rows = STATUSES.iter().map(|status| {
            let status: &'static str = status;
            let description = (status == "invisible").then(|| t!("presence-invisible-hint"));
            div()
                .id(SharedString::from(format!("status-{status}")))
                .min_h(px(36.))
                .py_1()
                .px_2()
                .flex()
                .flex_row()
                .items_center()
                .gap_3()
                .rounded(px(6.))
                .cursor_pointer()
                .when(status == chosen, |d| d.bg(theme::selected_bg()))
                .hover(|s| s.bg(theme::hover_bg()))
                .on_click(cx.listener(move |this, _, _, cx| this.pick_status(status, cx)))
                .child(status_dot(status, 12.))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .font_weight(FontWeight::MEDIUM)
                                .child(status_label(status)),
                        )
                        .children(description.map(|d| {
                            div()
                                .text_size(px(12.))
                                .text_color(theme::text_tertiary())
                                .child(d)
                        })),
                )
                .into_any_element()
        });
        let separator = || div().my_1().h(px(1.)).bg(theme::accent_border());
        div()
            .flex()
            .flex_col()
            .gap(px(2.))
            .child(custom_row)
            .child(separator())
            .children(rows)
            .child(separator())
            .child(
                div()
                    .id("status-switch-accounts")
                    .h(px(36.))
                    .px_2()
                    .flex()
                    .flex_row()
                    .items_center()
                    .rounded(px(6.))
                    .cursor_pointer()
                    .hover(|s| s.bg(theme::hover_bg()))
                    .child(t!("presence-switch-accounts"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.social.picker = None;
                        this.switcher.open = true;
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    fn render_custom_editor(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(editor) = self.social.picker.as_ref().and_then(|p| p.editor.as_ref()) else {
            return div().into_any_element();
        };
        let (input, picked, clear) = (editor.input.clone(), editor.emoji.clone(), editor.clear);
        let heading = |text: String| {
            div()
                .mt_1()
                .text_size(px(12.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme::text_muted())
                .child(text.to_uppercase())
        };
        let emoji_row = EMOJI.iter().enumerate().map(|(ix, glyph)| {
            let glyph: &'static str = glyph;
            let selected = picked.as_deref() == Some(glyph);
            div()
                .id(("status-emoji", ix))
                .size(px(28.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.))
                .cursor_pointer()
                .when(selected, |d| d.bg(theme::selected_bg()))
                .hover(|s| s.bg(theme::hover_bg()))
                .child(emoji(glyph, 20.))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(e) = this.social.picker.as_mut().and_then(|p| p.editor.as_mut()) {
                        e.emoji = (e.emoji.as_deref() != Some(glyph)).then(|| glyph.to_owned());
                    }
                    cx.notify();
                }))
                .into_any_element()
        });
        let clear_row = ClearAfter::ALL.iter().enumerate().map(|(ix, c)| {
            let c = *c;
            div()
                .id(("status-clear", ix))
                .px_2()
                .py(px(2.))
                .rounded(px(4.))
                .cursor_pointer()
                .text_size(px(12.))
                .when(c == clear, |d| {
                    d.bg(theme::selected_bg()).text_color(theme::text_strong())
                })
                .when(c != clear, |d| d.text_color(theme::text_tertiary()))
                .hover(|s| s.bg(theme::hover_bg()))
                .child(c.label())
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(e) = this.social.picker.as_mut().and_then(|p| p.editor.as_mut()) {
                        e.clear = c;
                    }
                    cx.notify();
                }))
                .into_any_element()
        });
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(heading(t!("presence-custom-title")))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap_1()
                    .children(emoji_row),
            )
            .child(input)
            .child(heading(t!("presence-clear-after")))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap_1()
                    .children(clear_row),
            )
            .child(
                div()
                    .mt_1()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap_2()
                    .child(
                        button("status-custom-cancel", t!("presence-cancel"), false).on_click(
                            cx.listener(|this, _, _, cx| {
                                if let Some(p) = this.social.picker.as_mut() {
                                    p.editor = None;
                                }
                                cx.notify();
                            }),
                        ),
                    )
                    .child(
                        button("status-custom-save", t!("presence-save"), true)
                            .on_click(cx.listener(|this, _, _, cx| this.save_custom_status(cx))),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_editor_builds_the_custom_status_fluxer_takes() {
        let now = 1_767_225_600_000; // 2026-01-01T00:00:00Z
        assert_eq!(custom_from("   ", None, ClearAfter::OneHour, now), None);
        let c = custom_from("  Lunch ", Some("\u{1F355}"), ClearAfter::OneHour, now).unwrap();
        assert_eq!(c.text.as_deref(), Some("Lunch"));
        assert_eq!(c.emoji_name.as_deref(), Some("\u{1F355}"));
        assert_eq!(c.expires_at.as_deref(), Some("2026-01-01T01:00:00.000Z"));
        let only_emoji = custom_from("", Some("\u{1F3A7}"), ClearAfter::Never, now).unwrap();
        assert_eq!((only_emoji.text, only_emoji.expires_at), (None, None));
        let long = custom_from(&"y".repeat(500), None, ClearAfter::Never, now).unwrap();
        assert_eq!(long.text.unwrap().chars().count(), CUSTOM_STATUS_TEXT_LIMIT);
    }
}
