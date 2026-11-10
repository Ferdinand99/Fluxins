// SPDX-License-Identifier: AGPL-3.0-or-later
//! Accounts: restoring the stored session, signing in (browser handoff or a pasted
//! token), confirming the account, signing out, and the session's life cycle
//! (offline retries, the end of a session, gateway restarts). The token goes from the
//! handoff or the paste field to the keychain and the protocol session, nowhere else.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use inlet_protocol as p;

use super::gateway::GatewayEnd;
use super::messages::{
    self,
    parse::{ChannelCache, SharedNames},
};
use super::session::{self, Credential, Failure, FailureKind};
use super::{
    backoff_secs, same_session, Active, Backend, Internal, Mode, Offline, PendingConfirm,
    RealActive, Ui,
};
use crate::reconnect::{ReconnectPolicy, Stop};
use crate::token_store;

/// Default instance shown on the sign-in screen.
pub const DEFAULT_INSTANCE: &str = "fluxer.opland.net";

/// Commands of the accounts domain: sign-in, sign-out and the session's connection.
pub enum Cmd {
    /// Look for a saved session at start-up.
    Restore,
    /// Offline: try to connect now instead of waiting for the next retry.
    RetryNow,
    /// Stop connecting with the saved session and show sign-in. The token stays stored.
    CancelStartup,
    SignInBrowser {
        instance: String,
    },
    SignInToken {
        instance: String,
        token: p::Token,
    },
    /// Stop a browser sign-in that is waiting. Also tells the server to drop the code.
    CancelSignIn,
    /// The account shown after sign-in is the right one: store the token and continue.
    ConfirmAccount,
    /// Wrong account: end the new token on the server and discard it.
    RejectAccount,
    SignOut,
    /// From the recovery screen: end the stored session on the server (best effort) and
    /// remove it from the keychain, without connecting first.
    ForgetStored,
    /// The gateway stopped and is not retried automatically: try again now.
    Reconnect,
    /// The app is closing: end an unconfirmed browser sign-in, then answer.
    Shutdown(std::sync::mpsc::Sender<()>),
}

/// Events of the accounts domain. `Status`, `Error` and `GatewayDown` are about the
/// whole session, whatever part of it failed.
pub enum Ev {
    /// Connecting with a saved session.
    Starting {
        instance: String,
    },
    /// The saved session could not connect (network, server error). It is kept, and the
    /// backend tries again after `retry_in_secs`.
    Offline {
        instance: String,
        message: String,
        retry_in_secs: u64,
    },
    /// No saved session; show the sign-in screen.
    NeedsSignIn {
        instance: Option<String>,
    },
    /// Browser sign-in started: the page to open and the code to enter there.
    HandoffStarted {
        url: String,
        code: String,
    },
    SignInFailed(String),
    /// A new sign-in worked; show which account it is before anything is stored.
    /// `pasted`: the token came from the paste field (cancelling only forgets it).
    ConfirmAccount {
        display_name: String,
        account: String,
        instance: String,
        pasted: bool,
    },
    /// The gateway stopped and Inlet will not retry on its own; offer "Reconnect".
    GatewayDown(String),
    /// Something to tell the person on the sign-in screen (not an error).
    Notice(String),
    SignedIn {
        username: String,
        instance: String,
    },
    SignedOut,
    /// The session ended on the server side (token rejected or revoked); show sign-in.
    SessionEnded(String),
    /// The gateway identified again after a drop; reload what is on screen.
    Reconnected,
    /// Connection state for the status line ("Connected", "Reconnecting…").
    Status(String),
    /// Something failed that the user should see.
    Error(String),
}

pub(super) fn handle(b: &mut Backend, cmd: Cmd) {
    match cmd {
        Cmd::Restore => b.restore(),
        Cmd::RetryNow => {
            if let Some(off) = &mut b.offline {
                if let Some(timer) = off.timer.take() {
                    timer.abort();
                }
                let (instance, credential) = (off.instance.clone(), off.credential.clone());
                b.start_connect(instance, None, credential, Mode::Restore);
            }
        }
        Cmd::CancelStartup => {
            if b.active.is_some() || b.pending_confirm.is_some() {
                // Already connected: the UI gets SignedIn (or the confirm screen).
                return;
            }
            b.stop_connecting();
            let instance = b.offline.take().map(|o| {
                if let Some(t) = o.timer {
                    t.abort();
                }
                o.instance
            });
            b.emit(Ev::NeedsSignIn { instance });
        }
        Cmd::SignInBrowser { instance } => b.start_handoff(instance),
        Cmd::SignInToken { instance, token } => {
            b.offline = None;
            b.start_connect(instance, None, Credential::Pasted(token), Mode::Fresh);
        }
        Cmd::CancelSignIn => {
            if let Some((_, cancel)) = b.handoff.take() {
                let _ = cancel.send(());
            }
            b.stop_connecting();
            b.drop_pending_confirm();
        }
        Cmd::ConfirmAccount => {
            if let Some(pending) = b.pending_confirm.take() {
                b.install(pending.connected, true, !pending.from_handoff);
            }
        }
        Cmd::RejectAccount => {
            if let Some(pending) = b.pending_confirm.take() {
                let instance = pending.connected.session.instance().origin().to_string();
                if pending.from_handoff {
                    let events = b.events.clone();
                    b.spawn_guarded("reject account", None, async move {
                        if !session::revoke(&pending.connected.session).await {
                            let _ =
                                events.unbounded_send(Ev::Notice(t!("notice-reject-not-revoked")));
                        }
                    });
                }
                b.emit(Ev::NeedsSignIn {
                    instance: Some(instance),
                });
            }
        }
        Cmd::SignOut => b.sign_out(),
        Cmd::ForgetStored => b.forget_stored(),
        Cmd::Reconnect => {
            if let Some(Active::Real(r)) = &mut b.active {
                if let Some(t) = r.restart_timer.take() {
                    t.abort();
                }
                let delay = r.policy.manual(Instant::now());
                let gen = r.gen;
                b.schedule_restart(gen, delay);
            }
        }
        Cmd::Shutdown(reply) => {
            if let Some((_, cancel)) = b.handoff.take() {
                let _ = cancel.send(());
            }
            let pending = b.pending_confirm.take().filter(|p| p.from_handoff);
            tokio::spawn(async move {
                if let Some(pending) = pending {
                    let _ = tokio::time::timeout(
                        Duration::from_secs(3),
                        session::revoke(&pending.connected.session),
                    )
                    .await;
                }
                let _ = reply.send(());
            });
        }
    }
}

impl Backend {
    pub(super) fn restore(&mut self) {
        #[cfg(feature = "demo")]
        if std::env::var_os("INLET_DEMO").is_some() {
            // Lets screenshots show an instance accent, as on a branded instance.
            crate::theme::set_accent(
                std::env::var("INLET_DEMO_ACCENT")
                    .ok()
                    .and_then(|c| crate::theme::parse_hex(&c)),
            );
            self.active = Some(Active::Demo(crate::demo::MockSession::start(
                self.events.clone(),
            )));
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
                    let _ =
                        tokio::task::spawn_blocking(move || token_store::delete_token(&inst)).await;
                    let _ = events.unbounded_send(Ev::NeedsSignIn {
                        instance: Some(instance),
                    });
                    let _ = events.unbounded_send(Ev::SignInFailed(t!("sign-in-again-for-pins")));
                }
                Some((
                    instance,
                    Some(token_store::StoredSession {
                        token,
                        pins: Some(pins),
                        pasted,
                    }),
                )) => {
                    let _ = events.unbounded_send(Ev::Starting {
                        instance: instance.clone(),
                    });
                    let credential = Credential::Stored {
                        token,
                        pins,
                        pasted,
                    };
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
                    let _ = events.unbounded_send(Ev::NeedsSignIn {
                        instance: Some(instance),
                    });
                }
                None => {
                    let _ = events.unbounded_send(Ev::NeedsSignIn { instance: None });
                }
            }
        });
        self.connecting = Some((attempt, task, Mode::Restore));
    }

    /// Stops waiting for a connect. A restore is aborted. A new sign-in is left to finish
    /// in the background so its result arrives as stale and the new token is ended
    /// on the server instead of being lost.
    pub(super) fn stop_connecting(&mut self) {
        if let Some((_, task, mode)) = self.connecting.take() {
            if mode == Mode::Restore {
                task.abort();
            }
        }
    }

    pub(super) fn start_connect(
        &mut self,
        instance: String,
        discovered: Option<p::Instance>,
        credential: Credential,
        mode: Mode,
    ) {
        self.stop_connecting();
        self.attempt += 1;
        let attempt = self.attempt;
        if mode == Mode::Restore {
            self.emit(Ev::Starting {
                instance: instance.clone(),
            });
        }
        let internal = self.internal.clone();
        let task = self.spawn_guarded("connect", None, async move {
            let result = connect_task(&instance, discovered, credential.clone()).await;
            let _ = internal.send(Internal::Connected {
                attempt,
                mode,
                instance,
                credential,
                result,
            });
        });
        self.connecting = Some((attempt, task, mode));
    }

    pub(super) fn start_handoff(&mut self, instance: String) {
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
            let started = match session::discover(&instance).await {
                Ok(inst) => p::Handoff::start(&inst)
                    .await
                    .map(|h| (inst, h))
                    .map_err(Failure::from),
                Err(err) => Err(err),
            };
            let (inst, handoff) = match started {
                Ok(v) => v,
                Err(err) => {
                    let _ = internal.send(Internal::HandoffDone {
                        id,
                        result: Err(err.message),
                    });
                    return;
                }
            };
            let _ = events.unbounded_send(Ev::HandoffStarted {
                url: handoff.browser_url(),
                code: handoff.display_code(),
            });
            if let Some(result) = session::run_handoff(inst, handoff, cancel_rx).await {
                let _ = internal.send(Internal::HandoffDone { id, result });
            }
        });
    }

    pub(super) fn internal(&mut self, msg: Internal) {
        match msg {
            Internal::Connected {
                attempt,
                mode,
                instance,
                credential,
                result,
            } => {
                if self.connecting.as_ref().map(|(a, _, _)| *a) != Some(attempt) {
                    // Cancelled meanwhile. A token Inlet got from the browser is ended on
                    // the server; a pasted one is only forgotten.
                    if let (Mode::Fresh, Credential::SignedIn(_), Ok(c)) =
                        (mode, &credential, result)
                    {
                        self.spawn_guarded("revoke", None, async move {
                            session::revoke(&c.session).await;
                        });
                    }
                    return;
                }
                self.connecting = None;
                match result {
                    Ok(connected) if mode == Mode::Fresh => {
                        let from_handoff = matches!(credential, Credential::SignedIn(_));
                        self.emit(Ev::ConfirmAccount {
                            display_name: connected.username.clone(),
                            account: connected.account.clone(),
                            instance: connected.session.instance().origin().to_string(),
                            pasted: !from_handoff,
                        });
                        self.pending_confirm = Some(PendingConfirm {
                            connected,
                            from_handoff,
                        });
                    }
                    Ok(connected) => {
                        let pasted = credential.is_pasted();
                        // An entry stored before the media pin existed gets it now, right
                        // after `me()` worked, so the media host is pinned from here on.
                        let upgrade = match &credential {
                            Credential::Stored { token, pins, .. } => {
                                upgraded_pins(pins, &connected.session.session_pins())
                                    .map(|pins| (token.clone(), pins))
                            }
                            _ => None,
                        };
                        let origin = connected.session.instance().origin().to_string();
                        self.install(connected, false, pasted);
                        if let Some((token, pins)) = upgrade {
                            self.save_upgraded_pins(origin, token, pins, pasted);
                        }
                    }
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
                        self.start_connect(
                            name,
                            Some(inst),
                            Credential::SignedIn(signed_in),
                            Mode::Fresh,
                        );
                    }
                    Ok((inst, signed_in)) => {
                        // Cancelled after the browser approved: end that token again.
                        self.spawn_guarded("revoke", None, async move {
                            session::revoke(&p::Session::from_signed_in(inst, signed_in)).await;
                        });
                    }
                    Err(message) if current => {
                        self.handoff = None;
                        self.emit(Ev::SignInFailed(message));
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
                        let deleted = tokio::task::spawn_blocking(move || {
                            token_store::delete_token(&instance)
                        })
                        .await
                        .map(|r| r.is_ok())
                        .unwrap_or(false);
                        let message = if deleted {
                            message
                        } else {
                            t!("session-ended-not-deleted", message = message)
                        };
                        let _ = events.unbounded_send(Ev::SessionEnded(message));
                    });
                }
            }
            Internal::GatewayEnded { gen, end } => {
                if !same_session(self.active_gen(), gen) {
                    return;
                }
                let stop = match end {
                    GatewayEnd::SessionEnded(message) => {
                        return self.internal(Internal::Ended { gen, message });
                    }
                    GatewayEnd::Terminal(message) if message.is_empty() => return,
                    GatewayEnd::Terminal(message) => Stop::Terminal(message),
                    GatewayEnd::ChannelClosed => Stop::Unexpected,
                };
                self.gateway_stopped(gen, stop);
            }
            Internal::GatewayConnected { gen } => {
                if let Some(Active::Real(r)) = &mut self.active {
                    if r.gen == gen {
                        r.policy.connected();
                    }
                }
            }
            Internal::GatewayRestart { gen } => {
                if same_session(self.active_gen(), gen) {
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
                self.emit(Ev::Error(t!("error-task-panicked", what = what)));
                if let Some(gen) = gateway_of {
                    if same_session(self.active_gen(), gen) {
                        self.gateway_stopped(gen, Stop::Panicked);
                    }
                }
                if what == "connect" || what == "restore" {
                    self.connecting = None;
                    self.emit(Ev::NeedsSignIn { instance: None });
                }
            }
        }
    }

    pub(super) fn connect_failed(
        &mut self,
        failure: Failure,
        mode: Mode,
        instance: String,
        credential: Credential,
    ) {
        match (mode, failure.kind) {
            (Mode::Restore, FailureKind::SessionEnded) => {
                self.offline = None;
                let inst = instance.clone();
                self.spawn_guarded("forget token", None, async move {
                    let _ =
                        tokio::task::spawn_blocking(move || token_store::delete_token(&inst)).await;
                });
                self.emit(Ev::NeedsSignIn {
                    instance: Some(instance),
                });
                self.emit(Ev::SignInFailed(failure.message));
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
                self.offline = Some(Offline {
                    instance: instance.clone(),
                    credential,
                    failures,
                    timer: Some(timer),
                });
                self.emit(Ev::Offline {
                    instance,
                    message: failure.message,
                    retry_in_secs: wait,
                });
            }
            (Mode::Fresh, _) => self.emit(Ev::SignInFailed(failure.message)),
        }
    }

    /// Makes a checked session the active one: starts the gateway and, for a confirmed
    /// new sign-in, stores the token with its endpoint pins, ending an older stored token
    /// for the same instance.
    pub(super) fn install(&mut self, connected: session::Connected, save: bool, pasted: bool) {
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
        // Instance branding: its accent colour, as Fluxer's own client applies it.
        let accent = session
            .instance()
            .discovery()
            .app_public
            .get("branding")
            .and_then(|b| b.get("theme_color"))
            .and_then(|c| c.as_str())
            .and_then(crate::theme::parse_hex);
        crate::theme::set_accent(accent);
        let names: SharedNames = Arc::default();
        let (gateway, forward) = self.spawn_gateway(gen, &session, &guilds, &cache, &names);
        let mut policy = ReconnectPolicy::default();
        policy.started(Instant::now());

        self.emit(Ev::SignedIn {
            username: connected.username,
            instance: instance.clone(),
        });
        self.emit(messages::Ev::Guilds(connected.guilds));

        if save {
            let token = session.token().clone();
            let pins = session.session_pins();
            let old_instance = session.instance().clone();
            let events = self.events.clone();
            // Not guarded through spawn_guarded: sign-out needs the JoinHandle to wait on.
            self.saving = Some(tokio::spawn(async move {
                let inst = instance.clone();
                let stored = tokio::task::spawn_blocking(move || {
                    // An older pasted token is only forgotten (overwritten), never revoked.
                    let old = token_store::load_session(&inst)
                        .ok()
                        .flatten()
                        .filter(|s| should_revoke(s.pasted))
                        .and_then(|s| Some((s.token, s.pins?)));
                    let saved = token_store::save_session(&inst, &token, &pins, pasted);
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
                            if let Ok(old_session) =
                                p::Session::from_token_session_pinned(old_instance, old, &old_pins)
                            {
                                session::revoke(&old_session).await;
                            }
                        }
                    }
                    _ => {
                        let _ = events.unbounded_send(Ev::Error(t!("error-keychain-refused")));
                    }
                }
            }));
        }

        self.active = Some(Active::Real(RealActive {
            gen,
            pasted,
            session,
            gateway,
            forward,
            guilds,
            cache,
            names,
            active_guild: None,
            policy,
            restart_timer: None,
            settings_writes: Vec::new(),
        }));
    }

    pub(super) fn teardown(&mut self, r: &RealActive) {
        r.gateway.close();
        r.forward.abort();
        if let Some(t) = &r.restart_timer {
            t.abort();
        }
        for t in &r.settings_writes {
            t.abort();
        }
        for task in [self.open_guild.take(), self.open_channel.take()]
            .into_iter()
            .flatten()
        {
            task.abort();
        }
    }

    /// Forgets an unconfirmed new sign-in; a token from the browser is also ended on the
    /// server.
    pub(super) fn drop_pending_confirm(&mut self) {
        if let Some(pending) = self.pending_confirm.take() {
            if pending.from_handoff {
                self.spawn_guarded("revoke", None, async move {
                    session::revoke(&pending.connected.session).await;
                });
            }
        }
    }

    /// Recovery-screen sign-out: no session is running. Ends the stored token on the
    /// server if it can be reached within the timeouts, then removes it.
    pub(super) fn forget_stored(&mut self) {
        let events = self.events.clone();
        self.spawn_guarded("forget stored", None, async move {
            let stored = tokio::task::spawn_blocking(|| {
                let instance = token_store::last_instance()?;
                Some((
                    instance.clone(),
                    token_store::load_session(&instance).ok().flatten(),
                ))
            })
            .await
            .ok()
            .flatten();
            let Some((instance, stored)) = stored else {
                let _ = events.unbounded_send(Ev::NeedsSignIn { instance: None });
                return;
            };
            let mut told_server = false;
            let mut forgotten_only = false;
            if let Some(token_store::StoredSession { pasted: true, .. }) = &stored {
                forgotten_only = true;
            } else if let Some(token_store::StoredSession {
                token,
                pins: Some(pins),
                ..
            }) = stored
            {
                if let Ok(inst) = session::discover(&instance).await {
                    if let Ok(session) = p::Session::from_token_session_pinned(inst, token, &pins) {
                        told_server = session::revoke(&session).await;
                    }
                }
            }
            let inst = instance.clone();
            let deleted = tokio::task::spawn_blocking(move || token_store::delete_token(&inst))
                .await
                .map(|r| r.is_ok())
                .unwrap_or(false);
            let _ = events.unbounded_send(Ev::NeedsSignIn {
                instance: Some(instance),
            });
            if forgotten_only {
                notice_forgotten(&events, deleted);
            } else {
                notice_sign_out(&events, told_server, deleted);
            }
        });
    }

    pub(super) fn sign_out(&mut self) {
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
        crate::theme::set_accent(None);
        self.emit(Ev::SignedOut);
        let events = self.events.clone();
        // A keychain write still running must finish first, or it could store the token
        // this sign-out is about to revoke.
        let saving = self.saving.take();
        match active {
            Some(Active::Real(r)) => {
                // Close the gateway first, so the server's reaction to the revoked token
                // cannot be read as "the session ended" (the generation also changed).
                self.teardown(&r);
                let session = r.session.clone();
                let pasted = r.pasted;
                self.spawn_guarded("sign out", None, async move {
                    if let Some(saving) = saving {
                        let _ = saving.await;
                    }
                    if !should_revoke(pasted) {
                        let instance = session.instance().origin().to_string();
                        let deleted = tokio::task::spawn_blocking(move || {
                            token_store::delete_token(&instance)
                        })
                        .await
                        .map(|r| r.is_ok())
                        .unwrap_or(false);
                        notice_forgotten(&events, deleted);
                        return;
                    }
                    let told_server = session::revoke(&session).await;
                    let instance = session.instance().origin().to_string();
                    let deleted =
                        tokio::task::spawn_blocking(move || token_store::delete_token(&instance))
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
                        if let Some(saving) = saving {
                            let _ = saving.await;
                        }
                        let instance = off.instance.clone();
                        if off.credential.is_pasted() {
                            let deleted = tokio::task::spawn_blocking(move || {
                                token_store::delete_token(&instance)
                            })
                            .await
                            .map(|r| r.is_ok())
                            .unwrap_or(false);
                            notice_forgotten(&events, deleted);
                            return;
                        }
                        let told_server = match session::discover(&off.instance).await {
                            Ok(inst) => match &off.credential {
                                Credential::Stored { token, pins, .. } => {
                                    match p::Session::from_token_session_pinned(
                                        inst,
                                        token.clone(),
                                        pins,
                                    ) {
                                        Ok(session) => session::revoke(&session).await,
                                        Err(_) => false,
                                    }
                                }
                                _ => false,
                            },
                            Err(_) => false,
                        };
                        let deleted = tokio::task::spawn_blocking(move || {
                            token_store::delete_token(&instance)
                        })
                        .await
                        .map(|r| r.is_ok())
                        .unwrap_or(false);
                        notice_sign_out(&events, told_server, deleted);
                    });
                }
            }
        }
    }
}

/// The start-up checks for a token: discovery (unless done already), then `connect`.
pub(super) async fn connect_task(
    instance: &str,
    discovered: Option<p::Instance>,
    credential: Credential,
) -> Result<session::Connected, Failure> {
    let inst = match discovered {
        Some(inst) => inst,
        None => session::discover(instance).await?,
    };
    session::connect(inst, credential).await
}

/// The pins to store for a restored session whose entry has no media host yet: the
/// session's pins when its API and gateway hosts are the stored ones and it has a media
/// host. `None` when there is nothing to upgrade.
pub fn upgraded_pins(stored: &p::SessionPins, now: &p::SessionPins) -> Option<p::SessionPins> {
    let same_hosts = stored.api == now.api && stored.gateway == now.gateway;
    (stored.media.is_none() && now.media.is_some() && same_hosts).then(|| now.clone())
}

impl Backend {
    /// Writes the upgraded pins of a restored session to its keychain entry. Runs as the
    /// `saving` task, so a sign-out waits for it and cannot be undone by it.
    fn save_upgraded_pins(
        &mut self,
        instance: String,
        token: p::Token,
        pins: p::SessionPins,
        pasted: bool,
    ) {
        let previous = self.saving.take();
        self.saving = Some(tokio::spawn(async move {
            if let Some(previous) = previous {
                let _ = previous.await;
            }
            let saved = tokio::task::spawn_blocking(move || {
                token_store::save_session(&instance, &token, &pins, pasted)
            })
            .await;
            if !matches!(saved, Ok(Ok(()))) {
                log::warn!("could not store the media pin with the session");
            }
        }));
    }
}

/// Whether signing out may end a token on the server. A pasted token belongs to another
/// client and is only forgotten here.
pub fn should_revoke(pasted: bool) -> bool {
    !pasted
}

fn notice_forgotten(events: &Ui, deleted: bool) {
    let mut text = t!("notice-pasted-forgotten");
    if !deleted {
        text.push(' ');
        text.push_str(&t!("notice-not-deleted"));
    }
    let _ = events.unbounded_send(Ev::Notice(text));
}

fn notice_sign_out(events: &Ui, told_server: bool, deleted: bool) {
    let mut parts = Vec::new();
    if !told_server {
        parts.push(t!("notice-sign-out-not-revoked"));
    }
    if !deleted {
        parts.push(t!("notice-not-deleted"));
    }
    if !parts.is_empty() {
        let _ = events.unbounded_send(Ev::Notice(parts.join(" ")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restored_entries_without_a_media_pin_are_upgraded_once() {
        let old = p::SessionPins {
            api: "https://fluxer.example".into(),
            gateway: "wss://fluxer.example".into(),
            media: None,
        };
        let now = p::SessionPins {
            media: Some("https://media.fluxer.example".into()),
            ..old.clone()
        };
        // Old entry, instance has a media host: store the full pins.
        assert_eq!(upgraded_pins(&old, &now), Some(now.clone()));
        // Already pinned: nothing to write.
        assert_eq!(upgraded_pins(&now, &now), None);
        // The instance has no media host: nothing to pin.
        assert_eq!(upgraded_pins(&old, &old), None);
        // Other API or gateway host (cannot happen after a pinned connect): never.
        let moved = p::SessionPins {
            api: "https://elsewhere.example".into(),
            ..now.clone()
        };
        assert_eq!(upgraded_pins(&old, &moved), None);
    }

    #[test]
    fn pasted_tokens_are_never_revoked() {
        assert!(!should_revoke(true));
        assert!(should_revoke(false));
        let pins = p::SessionPins {
            api: "https://fluxer.example".into(),
            gateway: "wss://fluxer.example".into(),
            media: None,
        };
        let stored_pasted = Credential::Stored {
            token: p::Token::new("t"),
            pins: pins.clone(),
            pasted: true,
        };
        let stored_issued = Credential::Stored {
            token: p::Token::new("t"),
            pins,
            pasted: false,
        };
        assert!(stored_pasted.is_pasted());
        assert!(!stored_issued.is_pasted());
        assert!(Credential::Pasted(p::Token::new("t")).is_pasted());
    }
}
