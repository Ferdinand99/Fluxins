//! Inlet: a native desktop client for Fluxer.

mod backend;
mod markdown;
mod mock;
mod model;
mod real;
mod text_input;
mod theme;
mod token_store;

use futures::StreamExt;
use gpui::{
    AnyElement, App, Bounds, ClickEvent, Context, Entity, FocusHandle, Focusable, FollowMode,
    FontWeight, ListAlignment, ListState, SharedString, Subscription, TitlebarOptions, Window,
    WindowBounds, WindowOptions, div, list, prelude::*, px, size,
};

use backend::{BackendHandle, Command, DEFAULT_INSTANCE, Event};
use markdown::Block;
use model::{Channel, ChannelKind, Guild, Message};
use text_input::{FocusNext, FocusPrev, InputEvent, TextInput};

#[derive(Clone, Copy, PartialEq)]
enum Screen {
    Starting,
    SignIn,
    Main,
}

#[derive(Clone, Copy, PartialEq)]
enum SignInMode {
    Browser,
    Token,
}

struct ShownMessage {
    message: Message,
    blocks: Vec<Block>,
    /// Same author as the message above: hide the header.
    grouped: bool,
}

struct Inlet {
    backend: BackendHandle,
    focus: FocusHandle,
    screen: Screen,

    // Sign-in
    instance_input: Entity<TextInput>,
    token_input: Entity<TextInput>,
    sign_in_mode: SignInMode,
    busy: bool,
    sign_in_error: Option<String>,
    /// Browser sign-in in progress: page URL and the code to enter there.
    handoff: Option<(String, String)>,

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

    _subscriptions: Vec<Subscription>,
}

impl Inlet {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (events_tx, mut events_rx) = futures::channel::mpsc::unbounded();
        let backend = backend::start(events_tx);

        let instance_input = cx.new(|cx| TextInput::new("fluxer.opland.net", cx).with_text(DEFAULT_INSTANCE));
        let token_input = cx.new(|cx| TextInput::new("Paste your token", cx).masked());
        let composer = cx.new(|cx| TextInput::new("Message", cx));

        let mut subscriptions = Vec::new();
        for input in [&instance_input, &token_input] {
            subscriptions.push(cx.subscribe_in(input, window, |this, _, event: &InputEvent, window, cx| {
                match event {
                    InputEvent::Submit => this.submit_sign_in(window, cx),
                }
            }));
        }
        subscriptions.push(cx.subscribe_in(&composer, window, |this, _, event: &InputEvent, _window, cx| {
            match event {
                InputEvent::Submit => this.send_message(cx),
            }
        }));

        // Events from the backend arrive here, on the UI thread.
        cx.spawn_in(window, async move |this, cx| {
            while let Some(event) = events_rx.next().await {
                if this.update_in(cx, |this, window, cx| this.handle_event(event, window, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();

        backend.send(Command::Restore);

        let message_list = ListState::new(0, ListAlignment::Bottom, px(800.));
        message_list.set_follow_mode(FollowMode::Tail);

        Self {
            backend,
            focus: cx.focus_handle(),
            screen: Screen::Starting,
            instance_input,
            token_input,
            sign_in_mode: SignInMode::Browser,
            busy: false,
            sign_in_error: None,
            handoff: None,
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
            _subscriptions: subscriptions,
        }
    }

    fn handle_event(&mut self, event: Event, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            Event::NeedsSignIn { instance } => {
                if let Some(instance) = instance {
                    self.instance_input.update(cx, |i, cx| i.set_text(instance, cx));
                }
                self.screen = Screen::SignIn;
                self.busy = false;
                self.instance_input.focus_handle(cx).focus(window, cx);
            }
            Event::HandoffStarted { url, code } => {
                self.busy = false;
                self.sign_in_error = None;
                if url.starts_with("https://") || url.starts_with("http://") {
                    cx.open_url(&url);
                }
                self.handoff = Some((url, code));
            }
            Event::SignInFailed(message) => {
                self.busy = false;
                self.handoff = None;
                        self.sign_in_error = Some(message);
                if self.screen == Screen::Starting {
                    self.screen = Screen::SignIn;
                }
            }
            Event::SignedIn { username, instance } => {
                self.busy = false;
                self.handoff = None;
                        self.sign_in_error = None;
                self.token_input.update(cx, |i, cx| i.clear(cx));
                self.username = username;
                self.instance = instance;
                self.screen = Screen::Main;
                self.select_guild(None, cx);
            }
            Event::SessionEnded(message) => {
                self.sign_in_error = Some(message);
                self.reset_main();
            }
            Event::SignedOut => {
                self.reset_main();
            }
            Event::Guilds(guilds) => self.guilds = guilds,
            Event::Channels { guild_id, channels } => {
                if guild_id == self.selected_guild {
                    self.channels = model::sidebar_order(&channels);
                    if self.selected_channel.is_none() {
                        if let Some(first) = self.channels.iter().find(|c| c.is_messageable()).cloned() {
                            self.select_channel(first.id, window, cx);
                        }
                    }
                }
            }
            Event::Messages { channel_id, messages } => {
                if Some(&channel_id) == self.selected_channel.as_ref() {
                    self.set_messages(messages);
                }
            }
            Event::MessageCreated(message) => {
                if Some(&message.channel_id) == self.selected_channel.as_ref()
                    && !self.messages.iter().any(|m| m.message.id == message.id)
                {
                    let grouped = self.messages.last().is_some_and(|m| m.message.author == message.author);
                    let blocks = markdown::parse(&message.content);
                    let at = self.messages.len();
                    self.messages.push(ShownMessage { message, blocks, grouped });
                    self.message_list.splice(at..at, 1);
                }
            }
            Event::MessageUpdated(message) => {
                if let Some(ix) = self.messages.iter().position(|m| m.message.id == message.id) {
                    self.messages[ix].blocks = markdown::parse(&message.content);
                    self.messages[ix].message = message;
                    self.message_list.splice(ix..ix + 1, 1);
                }
            }
            Event::MessageDeleted { channel_id, message_id } => {
                if Some(&channel_id) == self.selected_channel.as_ref() {
                    if let Some(ix) = self.messages.iter().position(|m| m.message.id == message_id) {
                        self.messages.remove(ix);
                        self.message_list.splice(ix..ix + 1, 0);
                    }
                }
            }
            Event::Status(status) => self.status = status,
            Event::Error(message) => self.error = Some(message),
        }
        cx.notify();
    }

    fn reset_main(&mut self) {
        self.screen = Screen::SignIn;
        self.busy = false;
        self.handoff = None;
        self.guilds.clear();
        self.channels.clear();
        self.selected_guild = None;
        self.selected_channel = None;
        self.set_messages(Vec::new());
        self.status.clear();
        self.error = None;
    }

    fn set_messages(&mut self, messages: Vec<Message>) {
        let mut shown: Vec<ShownMessage> = Vec::with_capacity(messages.len());
        for message in messages {
            let grouped = shown.last().is_some_and(|m| m.message.author == message.author);
            let blocks = markdown::parse(&message.content);
            shown.push(ShownMessage { message, blocks, grouped });
        }
        self.messages = shown;
        self.message_list.reset(self.messages.len());
        self.message_list.set_follow_mode(FollowMode::Tail);
    }

    fn submit_sign_in(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
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
                Command::SignInToken { instance, token }
            }
        };
        self.busy = true;
        self.sign_in_error = None;
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
        let name = self.channels.iter().find(|c| c.id == channel_id).map(channel_label).unwrap_or_default();
        self.composer.update(cx, |c, cx| c.set_placeholder(format!("Message {name}"), cx));
        self.composer.focus_handle(cx).focus(window, cx);
        self.backend.send(Command::OpenChannel { channel_id });
        cx.notify();
    }

    fn send_message(&mut self, cx: &mut Context<Self>) {
        let Some(channel_id) = self.selected_channel.clone() else { return };
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

    // ---------- views ----------

    fn render_sign_in(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let mode = self.sign_in_mode;
        let tab = |id: &'static str, label: &'static str, active: bool| {
            div()
                .id(id)
                .px_3()
                .py_1()
                .rounded_md()
                .cursor_pointer()
                .text_color(if active { theme::text_strong() } else { theme::text_muted() })
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
                .child(button("reopen", "Open the page again", false).on_click(move |_, _, cx| cx.open_url(&url)))
                .child(button("cancel-wait", "Cancel", false).on_click(cx.listener(Self::cancel_wait)));
        } else {
            form = form.child(field("Instance", self.instance_input.clone())).child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(tab("mode-browser", "Sign in with browser", mode == SignInMode::Browser).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.sign_in_mode = SignInMode::Browser;
                            cx.notify();
                        }),
                    ))
                    .child(tab("mode-token", "Paste token", mode == SignInMode::Token).on_click(cx.listener(
                        |this, _, _, cx| {
                            this.sign_in_mode = SignInMode::Token;
                            cx.notify();
                        },
                    ))),
            );
            form = match mode {
                SignInMode::Browser => form.child(div().text_size(px(13.)).text_color(theme::text_muted()).child(
                    "Inlet opens the instance's sign-in page in your browser. Sign in there (passkeys, SSO and email codes all work) and enter the code Inlet shows.",
                )),
                SignInMode::Token => form.child(field("Token", self.token_input.clone())).child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme::text_muted())
                        .child("The token is kept in your system keychain, never in a file."),
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

        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(theme::rail_bg())
            .child(
                div()
                    .w(px(460.))
                    .p_8()
                    .rounded_lg()
                    .bg(theme::chat_bg())
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
                    .child(form),
            )
    }

    fn render_guild_rail(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.selected_guild.clone();
        let pill = |id: SharedString, label: String, active: bool| {
            div()
                .id(id)
                .size(px(48.))
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .rounded(if active { px(16.) } else { px(24.) })
                .bg(if active { theme::accent() } else { theme::chat_bg() })
                .text_color(theme::text_strong())
                .font_weight(FontWeight::SEMIBOLD)
                .cursor_pointer()
                .hover(|s| s.rounded(px(16.)).bg(theme::accent()))
                .child(label)
        };
        div()
            .id("guild-rail")
            .w(px(72.))
            .h_full()
            .flex()
            .flex_col()
            .items_center()
            .gap_2()
            .py_3()
            .bg(theme::rail_bg())
            .overflow_y_scroll()
            .child(
                pill("home".into(), "DM".into(), selected.is_none())
                    .on_click(cx.listener(|this, _, _, cx| this.select_guild(None, cx))),
            )
            .child(div().w(px(32.)).h(px(2.)).flex_none().bg(theme::chat_bg()))
            .children(self.guilds.iter().map(|g| {
                let id = g.id.clone();
                pill(format!("guild-{}", g.id).into(), g.initials(), selected.as_deref() == Some(g.id.as_str()))
                    .on_click(cx.listener(move |this, _, _, cx| this.select_guild(Some(id.clone()), cx)))
            }))
    }

    fn render_channel_list(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let title = match &self.selected_guild {
            None => "Direct messages".to_string(),
            Some(id) => self.guilds.iter().find(|g| &g.id == id).map(|g| g.name.clone()).unwrap_or_default(),
        };
        let selected = self.selected_channel.clone();
        div()
            .w(px(240.))
            .h_full()
            .flex()
            .flex_col()
            .bg(theme::sidebar_bg())
            .child(
                div()
                    .h(px(48.))
                    .flex_none()
                    .px_4()
                    .flex()
                    .items_center()
                    .border_b_1()
                    .border_color(theme::header_border())
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::text_strong())
                    .child(title),
            )
            .child(
                div()
                    .id("channels")
                    .flex_1()
                    .overflow_y_scroll()
                    .p_2()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .children(self.channels.iter().map(|c| {
                        if c.kind == ChannelKind::Category {
                            return div()
                                .id(SharedString::from(format!("ch-{}", c.id)))
                                .pt_4()
                                .px_2()
                                .text_size(px(12.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme::text_muted())
                                .child(c.name.to_uppercase());
                        }
                        let active = selected.as_deref() == Some(c.id.as_str());
                        let messageable = c.is_messageable();
                        let id = c.id.clone();
                        div()
                            .id(SharedString::from(format!("ch-{}", c.id)))
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .text_color(if active { theme::text_strong() } else { theme::text_muted() })
                            .when(active, |d| d.bg(theme::selected_bg()))
                            .when(messageable, |d| {
                                d.cursor_pointer()
                                    .hover(|s| s.bg(theme::hover_bg()).text_color(theme::text()))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.select_channel(id.clone(), window, cx)
                                    }))
                            })
                            .child(channel_label(c))
                    })),
            )
            .child(
                div()
                    .h(px(52.))
                    .flex_none()
                    .px_3()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .bg(theme::rail_bg())
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_color(theme::text_strong())
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(self.username.clone()),
                            )
                            .child(div().text_size(px(11.)).text_color(theme::text_muted()).child(self.instance.clone())),
                    )
                    .child(button("sign-out", "Sign out", false).on_click(cx.listener(Self::sign_out))),
            )
    }

    fn render_message(&mut self, ix: usize) -> AnyElement {
        let Some(shown) = self.messages.get(ix) else { return div().into_any_element() };
        let m = &shown.message;
        let mut body = div().flex().flex_col().gap_1().child(markdown::render(&format!("m{}", m.id), &shown.blocks));
        if m.edited {
            body = body.child(div().text_size(px(11.)).text_color(theme::text_muted()).child("(edited)"));
        }
        for (i, a) in m.attachments.iter().enumerate() {
            let url = a.url.clone();
            body = body.child(
                div()
                    .id(SharedString::from(format!("att-{}-{i}", m.id)))
                    .text_color(theme::link())
                    .cursor_pointer()
                    .child(format!("Attachment: {}", a.filename))
                    .on_click(move |_, _, cx| {
                        if url.starts_with("https://") || url.starts_with("http://") {
                            cx.open_url(&url)
                        }
                    }),
            );
        }
        let header = (!shown.grouped).then(|| {
            div()
                .flex()
                .flex_row()
                .items_baseline()
                .gap_2()
                .child(div().font_weight(FontWeight::SEMIBOLD).text_color(theme::text_strong()).child(m.author.clone()))
                .child(div().text_size(px(11.)).text_color(theme::text_muted()).child(m.timestamp.clone()))
        });
        div()
            .px_4()
            .when(!shown.grouped, |d| d.pt_3())
            .pb(px(2.))
            .flex()
            .flex_col()
            .text_color(theme::text())
            .children(header)
            .child(body)
            .into_any_element()
    }

    fn render_chat(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let title = self
            .selected_channel
            .as_ref()
            .and_then(|id| self.channels.iter().find(|c| &c.id == id))
            .map(channel_label)
            .unwrap_or_default();
        let has_channel = self.selected_channel.is_some();
        div()
            .flex_1()
            .h_full()
            .flex()
            .flex_col()
            .bg(theme::chat_bg())
            .child(
                div()
                    .h(px(48.))
                    .flex_none()
                    .px_4()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(theme::header_border())
                    .child(div().font_weight(FontWeight::SEMIBOLD).text_color(theme::text_strong()).child(title))
                    .child(div().text_size(px(12.)).text_color(theme::text_muted()).child(self.status.clone())),
            )
            .children(self.error.clone().map(|e| {
                div()
                    .id("error")
                    .px_4()
                    .py_2()
                    .bg(theme::danger())
                    .text_color(theme::text_strong())
                    .cursor_pointer()
                    .child(format!("{e}  (click to dismiss)"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.error = None;
                        cx.notify();
                    }))
            }))
            .child(
                list(self.message_list.clone(), cx.processor(|this, ix: usize, _window, _cx| this.render_message(ix)))
                    .flex_1()
                    .w_full(),
            )
            .when(has_channel, |d| d.child(div().flex_none().px_4().pb_4().pt_1().child(self.composer.clone())))
    }
}

fn channel_label(c: &Channel) -> String {
    match c.kind {
        ChannelKind::Text => format!("# {}", c.name),
        ChannelKind::Voice => format!("(voice) {}", c.name),
        ChannelKind::Dm => format!("@ {}", c.name),
        ChannelKind::GroupDm => format!("@ {}", c.name),
        _ => c.name.clone(),
    }
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

fn button(id: &'static str, label: &'static str, primary: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_4()
        .py_2()
        .rounded_md()
        .flex()
        .justify_center()
        .cursor_pointer()
        .text_color(theme::text_strong())
        .bg(if primary { theme::accent() } else { theme::input_bg() })
        .hover(|s| s.opacity(0.85))
        .child(label)
}

impl Focusable for Inlet {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Inlet {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let root = div()
            .size_full()
            .track_focus(&self.focus)
            .on_action(|_: &FocusNext, window, cx| window.focus_next(cx))
            .on_action(|_: &FocusPrev, window, cx| window.focus_prev(cx))
            .text_color(theme::text())
            .text_size(px(15.))
            .font_family(ui_font());
        match self.screen {
            Screen::Starting => root
                .flex()
                .items_center()
                .justify_center()
                .bg(theme::rail_bg())
                .child("Starting\u{2026}")
                .into_any_element(),
            Screen::SignIn => root.child(self.render_sign_in(cx)).into_any_element(),
            Screen::Main => root
                .flex()
                .flex_row()
                .child(self.render_guild_rail(cx))
                .child(self.render_channel_list(cx))
                .child(self.render_chat(cx))
                .into_any_element(),
        }
    }
}

fn ui_font() -> &'static str {
    if cfg!(target_os = "macos") {
        ".SystemUIFont"
    } else if cfg!(target_os = "windows") {
        "Segoe UI"
    } else {
        "DejaVu Sans"
    }
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    gpui_platform::application().run(|cx: &mut App| {
        text_input::bind_keys(cx);
        let bounds = Bounds::centered(None, size(px(1200.), px(780.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions { title: Some("Inlet \u{2013} works with Fluxer".into()), ..Default::default() }),
                app_id: Some("no.opland.inlet".into()),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| Inlet::new(window, cx)),
        )
        .expect("open the main window");
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });
}
