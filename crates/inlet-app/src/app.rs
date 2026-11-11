// SPDX-License-Identifier: AGPL-3.0-or-later
//! The app's one view, [`Inlet`]: which screen is up, the state shared by the domains,
//! each domain's own view state, and the bridge to the backend thread. The views and the
//! event handlers live in the `ui` domains.

use futures::StreamExt;
use gpui::{App, Context, Entity, FocusHandle, Focusable, Subscription, Window};
use inlet_state::AccountState;

use crate::backend::{self, accounts, BackendHandle, Event};
use crate::model::{Channel, Guild};
use crate::ui;

#[derive(Clone, PartialEq)]
pub enum Screen {
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

pub struct Inlet {
    pub backend: BackendHandle,
    /// Bumped on every backend (re)start, so events from a dead backend are ignored.
    pub backend_epoch: u64,
    pub focus: FocusHandle,
    pub screen: Screen,
    pub show_about: bool,
    pub show_licences: bool,

    // Shared by the domains: who is signed in and what is open.
    pub username: String,
    pub instance: String,
    pub guilds: Vec<Guild>,
    /// `None` shows direct messages.
    pub selected_guild: Option<String>,
    pub channels: Vec<Channel>,
    pub selected_channel: Option<String>,
    /// What the signed-in account knows (guilds, channels, members, read state and the
    /// message windows), fed with gateway events and REST pages. `None` until READY.
    pub account: Option<Entity<AccountState>>,

    // View state, one per domain. The empty ones are slots for later tasks.
    pub sign_in: ui::accounts::SignInState,
    pub sidebar: ui::sidebar::SidebarState,
    pub chat: ui::chat::ChatState,
    pub composer: ui::composer::ComposerState,
    pub overlays: ui::overlays::OverlayState,
    pub media: ui::media::State,
    pub members: ui::members::State,
    pub profile: ui::profile::State,
    #[allow(dead_code)] // filled by the task that builds the domain
    pub social: ui::social::State,
    #[allow(dead_code)] // filled by the task that builds the domain
    pub search: ui::search::State,
    pub inbox: ui::inbox::State,
    pub threads: ui::threads::State,
    #[allow(dead_code)] // filled by the task that builds the domain
    pub voice: ui::voice::State,
    #[allow(dead_code)] // filled by the task that builds the domain
    pub notifications: ui::notifications::State,
    #[allow(dead_code)] // filled by the task that builds the domain
    pub settings: ui::settings::State,
    /// The signed-in accounts and the account switcher.
    pub switcher: ui::overlays::account_switcher::Switcher,

    /// Demo screenshots: the channel to open once the guild's channels arrive.
    #[cfg(feature = "demo")]
    pub demo_channel: Option<String>,

    _subscriptions: Vec<Subscription>,
}

/// The running backend, for the window-close handler.
pub struct CurrentBackend(pub BackendHandle);
impl gpui::Global for CurrentBackend {}

impl Inlet {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::new_with(window, cx, backend::start)
    }

    /// [`Inlet::new`] with the backend `start` gives (the system's in the app, one with
    /// test accounts in tests).
    pub(crate) fn new_with(
        window: &mut Window,
        cx: &mut Context<Self>,
        start: impl FnOnce(futures::channel::mpsc::UnboundedSender<Event>) -> BackendHandle,
    ) -> Self {
        Self::build(window, cx, move |window, cx| {
            Self::spawn_backend_with(0, true, window, cx, start).0
        })
    }

    /// The view without a backend thread: commands go nowhere. The layout tests feed
    /// it its events themselves.
    #[cfg(feature = "demo")]
    pub(crate) fn detached(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::build(window, cx, |_, _| BackendHandle::detached())
    }

    /// The view, with the backend `backend` gives once the domains' state exists.
    fn build(
        window: &mut Window,
        cx: &mut Context<Self>,
        backend: impl FnOnce(&mut Window, &mut Context<Self>) -> BackendHandle,
    ) -> Self {
        let mut subscriptions = Vec::new();
        let sign_in = ui::accounts::SignInState::new(window, cx, &mut subscriptions);
        let composer = ui::composer::ComposerState::new(window, cx, &mut subscriptions);
        // Whether the window has focus, for the once-a-second read check; getting focus
        // again reads the open channel at once, if it is at the bottom.
        subscriptions.push(cx.observe_window_activation(window, |this, window, cx| {
            let active = window.is_window_active();
            this.notifications.window_active = active;
            if active {
                let at_bottom = this.chat.message_list.is_following_tail();
                this.read_if_watched(true, at_bottom, cx);
                // Coming back to the window counts as input.
                this.note_input(cx);
            }
            // Animations stop without the focus and go on with it: draw once now.
            cx.notify();
        }));

        Self::watch_reduce_motion(cx);
        let backend = backend(window, cx);
        let notifications = ui::notifications::State::new(window.is_window_active());
        Self {
            backend,
            backend_epoch: 0,
            focus: cx.focus_handle(),
            screen: Screen::Starting { instance: None },
            show_about: false,
            show_licences: false,
            username: String::new(),
            instance: String::new(),
            guilds: Vec::new(),
            selected_guild: None,
            channels: Vec::new(),
            selected_channel: None,
            account: None,
            sign_in,
            sidebar: Default::default(),
            chat: ui::chat::ChatState::new(),
            composer,
            overlays: Default::default(),
            media: ui::media::State::with_settings(),
            members: Default::default(),
            profile: Default::default(),
            social: Default::default(),
            search: Default::default(),
            inbox: Default::default(),
            threads: Default::default(),
            voice: Default::default(),
            notifications,
            settings: Default::default(),
            switcher: Default::default(),
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
        Self::spawn_backend_with(epoch, restore, window, cx, backend::start)
    }

    fn spawn_backend_with(
        epoch: u64,
        restore: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
        start: impl FnOnce(futures::channel::mpsc::UnboundedSender<Event>) -> BackendHandle,
    ) -> (BackendHandle, u64) {
        let (events_tx, mut events_rx) = futures::channel::mpsc::unbounded();
        let backend = start(events_tx);
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
            backend.send(accounts::Cmd::Restore);
        }
        (backend, epoch)
    }

    pub fn restart_backend(&mut self, restore: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.backend_epoch += 1;
        let (backend, _) = Self::spawn_backend(self.backend_epoch, restore, window, cx);
        self.backend = backend;
        self.reset_main();
        self.screen = Screen::Starting { instance: None };
        cx.notify();
    }

    /// Hands a backend event to the domain it belongs to.
    pub(crate) fn handle_event(
        &mut self,
        event: Event,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            Event::Accounts(ev) => self.on_accounts_event(ev, window, cx),
            Event::Messages(ev) => self.on_messages_event(ev, window, cx),
            Event::Composer(ev) => self.on_composer_event(ev, window, cx),
            Event::Media(ev) => self.on_media_event(ev, window, cx),
            Event::Social(ev) => self.on_social_event(ev, window, cx),
            Event::Settings(ev) => self.on_settings_event(ev, window, cx),
            Event::Search(ev) => self.on_search_event(ev, window, cx),
            Event::Notifications(ev) => self.on_notifications_event(ev, window, cx),
            Event::Voice(ev) => self.on_voice_event(ev, window, cx),
            Event::Platform(ev) => self.on_platform_event(ev, window, cx),
            Event::Tagged { account, event } => self.on_tagged_event(account, *event, window, cx),
        }
        cx.notify();
    }

    /// Events from the platform integration (later starts, tray, deep links,
    /// notifications clicked); handled in `desktop`.
    pub(crate) fn on_platform_event(
        &mut self,
        event: backend::platform::Ev,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.on_desktop_event(event, window, cx);
    }

    /// Back to the sign-in screen with nothing of the old session on screen.
    pub fn reset_main(&mut self) {
        self.screen = Screen::SignIn;
        self.sign_in.busy = false;
        self.sign_in.handoff = None;
        // A new session asks again for the account ID and limit.
        self.composer.reset_session();
        self.overlays.pending_link = None;
        self.overlays.link_prompt = None;
        self.overlays.link_channel = None;
        self.guilds.clear();
        self.channels.clear();
        self.selected_guild = None;
        self.selected_channel = None;
        self.reset_messages();
        self.inbox.clear();
        self.media.reset();
        self.notifications.clear();
        self.switcher.signed_out();
        self.backend.set_account(None);
        self.chat.status.clear();
        self.chat.error = None;
        self.chat.notice = None;
        self.chat.gateway_down = None;
    }
}

impl Focusable for Inlet {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
