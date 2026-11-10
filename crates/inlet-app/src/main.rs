// SPDX-License-Identifier: AGPL-3.0-or-later
//! Inlet: a native desktop client that works with Fluxer.

mod about;
mod assets;
mod backend;
mod markdown;
#[cfg(feature = "demo")]
mod mock;
mod model;
mod real;
mod reconnect;
mod text_input;
mod theme;
mod token_store;

use std::hash::{DefaultHasher, Hash, Hasher};
use std::rc::Rc;

use futures::StreamExt;
use gpui::{
    div, list, prelude::*, px, rgba, size, AnyElement, App, Bounds, ClickEvent, ClipboardItem,
    Context, Entity, FocusHandle, Focusable, FollowMode, FontWeight, ListAlignment, ListState,
    SharedString, Subscription, TitlebarOptions, Window, WindowBounds, WindowOptions,
};

use assets::Icon;
use backend::{BackendHandle, Command, Event, DEFAULT_INSTANCE};
use markdown::RenderCtx;
use model::{Channel, ChannelKind, Guild, Message};
use text_input::{FocusNext, FocusPrev, InputEvent, TextInput};

#[derive(Clone, PartialEq)]
enum Screen {
    /// Connecting with a saved session (or before the backend has answered).
    Starting {
        instance: Option<String>,
    },
    /// "Continue" was pressed after sign-in; no cancel from here.
    Finishing,
    /// The saved session could not connect; the backend retries.
    Offline {
        instance: String,
        message: String,
        retry_in_secs: u64,
    },
    SignIn,
    /// A new sign-in worked; the person confirms the account before it is stored.
    Confirm {
        display_name: String,
        account: String,
        instance: String,
        pasted: bool,
    },
    Main,
    /// The backend thread stopped.
    Fatal,
}

#[derive(Clone, Copy, PartialEq)]
enum SignInMode {
    Browser,
    Token,
}

struct ShownMessage {
    message: Message,
    /// Same account as the message above, shortly after: hide the header.
    grouped: bool,
    /// Spoilers are shown.
    revealed: bool,
}

struct Inlet {
    backend: BackendHandle,
    /// Bumped on every backend (re)start, so events from a dead backend are ignored.
    backend_epoch: u64,
    focus: FocusHandle,
    screen: Screen,
    show_about: bool,
    show_licences: bool,
    /// A masked link waiting for confirmation.
    pending_link: Option<String>,

    // Sign-in
    instance_input: Entity<TextInput>,
    token_input: Entity<TextInput>,
    sign_in_mode: SignInMode,
    busy: bool,
    sign_in_error: Option<String>,
    sign_in_notice: Option<String>,
    /// Browser sign-in in progress: page URL and the code to enter there.
    handoff: Option<(String, String)>,
    /// Hash of a pasted token, to clear the clipboard after sign-in if it still holds it.
    pasted_token_hash: Option<u64>,

    // Main
    username: String,
    instance: String,
    guilds: Vec<Guild>,
    /// `None` shows direct messages.
    selected_guild: Option<String>,
    channels: Vec<Channel>,
    selected_channel: Option<String>,
    messages: Vec<ShownMessage>,
    message_list: ListState,
    composer: Entity<TextInput>,
    status: String,
    error: Option<String>,
    notice: Option<String>,
    /// The gateway stopped and is not retried automatically.
    gateway_down: Option<String>,
    /// Categories the person folded away in the channel list.
    collapsed: std::collections::HashSet<String>,
    /// A channel mention was clicked; opened on the next render (needs the window).
    pending_channel_jump: Option<String>,
    /// Demo screenshots: the channel to open once the guild's channels arrive.
    #[cfg(feature = "demo")]
    demo_channel: Option<String>,

    _subscriptions: Vec<Subscription>,
}

fn hash_text(text: &str) -> u64 {
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

/// The handoff page may only be opened over https (http only for a loopback test instance).
fn safe_handoff_url(url: &str) -> bool {
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
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let instance_input = cx.new(|cx| {
            TextInput::new("fluxer.opland.net", cx)
                .with_text(DEFAULT_INSTANCE)
                .max_chars(253)
        });
        let token_input = cx.new(|cx| {
            TextInput::new("Paste your token", cx)
                .masked()
                .max_chars(512)
        });
        let composer = cx.new(|cx| TextInput::new("Message", cx).composer().max_chars(4_000));

        let mut subscriptions = Vec::new();
        for input in [&instance_input, &token_input] {
            subscriptions.push(cx.subscribe_in(
                input,
                window,
                |this, _, event: &InputEvent, window, cx| match event {
                    InputEvent::Submit => this.submit_sign_in(window, cx),
                },
            ));
        }
        subscriptions.push(cx.subscribe_in(
            &composer,
            window,
            |this, _, event: &InputEvent, _window, cx| match event {
                InputEvent::Submit => this.send_message(cx),
            },
        ));

        let message_list = ListState::new(0, ListAlignment::Bottom, px(800.));
        message_list.set_follow_mode(FollowMode::Tail);

        let (backend, _) = Self::spawn_backend(0, true, window, cx);
        Self {
            backend,
            backend_epoch: 0,
            focus: cx.focus_handle(),
            screen: Screen::Starting { instance: None },
            show_about: false,
            show_licences: false,
            pending_link: None,
            instance_input,
            token_input,
            sign_in_mode: SignInMode::Browser,
            busy: false,
            sign_in_error: None,
            sign_in_notice: None,
            handoff: None,
            pasted_token_hash: None,
            username: String::new(),
            instance: String::new(),
            guilds: Vec::new(),
            selected_guild: None,
            channels: Vec::new(),
            selected_channel: None,
            messages: Vec::new(),
            message_list,
            composer,
            status: String::new(),
            error: None,
            notice: None,
            gateway_down: None,
            collapsed: std::collections::HashSet::new(),
            pending_channel_jump: None,
            #[cfg(feature = "demo")]
            demo_channel: None,
            _subscriptions: subscriptions,
        }
    }

    /// Starts a backend thread and the task that feeds its events into this view.
    fn spawn_backend(
        epoch: u64,
        restore: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (BackendHandle, u64) {
        let (events_tx, mut events_rx) = futures::channel::mpsc::unbounded();
        let backend = backend::start(events_tx);
        cx.set_global(CurrentBackend(backend.clone()));
        cx.spawn_in(window, async move |this, cx| {
            while let Some(event) = events_rx.next().await {
                let alive = this.update_in(cx, |this, window, cx| {
                    if this.backend_epoch == epoch {
                        this.handle_event(event, window, cx);
                    }
                });
                if alive.is_err() {
                    return;
                }
            }
            // The backend dropped its sender: the thread has ended.
            let _ = this.update(cx, |this, cx| {
                if this.backend_epoch == epoch {
                    log::error!("the backend thread stopped");
                    this.screen = Screen::Fatal;
                    cx.notify();
                }
            });
        })
        .detach();
        if restore {
            backend.send(Command::Restore);
        }
        (backend, epoch)
    }

    fn restart_backend(&mut self, restore: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.backend_epoch += 1;
        let (backend, _) = Self::spawn_backend(self.backend_epoch, restore, window, cx);
        self.backend = backend;
        self.reset_main();
        self.screen = Screen::Starting { instance: None };
        cx.notify();
    }

    /// From the fatal screen: start a fresh backend that ends the stored session on the
    /// server (best effort, with timeouts) and removes it, all off the UI thread.
    fn fatal_sign_out(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.restart_backend(false, window, cx);
        self.backend.send(Command::ForgetStored);
    }

    /// Merges a REST page into the view, keeping live messages that arrived meanwhile and
    /// what the person already revealed.
    fn merge_messages(&mut self, loaded: Vec<Message>) {
        let mut by_id: std::collections::HashMap<String, ShownMessage> = self
            .messages
            .drain(..)
            .map(|m| (m.message.id.clone(), m))
            .collect();
        let newest_loaded = loaded
            .iter()
            .map(|m| real::snowflake_key(&m.id))
            .max()
            .map(|(l, s)| (l, s.to_string()));
        let mut merged: Vec<ShownMessage> = loaded
            .into_iter()
            .map(|message| {
                let revealed = by_id.remove(&message.id).is_some_and(|old| old.revealed);
                ShownMessage {
                    message,
                    grouped: false,
                    revealed,
                }
            })
            .collect();
        // Live messages newer than the page stay; older leftovers are what the page replaced.
        for (_, old) in by_id {
            let key = real::snowflake_key(&old.message.id);
            let newer = newest_loaded
                .as_ref()
                .is_none_or(|(l, s)| (key.0, key.1) > (*l, s.as_str()));
            if newer {
                merged.push(old);
            }
        }
        merged.sort_by(|a, b| {
            real::snowflake_key(&a.message.id).cmp(&real::snowflake_key(&b.message.id))
        });
        self.messages = merged;
        self.regroup();
        self.message_list.reset(self.messages.len());
        self.message_list.set_follow_mode(FollowMode::Tail);
    }

    fn handle_event(&mut self, event: Event, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            Event::Starting { instance } => {
                self.screen = Screen::Starting {
                    instance: Some(instance),
                };
            }
            Event::Offline {
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
            Event::NeedsSignIn { instance } => {
                if let Some(instance) = instance {
                    self.instance_input
                        .update(cx, |i, cx| i.set_text(instance, cx));
                }
                self.screen = Screen::SignIn;
                self.busy = false;
                self.instance_input.focus_handle(cx).focus(window, cx);
            }
            Event::HandoffStarted { url, code } => {
                self.busy = false;
                self.sign_in_error = None;
                if safe_handoff_url(&url) {
                    cx.open_url(&url);
                    self.handoff = Some((url, code));
                } else {
                    self.backend.send(Command::CancelSignIn);
                    self.sign_in_error =
                        Some("The instance gave a sign-in page that is not https. Inlet did not open it.".into());
                }
            }
            Event::SignInFailed(message) => {
                self.busy = false;
                self.handoff = None;
                self.pasted_token_hash = None;
                self.sign_in_error = Some(message);
                if matches!(self.screen, Screen::Starting { .. }) {
                    self.screen = Screen::SignIn;
                }
            }
            Event::Notice(message) => self.sign_in_notice = Some(message),
            Event::ConfirmAccount {
                display_name,
                account,
                instance,
                pasted,
            } => {
                self.busy = false;
                self.handoff = None;
                self.screen = Screen::Confirm {
                    display_name,
                    account,
                    instance,
                    pasted,
                };
            }
            Event::GatewayDown(message) => self.gateway_down = Some(message),
            Event::SignedIn { username, instance } => {
                self.busy = false;
                self.handoff = None;
                self.sign_in_error = None;
                self.sign_in_notice = None;
                self.token_input.update(cx, |i, cx| i.clear(cx));
                if let Some(hash) = self.pasted_token_hash.take() {
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
                    self.select_guild(Some(guild), cx);
                    self.demo_channel = std::env::var("INLET_DEMO_CHANNEL").ok();
                }
            }
            Event::SessionEnded(message) => {
                self.reset_main();
                self.sign_in_error = Some(message);
            }
            Event::SignedOut => self.reset_main(),
            Event::Guilds(guilds) => {
                self.guilds = guilds;
                if let Some(selected) = &self.selected_guild {
                    if !self.guilds.iter().any(|g| &g.id == selected) {
                        self.notice = Some("You are no longer in that server.".into());
                        self.select_guild(None, cx);
                    }
                }
            }
            Event::Channels { guild_id, channels } => {
                if guild_id == self.selected_guild {
                    self.channels = model::sidebar_order(&channels);
                    let open_still_exists = self
                        .selected_channel
                        .as_ref()
                        .is_some_and(|id| self.channels.iter().any(|c| &c.id == id));
                    if self.selected_channel.is_some() && !open_still_exists {
                        self.notice = Some("The channel you had open was deleted.".into());
                        self.selected_channel = None;
                    }
                    #[cfg(feature = "demo")]
                    if let Some(wanted) = self.demo_channel.take() {
                        if self.channels.iter().any(|c| c.id == wanted) {
                            self.select_channel(wanted, window, cx);
                        }
                    }
                    if self.selected_channel.is_none() {
                        match self.channels.iter().find(|c| c.is_messageable()).cloned() {
                            Some(first) => self.select_channel(first.id, window, cx),
                            None => self.set_messages(Vec::new()),
                        }
                    }
                }
            }
            Event::Messages {
                channel_id,
                messages,
            } => {
                if Some(&channel_id) == self.selected_channel.as_ref() {
                    self.merge_messages(messages);
                }
            }
            Event::MessageCreated(message) => {
                if Some(&message.channel_id) == self.selected_channel.as_ref()
                    && !self.messages.iter().any(|m| m.message.id == message.id)
                {
                    let grouped = self
                        .messages
                        .last()
                        .is_some_and(|m| model::should_group(&m.message, &message));
                    let at = self.messages.len();
                    self.messages.push(ShownMessage {
                        message,
                        grouped,
                        revealed: false,
                    });
                    self.message_list.splice(at..at, 1);
                }
            }
            Event::MessageUpdated(message) => {
                if let Some(ix) = self
                    .messages
                    .iter()
                    .position(|m| m.message.id == message.id)
                {
                    self.messages[ix].message = message;
                    self.message_list.splice(ix..ix + 1, 1);
                }
            }
            Event::MessageDeleted {
                channel_id,
                message_id,
            } => {
                if Some(&channel_id) == self.selected_channel.as_ref() {
                    if let Some(ix) = self
                        .messages
                        .iter()
                        .position(|m| m.message.id == message_id)
                    {
                        self.messages.remove(ix);
                        self.regroup();
                        self.message_list.splice(ix..ix + 1, 0);
                    }
                }
            }
            Event::Reconnected => {
                if let Some(channel_id) = self.selected_channel.clone() {
                    self.backend.send(Command::OpenChannel { channel_id });
                }
            }
            Event::Status(status) => {
                if status == "Connected" {
                    self.gateway_down = None;
                }
                self.status = status;
            }
            Event::Error(message) => self.error = Some(message),
        }
        cx.notify();
    }

    fn reset_main(&mut self) {
        self.screen = Screen::SignIn;
        self.busy = false;
        self.handoff = None;
        self.pending_link = None;
        self.guilds.clear();
        self.channels.clear();
        self.selected_guild = None;
        self.selected_channel = None;
        self.set_messages(Vec::new());
        self.status.clear();
        self.error = None;
        self.notice = None;
        self.gateway_down = None;
    }

    fn regroup(&mut self) {
        for i in 0..self.messages.len() {
            self.messages[i].grouped = i > 0
                && model::should_group(&self.messages[i - 1].message, &self.messages[i].message)
                && self.messages[i].message.reply.is_none()
                && self.messages[i - 1].message.day() == self.messages[i].message.day();
        }
    }

    fn set_messages(&mut self, messages: Vec<Message>) {
        self.messages = messages
            .into_iter()
            .map(|message| ShownMessage {
                message,
                grouped: false,
                revealed: false,
            })
            .collect();
        self.regroup();
        self.message_list.reset(self.messages.len());
        self.message_list.set_follow_mode(FollowMode::Tail);
    }

    fn submit_sign_in(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.screen != Screen::SignIn || self.handoff.is_some() {
            return;
        }
        let instance = self.instance_input.read(cx).text().trim().to_string();
        if instance.is_empty() {
            self.sign_in_error = Some("Enter the address of a Fluxer instance.".into());
            cx.notify();
            return;
        }
        let command = match self.sign_in_mode {
            SignInMode::Browser => Command::SignInBrowser { instance },
            SignInMode::Token => {
                let token = self.token_input.read(cx).text().trim().to_string();
                if token.is_empty() {
                    self.sign_in_error = Some("Paste a token first.".into());
                    cx.notify();
                    return;
                }
                self.pasted_token_hash = Some(hash_text(&token));
                Command::SignInToken {
                    instance,
                    token: inlet_protocol::Token::new(token),
                }
            }
        };
        self.busy = true;
        self.sign_in_error = None;
        self.sign_in_notice = None;
        self.backend.send(command);
        cx.notify();
    }

    fn select_guild(&mut self, guild_id: Option<String>, cx: &mut Context<Self>) {
        self.selected_guild = guild_id.clone();
        self.selected_channel = None;
        self.channels.clear();
        self.set_messages(Vec::new());
        self.backend.send(Command::OpenGuild { guild_id });
        cx.notify();
    }

    fn select_channel(&mut self, channel_id: String, window: &mut Window, cx: &mut Context<Self>) {
        self.selected_channel = Some(channel_id.clone());
        self.set_messages(Vec::new());
        let name = self
            .channels
            .iter()
            .find(|c| c.id == channel_id)
            .map(channel_label)
            .unwrap_or_default();
        self.composer
            .update(cx, |c, cx| c.set_placeholder(format!("Message {name}"), cx));
        self.composer.focus_handle(cx).focus(window, cx);
        self.backend.send(Command::OpenChannel { channel_id });
        cx.notify();
    }

    fn send_message(&mut self, cx: &mut Context<Self>) {
        let Some(channel_id) = self.selected_channel.clone() else {
            return;
        };
        let text = self.composer.read(cx).text().trim().to_string();
        if text.is_empty() {
            return;
        }
        self.composer.update(cx, |c, cx| c.clear(cx));
        self.backend.send(Command::Send { channel_id, text });
    }

    fn cancel_wait(&mut self, _: &ClickEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.backend.send(Command::CancelSignIn);
        self.handoff = None;
        self.busy = false;
        cx.notify();
    }

    fn sign_out(&mut self, _: &ClickEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.backend.send(Command::SignOut);
        cx.notify();
    }

    fn open_link(&mut self, url: &str, masked: bool, cx: &mut Context<Self>) {
        if let Some(channel_id) = url.strip_prefix(markdown::CHANNEL_LINK) {
            self.pending_channel_jump = Some(channel_id.to_string());
            cx.notify();
            return;
        }
        if markdown::openable(url).is_none() {
            self.error = Some("Inlet only opens http and https links.".into());
        } else if masked {
            self.pending_link = Some(url.to_string());
        } else {
            cx.open_url(url);
        }
        cx.notify();
    }

    // ---------- views ----------

    fn render_card(&self, body: impl IntoElement) -> impl IntoElement {
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
                                    .child("Inlet"),
                            )
                            .child(
                                div()
                                    .text_color(theme::text_muted())
                                    .child("A desktop client that works with Fluxer"),
                            ),
                    )
                    .child(body),
            )
    }

    fn about_link(label: &'static str, cx: &mut Context<Self>) -> impl IntoElement {
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

    fn render_starting(
        &self,
        instance: Option<String>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let text = match instance {
            Some(i) => format!("Connecting to {i}\u{2026}"),
            None => "Starting\u{2026}".to_string(),
        };
        self.render_card(
            div()
                .flex()
                .flex_col()
                .gap_3()
                .child(div().child(text))
                .child(
                    button("cancel-start", "Cancel", false).on_click(cx.listener(
                        |this, _, _, cx| {
                            this.backend.send(Command::CancelStartup);
                            cx.notify();
                        },
                    )),
                ),
        )
    }

    fn render_offline(
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
                        .child(format!("Offline: {instance}")),
                )
                .child(
                    div()
                        .text_color(theme::text_muted())
                        .child(message.to_string()),
                )
                .child(div().text_color(theme::text_muted()).child(format!(
                    "You are still signed in. Inlet tries again in {retry_in} s."
                )))
                .child(button("retry", "Retry now", true).on_click(cx.listener(
                    |this, _, _, cx| {
                        this.backend.send(Command::RetryNow);
                        cx.notify();
                    },
                )))
                .child(
                    button("offline-sign-out", "Sign out", false)
                        .on_click(cx.listener(Self::sign_out)),
                )
                .child(Self::about_link("About Inlet and licences", cx)),
        )
    }

    fn render_confirm(
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
                .child(div().child("You signed in as:"))
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
                                .child(div().font_weight(FontWeight::SEMIBOLD).text_color(theme::text_strong()).child(display_name.to_string()))
                                .child(div().text_color(theme::text_muted()).child(format!("@{account}")))
                                .child(div().text_size(px(12.)).text_color(theme::text_muted()).child(instance.to_string())),
                        ),
                )
                .child(div().text_color(theme::text_muted()).child("Inlet stores the session in your system keychain only after you continue."))
                .child(div().text_size(px(12.)).text_color(theme::text_muted()).child(if pasted {
                    "Cancel only forgets the pasted token here. The app it came from stays signed in."
                } else {
                    "Cancel ends this new session on the server."
                }))
                .child(button("confirm-continue", "Continue", true).on_click(cx.listener(|this, _, _, cx| {
                    this.backend.send(Command::ConfirmAccount);
                    this.screen = Screen::Finishing;
                    cx.notify();
                })))
                .child(button("confirm-cancel", "Cancel", false).on_click(cx.listener(|this, _, _, cx| {
                    this.pasted_token_hash = None;
                    this.backend.send(Command::RejectAccount);
                    cx.notify();
                }))),
        )
    }

    fn render_fatal(&self, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_card(
            div()
                .flex()
                .flex_col()
                .gap_3()
                .child(
                    div()
                        .text_color(theme::danger())
                        .child("The network part of Inlet stopped."),
                )
                .child(
                    div()
                        .text_color(theme::text_muted())
                        .child("Restart it to reconnect. Your session is kept."),
                )
                .child(button("restart-backend", "Restart backend", true).on_click(
                    cx.listener(|this, _, window, cx| this.restart_backend(true, window, cx)),
                ))
                .child(
                    button("fatal-sign-out", "Sign out", false).on_click(
                        cx.listener(|this, _, window, cx| this.fatal_sign_out(window, cx)),
                    ),
                ),
        )
    }

    fn render_sign_in(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let mode = self.sign_in_mode;
        let tab = |id: &'static str, label: &'static str, active: bool| {
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
        if let Some((url, code)) = self.handoff.clone() {
            form = form
                .child(div().child("Your browser should now show the sign-in page. Sign in there, then enter this code:"))
                .child(
                    div()
                        .text_size(px(28.))
                        .font_weight(FontWeight::BOLD)
                        .text_color(theme::text_strong())
                        .font_family(markdown::mono_font())
                        .child(code),
                )
                .child(div().text_size(px(12.)).text_color(theme::text_muted()).child(url.clone()))
                .child(div().text_color(theme::text_muted()).child("Waiting for the browser\u{2026}"))
                .child(button("reopen", "Open the page again", false).on_click(move |_, _, cx| {
                    if safe_handoff_url(&url) {
                        cx.open_url(&url)
                    }
                }))
                .child(button("cancel-wait", "Cancel", false).on_click(cx.listener(Self::cancel_wait)));
        } else {
            form = form
                .child(field("Instance", self.instance_input.clone()))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_2()
                        .child(
                            tab(
                                "mode-browser",
                                "Sign in with browser",
                                mode == SignInMode::Browser,
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.sign_in_mode = SignInMode::Browser;
                                cx.notify();
                            })),
                        )
                        .child(
                            tab("mode-token", "Paste token", mode == SignInMode::Token).on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.sign_in_mode = SignInMode::Token;
                                    cx.notify();
                                }),
                            ),
                        ),
                );
            form = match mode {
                SignInMode::Browser => form.child(div().text_size(px(13.)).text_color(theme::text_muted()).child(
                    "Inlet opens the instance's sign-in page in your browser. Sign in there (passkeys, SSO and email codes all work) and enter the code Inlet shows.",
                )),
                SignInMode::Token => form.child(field("Token", self.token_input.clone())).child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme::text_muted())
                        .child("The token is kept in your system keychain, never in a file. Inlet clears it from the clipboard after sign-in."),
                ),
            };
            let label = match (self.busy, mode) {
                (true, _) => "Signing in\u{2026}",
                (false, SignInMode::Browser) => "Open the sign-in page",
                (false, SignInMode::Token) => "Sign in",
            };
            form = form.child(
                button("sign-in", label, true)
                    .on_click(cx.listener(|this, _, window, cx| this.submit_sign_in(window, cx))),
            );
        }
        if let Some(err) = &self.sign_in_error {
            form = form.child(div().text_color(theme::danger()).child(err.clone()));
        }
        if let Some(notice) = &self.sign_in_notice {
            form = form.child(div().text_color(theme::text_muted()).child(notice.clone()));
        }
        form = form.child(Self::about_link("About Inlet and licences", cx));
        self.render_card(form)
    }

    fn render_guild_rail(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.selected_guild.clone();
        // One rail entry: a 4 px pill on the left (8 px at rest, 20 on hover, 40 when
        // selected) and a 44 px icon, a circle that becomes a rounded square.
        let item = |id: SharedString, content: AnyElement, active: bool| {
            let group = SharedString::from(format!("{id}-g"));
            div()
                .id(id)
                .group(group.clone())
                .relative()
                .w(px(RAIL_WIDTH))
                .h(px(48.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .child(
                    div()
                        .absolute()
                        .left_0()
                        .top(px(if active { 4. } else { 20. }))
                        .w(px(4.))
                        .h(px(if active { 40. } else { 8. }))
                        .rounded_r(px(4.))
                        .bg(theme::text_strong())
                        .when(!active, |d| {
                            d.opacity(0.).group_hover(group.clone(), |s| {
                                s.opacity(1.).top(px(14.)).h(px(20.))
                            })
                        }),
                )
                .child(
                    div()
                        .size(px(44.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(if active { 13. } else { 22. }))
                        .bg(if active {
                            theme::brand()
                        } else {
                            theme::guild_item()
                        })
                        .text_color(theme::text_strong())
                        .font_weight(FontWeight::MEDIUM)
                        .text_size(px(15.))
                        .group_hover(group, |s| s.rounded(px(13.)).bg(theme::brand()))
                        .child(content),
                )
        };
        div()
            .id("guild-rail")
            .w(px(RAIL_WIDTH))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(6.))
            .pt_2()
            .bg(theme::bg_app())
            .overflow_y_scroll()
            .child(
                item(
                    "home".into(),
                    Icon::ChatCircle
                        .el(px(22.), theme::text_strong())
                        .into_any_element(),
                    selected.is_none(),
                )
                .on_click(cx.listener(|this, _, _, cx| this.select_guild(None, cx))),
            )
            .child(
                div()
                    .w(px(32.))
                    .h(px(2.))
                    .flex_none()
                    .rounded_full()
                    .bg(theme::guild_item()),
            )
            .children(self.guilds.iter().map(|g| {
                let id = g.id.clone();
                item(
                    format!("guild-{}", g.id).into(),
                    g.initials().into_any_element(),
                    selected.as_deref() == Some(g.id.as_str()),
                )
                .on_click(
                    cx.listener(move |this, _, _, cx| this.select_guild(Some(id.clone()), cx)),
                )
            }))
    }

    fn render_channel_list(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let title = match &self.selected_guild {
            None => "Direct messages".to_string(),
            Some(id) => self
                .guilds
                .iter()
                .find(|g| &g.id == id)
                .map(|g| g.name.clone())
                .unwrap_or_default(),
        };
        let selected = self.selected_channel.clone();
        let mut previous_was_channel = false;
        let rows: Vec<AnyElement> = self
            .channels
            .iter()
            .filter(|c| {
                // Folded categories hide their channels, except the open one.
                c.kind == ChannelKind::Category
                    || c.parent_id
                        .as_ref()
                        .is_none_or(|p| !self.collapsed.contains(p))
                    || selected.as_deref() == Some(c.id.as_str())
            })
            .map(|c| {
                if c.kind == ChannelKind::Category {
                    let folded = self.collapsed.contains(&c.id);
                    let cat = c.id.clone();
                    let spaced = std::mem::replace(&mut previous_was_channel, false);
                    return div()
                        .id(SharedString::from(format!("ch-{}", c.id)))
                        .flex_none()
                        .when(spaced, |d| d.mt_4())
                        .h(px(32.))
                        .ml_2()
                        .px_2()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_1()
                        .cursor_pointer()
                        .text_size(px(14.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme::text_channel())
                        .hover(|s| s.text_color(theme::text_strong()))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .child(c.name.clone()),
                        )
                        .child(
                            if folded {
                                Icon::CaretRight
                            } else {
                                Icon::CaretDown
                            }
                            .el(px(12.), theme::text_channel()),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if !this.collapsed.remove(&cat) {
                                this.collapsed.insert(cat.clone());
                            }
                            cx.notify();
                        }))
                        .into_any_element();
                }
                previous_was_channel = true;
                let active = selected.as_deref() == Some(c.id.as_str());
                let messageable = c.is_messageable();
                let id = c.id.clone();
                let color = if active {
                    theme::text_strong()
                } else {
                    theme::text_channel()
                };
                div()
                    .id(SharedString::from(format!("ch-{}", c.id)))
                    // Rows must not shrink: a scrolling flex column shrinks its children
                    // when the list is taller than the view, and `overflow_hidden` lets
                    // a row shrink to nothing.
                    .flex_none()
                    .h(px(32.))
                    .ml_2()
                    .px_2()
                    .rounded(px(6.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(color)
                    .when(active, |d| d.bg(theme::selected_bg()))
                    .when(messageable, |d| {
                        d.cursor_pointer()
                            .hover(|s| s.bg(theme::hover_bg()).text_color(theme::text()))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.select_channel(id.clone(), window, cx)
                            }))
                    })
                    .child(channel_icon(c.kind).el(px(20.), color))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(c.name.clone()),
                    )
                    .into_any_element()
            })
            .collect();
        div()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .bg(theme::bg_app())
            .child(
                div()
                    .h(px(HEADER_HEIGHT))
                    .flex_none()
                    .px_4()
                    .flex()
                    .items_center()
                    .border_b_1()
                    .border_color(theme::divider())
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::text_strong())
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(title),
            )
            .child(
                div()
                    .id("channels")
                    .flex_1()
                    .overflow_y_scroll()
                    .pt_2()
                    .pr_2()
                    .pb_2()
                    .flex()
                    .flex_col()
                    .gap(px(1.))
                    .children(rows),
            )
    }

    /// The floating user panel under the rail and channel list.
    fn render_user_panel(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let initials = Guild {
            id: String::new(),
            name: self.username.clone(),
        }
        .initials();
        let icon_button = |id: &'static str, icon: Icon| {
            div()
                .id(id)
                .size(px(32.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.))
                .cursor_pointer()
                .hover(|s| s.bg(theme::hover_bg()))
                .child(icon.el(px(20.), theme::text_muted()))
        };
        div().flex_none().p(px(6.)).bg(theme::bg_app()).child(
            div()
                .h(px(58.))
                .px_3()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .rounded(px(8.))
                .bg(theme::panel_bg())
                .border_1()
                .border_color(theme::ring())
                .child(
                    div()
                        .size(px(32.))
                        .flex_none()
                        .rounded_full()
                        .bg(theme::brand())
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(12.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme::text_strong())
                        .child(initials),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .child(
                            div()
                                .text_size(px(14.))
                                .line_height(px(18.))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme::text_strong())
                                .text_ellipsis()
                                .child(self.username.clone()),
                        )
                        .child(
                            div()
                                .text_size(px(11.))
                                .line_height(px(14.))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme::text_muted())
                                .text_ellipsis()
                                .child(self.instance.clone()),
                        ),
                )
                .child(
                    icon_button("about", Icon::Info).on_click(cx.listener(|this, _, _, cx| {
                        this.show_about = true;
                        cx.notify();
                    })),
                )
                .child(
                    icon_button("sign-out", Icon::SignOut).on_click(cx.listener(Self::sign_out)),
                ),
        )
    }

    fn render_message(&mut self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(shown) = self.messages.get(ix) else {
            return div().into_any_element();
        };
        let m = &shown.message;
        let weak = cx.entity().downgrade();
        let message_id = m.id.clone();
        let ctx = RenderCtx {
            revealed: shown.revealed,
            on_link: Rc::new({
                let weak = weak.clone();
                move |url, masked, _window, cx| {
                    let url = url.to_string();
                    let _ = weak.update(cx, |this, cx| this.open_link(&url, masked, cx));
                }
            }),
            on_reveal: Rc::new({
                let weak = weak.clone();
                let message_id = message_id.clone();
                move |_window, cx| {
                    let _ = weak.update(cx, |this, cx| {
                        if let Some(ix) = this
                            .messages
                            .iter()
                            .position(|s| s.message.id == message_id)
                        {
                            this.messages[ix].revealed = true;
                            this.message_list.splice(ix..ix + 1, 1);
                            cx.notify();
                        }
                    });
                }
            }),
        };
        let mut body = div().flex().flex_col().gap_1().child(markdown::render(
            &format!("m{}", m.id),
            &m.blocks,
            &ctx,
        ));
        if m.truncated {
            let message_id = message_id.clone();
            body = body.child(
                div()
                    .id(SharedString::from(format!("more-{}", m.id)))
                    .text_color(theme::link())
                    .cursor_pointer()
                    .child("Show more")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(ix) = this
                            .messages
                            .iter()
                            .position(|s| s.message.id == message_id)
                        {
                            let msg = &mut this.messages[ix].message;
                            msg.blocks = markdown::parse(&msg.content);
                            msg.truncated = false;
                            this.message_list.splice(ix..ix + 1, 1);
                            cx.notify();
                        }
                    })),
            );
        }
        for (i, e) in m.embeds.iter().enumerate() {
            body = body.child(render_embed(&format!("m{}-e{i}", m.id), e, &ctx, cx));
        }
        if m.is_empty() {
            body = body.child(
                div()
                    .text_size(px(14.))
                    .text_color(theme::text_muted())
                    .child("(no text)"),
            );
        }
        for (i, a) in m.attachments.iter().enumerate() {
            let url = a.url.clone();
            body = body.child(
                div()
                    .id(SharedString::from(format!("att-{}-{i}", m.id)))
                    .text_color(theme::link())
                    .cursor_pointer()
                    .child(format!(
                        "Attachment: {} ({})",
                        a.filename,
                        markdown::link_target_label(&a.url)
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| this.open_link(&url, false, cx))),
            );
        }
        let header = (!shown.grouped).then(|| {
            div()
                .flex()
                .flex_row()
                .items_baseline()
                .gap_2()
                .child(
                    div()
                        .font_weight(FontWeight::MEDIUM)
                        .line_height(px(22.))
                        .text_color(theme::text_strong())
                        .child(m.author.clone()),
                )
                .when(m.author_bot, |d| {
                    d.child(
                        div()
                            .px(px(5.6))
                            .py(px(2.))
                            .rounded(px(4.))
                            .bg(theme::brand())
                            .text_size(px(10.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(gpui::white())
                            .child("BOT"),
                    )
                })
                .when(m.author_username != m.author, |d| {
                    d.child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme::text_muted())
                            .child(format!("@{}", m.author_username)),
                    )
                })
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme::timestamp())
                        .child(m.timestamp.clone()),
                )
        });
        let reply = m.reply.as_ref().map(|r| {
            div()
                .flex()
                .flex_row()
                .gap_1()
                .text_size(px(13.))
                .text_color(theme::text_muted())
                .whitespace_nowrap()
                .overflow_hidden()
                .child("\u{21B1}")
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(r.author.clone()),
                )
                .child(r.snippet.clone())
        });
        let new_day = match m.day() {
            Some(day) => {
                ix == 0 || self.messages.get(ix - 1).and_then(|p| p.message.day()) != Some(day)
            }
            None => false,
        };
        let separator = m.day().filter(|_| new_day).map(|day| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .px_4()
                .pt_4()
                .child(div().flex_1().h(px(2.)).bg(theme::separator_line()))
                .child(
                    div()
                        .text_size(px(12.))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme::text_tertiary())
                        .child(model::day_label(day)),
                )
                .child(div().flex_1().h(px(2.)).bg(theme::separator_line()))
        });
        // Avatar slot: initials until avatars arrive (next milestone).
        let avatar = (!shown.grouped).then(|| {
            div()
                .size(px(AVATAR_COLUMN))
                .rounded_full()
                .bg(theme::guild_item())
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(14.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme::text_muted())
                .child(
                    Guild {
                        id: String::new(),
                        name: m.author.clone(),
                    }
                    .initials(),
                )
        });
        div()
            .w_full()
            .flex()
            .flex_col()
            .children(separator)
            .child(
                div()
                    .w_full()
                    .when(!shown.grouped, |d| d.mt_4())
                    .pl_4()
                    .pr_4()
                    .py(px(2.))
                    .flex()
                    .flex_row()
                    .gap_3()
                    .text_color(theme::text())
                    .line_height(px(22.))
                    .when(m.mentions_me, |d| {
                        d.bg(theme::mention_me_bg())
                            .border_l_2()
                            .border_color(theme::mention_me_bar())
                    })
                    .when(!m.mentions_me, |d| d.hover(|s| s.bg(theme::hover_bg())))
                    .child(div().w(px(AVATAR_COLUMN)).flex_none().children(avatar))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .children(reply)
                            .children(header)
                            .child(body),
                    ),
            )
            .into_any_element()
    }

    fn render_chat(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let open = self
            .selected_channel
            .as_ref()
            .and_then(|id| self.channels.iter().find(|c| &c.id == id));
        let title = open.map(|c| c.name.clone()).unwrap_or_default();
        let title_icon = open.map(|c| channel_icon(c.kind));
        let topic = self
            .selected_channel
            .as_ref()
            .and_then(|id| self.channels.iter().find(|c| &c.id == id))
            .and_then(|c| c.topic.clone());
        let has_channel = self.selected_channel.is_some();
        let bar = |id: &'static str, text: String, bg| {
            div()
                .id(id)
                .px_4()
                .py_2()
                .bg(bg)
                .text_color(theme::text_strong())
                .cursor_pointer()
                .child(format!("{text}  (click to dismiss)"))
        };
        div()
            .flex_1()
            .h_full()
            .flex()
            .flex_col()
            .bg(theme::bg_chat())
            .child(
                div()
                    .h(px(HEADER_HEIGHT))
                    .flex_none()
                    .px_4()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(theme::divider())
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .children(title_icon.map(|i| i.el(px(24.), theme::text_muted())))
                            .child(
                                div()
                                    .flex_none()
                                    .font_weight(FontWeight::MEDIUM)
                                    .line_height(px(24.))
                                    .text_color(theme::text_strong())
                                    .child(title),
                            )
                            .children(topic.map(|t| {
                                div()
                                    .min_w_0()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .w(px(1.))
                                            .h(px(20.))
                                            .flex_none()
                                            .bg(theme::accent_border()),
                                    )
                                    .child(
                                        div()
                                            .min_w_0()
                                            .overflow_hidden()
                                            .text_ellipsis()
                                            .text_size(px(13.))
                                            .line_height(px(18.))
                                            .text_color(theme::text_tertiary())
                                            .child(t),
                                    )
                            })),
                    )
                    .child(
                        div()
                            .flex_none()
                            .pl_3()
                            .text_size(px(12.))
                            .text_color(theme::text_muted())
                            .child(self.status.clone()),
                    ),
            )
            .children(self.error.clone().map(|e| {
                bar("error", e, theme::danger()).on_click(cx.listener(|this, _, _, cx| {
                    this.error = None;
                    cx.notify();
                }))
            }))
            .children(self.gateway_down.clone().map(|message| {
                div()
                    .px_4()
                    .py_2()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .bg(theme::selected_bg())
                    .text_color(theme::text_strong())
                    .child(format!("Disconnected. {message}"))
                    .child(button("reconnect", "Reconnect", true).on_click(cx.listener(
                        |this, _, _, cx| {
                            this.gateway_down = None;
                            this.status = "Reconnecting\u{2026}".into();
                            this.backend.send(Command::Reconnect);
                            cx.notify();
                        },
                    )))
            }))
            .children(self.notice.clone().map(|n| {
                bar("notice", n, theme::selected_bg()).on_click(cx.listener(|this, _, _, cx| {
                    this.notice = None;
                    cx.notify();
                }))
            }))
            .child(
                list(
                    self.message_list.clone(),
                    cx.processor(|this, ix: usize, _window, cx| this.render_message(ix, cx)),
                )
                .flex_1()
                .w_full(),
            )
            .when(has_channel, |d| {
                d.child(
                    div()
                        .flex_none()
                        .px(px(6.))
                        .pb(px(6.))
                        .pt_1()
                        .child(self.composer.clone()),
                )
            })
    }

    fn render_link_confirm(&self, url: &str, cx: &mut Context<Self>) -> impl IntoElement {
        let host = markdown::link_target_label(url);
        // Open exactly what was parsed and shown, never the raw text.
        let parsed = markdown::openable(url);
        let shown = parsed
            .as_ref()
            .map(|u| u.as_str().to_string())
            .unwrap_or_else(|| url.to_string());
        let lines = wrap_url(&shown, 64, 6);
        div()
            .id("link-confirm")
            .occlude()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x000000a0))
            .child(
                div()
                    .w(px(520.))
                    .p_6()
                    .rounded_lg()
                    .bg(theme::bg_chat())
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        div()
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme::text_strong())
                            .child("Open this link?"),
                    )
                    .child(
                        div()
                            .text_color(theme::text_muted())
                            .child("The link text does not show where it goes. It opens:"),
                    )
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::text_strong())
                            .child(host),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .text_size(px(12.))
                            .font_family(markdown::mono_font())
                            .text_color(theme::text())
                            .children(lines),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .child(button("link-open", "Open", true).on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.pending_link = None;
                                    if let Some(u) = &parsed {
                                        cx.open_url(u.as_str());
                                    }
                                    cx.notify();
                                },
                            )))
                            .child(button("link-cancel", "Cancel", false).on_click(cx.listener(
                                |this, _, _, cx| {
                                    this.pending_link = None;
                                    cx.notify();
                                },
                            ))),
                    ),
            )
    }
}

/// Splits a URL into lines of `width` characters, at most `max_lines`, ending with "…"
/// when cut. URLs have no spaces, so normal text wrapping would let them run out of the box.
fn wrap_url(url: &str, width: usize, max_lines: usize) -> Vec<String> {
    let chars: Vec<char> = url.chars().collect();
    let mut lines: Vec<String> = chars.chunks(width).map(|c| c.iter().collect()).collect();
    if lines.len() > max_lines {
        lines.truncate(max_lines);
        if let Some(last) = lines.last_mut() {
            last.push('\u{2026}');
        }
    }
    lines
}

/// The running backend, for the window-close handler.
struct CurrentBackend(BackendHandle);
impl gpui::Global for CurrentBackend {}

/// Avatar column of the message grid (16 | 40 | 12 | text).
const AVATAR_COLUMN: f32 = 40.;
/// Guild rail width.
const RAIL_WIDTH: f32 = 72.;
/// Channel list / DM list width.
const SIDEBAR_WIDTH: f32 = 320.;
/// Height of the guild header and the channel header.
const HEADER_HEIGHT: f32 = 56.;

fn channel_icon(kind: ChannelKind) -> Icon {
    match kind {
        ChannelKind::Text => Icon::Hash,
        ChannelKind::Announcement => Icon::Megaphone,
        ChannelKind::Voice => Icon::Speaker,
        ChannelKind::Dm | ChannelKind::GroupDm => Icon::At,
        _ => Icon::Chats,
    }
}

fn channel_label(c: &Channel) -> String {
    match c.kind {
        ChannelKind::Dm | ChannelKind::GroupDm => format!("@{}", c.name),
        ChannelKind::Text | ChannelKind::Announcement => format!("#{}", c.name),
        _ => c.name.clone(),
    }
}

/// Draws an embed: colour bar, author, title, description, fields, footer, media links.
fn render_embed(
    id: &str,
    e: &model::Embed,
    ctx: &RenderCtx,
    cx: &mut Context<Inlet>,
) -> AnyElement {
    let bar = e
        .color
        .map(|c| gpui::rgb(c).into())
        .unwrap_or(theme::brand());
    let mut content = div()
        .flex()
        .flex_col()
        .gap_2()
        .pt_3()
        .px_3()
        .pb(px(14.))
        .flex_1()
        .min_w_0();
    if let Some(author) = &e.author {
        content = content.child(
            div()
                .text_size(px(13.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(author.clone()),
        );
    }
    if let Some(title) = &e.title {
        let link = e.url.clone().filter(|u| markdown::openable(u).is_some());
        let mut t = div()
            .id(SharedString::from(format!("{id}-title")))
            .font_weight(FontWeight::BOLD)
            .text_color(if link.is_some() {
                theme::link()
            } else {
                theme::text_strong()
            })
            .child(title.clone());
        if let Some(url) = link {
            t = t
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| this.open_link(&url, true, cx)));
        }
        content = content.child(t);
    }
    if !e.description.is_empty() {
        content = content.child(div().text_size(px(14.)).child(markdown::render(
            &format!("{id}-d"),
            &e.description,
            ctx,
        )));
    }
    for (i, f) in e.fields.iter().enumerate() {
        content = content.child(
            div()
                .pt_1()
                .flex()
                .flex_col()
                .text_size(px(14.))
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme::text_strong())
                        .child(f.name.clone()),
                )
                .child(markdown::render(&format!("{id}-f{i}"), &f.value, ctx)),
        );
    }
    for (i, a) in e.media.iter().enumerate() {
        let url = a.url.clone();
        content = content.child(
            div()
                .id(SharedString::from(format!("{id}-media{i}")))
                .text_size(px(13.))
                .text_color(theme::link())
                .cursor_pointer()
                .child(format!(
                    "{}: {}",
                    a.filename,
                    markdown::link_target_label(&a.url)
                ))
                .on_click(cx.listener(move |this, _, _, cx| this.open_link(&url, false, cx))),
        );
    }
    if let Some(footer) = &e.footer {
        content = content.child(
            div()
                .pt_1()
                .text_size(px(12.))
                .text_color(theme::text_muted())
                .child(footer.clone()),
        );
    }
    div()
        .mt_1()
        .max_w(px(516.))
        .flex()
        .flex_row()
        .rounded(px(8.))
        .overflow_hidden()
        .border_1()
        .border_color(theme::accent_border())
        .bg(theme::embed_bg())
        .child(div().w(px(4.)).flex_none().bg(bar))
        .child(content)
        .into_any_element()
}

fn field(label: &'static str, input: Entity<TextInput>) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_size(px(12.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme::text_muted())
                .child(label.to_uppercase()),
        )
        .child(input)
}

pub fn button(id: &'static str, label: &'static str, primary: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_4()
        .py_2()
        .rounded_md()
        .flex()
        .justify_center()
        .cursor_pointer()
        .text_color(theme::text_strong())
        .bg(if primary {
            theme::brand()
        } else {
            theme::bg_textarea()
        })
        .hover(|s| s.opacity(0.85))
        .child(label)
}

impl Focusable for Inlet {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Inlet {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(channel_id) = self.pending_channel_jump.take() {
            if self
                .channels
                .iter()
                .any(|c| c.id == channel_id && c.is_messageable())
            {
                self.select_channel(channel_id, window, cx);
            } else {
                self.notice = Some("That channel is not in the list on screen.".into());
            }
        }
        let root = div()
            .relative()
            .size_full()
            .track_focus(&self.focus)
            .on_action(|_: &FocusNext, window, cx| window.focus_next(cx))
            .on_action(|_: &FocusPrev, window, cx| window.focus_prev(cx))
            .text_color(theme::text())
            .text_size(px(16.))
            .font_family(ui_font());
        if self.show_about {
            return root
                .child(about::render(self.show_licences, cx))
                .into_any_element();
        }
        let content = match self.screen.clone() {
            Screen::Starting { instance } => self.render_starting(instance, cx).into_any_element(),
            Screen::Offline {
                instance,
                message,
                retry_in_secs,
            } => self
                .render_offline(&instance, &message, retry_in_secs, cx)
                .into_any_element(),
            Screen::Fatal => self.render_fatal(cx).into_any_element(),
            Screen::Confirm {
                display_name,
                account,
                instance,
                pasted,
            } => self
                .render_confirm(&display_name, &account, &instance, pasted, cx)
                .into_any_element(),
            Screen::Finishing => self
                .render_card(div().child("Signing in\u{2026}"))
                .into_any_element(),
            Screen::SignIn => self.render_sign_in(cx).into_any_element(),
            Screen::Main => div()
                .size_full()
                .flex()
                .flex_row()
                .child(
                    div()
                        .h_full()
                        .flex_none()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .flex_1()
                                .min_h_0()
                                .flex()
                                .flex_row()
                                .child(self.render_guild_rail(cx))
                                .child(self.render_channel_list(cx)),
                        )
                        .child(self.render_user_panel(cx)),
                )
                .child(self.render_chat(cx))
                .into_any_element(),
        };
        let confirm = self
            .pending_link
            .clone()
            .map(|url| self.render_link_confirm(&url, cx));
        root.child(content).children(confirm).into_any_element()
    }
}

fn ui_font() -> &'static str {
    assets::ui_font()
}

fn main() {
    // Network libraries log frame contents at trace level; keep them quiet whatever
    // RUST_LOG says, so a token can never end up in a log.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
        .filter_module("tungstenite", log::LevelFilter::Off)
        .filter_module("tokio_tungstenite", log::LevelFilter::Off)
        .filter_module("hyper", log::LevelFilter::Off)
        .filter_module("hyper_util", log::LevelFilter::Off)
        .filter_module("reqwest", log::LevelFilter::Warn)
        .filter_module("rustls", log::LevelFilter::Warn)
        .init();
    gpui_platform::application().run(|cx: &mut App| {
        text_input::bind_keys(cx);
        assets::load_fonts(cx);
        // Fit small screens (CI runners have 1024x768): at most 90 % of the display.
        let (mut w, mut h) = (1200.0_f32, 780.0_f32);
        if let Some(display) = cx.primary_display() {
            let d = display.bounds().size;
            w = w.min(d.width.as_f32() * 0.9);
            h = h.min(d.height.as_f32() * 0.9);
        }
        let bounds = Bounds::centered(None, size(px(w), px(h)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Inlet \u{2013} works with Fluxer".into()),
                    ..Default::default()
                }),
                app_id: Some("no.opland.inlet".into()),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| Inlet::new(window, cx)),
        )
        .expect("open the main window");
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                // Let the backend end an unconfirmed browser sign-in first (bounded wait).
                if let Some(current) = cx.try_global::<CurrentBackend>() {
                    let (tx, rx) = std::sync::mpsc::channel();
                    current.0.send(Command::Shutdown(tx));
                    let _ = rx.recv_timeout(std::time::Duration::from_secs(4));
                }
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_urls_are_wrapped_and_capped() {
        let url = format!("https://example.com/{}", "a".repeat(1000));
        let lines = wrap_url(&url, 64, 6);
        assert_eq!(lines.len(), 6);
        assert!(lines.iter().all(|l| l.chars().count() <= 65));
        assert!(lines.last().unwrap().ends_with('\u{2026}'));
        assert_eq!(
            wrap_url("https://a.b", 64, 6),
            vec!["https://a.b".to_string()]
        );
    }

    #[test]
    fn handoff_page_must_be_https() {
        assert!(safe_handoff_url("https://web.fluxer.app/login?handoff=1"));
        assert!(!safe_handoff_url("http://fluxer.example/login"));
        assert!(safe_handoff_url("http://localhost:8080/login"));
        assert!(!safe_handoff_url("fluxer://handoff"));
        assert!(!safe_handoff_url("not a url"));
    }
}
