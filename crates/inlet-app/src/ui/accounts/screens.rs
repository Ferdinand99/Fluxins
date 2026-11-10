// SPDX-License-Identifier: AGPL-3.0-or-later
//! The session screens around sign-in: starting, offline, confirm, finishing, fatal.

use gpui::{div, prelude::*, px, Context, FontWeight};

use crate::app::{Inlet, Screen};
use crate::backend::accounts::Cmd;
use crate::model::Guild;
use crate::theme;
use crate::ui::widgets::button;

impl Inlet {
    pub(crate) fn render_starting(
        &self,
        instance: Option<String>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let text = match instance {
            Some(i) => t!("starting-connecting-to", instance = i),
            None => t!("starting"),
        };
        self.render_card(
            div()
                .flex()
                .flex_col()
                .gap_3()
                .child(div().child(text))
                .child(
                    button("cancel-start", t!("common-cancel"), false).on_click(cx.listener(
                        |this, _, _, cx| {
                            this.backend.send(Cmd::CancelStartup);
                            cx.notify();
                        },
                    )),
                ),
        )
    }

    pub(crate) fn render_offline(
        &self,
        instance: &str,
        message: &str,
        retry_in: u64,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        self.render_card(
            div()
                .flex()
                .flex_col()
                .gap_3()
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(t!("offline-title", instance = instance)),
                )
                .child(
                    div()
                        .text_color(theme::text_muted())
                        .child(message.to_string()),
                )
                .child(
                    div()
                        .text_color(theme::text_muted())
                        .child(t!("offline-retry-in", seconds = retry_in)),
                )
                .child(
                    button("retry", t!("offline-retry-now"), true).on_click(cx.listener(
                        |this, _, _, cx| {
                            this.backend.send(Cmd::RetryNow);
                            cx.notify();
                        },
                    )),
                )
                .child(
                    button("offline-sign-out", t!("common-sign-out"), false)
                        .on_click(cx.listener(Self::sign_out)),
                )
                .child(Self::about_link(t!("about-link"), cx)),
        )
    }

    pub(crate) fn render_confirm(
        &self,
        display_name: &str,
        account: &str,
        instance: &str,
        pasted: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let initials = Guild {
            id: String::new(),
            name: display_name.to_string(),
        }
        .initials();
        self.render_card(
            div()
                .flex()
                .flex_col()
                .gap_3()
                .child(div().child(t!("confirm-signed-in-as")))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_3()
                        .child(
                            div()
                                .size(px(48.))
                                .flex_none()
                                .rounded(px(24.))
                                .bg(theme::brand())
                                .flex()
                                .items_center()
                                .justify_center()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme::text_strong())
                                .child(initials),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .child(
                                    div()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(theme::text_strong())
                                        .child(display_name.to_string()),
                                )
                                .child(
                                    div()
                                        .text_color(theme::text_muted())
                                        .child(format!("@{account}")),
                                )
                                .child(
                                    div()
                                        .text_size(px(12.))
                                        .text_color(theme::text_muted())
                                        .child(instance.to_string()),
                                ),
                        ),
                )
                .child(
                    div()
                        .text_color(theme::text_muted())
                        .child(t!("confirm-stored-after-continue")),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme::text_muted())
                        .child(if pasted {
                            t!("confirm-cancel-pasted")
                        } else {
                            t!("confirm-cancel-issued")
                        }),
                )
                .child(
                    button("confirm-continue", t!("confirm-continue"), true).on_click(cx.listener(
                        |this, _, _, cx| {
                            this.backend.send(Cmd::ConfirmAccount);
                            this.screen = Screen::Finishing;
                            cx.notify();
                        },
                    )),
                )
                .child(
                    button("confirm-cancel", t!("common-cancel"), false).on_click(cx.listener(
                        |this, _, _, cx| {
                            this.sign_in.pasted_token_hash = None;
                            this.backend.send(Cmd::RejectAccount);
                            cx.notify();
                        },
                    )),
                ),
        )
    }

    pub(crate) fn render_fatal(&self, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_card(
            div()
                .flex()
                .flex_col()
                .gap_3()
                .child(div().text_color(theme::danger()).child(t!("fatal-title")))
                .child(
                    div()
                        .text_color(theme::text_muted())
                        .child(t!("fatal-hint")),
                )
                .child(
                    button("restart-backend", t!("fatal-restart"), true).on_click(
                        cx.listener(|this, _, window, cx| this.restart_backend(true, window, cx)),
                    ),
                )
                .child(
                    button("fatal-sign-out", t!("common-sign-out"), false).on_click(
                        cx.listener(|this, _, window, cx| this.fatal_sign_out(window, cx)),
                    ),
                ),
        )
    }

    pub(crate) fn render_finishing(&self) -> impl IntoElement {
        self.render_card(div().child(t!("signing-in")))
    }
}
