// SPDX-License-Identifier: AGPL-3.0-or-later
//! Accounts in the UI: the sign-in form and the session screens, and what the accounts
//! domain of the backend reports (sign-in progress, session state, connection status).

mod screens;
mod sign_in;

use std::hash::{DefaultHasher, Hash, Hasher};

use gpui::{
    div, prelude::*, px, ClickEvent, ClipboardItem, Context, Entity, Focusable, FontWeight,
    Subscription, Window,
};

use crate::app::{Inlet, Screen};
use crate::backend::accounts::{Cmd, Ev, DEFAULT_INSTANCE};
use crate::text_input::{InputEvent, TextInput};
use crate::theme;

#[derive(Clone, Copy, PartialEq)]
pub enum SignInMode {
    Browser,
    Token,
}

/// The sign-in form.
pub struct SignInState {
    pub instance_input: Entity<TextInput>,
    pub token_input: Entity<TextInput>,
    pub mode: SignInMode,
    pub busy: bool,
    pub error: Option<String>,
    pub notice: Option<String>,
    /// Browser sign-in in progress: page URL and the code to enter there.
    pub handoff: Option<(String, String)>,
    /// Hash of a pasted token, to clear the clipboard after sign-in if it still holds it.
    pub pasted_token_hash: Option<u64>,
}

impl SignInState {
    pub fn new(
        window: &mut Window,
        cx: &mut Context<Inlet>,
        subscriptions: &mut Vec<Subscription>,
    ) -> Self {
        let instance_input = cx.new(|cx| {
            TextInput::new(t!("sign-in-instance-placeholder"), cx)
                .with_text(DEFAULT_INSTANCE)
                .max_chars(253)
        });
        let token_input = cx.new(|cx| {
            TextInput::new(t!("sign-in-token-placeholder"), cx)
                .masked()
                .max_chars(512)
        });
        for input in [&instance_input, &token_input] {
            subscriptions.push(cx.subscribe_in(
                input,
                window,
                |this, _, event: &InputEvent, window, cx| match event {
                    InputEvent::Submit => this.submit_sign_in(window, cx),
                },
            ));
        }
        Self {
            instance_input,
            token_input,
            mode: SignInMode::Browser,
            busy: false,
            error: None,
            notice: None,
            handoff: None,
            pasted_token_hash: None,
        }
    }
}

pub(crate) fn hash_text(text: &str) -> u64 {
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

/// The handoff page may only be opened over https (http only for a loopback test instance).
pub(crate) fn safe_handoff_url(url: &str) -> bool {
    let Ok(u) = url::Url::parse(url) else {
        return false;
    };
    match u.scheme() {
        "https" => true,
        "http" => matches!(u.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")),
        _ => false,
    }
}

impl Inlet {
    pub(crate) fn on_accounts_event(
        &mut self,
        event: Ev,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            Ev::Starting { instance } => {
                self.screen = Screen::Starting {
                    instance: Some(instance),
                };
            }
            Ev::Offline {
                instance,
                message,
                retry_in_secs,
            } => {
                self.screen = Screen::Offline {
                    instance,
                    message,
                    retry_in_secs,
                };
            }
            Ev::NeedsSignIn { instance } => {
                if let Some(instance) = instance {
                    self.sign_in
                        .instance_input
                        .update(cx, |i, cx| i.set_text(instance, cx));
                }
                self.screen = Screen::SignIn;
                self.sign_in.busy = false;
                self.sign_in
                    .instance_input
                    .focus_handle(cx)
                    .focus(window, cx);
            }
            Ev::HandoffStarted { url, code } => {
                self.sign_in.busy = false;
                self.sign_in.error = None;
                if safe_handoff_url(&url) {
                    cx.open_url(&url);
                    self.sign_in.handoff = Some((url, code));
                } else {
                    self.backend.send(Cmd::CancelSignIn);
                    self.sign_in.error = Some(t!("sign-in-handoff-not-https"));
                }
            }
            Ev::SignInFailed(message) => {
                self.sign_in.busy = false;
                self.sign_in.handoff = None;
                self.sign_in.pasted_token_hash = None;
                self.sign_in.error = Some(message);
                if matches!(self.screen, Screen::Starting { .. }) {
                    self.screen = Screen::SignIn;
                }
            }
            Ev::Notice(message) => self.sign_in.notice = Some(message),
            Ev::ConfirmAccount {
                display_name,
                account,
                instance,
                pasted,
            } => {
                self.sign_in.busy = false;
                self.sign_in.handoff = None;
                self.screen = Screen::Confirm {
                    display_name,
                    account,
                    instance,
                    pasted,
                };
            }
            Ev::GatewayDown(message) => self.chat.gateway_down = Some(message),
            Ev::SignedIn { username, instance } => {
                self.sign_in.busy = false;
                self.sign_in.handoff = None;
                self.sign_in.error = None;
                self.sign_in.notice = None;
                self.sign_in.token_input.update(cx, |i, cx| i.clear(cx));
                if let Some(hash) = self.sign_in.pasted_token_hash.take() {
                    let still_there = cx
                        .read_from_clipboard()
                        .and_then(|item| item.text())
                        .is_some_and(|text| hash_text(text.trim()) == hash);
                    if still_there {
                        cx.write_to_clipboard(ClipboardItem::new_string(String::new()));
                    }
                }
                self.username = username;
                self.instance = instance;
                self.screen = Screen::Main;
                self.select_guild(None, cx);
                // Demo screenshots (CI): open a guild and channel straight away.
                #[cfg(feature = "demo")]
                if let Ok(guild) = std::env::var("INLET_DEMO_GUILD") {
                    self.select_guild(Some(crate::demo::snowflake(&guild)), cx);
                    self.demo_channel = std::env::var("INLET_DEMO_CHANNEL")
                        .ok()
                        .map(|c| crate::demo::snowflake(&c));
                }
            }
            Ev::SessionEnded(message) => {
                // The account is gone: its drafts go too.
                self.composer.sign_out();
                self.reset_main();
                self.sign_in.error = Some(message);
            }
            Ev::SignedOut => {
                self.composer.sign_out();
                self.reset_main();
            }
            // The messages domain loads the open channel again on every READY.
            Ev::Reconnected => {}
            Ev::Status(status) => {
                if status == t!("status-connected") {
                    self.chat.gateway_down = None;
                }
                self.chat.status = status;
            }
            Ev::Error(message) => self.chat.error = Some(message),
        }
    }

    /// From the fatal screen: start a fresh backend that ends the stored session on the
    /// server (best effort, with timeouts) and removes it, all off the UI thread.
    pub(crate) fn fatal_sign_out(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.composer.sign_out();
        self.restart_backend(false, window, cx);
        self.backend.send(Cmd::ForgetStored);
    }

    pub(crate) fn sign_out(
        &mut self,
        _: &ClickEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.backend.send(Cmd::SignOut);
        cx.notify();
    }

    // ---------- views ----------

    pub(crate) fn render_card(&self, body: impl IntoElement) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(theme::bg_app())
            .child(
                div()
                    .w(px(480.))
                    .p_8()
                    .rounded_lg()
                    .bg(theme::bg_chat())
                    .flex()
                    .flex_col()
                    .gap_4()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .text_size(px(24.))
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(theme::text_strong())
                                    .child(t!("app-name")),
                            )
                            .child(
                                div()
                                    .text_color(theme::text_muted())
                                    .child(t!("app-tagline")),
                            ),
                    )
                    .child(body),
            )
    }

    pub(crate) fn about_link(label: String, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("about-link")
            .text_size(px(12.))
            .text_color(theme::text_muted())
            .cursor_pointer()
            .hover(|s| s.text_color(theme::text()))
            .child(label)
            .on_click(cx.listener(|this, _, _, cx| {
                this.show_about = true;
                cx.notify();
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handoff_page_must_be_https() {
        assert!(safe_handoff_url("https://web.fluxer.app/login?handoff=1"));
        assert!(!safe_handoff_url("http://fluxer.example/login"));
        assert!(safe_handoff_url("http://localhost:8080/login"));
        assert!(!safe_handoff_url("fluxer://handoff"));
        assert!(!safe_handoff_url("not a url"));
    }
}
