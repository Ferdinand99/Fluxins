// SPDX-License-Identifier: AGPL-3.0-or-later
//! The 24 px row above the box. Left: who is typing (from the typing indicator), or a
//! message on its way or refused, with Retry and Discard. Right: the slowmode countdown
//! and how many characters are left near the limit. A short error line (too long, a
//! command that needs a guild) takes the left side while it stands.

use gpui::{div, prelude::*, px, AnyElement, Context, Window};

use super::editor::ComposerView;
use super::slowmode::clock;
use crate::theme;

/// Height of the row.
pub const STATUS_ROW_HEIGHT: f32 = 24.;
/// The character count shows when fewer than this many are left.
const COUNT_FROM: usize = 200;

impl ComposerView {
    /// Who is typing in the open channel. Empty until the typing indicator fills it.
    fn typing_indicator(&self, _cx: &mut Context<Self>) -> Option<AnyElement> {
        None
    }

    pub(super) fn render_status_row(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let channel = self.channel.as_ref().map(|c| c.channel_id.clone());
        let failed = channel.as_ref().and_then(|ch| {
            self.pending
                .iter()
                .find(|p| &p.outgoing.channel_id == ch && p.failed.is_some())
        });
        let sending = channel.as_ref().is_some_and(|ch| {
            self.pending
                .iter()
                .any(|p| &p.outgoing.channel_id == ch && p.failed.is_none())
        });
        let left: Option<AnyElement> = if let Some(error) = &self.error {
            Some(
                div()
                    .text_color(theme::danger())
                    .child(error.clone())
                    .into_any_element(),
            )
        } else if let Some(p) = failed {
            let (retry, discard) = (p.outgoing.nonce.clone(), p.outgoing.nonce.clone());
            let reason = p.failed.clone().unwrap_or_default();
            Some(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .overflow_hidden()
                    .child(
                        div()
                            .text_color(theme::danger())
                            .overflow_hidden()
                            .child(t!("error-send", error = reason)),
                    )
                    .child(
                        div()
                            .id("send-retry")
                            .cursor_pointer()
                            .text_color(theme::link())
                            .child(t!("composer-retry"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.retry(&retry, window, cx)
                            })),
                    )
                    .child(
                        div()
                            .id("send-discard")
                            .cursor_pointer()
                            .text_color(theme::link())
                            .child(t!("composer-discard"))
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.discard(&discard, cx)),
                            ),
                    )
                    .into_any_element(),
            )
        } else if let Some(typing) = self.typing_indicator(cx) {
            Some(typing)
        } else if sending {
            Some(div().child(t!("composer-sending")).into_any_element())
        } else {
            None
        };

        let slowmode_left = self.slowmode_left();
        let slowmode = self
            .channel
            .as_ref()
            .filter(|c| c.guild_id.is_some() && c.slowmode_secs > 0);
        let right_slowmode = slowmode.map(|_| {
            if self.bypasses_slowmode() {
                t!("composer-slowmode-immune")
            } else if slowmode_left > 0 {
                t!("composer-slowmode-wait", time = clock(slowmode_left))
            } else {
                t!("composer-slowmode-on")
            }
        });
        let used = self.editor.read(cx).buffer().len_utf16();
        let count = (used + COUNT_FROM > self.max_len).then(|| {
            let left = self.max_len as i64 - used as i64;
            div()
                .text_color(if left < 0 {
                    theme::danger()
                } else {
                    theme::text_tertiary()
                })
                .child(left.to_string())
        });

        div()
            .h(px(STATUS_ROW_HEIGHT))
            .mx(px(6.))
            .px_2()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_2()
            .text_size(px(13.))
            .text_color(theme::text_tertiary())
            .overflow_hidden()
            .child(div().flex_1().overflow_hidden().children(left))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_3()
                    .children(right_slowmode)
                    .children(count),
            )
    }
}
