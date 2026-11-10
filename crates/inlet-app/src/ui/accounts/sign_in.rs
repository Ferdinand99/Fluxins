// SPDX-License-Identifier: AGPL-3.0-or-later
//! The sign-in form: browser handoff or a pasted token.

use gpui::{div, prelude::*, px, ClickEvent, Context, FontWeight, Window};

use super::{hash_text, safe_handoff_url, SignInMode};
use crate::app::{Inlet, Screen};
use crate::backend::accounts::Cmd;
use crate::theme;
use crate::ui::markdown;
use crate::ui::widgets::{button, field};

impl Inlet {
    pub(crate) fn submit_sign_in(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.sign_in.busy || self.screen != Screen::SignIn || self.sign_in.handoff.is_some() {
            return;
        }
        let instance = self
            .sign_in
            .instance_input
            .read(cx)
            .text()
            .trim()
            .to_string();
        if instance.is_empty() {
            self.sign_in.error = Some(t!("sign-in-enter-instance"));
            cx.notify();
            return;
        }
        let command = match self.sign_in.mode {
            SignInMode::Browser => Cmd::SignInBrowser { instance },
            SignInMode::Token => {
                let token = self.sign_in.token_input.read(cx).text().trim().to_string();
                if token.is_empty() {
                    self.sign_in.error = Some(t!("sign-in-paste-first"));
                    cx.notify();
                    return;
                }
                self.sign_in.pasted_token_hash = Some(hash_text(&token));
                Cmd::SignInToken {
                    instance,
                    token: inlet_protocol::Token::new(token),
                }
            }
        };
        self.sign_in.busy = true;
        self.sign_in.error = None;
        self.sign_in.notice = None;
        self.backend.send(command);
        cx.notify();
    }

    fn cancel_wait(&mut self, _: &ClickEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.backend.send(Cmd::CancelSignIn);
        self.sign_in.handoff = None;
        self.sign_in.busy = false;
        cx.notify();
    }

    pub(crate) fn render_sign_in(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let mode = self.sign_in.mode;
        let tab = |id: &'static str, label: String, active: bool| {
            div()
                .id(id)
                .px_3()
                .py_1()
                .rounded_md()
                .cursor_pointer()
                .text_color(if active {
                    theme::text_strong()
                } else {
                    theme::text_muted()
                })
                .when(active, |d| d.bg(theme::selected_bg()))
                .hover(|s| s.bg(theme::hover_bg()))
                .child(label)
        };

        let mut form = div().flex().flex_col().gap_3();
        if let Some((url, code)) = self.sign_in.handoff.clone() {
            form = form
                .child(div().child(t!("sign-in-handoff-instructions")))
                .child(
                    div()
                        .text_size(px(28.))
                        .font_weight(FontWeight::BOLD)
                        .text_color(theme::text_strong())
                        .font_family(markdown::mono_font())
                        .child(code),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme::text_muted())
                        .child(url.clone()),
                )
                .child(
                    div()
                        .text_color(theme::text_muted())
                        .child(t!("sign-in-waiting")),
                )
                .child(
                    button("reopen", t!("sign-in-reopen"), false).on_click(move |_, _, cx| {
                        if safe_handoff_url(&url) {
                            cx.open_url(&url)
                        }
                    }),
                )
                .child(
                    button("cancel-wait", t!("common-cancel"), false)
                        .on_click(cx.listener(Self::cancel_wait)),
                );
        } else {
            form = form
                .child(field(
                    t!("sign-in-instance-label"),
                    self.sign_in.instance_input.clone(),
                ))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_2()
                        .child(
                            tab(
                                "mode-browser",
                                t!("sign-in-mode-browser"),
                                mode == SignInMode::Browser,
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.sign_in.mode = SignInMode::Browser;
                                cx.notify();
                            })),
                        )
                        .child(
                            tab(
                                "mode-token",
                                t!("sign-in-mode-token"),
                                mode == SignInMode::Token,
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.sign_in.mode = SignInMode::Token;
                                cx.notify();
                            })),
                        ),
                );
            form = match mode {
                SignInMode::Browser => form.child(
                    div()
                        .text_size(px(13.))
                        .text_color(theme::text_muted())
                        .child(t!("sign-in-browser-hint")),
                ),
                SignInMode::Token => form
                    .child(field(
                        t!("sign-in-token-label"),
                        self.sign_in.token_input.clone(),
                    ))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme::text_muted())
                            .child(t!("sign-in-token-hint")),
                    ),
            };
            let label = match (self.sign_in.busy, mode) {
                (true, _) => t!("signing-in"),
                (false, SignInMode::Browser) => t!("sign-in-open-page"),
                (false, SignInMode::Token) => t!("sign-in-submit"),
            };
            form = form.child(
                button("sign-in", label, true)
                    .on_click(cx.listener(|this, _, window, cx| this.submit_sign_in(window, cx))),
            );
        }
        if let Some(err) = &self.sign_in.error {
            form = form.child(div().text_color(theme::danger()).child(err.clone()));
        }
        if let Some(notice) = &self.sign_in.notice {
            form = form.child(div().text_color(theme::text_muted()).child(notice.clone()));
        }
        if self.sign_in_has_way_back() && self.sign_in.handoff.is_none() && !self.sign_in.busy {
            form = form.child(
                button("sign-in-back", t!("sign-in-back"), false)
                    .on_click(cx.listener(|this, _, _, cx| this.back_from_sign_in(cx))),
            );
        }
        form = form.child(Self::about_link(t!("about-link"), cx));
        self.render_card(form)
    }
}
