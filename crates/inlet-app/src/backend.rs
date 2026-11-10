// SPDX-License-Identifier: AGPL-3.0-or-later
//! The bridge between the UI and the network.
//!
//! The UI runs on gpui's main thread. Everything that talks to a Fluxer instance runs
//! on a background thread with its own tokio runtime. The UI sends [`Command`]s, the
//! backend answers with [`Event`]s.
//!
//! The backend loop only holds state and starts tasks; it never waits on the network
//! itself. Each REST call runs in its own task with a clone of the session, so a slow
//! request never holds up sign-out, channel switches or the end of a session. Opening a
//! channel or guild aborts the previous open. Task panics are caught and reported.
//!
//! The token never travels to the UI. It goes from the browser handoff (or the paste
//! field) to the keychain and the protocol session, nowhere else.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::channel::mpsc::UnboundedSender;
use inlet_protocol as p;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender as TokioSender, unbounded_channel};
use tokio::task::AbortHandle;

use crate::model::{Channel, Guild, Message};
use crate::real::{self, ChannelCache, Credential, Failure, FailureKind};
use crate::token_store;

/// Default instance shown on the sign-in screen.
pub const DEFAULT_INSTANCE: &str = "fluxer.opland.net";

pub enum Command {
    /// Look for a saved session at start-up.
    Restore,
    /// Offline: try to connect now instead of waiting for the next retry.
    RetryNow,
    /// Stop connecting with the saved session and show sign-in. The token stays stored.
    CancelStartup,
    SignInBrowser { instance: String },
    SignInToken { instance: String, token: p::Token },
    /// Stop a browser sign-in that is waiting. Also tells the server to drop the code.
    CancelSignIn,
    /// The account shown after sign-in is the right one: store the token and continue.
    ConfirmAccount,
    /// Wrong account: end the new token on the server and discard it.
    RejectAccount,
    SignOut,
    /// Load channels for a guild, or DMs when `guild_id` is `None`.
    OpenGuild { guild_id: Option<String> },
    OpenChannel { channel_id: String },
    Send { channel_id: String, text: String },
}

pub enum Event {
    /// Connecting with a saved session.
    Starting { instance: String },
    /// The saved session could not connect (network, server error). It is kept, and the
    /// backend tries again after `retry_in_secs`.
    Offline { instance: String, message: String, retry_in_secs: u64 },
    /// No saved session; show the sign-in screen.
    NeedsSignIn { instance: Option<String> },
    /// Browser sign-in started: the page to open and the code to enter there.
    HandoffStarted { url: String, code: String },
    SignInFailed(String),
    /// A new sign-in worked; show which account it is before anything is stored.
    ConfirmAccount { display_name: String, account: String, instance: String },
    /// Something to tell the person on the sign-in screen (not an error).
    Notice(String),
    SignedIn { username: String, instance: String },
    SignedOut,
    /// The session ended on the server side (token rejected or revoked); show sign-in.
    SessionEnded(String),
    Guilds(Vec<Guild>),
    Channels { guild_id: Option<String>, channels: Vec<Channel> },
    Messages { channel_id: String, messages: Vec<Message> },
    MessageCreated(Message),
    MessageUpdated(Message),
    MessageDeleted { channel_id: String, message_id: String },
    /// The gateway identified again after a drop; reload what is on screen.
    Reconnected,
    /// Connection state for the status line ("Connected", "Reconnecting…").
    Status(String),
    /// Something failed that the user should see.
    Error(String),
}

#[derive(Clone)]
pub struct BackendHandle {
    tx: TokioSender<Command>,
}

impl BackendHandle {
    /// Sends a command. If the backend thread has died, the UI notices because the
    /// event channel closes; nothing to do here.
    pub fn send(&self, command: Command) {
        let _ = self.tx.send(command);
    }
}

/// Starts the backend thread. Events arrive on `events`. When the thread ends (a panic in
/// the loop itself), `events` is dropped and the UI's receiver ends.
pub fn start(events: UnboundedSender<Event>) -> BackendHandle {
    let (tx, rx) = unbounded_channel();
    std::thread::Builder::new()
        .name("inlet-backend".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("tokio runtime");
            runtime.block_on(Backend::new(events).run(rx));
        })
        .expect("backend thread");
    BackendHandle { tx }
}

/// Messages from tasks back to the loop.
enum Internal {
    Connected {
        attempt: u64,
        mode: Mode,
        instance: String,
        credential: Credential,
        result: Result<real::Connected, Failure>,
    },
    HandoffDone { id: u64, result: Result<(p::Instance, p::SignedIn), String> },
    /// The session `gen` ended for good (auth failure).
    Ended { gen: u64, message: String },
    /// The gateway of session `gen` stopped without an auth failure.
    GatewayStopped { gen: u64 },
    Retry { attempt: u64 },
    TaskPanicked { what: &'static str, gateway_of: Option<u64> },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    /// The token came from the keychain.
    Restore,
    /// The token was just issued or pasted.
    Fresh,
}

/// Whether a message from session generation `msg_gen` still applies.
pub fn same_session(active: Option<u64>, msg_gen: u64) -> bool {
    active == Some(msg_gen)
}

/// Seconds to wait before retry number `failures` (1-based): 2, 4, 8 … 60.
pub fn backoff_secs(failures: u32) -> u64 {
    2u64.saturating_pow(failures.clamp(1, 6)).min(60)
}

struct RealActive {
    gen: u64,
    session: p::Session,
    gateway: p::GatewayHandle,
    forward: AbortHandle,
    guilds: Arc<Mutex<Vec<Guild>>>,
    cache: ChannelCache,
    active_guild: Option<String>,
}

enum Active {
    Real(RealActive),
    #[cfg(feature = "demo")]
    Demo(crate::mock::MockSession),
}

struct Offline {
    instance: String,
    credential: Credential,
    failures: u32,
    timer: Option<AbortHandle>,
}

struct Backend {
    events: UnboundedSender<Event>,
    internal: TokioSender<Internal>,
    next_gen: u64,
    active: Option<Active>,
    attempt: u64,
    connecting: Option<(u64, AbortHandle)>,
    offline: Option<Offline>,
    handoff_seq: u64,
    handoff: Option<(u64, tokio::sync::oneshot::Sender<()>)>,
    /// A new sign-in waiting for the person to confirm the account.
    pending_confirm: Option<real::Connected>,
    open_guild: Option<AbortHandle>,
    open_channel: Option<AbortHandle>,
}

impl Backend {
    fn new(events: UnboundedSender<Event>) -> Self {
        let (internal, _) = unbounded_channel();
        Self {
            events,
            internal,
            next_gen: 1,
            active: None,
            attempt: 0,
            connecting: None,
            offline: None,
            handoff_seq: 0,
            handoff: None,
            pending_confirm: None,
            open_guild: None,
            open_channel: None,
        }
    }

    fn emit(&self, event: Event) {
        let _ = self.events.unbounded_send(event);
    }

    fn active_gen(&self) -> Option<u64> {
        match &self.active {
            Some(Active::Real(r)) => Some(r.gen),
            _ => None,
        }
    }

    /// Spawns a task and reports a panic in it to the loop.
    fn spawn_guarded(
        &self,
        what: &'static str,
        gateway_of: Option<u64>,
        fut: impl Future<Output = ()> + Send + 'static,
    ) -> AbortHandle {
        let task = tokio::spawn(fut);
        let abort = task.abort_handle();
        let internal = self.internal.clone();
        tokio::spawn(async move {
            if let Err(err) = task.await {
                if err.is_panic() {
                    log::error!("backend task {what} panicked");
                    let _ = internal.send(Internal::TaskPanicked { what, gateway_of });
                }
            }
        });
        abort
    }

    async fn run(mut self, mut rx: UnboundedReceiver<Command>) {
        let (internal, mut internal_rx) = unbounded_channel();
        self.internal = internal;
        loop {
            tokio::select! {
                // Internal results first: session end must not wait behind commands.
                biased;
                Some(msg) = internal_rx.recv() => self.internal(msg),
                command = rx.recv() => match command {
                    Some(command) => self.handle(command),
                    None => break,
                },
            }
        }
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::Restore => self.restore(),
            Command::RetryNow => {
                if let Some(off) = &mut self.offline {
                    if let Some(timer) = off.timer.take() {
                        timer.abort();
                    }
                    let (instance, credential) = (off.instance.clone(), off.credential.clone());
                    self.start_connect(instance, None, credential, Mode::Restore);
                }
            }
            Command::CancelStartup => {
                self.stop_connecting();
                let instance = self.offline.take().map(|o| {
                    if let Some(t) = o.timer {
                        t.abort();
                    }
                    o.instance
                });
                self.emit(Event::NeedsSignIn { instance });
            }
            Command::SignInBrowser { instance } => self.start_handoff(instance),
            Command::SignInToken { instance, token } => {
                self.offline = None;
                self.start_connect(instance, None, Credential::Pasted(token), Mode::Fresh);
            }
            Command::CancelSignIn => {
                if let Some((_, cancel)) = self.handoff.take() {
                    let _ = cancel.send(());
                }
                self.stop_connecting();
                self.drop_pending_confirm();
            }
            Command::ConfirmAccount => {
                if let Some(connected) = self.pending_confirm.take() {
                    self.install(connected, true);
                }
            }
            Command::RejectAccount => {
                if let Some(connected) = self.pending_confirm.take() {
                    let instance = connected.session.instance().origin().to_string();
                    let events = self.events.clone();
                    self.spawn_guarded("reject account", None, async move {
                        if !real::revoke(&connected.session).await {
                            let _ = events.unbounded_send(Event::Notice(
                                "The instance could not be told to end that session. End it under Devices in Fluxer's settings."
                                    .into(),
                            ));
                        }
                    });
                    self.emit(Event::NeedsSignIn { instance: Some(instance) });
                }
            }
            Command::SignOut => self.sign_out(),
            Command::OpenGuild { guild_id } => self.open_guild(guild_id),
            Command::OpenChannel { channel_id } => self.open_channel(channel_id),
            Command::Send { channel_id, text } => self.send(channel_id, text),
        }
    }

    fn restore(&mut self) {
        #[cfg(feature = "demo")]
        if std::env::var_os("INLET_DEMO").is_some() {
            self.active = Some(Active::Demo(crate::mock::MockSession::start(self.events.clone())));
            return;
        }
        let events = self.events.clone();
        let internal = self.internal.clone();
        self.attempt += 1;
        let attempt = self.attempt;
        let task = self.spawn_guarded("restore", None, async move {
            let stored = tokio::task::spawn_blocking(|| {
                let instance = token_store::last_instance()?;
                match token_store::load_session(&instance) {
                    Ok(stored) => Some((instance, stored)),
                    Err(err) => {
                        log::warn!("keychain read failed: {err:#}");
                        Some((instance, None))
                    }
                }
            })
            .await
            .ok()
            .flatten();
            match stored {
                Some((instance, Some(token_store::StoredSession { pins: None, .. }))) => {
                    // Written before endpoint pinning: never start without pins.
                    let inst = instance.clone();
                    let _ = tokio::task::spawn_blocking(move || token_store::delete_token(&inst)).await;
                    let _ = events.unbounded_send(Event::NeedsSignIn { instance: Some(instance) });
                    let _ = events.unbounded_send(Event::SignInFailed(
                        "Please sign in again once: Inlet now also stores which server addresses your session belongs to."
                            .into(),
                    ));
                }
                Some((instance, Some(token_store::StoredSession { token, pins: Some(pins) }))) => {
                    let _ = events.unbounded_send(Event::Starting { instance: instance.clone() });
                    let credential = Credential::Stored { token, pins };
                    let result = connect_task(&instance, None, credential.clone()).await;
                    let _ = internal.send(Internal::Connected {
                        attempt,
                        mode: Mode::Restore,
                        instance,
                        credential,
                        result,
                    });
                }
                Some((instance, None)) => {
                    let _ = events.unbounded_send(Event::NeedsSignIn { instance: Some(instance) });
                }
                None => {
                    let _ = events.unbounded_send(Event::NeedsSignIn { instance: None });
                }
            }
        });
        self.connecting = Some((attempt, task));
    }

    fn stop_connecting(&mut self) {
        if let Some((_, task)) = self.connecting.take() {
            task.abort();
        }
    }

    fn start_connect(&mut self, instance: String, discovered: Option<p::Instance>, credential: Credential, mode: Mode) {
        self.stop_connecting();
        self.attempt += 1;
        let attempt = self.attempt;
        if mode == Mode::Restore {
            self.emit(Event::Starting { instance: instance.clone() });
        }
        let internal = self.internal.clone();
        let task = self.spawn_guarded("connect", None, async move {
            let result = connect_task(&instance, discovered, credential.clone()).await;
            let _ = internal.send(Internal::Connected { attempt, mode, instance, credential, result });
        });
        self.connecting = Some((attempt, task));
    }

    fn start_handoff(&mut self, instance: String) {
        if let Some((_, cancel)) = self.handoff.take() {
            let _ = cancel.send(());
        }
        self.handoff_seq += 1;
        let id = self.handoff_seq;
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
        self.handoff = Some((id, cancel_tx));
        let events = self.events.clone();
        let internal = self.internal.clone();
        self.spawn_guarded("handoff", None, async move {
            let started = match real::discover(&instance).await {
                Ok(inst) => p::Handoff::start(&inst).await.map(|h| (inst, h)).map_err(Failure::from),
                Err(err) => Err(err),
            };
            let (inst, handoff) = match started {
                Ok(v) => v,
                Err(err) => {
                    let _ = internal.send(Internal::HandoffDone { id, result: Err(err.message) });
                    return;
                }
            };
            let _ = events.unbounded_send(Event::HandoffStarted { url: handoff.browser_url(), code: handoff.display_code() });
            if let Some(result) = real::run_handoff(inst, handoff, cancel_rx).await {
                let _ = internal.send(Internal::HandoffDone { id, result });
            }
        });
    }

    fn internal(&mut self, msg: Internal) {
        match msg {
            Internal::Connected { attempt, mode, instance, credential, result } => {
                if self.connecting.as_ref().map(|(a, _)| *a) != Some(attempt) {
                    // Cancelled meanwhile. A fresh token nobody will use is ended on the server.
                    if mode == Mode::Fresh {
                        if let Ok(c) = result {
                            self.spawn_guarded("revoke", None, async move {
                                real::revoke(&c.session).await;
                            });
                        }
                    }
                    return;
                }
                self.connecting = None;
                match result {
                    Ok(connected) if mode == Mode::Fresh => {
                        self.emit(Event::ConfirmAccount {
                            display_name: connected.username.clone(),
                            account: connected.account.clone(),
                            instance: connected.session.instance().origin().to_string(),
                        });
                        self.pending_confirm = Some(connected);
                    }
                    Ok(connected) => self.install(connected, false),
                    Err(failure) => self.connect_failed(failure, mode, instance, credential),
                }
            }
            Internal::HandoffDone { id, result } => {
                let current = self.handoff.as_ref().map(|(i, _)| *i) == Some(id);
                match result {
                    Ok((inst, signed_in)) if current => {
                        self.handoff = None;
                        self.offline = None;
                        let name = inst.origin().to_string();
                        self.start_connect(name, Some(inst), Credential::SignedIn(signed_in), Mode::Fresh);
                    }
                    Ok((inst, signed_in)) => {
                        // Cancelled after the browser approved: end that token again.
                        self.spawn_guarded("revoke", None, async move {
                            real::revoke(&p::Session::from_signed_in(inst, signed_in)).await;
                        });
                    }
                    Err(message) if current => {
                        self.handoff = None;
                        self.emit(Event::SignInFailed(message));
                    }
                    Err(_) => {}
                }
            }
            Internal::Ended { gen, message } => {
                if !same_session(self.active_gen(), gen) {
                    return;
                }
                if let Some(Active::Real(r)) = self.active.take() {
                    self.teardown(&r);
                    let instance = r.session.instance().origin().to_string();
                    let events = self.events.clone();
                    self.spawn_guarded("forget token", None, async move {
                        let deleted = tokio::task::spawn_blocking(move || token_store::delete_token(&instance))
                            .await
                            .map(|r| r.is_ok())
                            .unwrap_or(false);
                        let message = if deleted {
                            message
                        } else {
                            format!("{message} The old token could not be removed from the system keychain.")
                        };
                        let _ = events.unbounded_send(Event::SessionEnded(message));
                    });
                }
            }
            Internal::GatewayStopped { gen } => {
                if same_session(self.active_gen(), gen) {
                    self.emit(Event::Status("Disconnected, reconnecting\u{2026}".into()));
                    self.restart_gateway();
                }
            }
            Internal::Retry { attempt } => {
                if let Some(off) = &self.offline {
                    if self.attempt == attempt {
                        let (instance, credential) = (off.instance.clone(), off.credential.clone());
                        self.start_connect(instance, None, credential, Mode::Restore);
                    }
                }
            }
            Internal::TaskPanicked { what, gateway_of } => {
                self.emit(Event::Error(format!("Part of Inlet stopped unexpectedly ({what}). Try again.")));
                if let Some(gen) = gateway_of {
                    if same_session(self.active_gen(), gen) {
                        self.restart_gateway();
                    }
                }
                if what == "connect" || what == "restore" {
                    self.connecting = None;
                    self.emit(Event::NeedsSignIn { instance: None });
                }
            }
        }
    }

    fn connect_failed(&mut self, failure: Failure, mode: Mode, instance: String, credential: Credential) {
        match (mode, failure.kind) {
            (Mode::Restore, FailureKind::SessionEnded) => {
                self.offline = None;
                let inst = instance.clone();
                self.spawn_guarded("forget token", None, async move {
                    let _ = tokio::task::spawn_blocking(move || token_store::delete_token(&inst)).await;
                });
                self.emit(Event::NeedsSignIn { instance: Some(instance) });
                self.emit(Event::SignInFailed(failure.message));
            }
            (Mode::Restore, _) => {
                // Not an auth failure: keep the token and try again later.
                let failures = self.offline.as_ref().map(|o| o.failures + 1).unwrap_or(1);
                let wait = backoff_secs(failures);
                self.attempt += 1;
                let attempt = self.attempt;
                let internal = self.internal.clone();
                let timer = self.spawn_guarded("retry timer", None, async move {
                    tokio::time::sleep(Duration::from_secs(wait)).await;
                    let _ = internal.send(Internal::Retry { attempt });
                });
                self.offline = Some(Offline { instance: instance.clone(), credential, failures, timer: Some(timer) });
                self.emit(Event::Offline { instance, message: failure.message, retry_in_secs: wait });
            }
            (Mode::Fresh, _) => self.emit(Event::SignInFailed(failure.message)),
        }
    }

    /// Makes a checked session the active one: starts the gateway and, for a confirmed
    /// new sign-in, stores the token with its endpoint pins, ending an older stored token
    /// for the same instance.
    fn install(&mut self, connected: real::Connected, save: bool) {
        if let Some(Active::Real(old)) = self.active.take() {
            self.teardown(&old);
        }
        self.offline = None;
        let gen = self.next_gen;
        self.next_gen += 1;
        let session = connected.session;
        let instance = session.instance().origin().to_string();
        let guilds = Arc::new(Mutex::new(connected.guilds.clone()));
        let cache: ChannelCache = Arc::default();
        let (gateway, forward) = self.spawn_gateway(gen, &session, &guilds, &cache);

        self.emit(Event::SignedIn { username: connected.username, instance: instance.clone() });
        self.emit(Event::Guilds(connected.guilds));

        if save {
            let token = session.token().clone();
            let pins = session.pins();
            let old_instance = session.instance().clone();
            let events = self.events.clone();
            self.spawn_guarded("store token", None, async move {
                let inst = instance.clone();
                let stored = tokio::task::spawn_blocking(move || {
                    let old = token_store::load_session(&inst).ok().flatten().and_then(|s| Some((s.token, s.pins?)));
                    let saved = token_store::save_session(&inst, &token, &pins);
                    if saved.is_ok() {
                        token_store::set_last_instance(&inst);
                    }
                    (old.filter(|(o, _)| o != &token), saved)
                })
                .await;
                match stored {
                    Ok((old, Ok(()))) => {
                        // An older token for this instance would otherwise live on forever.
                        if let Some((old, old_pins)) = old {
                            if let Ok(old_session) = p::Session::from_token_pinned(old_instance, old, &old_pins) {
                                real::revoke(&old_session).await;
                            }
                        }
                    }
                    _ => {
                        let _ = events.unbounded_send(Event::Error(
                            "Signed in, but the system keychain refused the token. You will have to sign in again next time."
                                .into(),
                        ));
                    }
                }
            });
        }

        self.active = Some(Active::Real(RealActive {
            gen,
            session,
            gateway,
            forward,
            guilds,
            cache,
            active_guild: None,
        }));
    }

    fn spawn_gateway(
        &self,
        gen: u64,
        session: &p::Session,
        guilds: &Arc<Mutex<Vec<Guild>>>,
        cache: &ChannelCache,
    ) -> (p::GatewayHandle, AbortHandle) {
        let (gateway, rx) = session.connect_gateway(p::GatewayOptions::default());
        let events = self.events.clone();
        let internal = self.internal.clone();
        let (guilds, cache) = (guilds.clone(), cache.clone());
        let forward = self.spawn_guarded("gateway", Some(gen), async move {
            match real::forward_gateway(rx, events, guilds, cache).await {
                Some(message) => {
                    let _ = internal.send(Internal::Ended { gen, message });
                }
                None => {
                    let _ = internal.send(Internal::GatewayStopped { gen });
                }
            }
        });
        (gateway, forward)
    }

    fn restart_gateway(&mut self) {
        let Some(Active::Real(r)) = &self.active else { return };
        r.forward.abort();
        r.gateway.close();
        let (gateway, forward) = self.spawn_gateway(r.gen, &r.session, &r.guilds, &r.cache);
        if let Some(guild) = &r.active_guild {
            gateway.set_guild_active(guild, true);
        }
        if let Some(Active::Real(r)) = &mut self.active {
            r.gateway = gateway;
            r.forward = forward;
        }
    }

    fn teardown(&mut self, r: &RealActive) {
        r.gateway.close();
        r.forward.abort();
        for task in [self.open_guild.take(), self.open_channel.take()].into_iter().flatten() {
            task.abort();
        }
    }

    /// Ends an unconfirmed new sign-in on the server and forgets it.
    fn drop_pending_confirm(&mut self) {
        if let Some(connected) = self.pending_confirm.take() {
            self.spawn_guarded("revoke", None, async move {
                real::revoke(&connected.session).await;
            });
        }
    }

    fn sign_out(&mut self) {
        self.stop_connecting();
        self.drop_pending_confirm();
        if let Some((_, cancel)) = self.handoff.take() {
            let _ = cancel.send(());
        }
        let offline = self.offline.take();
        if let Some(t) = offline.as_ref().and_then(|o| o.timer.as_ref()) {
            t.abort();
        }
        let active = self.active.take();
        self.emit(Event::SignedOut);
        let events = self.events.clone();
        match active {
            Some(Active::Real(r)) => {
                // Close the gateway first, so the server's reaction to the revoked token
                // cannot be read as "the session ended" (the generation also changed).
                self.teardown(&r);
                let session = r.session.clone();
                self.spawn_guarded("sign out", None, async move {
                    let told_server = real::revoke(&session).await;
                    let instance = session.instance().origin().to_string();
                    let deleted = tokio::task::spawn_blocking(move || token_store::delete_token(&instance))
                        .await
                        .map(|r| r.is_ok())
                        .unwrap_or(false);
                    notice_sign_out(&events, told_server, deleted);
                });
            }
            #[cfg(feature = "demo")]
            Some(Active::Demo(_)) => {}
            None => {
                if let Some(off) = offline {
                    self.spawn_guarded("sign out", None, async move {
                        let instance = off.instance.clone();
                        let told_server = match real::discover(&off.instance).await {
                            Ok(inst) => match &off.credential {
                                Credential::Stored { token, pins } => {
                                    match p::Session::from_token_pinned(inst, token.clone(), pins) {
                                        Ok(session) => real::revoke(&session).await,
                                        Err(_) => false,
                                    }
                                }
                                _ => false,
                            },
                            Err(_) => false,
                        };
                        let deleted = tokio::task::spawn_blocking(move || token_store::delete_token(&instance))
                            .await
                            .map(|r| r.is_ok())
                            .unwrap_or(false);
                        notice_sign_out(&events, told_server, deleted);
                    });
                }
            }
        }
    }

    fn open_guild(&mut self, guild_id: Option<String>) {
        if let Some(task) = self.open_guild.take() {
            task.abort();
        }
        match &mut self.active {
            #[cfg(feature = "demo")]
            Some(Active::Demo(m)) => m.open_guild(guild_id),
            Some(Active::Real(r)) => {
                if let Some(previous) = r.active_guild.take() {
                    r.gateway.set_guild_active(&previous, false);
                }
                if let Some(id) = &guild_id {
                    // Guilds with more than 250 members are passive until marked active.
                    r.gateway.set_guild_active(id, true);
                    r.active_guild = Some(id.clone());
                }
                let cached = r.cache.lock().unwrap().get(&guild_id).cloned();
                let (session, cache, gen) = (r.session.clone(), r.cache.clone(), r.gen);
                let (events, internal) = (self.events.clone(), self.internal.clone());
                if let Some(channels) = cached {
                    self.emit(Event::Channels { guild_id, channels });
                    return;
                }
                self.open_guild = Some(self.spawn_guarded("open guild", None, async move {
                    let result = match &guild_id {
                        Some(id) => session.client().channels(id).await,
                        None => session.client().dm_channels().await,
                    };
                    match result {
                        Ok(list) => {
                            let channels: Vec<Channel> = list.iter().map(real::to_channel).collect();
                            cache.lock().unwrap().insert(guild_id.clone(), channels.clone());
                            let _ = events.unbounded_send(Event::Channels { guild_id, channels });
                        }
                        Err(err) => report(&events, &internal, gen, "Could not load channels: ", err),
                    }
                }));
            }
            None => {}
        }
    }

    fn open_channel(&mut self, channel_id: String) {
        if let Some(task) = self.open_channel.take() {
            task.abort();
        }
        match &mut self.active {
            #[cfg(feature = "demo")]
            Some(Active::Demo(m)) => m.open_channel(channel_id),
            Some(Active::Real(r)) => {
                let (session, events, internal, gen) =
                    (r.session.clone(), self.events.clone(), self.internal.clone(), r.gen);
                self.open_channel = Some(self.spawn_guarded("open channel", None, async move {
                    match session.client().messages(&channel_id, None, 50).await {
                        Ok(list) => {
                            // The API returns newest first; the view wants oldest first.
                            let mut messages: Vec<Message> = list.iter().map(real::to_message).collect();
                            messages.sort_by(|a, b| real::snowflake_key(&a.id).cmp(&real::snowflake_key(&b.id)));
                            let _ = events.unbounded_send(Event::Messages { channel_id, messages });
                        }
                        Err(err) => report(&events, &internal, gen, "Could not load messages: ", err),
                    }
                }));
            }
            None => {}
        }
    }

    fn send(&mut self, channel_id: String, text: String) {
        match &mut self.active {
            #[cfg(feature = "demo")]
            Some(Active::Demo(m)) => m.send(channel_id, text),
            Some(Active::Real(r)) => {
                let (session, events, internal, gen) =
                    (r.session.clone(), self.events.clone(), self.internal.clone(), r.gen);
                self.spawn_guarded("send", None, async move {
                    let nonce = p::new_nonce();
                    match session.client().send_message(&channel_id, &text, Some(&nonce)).await {
                        Ok(message) => {
                            let _ = events.unbounded_send(Event::MessageCreated(real::to_message(&message)));
                        }
                        Err(err) => report(&events, &internal, gen, "Could not send: ", err),
                    }
                });
            }
            None => {}
        }
    }
}

/// The start-up checks for a token: discovery (unless done already), then `connect`.
async fn connect_task(
    instance: &str,
    discovered: Option<p::Instance>,
    credential: Credential,
) -> Result<real::Connected, Failure> {
    let inst = match discovered {
        Some(inst) => inst,
        None => real::discover(instance).await?,
    };
    real::connect(inst, credential).await
}

/// Sends a REST failure to the UI, or ends the session when the token is gone.
fn report(events: &UnboundedSender<Event>, internal: &TokioSender<Internal>, gen: u64, prefix: &str, err: p::Error) {
    let failure = Failure::from(err);
    if failure.kind == FailureKind::SessionEnded {
        let _ = internal.send(Internal::Ended { gen, message: failure.message });
    } else {
        let _ = events.unbounded_send(Event::Error(format!("{prefix}{}", failure.message)));
    }
}

fn notice_sign_out(events: &UnboundedSender<Event>, told_server: bool, deleted: bool) {
    let mut parts = Vec::new();
    if !told_server {
        parts.push(
            "Signed out here, but the instance could not be told. End the session under Devices in Fluxer's settings.",
        );
    }
    if !deleted {
        parts.push("The token could not be removed from the system keychain.");
    }
    if !parts.is_empty() {
        let _ = events.unbounded_send(Event::Notice(parts.join(" ")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_from_an_old_session_are_ignored() {
        assert!(same_session(Some(3), 3));
        assert!(!same_session(Some(4), 3));
        assert!(!same_session(None, 3));
    }

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff_secs(1), 2);
        assert_eq!(backoff_secs(2), 4);
        assert_eq!(backoff_secs(5), 32);
        assert_eq!(backoff_secs(6), 60);
        assert_eq!(backoff_secs(50), 60);
    }
}
