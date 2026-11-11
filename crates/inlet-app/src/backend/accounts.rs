// SPDX-License-Identifier: AGPL-3.0-or-later
//! Accounts: restoring the stored accounts, signing in (browser handoff or a pasted
//! token), confirming the account, switching between accounts, signing out, and each
//! session's life cycle (offline retries, the end of a session, gateway restarts). The
//! token goes from the handoff or the paste field to the keychain and the protocol
//! session, nowhere else.
//!
//! Several accounts, on one instance or several, can be signed in at once (at most
//! [`MAX_ACCOUNTS`]). Each has its own session, rate limiter, endpoint pins and gateway.
//! The active account is fully live. The others run in the background and are passive:
//! no active guild (no Lazy Request, op 14), no message windows, no typing events
//! (`ignored_events`), and no REST call unless the person does something with them. Their
//! gateway events still reach their account state in the UI, for unread counts.
//!
//! At start-up the active account connects first (with the token check and the guild
//! list, as before); the others follow two seconds apart, each with discovery and its
//! gateway only. Switching makes a background account active; its gateway starts again
//! once with typing events (a background gateway never asked for them). The account it
//! replaces stays connected in the background.

mod store;
#[cfg(test)]
pub(crate) mod testkit;
#[cfg(test)]
mod tests;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use inlet_protocol as p;
use tokio::task::AbortHandle;

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
pub use store::{
    AccountInfo, AccountKey, Accounts, Legacy, Migrated, Restored, SignedIn, MAX_ACCOUNTS,
};

/// Default instance shown on the sign-in screen.
pub const DEFAULT_INSTANCE: &str = "fluxer.opland.net";

/// Gateway events a background account does not ask for.
pub const BACKGROUND_IGNORED_EVENTS: &[&str] = &["TYPING_START"];

/// Time between the starts of the stored accounts.
pub const STARTUP_STAGGER: Duration = Duration::from_secs(2);

/// Commands of the accounts domain: sign-in, sign-out, switching and the connection.
pub enum Cmd {
    /// Look for the stored accounts at start-up.
    Restore,
    /// Offline: try to connect now instead of waiting for the next retry.
    RetryNow,
    /// Stop connecting with the stored session and show sign-in (or another account
    /// that is connected). The token stays stored.
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
    /// A new account is added next to the ones already signed in and becomes active.
    ConfirmAccount,
    /// Wrong account: end the new token on the server and discard it.
    RejectAccount,
    /// Sign the account the UI shows out (`None`: whichever is starting or offline). An
    /// account that is not the active one is signed out as from the switcher.
    SignOut(Option<AccountKey>),
    /// Sign one account out (from the account switcher).
    RemoveAccount(AccountKey),
    /// Make another signed-in account the active one.
    SwitchAccount(AccountKey),
    /// From the recovery screen: end the active stored session on the server (best
    /// effort) and remove it from the keychain, without connecting first.
    ForgetStored,
    /// The gateway stopped and is not retried automatically: try again now, if this is
    /// still the active account.
    Reconnect(Option<AccountKey>),
    /// The app is closing: end an unconfirmed browser sign-in, then answer.
    Shutdown(std::sync::mpsc::Sender<()>),
}

/// Events of the accounts domain. `Status`, `Error` and `GatewayDown` are about the
/// whole session, whatever part of it failed. A background account's own events
/// arrive tagged with it (`Event::Tagged`); the rest here are untagged.
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
    /// `key` is now the active account (after sign-in, at start-up, or a switch).
    SignedIn {
        key: AccountKey,
        username: String,
        instance: String,
    },
    /// No account is signed in any more.
    SignedOut,
    /// The session ended on the server side (token rejected or revoked) and no other
    /// account is signed in; show sign-in.
    SessionEnded(String),
    /// The stored accounts, in order (after start-up, sign-in and sign-out).
    Accounts(Vec<AccountInfo>),
    /// An account is gone: signed out, or ended by the server (`message`).
    AccountRemoved {
        key: AccountKey,
        message: Option<String>,
    },
    /// The gateway identified again after a drop; reload what is on screen.
    Reconnected,
    /// Connection state for the status line ("Connected", "Reconnecting…").
    Status(String),
    /// Something failed that the user should see.
    Error(String),
}

/// Which stored session a restore is for.
#[derive(Clone, Debug)]
pub enum Restore {
    /// An account from the list.
    Account(AccountKey),
    /// The single entry of an older Inlet, for this origin; moved once its user is known.
    Legacy(String),
}

/// A background start that is waiting or running.
pub struct PendingStart {
    restored: Restored,
    task: AbortHandle,
    /// When the start is due.
    #[cfg_attr(not(test), allow(dead_code))]
    due: tokio::time::Instant,
    /// Failed attempts so far (for the retry spacing).
    failures: u32,
}

/// The start-up order: the active account at once (it is `restored[0]`), the others
/// [`STARTUP_STAGGER`] apart after it.
pub fn startup_plan(restored: Vec<Restored>) -> (Option<Restored>, Vec<(Restored, Duration)>) {
    let mut rest = restored.into_iter();
    let first = rest.next();
    let later = rest
        .enumerate()
        .map(|(i, r)| (r, STARTUP_STAGGER * (i as u32 + 1)))
        .collect();
    (first, later)
}

pub(super) fn handle(b: &mut Backend, cmd: Cmd) {
    match cmd {
        Cmd::Restore => b.restore(),
        Cmd::RetryNow => {
            if let Some(off) = &mut b.offline {
                if let Some(timer) = off.timer.take() {
                    timer.abort();
                }
                let (instance, credential, restore) = (
                    off.instance.clone(),
                    off.credential.clone(),
                    off.restore.clone(),
                );
                b.start_connect(instance, None, credential, Mode::Restore, restore);
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
            if !b.promote_connected() {
                b.tell(Ev::NeedsSignIn { instance });
            }
        }
        Cmd::SignInBrowser { instance } => b.start_handoff(instance),
        Cmd::SignInToken { instance, token } => {
            b.offline = None;
            b.start_connect(instance, None, Credential::Pasted(token), Mode::Fresh, None);
        }
        Cmd::CancelSignIn => {
            if let Some((_, cancel)) = b.handoff.take() {
                let _ = cancel.send(());
            }
            if b.connecting
                .as_ref()
                .is_some_and(|(_, _, m)| *m == Mode::Fresh)
            {
                b.stop_connecting();
            }
            b.drop_pending_confirm();
        }
        Cmd::ConfirmAccount => {
            if let Some(pending) = b.pending_confirm.take() {
                let origin = pending.connected.session.instance().origin().to_string();
                let key = AccountKey::new(&origin, &pending.connected.user_id);
                let known = key
                    .as_ref()
                    .is_some_and(|k| b.known.iter().any(|a| &a.key == k));
                if !known && b.known.len() >= MAX_ACCOUNTS {
                    b.pending_confirm = Some(pending);
                    b.drop_pending_confirm();
                    b.tell(Ev::NeedsSignIn {
                        instance: Some(origin),
                    });
                    b.tell(Ev::SignInFailed(t!(
                        "sign-in-too-many-accounts",
                        max = MAX_ACCOUNTS
                    )));
                    return;
                }
                b.install(pending.connected, true, !pending.from_handoff);
            }
        }
        Cmd::RejectAccount => {
            if let Some(pending) = b.pending_confirm.take() {
                let instance = pending.connected.session.instance().origin().to_string();
                if pending.from_handoff {
                    let events = b.global.clone();
                    b.spawn_guarded("reject account", None, async move {
                        if !session::revoke(&pending.connected.session).await {
                            let _ =
                                events.unbounded_send(Ev::Notice(t!("notice-reject-not-revoked")));
                        }
                    });
                }
                b.tell(Ev::NeedsSignIn {
                    instance: Some(instance),
                });
            }
        }
        Cmd::SignOut(Some(key)) if !b.is_current(&key) => b.remove_account(key),
        Cmd::SignOut(_) => b.sign_out(),
        Cmd::RemoveAccount(key) => b.remove_account(key),
        Cmd::SwitchAccount(key) => b.switch_account(key),
        Cmd::ForgetStored => b.forget_stored(),
        Cmd::Reconnect(key) => {
            if let Some(Active::Real(r)) = &mut b.active {
                if key.as_ref().is_some_and(|k| k != &r.key) {
                    return;
                }
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
    /// Whether `key` is the account the backend is showing: the active one, or (with no
    /// active account) one that is starting or offline rather than in the background.
    fn is_current(&self, key: &AccountKey) -> bool {
        match &self.active {
            Some(Active::Real(r)) => &r.key == key,
            #[cfg(feature = "demo")]
            Some(Active::Demo(_)) => true,
            None => !self.background.iter().any(|r| &r.key == key),
        }
    }

    /// Sends an untagged event of this domain (sign-in, the account list, the switch).
    pub(super) fn tell(&self, ev: Ev) {
        let _ = self.global.unbounded_send(ev);
    }

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
                self.global.clone(),
            )));
            super::media::demo_started(self);
            return;
        }
        let store = self.store.clone();
        let internal = self.internal.clone();
        self.attempt += 1;
        let attempt = self.attempt;
        let task = self.spawn_guarded("restore", None, async move {
            let mut accounts = store.lock().await;
            accounts.restore_all().await;
            let list = accounts.list();
            let (restored, legacy) = accounts.take_restored();
            drop(accounts);
            let last_instance = tokio::task::spawn_blocking(crate::token_store::last_instance)
                .await
                .ok()
                .flatten();
            let _ = internal.send(Internal::Restored {
                attempt,
                restored,
                legacy,
                list,
                last_instance,
            });
        });
        self.connecting = Some((attempt, task, Mode::Restore));
    }

    /// The stored accounts are read: the active one connects now, the others later.
    fn on_restored(
        &mut self,
        restored: Vec<Restored>,
        legacy: Option<Legacy>,
        list: Vec<AccountInfo>,
        last_instance: Option<String>,
    ) {
        self.connecting = None;
        self.set_known(list);
        // An older Inlet's entry that is not moved yet connects first: its user is only
        // known after `me()`, and then it is moved. The listed accounts follow in the
        // background.
        let (first, later) = match &legacy {
            Some(_) => (
                None,
                restored
                    .into_iter()
                    .enumerate()
                    .map(|(i, r)| (r, STARTUP_STAGGER * (i as u32 + 1)))
                    .collect(),
            ),
            None => startup_plan(restored),
        };
        for (r, delay) in later {
            self.start_background(r, delay, 0);
        }
        match (first, legacy) {
            // Already running (start-up asked for again): nothing to start.
            (Some(r), _) if self.engine_of(&r.key).is_some() => {}
            (Some(r), _) => {
                let instance = r.key.origin.to_string();
                let credential = Credential::Stored {
                    token: r.token,
                    pins: r.pins,
                    pasted: r.pasted,
                };
                let restore = Some(Restore::Account(r.key));
                self.start_connect(instance, None, credential, Mode::Restore, restore);
            }
            (None, Some(l)) => {
                let credential = Credential::Stored {
                    token: l.token,
                    pins: l.pins,
                    pasted: l.pasted,
                };
                let restore = Some(Restore::Legacy(l.origin.clone()));
                self.start_connect(l.origin, None, credential, Mode::Restore, restore);
            }
            (None, None) => self.tell(Ev::NeedsSignIn {
                instance: last_instance,
            }),
        }
    }

    fn set_known(&mut self, list: Vec<AccountInfo>) {
        self.known = list.clone();
        self.tell(Ev::Accounts(list));
    }

    /// Runs a task on the stored accounts after every store task started before it, so
    /// keychain and list writes happen in the order the backend asked for them (a
    /// sign-out after a restore removes what the restore stored, never the other way).
    fn store_job(&mut self, job: impl std::future::Future<Output = ()> + Send + 'static) {
        let previous = self.store_tail.take();
        self.store_tail = Some(tokio::spawn(async move {
            if let Some(previous) = previous {
                let _ = previous.await;
            }
            job.await;
        }));
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
        restore: Option<Restore>,
    ) {
        self.stop_connecting();
        self.attempt += 1;
        let attempt = self.attempt;
        if mode == Mode::Restore {
            self.tell(Ev::Starting {
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
                restore,
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
        let events = self.global.clone();
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
            Internal::Restored {
                attempt,
                restored,
                legacy,
                list,
                last_instance,
            } => {
                if self.connecting.as_ref().map(|(a, _, _)| *a) == Some(attempt) {
                    self.on_restored(restored, legacy, list, last_instance);
                }
            }
            Internal::ListChanged(list) => self.set_known(list),
            Internal::Connected {
                attempt,
                mode,
                instance,
                credential,
                restore,
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
                        self.tell(Ev::ConfirmAccount {
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
                    Ok(connected) => self.restored_connected(connected, credential, restore),
                    Err(failure) => {
                        self.connect_failed(failure, mode, instance, credential, restore)
                    }
                }
            }
            Internal::BackgroundReady { key, result } => {
                let Some(start) = self.starts.remove(&key) else {
                    // Switched to, signed out or replaced meanwhile.
                    return;
                };
                match result {
                    Ok(session) => self.install_background(start.restored, session),
                    Err(failure) if failure.kind == FailureKind::SessionEnded => {
                        self.forget_account(key.clone(), false);
                        self.tell(Ev::AccountRemoved {
                            key,
                            message: Some(failure.message),
                        });
                    }
                    Err(_) => {
                        let failures = start.failures + 1;
                        let delay = Duration::from_secs(backoff_secs(failures));
                        self.start_background(start.restored, delay, failures);
                    }
                }
            }
            Internal::ForegroundLoaded(restored) => match restored {
                // Only the account last switched to (a later switch wins).
                Some(r)
                    if self.active.is_none()
                        && self.connecting.is_none()
                        && self.switch_target.as_ref() == Some(&r.key) =>
                {
                    self.start_foreground(r)
                }
                Some(_) => {}
                None => self.tell(Ev::NeedsSignIn { instance: None }),
            },
            Internal::NewerEntry {
                key,
                old,
                old_pasted,
            } => {
                let was_active = matches!(&self.active, Some(Active::Real(r)) if r.key == key);
                // Its gateway stops first, so ending the older token cannot read as the end
                // of the account (that would remove the newer entry).
                self.drop_engine(&key);
                if should_revoke(old_pasted) {
                    self.spawn_guarded("revoke older token", None, async move {
                        session::revoke(&old).await;
                    });
                }
                if was_active {
                    self.switch_target = Some(key.clone());
                }
                let store = self.store.clone();
                let internal = self.internal.clone();
                self.store_job(async move {
                    let restored = store.lock().await.load(&key).await;
                    let _ = internal.send(if was_active {
                        Internal::ForegroundLoaded(restored)
                    } else {
                        Internal::BackgroundLoaded(restored)
                    });
                });
            }
            Internal::BackgroundLoaded(restored) => {
                if let Some(r) = restored {
                    self.start_background(r, Duration::ZERO, 0);
                }
            }
            Internal::Forgotten => {
                if self.known.is_empty() {
                    self.tell(Ev::NeedsSignIn { instance: None });
                } else {
                    self.restore();
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
                            None,
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
                        self.tell(Ev::SignInFailed(message));
                    }
                    Err(_) => {}
                }
            }
            Internal::Ended { gen, message } => self.session_ended(gen, message),
            Internal::GatewayEnded { gen, end } => {
                if self.engine(gen).is_none() {
                    return;
                }
                let stop = match end {
                    GatewayEnd::SessionEnded(message) => {
                        return self.session_ended(gen, message);
                    }
                    GatewayEnd::Terminal(message) if message.is_empty() => return,
                    GatewayEnd::Terminal(message) => Stop::Terminal(message),
                    GatewayEnd::ChannelClosed => Stop::Unexpected,
                };
                self.gateway_stopped(gen, stop);
            }
            Internal::GatewayConnected { gen } => {
                if let Some(r) = self.engine_mut(gen) {
                    r.policy.connected();
                }
            }
            Internal::GatewayRestart { gen } => {
                if self.engine(gen).is_some() {
                    self.restart_gateway(gen);
                }
            }
            Internal::Retry { attempt } => {
                if let Some(off) = &self.offline {
                    if self.attempt == attempt {
                        let (instance, credential, restore) = (
                            off.instance.clone(),
                            off.credential.clone(),
                            off.restore.clone(),
                        );
                        self.start_connect(instance, None, credential, Mode::Restore, restore);
                    }
                }
            }
            Internal::TaskPanicked { what, gateway_of } => {
                self.tell(Ev::Error(t!("error-task-panicked", what = what)));
                if let Some(gen) = gateway_of {
                    if self.engine(gen).is_some() {
                        self.gateway_stopped(gen, Stop::Panicked);
                    }
                }
                if what == "connect" || what == "restore" {
                    self.connecting = None;
                    self.tell(Ev::NeedsSignIn { instance: None });
                }
            }
        }
    }

    /// A stored session checked out: it becomes the active account. An older Inlet's
    /// entry is moved to the account's own entry now that its user is known.
    fn restored_connected(
        &mut self,
        connected: session::Connected,
        credential: Credential,
        restore: Option<Restore>,
    ) {
        let pasted = credential.is_pasted();
        // An entry stored before the media pin existed gets it now, right after `me()`
        // worked, so the media host is pinned from here on.
        let upgrade = match &credential {
            Credential::Stored { token, pins, .. } => {
                upgraded_pins(pins, &connected.session.session_pins())
                    .map(|pins| (token.clone(), pins))
            }
            _ => None,
        };
        let session = connected.session.clone();
        let (name, handle) = (connected.username.clone(), connected.account.clone());
        // A stored token must belong to the account its entry is named after. Otherwise
        // it is not used (fail closed); the entry stays, nothing is stored under another
        // name.
        if let Some(Restore::Account(expected)) = &restore {
            let origin = session.instance().origin().to_string();
            if AccountKey::new(&origin, &connected.user_id).as_ref() != Some(expected) {
                log::warn!("a stored token belongs to another account than its entry");
                if !self.promote_next() {
                    self.tell(Ev::NeedsSignIn {
                        instance: Some(origin),
                    });
                    self.tell(Ev::SignInFailed(t!("error-account-mismatch")));
                }
                return;
            }
        }
        let Some(key) = self.install(connected, false, pasted) else {
            return;
        };
        if let Some(Restore::Legacy(origin)) = &restore {
            // Until the move is done, signing this account out also removes the old
            // entry.
            if let Some(Active::Real(r)) = &mut self.active {
                r.legacy = Some(origin.clone());
            }
        }
        let store = self.store.clone();
        let internal = self.internal.clone();
        self.store_job(async move {
            let mut accounts = store.lock().await;
            if let Some(Restore::Legacy(origin)) = restore {
                let info = AccountInfo {
                    key: key.clone(),
                    name,
                    handle,
                };
                let migrated = accounts
                    .migrate(&origin, info, session.clone(), pasted)
                    .await;
                if migrated.listed() {
                    let _ = internal.send(Internal::ListChanged(accounts.list()));
                }
                if migrated == Migrated::Newer {
                    let _ = internal.send(Internal::NewerEntry {
                        key: key.clone(),
                        old: session,
                        old_pasted: pasted,
                    });
                }
            } else {
                accounts.remember_session(key.clone(), session, pasted);
                if let Some((token, pins)) = upgrade {
                    accounts.update_pins(&key, token, pins, pasted).await;
                }
            }
        });
    }

    pub(super) fn connect_failed(
        &mut self,
        failure: Failure,
        mode: Mode,
        instance: String,
        credential: Credential,
        restore: Option<Restore>,
    ) {
        match (mode, failure.kind) {
            (Mode::Restore, FailureKind::SessionEnded) => {
                self.offline = None;
                match restore {
                    Some(Restore::Account(key)) => {
                        self.forget_account(key.clone(), false);
                        if self.promote_next() {
                            self.tell(Ev::AccountRemoved {
                                key,
                                message: Some(failure.message),
                            });
                            return;
                        }
                    }
                    Some(Restore::Legacy(origin)) => {
                        let store = self.store.clone();
                        self.store_job(async move {
                            store.lock().await.forget_legacy(&origin, false).await;
                        });
                    }
                    None => {}
                }
                self.tell(Ev::NeedsSignIn {
                    instance: Some(instance),
                });
                self.tell(Ev::SignInFailed(failure.message));
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
                    restore,
                    failures,
                    timer: Some(timer),
                });
                self.tell(Ev::Offline {
                    instance,
                    message: failure.message,
                    retry_in_secs: wait,
                });
            }
            (Mode::Fresh, _) => self.tell(Ev::SignInFailed(failure.message)),
        }
    }

    /// Makes a checked session the active account: starts its gateway and, for a
    /// confirmed new sign-in, stores the token with its endpoint pins (ending an older
    /// stored token of the same account). The account that was active stays signed in,
    /// in the background. Returns the account, or `None` when the server's user ID is
    /// not usable.
    pub(super) fn install(
        &mut self,
        connected: session::Connected,
        save: bool,
        pasted: bool,
    ) -> Option<AccountKey> {
        let session = connected.session;
        let instance = session.instance().origin().to_string();
        let Some(key) = AccountKey::new(&instance, &connected.user_id) else {
            // Leaves the start-up or "Finishing" screen for the sign-in form.
            self.tell(Ev::NeedsSignIn {
                instance: Some(instance),
            });
            self.tell(Ev::SignInFailed(t!("error-account-id")));
            return None;
        };
        // Signed in again to an account that is already here: its old session goes.
        self.drop_engine(&key);
        self.demote_active();
        self.offline = None;
        let gen = self.next_gen;
        self.next_gen += 1;
        let events = self.global.for_account(key.clone());
        let foreground = Arc::new(AtomicBool::new(true));
        let composer = super::composer::State::default();
        let guilds = Arc::new(Mutex::new(connected.guilds.clone()));
        let cache: ChannelCache = Arc::default();
        let names: SharedNames = Arc::default();
        apply_accent(&session);
        // The UI learns which account is active before any of its events.
        self.tell(Ev::SignedIn {
            key: key.clone(),
            username: connected.username.clone(),
            instance: instance.clone(),
        });
        let (gateway, forward) = self.spawn_gateway(
            gen,
            &session,
            &guilds,
            &cache,
            &names,
            &events,
            &foreground,
            &composer,
            false,
        );
        let mut policy = ReconnectPolicy::default();
        policy.started(Instant::now());
        let _ = events.unbounded_send(messages::Ev::Guilds(connected.guilds));

        if save {
            let signed_in = SignedIn {
                session: session.clone(),
                user_id: key.user_id,
                name: connected.username.clone(),
                handle: connected.account.clone(),
            };
            let store = self.store.clone();
            let internal = self.internal.clone();
            let global = self.global.clone();
            // Through the store's lock: a sign-out right after waits for this write, so
            // it cannot store a token that sign-out is about to end.
            self.store_job(async move {
                let mut accounts = store.lock().await;
                match accounts.add(signed_in, pasted).await {
                    Ok(_) => {
                        let _ = internal.send(Internal::ListChanged(accounts.list()));
                    }
                    Err(_) => {
                        let _ = global.unbounded_send(Ev::Error(t!("error-keychain-refused")));
                    }
                }
            });
        }

        self.events = events.clone();
        self.session_activated(&session);
        self.active = Some(Active::Real(RealActive {
            gen,
            key: key.clone(),
            name: connected.username,
            events,
            foreground,
            ignores_typing: false,
            legacy: None,
            down: false,
            composer,
            presence: Default::default(),
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
        Some(key)
    }

    /// Starts a stored account in the background after `delay`: discovery, the session
    /// with its stored pins, then its gateway (in [`Backend::install_background`]).
    /// No REST call with the token.
    pub(super) fn start_background(&mut self, restored: Restored, delay: Duration, failures: u32) {
        let key = restored.key.clone();
        if self.engine_of(&key).is_some() {
            return;
        }
        let internal = self.internal.clone();
        let origin = key.origin.to_string();
        let (token, pins) = (restored.token.clone(), restored.pins.clone());
        let task_key = key.clone();
        let task = self.spawn_guarded("background start", None, async move {
            tokio::time::sleep(delay).await;
            let result = match session::discover(&origin).await {
                Ok(inst) => {
                    p::Session::from_token_session_pinned(inst, token, &pins).map_err(Failure::from)
                }
                Err(err) => Err(err),
            };
            let _ = internal.send(Internal::BackgroundReady {
                key: task_key,
                result,
            });
        });
        if let Some(old) = self.starts.insert(
            key,
            PendingStart {
                restored,
                task,
                due: tokio::time::Instant::now() + delay,
                failures,
            },
        ) {
            old.task.abort();
        }
    }

    /// A background account's session is ready: its gateway starts, passive.
    fn install_background(&mut self, restored: Restored, session: p::Session) {
        let key = restored.key;
        if self.engine_of(&key).is_some() {
            return;
        }
        let gen = self.next_gen;
        self.next_gen += 1;
        let events = self.global.for_account(key.clone());
        let foreground = Arc::new(AtomicBool::new(false));
        let composer = super::composer::State::default();
        let guilds = Arc::new(Mutex::new(Vec::new()));
        let cache: ChannelCache = Arc::default();
        let names: SharedNames = Arc::default();
        let (gateway, forward) = self.spawn_gateway(
            gen,
            &session,
            &guilds,
            &cache,
            &names,
            &events,
            &foreground,
            &composer,
            true,
        );
        let mut policy = ReconnectPolicy::default();
        policy.started(Instant::now());
        let name = self
            .known
            .iter()
            .find(|a| a.key == key)
            .map(|a| a.name.clone())
            .unwrap_or_default();
        let store = self.store.clone();
        let (k, s, pasted) = (key.clone(), session.clone(), restored.pasted);
        self.store_job(async move {
            store.lock().await.remember_session(k, s, pasted);
        });
        self.background.push(RealActive {
            gen,
            key,
            name,
            events,
            foreground,
            ignores_typing: true,
            legacy: None,
            down: false,
            composer,
            presence: Default::default(),
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
        });
    }

    /// The account with this key that has a session (active or background).
    fn engine_of(&self, key: &AccountKey) -> Option<&RealActive> {
        match &self.active {
            Some(Active::Real(r)) if &r.key == key => Some(r),
            _ => self.background.iter().find(|r| &r.key == key),
        }
    }

    /// The active account goes to the background: its open guild is made passive (the
    /// one Lazy Request it sends, so a reconnect does not make it active again), the
    /// requests of what was on screen stop, and its events are no longer rendered.
    pub(super) fn demote_active(&mut self) {
        match self.active.take() {
            Some(Active::Real(mut r)) => {
                r.foreground.store(false, Ordering::Relaxed);
                if let Some(guild) = r.active_guild.take() {
                    r.gateway.set_guild_active(&guild, false);
                }
                r.presence.to_background(&r.gateway);
                for t in r.settings_writes.drain(..) {
                    t.abort();
                }
                self.stop_screen_tasks();
                // Uploads never go on behind another account.
                self.stop_uploads(&r);
                self.background.push(r);
            }
            #[cfg(feature = "demo")]
            Some(Active::Demo(m)) => {
                self.active = Some(Active::Demo(m));
                return;
            }
            None => {}
        }
        self.events = self.global.clone();
    }

    /// Makes a background account the active one. Its gateway starts again once with
    /// typing events when it was started in the background (or is down).
    fn promote(&mut self, key: &AccountKey) -> bool {
        let Some(ix) = self.background.iter().position(|r| &r.key == key) else {
            return false;
        };
        self.demote_active();
        // `demote_active` pushed the old active account at the end: `ix` still holds.
        let mut r = self.background.remove(ix);
        r.foreground.store(true, Ordering::Relaxed);
        apply_accent(&r.session);
        self.tell(Ev::SignedIn {
            key: r.key.clone(),
            username: r.name.clone(),
            instance: r.session.instance().origin().to_string(),
        });
        let restart = r.ignores_typing || r.down;
        let gen = r.gen;
        // A gateway that is down waits the spacing of "Reconnect", so a quick switch back
        // and forth to an account whose gateway keeps failing does not identify on every
        // click. A healthy one started for the background starts again at once, only to
        // get typing events (no status line, no spacing).
        let wait = if r.down {
            r.policy.manual(Instant::now())
        } else {
            Duration::ZERO
        };
        let guilds = r.guilds.lock().unwrap().clone();
        if !restart {
            // Its gateway stays: it shows the status asked for last, not `invisible`.
            r.presence.to_front(&r.gateway);
        }
        self.events = r.events.clone();
        self.session_activated(&r.session);
        self.active = Some(Active::Real(r));
        if restart && wait.is_zero() {
            self.restart_gateway(gen);
        } else if restart {
            let _ = self
                .events
                .unbounded_send(Ev::Status(t!("status-reconnecting")));
            self.schedule_restart(gen, wait);
        }
        let _ = self.events.unbounded_send(messages::Ev::Guilds(guilds));
        let store = self.store.clone();
        let key = key.clone();
        self.store_job(async move {
            let mut accounts = store.lock().await;
            accounts.set_active(&key);
            accounts.save().await;
        });
        true
    }

    /// What every account needs when it becomes the active one, however it got there
    /// (sign-in, start-up, switch): the image client for its own media endpoint, sent
    /// with its tag so the UI never keeps the previous account's.
    fn session_activated(&self, session: &p::Session) {
        super::media::session_started(self, session);
    }

    /// Makes `key` the active account: a connected one at once, a stored one that is not
    /// connected yet by connecting it now (start-up screens as for the first account).
    pub(super) fn switch_account(&mut self, key: AccountKey) {
        #[cfg(feature = "demo")]
        if let Some(Active::Demo(m)) = &mut self.active {
            m.switch(&key);
            // The UI let go of its image client at the switch.
            super::media::demo_started(self);
            return;
        }
        if let Some(Active::Real(r)) = &self.active {
            if r.key == key {
                self.tell(Ev::SignedIn {
                    key,
                    username: r.name.clone(),
                    instance: r.session.instance().origin().to_string(),
                });
                return;
            }
        }
        if self.connecting.is_some() {
            log::info!("account switch while connecting; ignored");
            // The UI has switched already: tell it which account is really active.
            if let Some(Active::Real(r)) = &self.active {
                self.tell(Ev::SignedIn {
                    key: r.key.clone(),
                    username: r.name.clone(),
                    instance: r.session.instance().origin().to_string(),
                });
            }
            return;
        }
        self.switch_target = Some(key.clone());
        if self.promote(&key) {
            return;
        }
        if let Some(start) = self.starts.remove(&key) {
            start.task.abort();
            self.demote_active();
            self.start_foreground(start.restored);
            return;
        }
        if self.known.iter().any(|a| a.key == key) {
            // Stored but not running (its start was cancelled): read it again.
            self.demote_active();
            let store = self.store.clone();
            let internal = self.internal.clone();
            self.store_job(async move {
                let restored = store.lock().await.load(&key).await;
                let _ = internal.send(Internal::ForegroundLoaded(restored));
            });
            return;
        }
        log::warn!("switch to an account that is not signed in");
    }

    fn start_foreground(&mut self, r: Restored) {
        let instance = r.key.origin.to_string();
        let credential = Credential::Stored {
            token: r.token,
            pins: r.pins,
            pasted: r.pasted,
        };
        let restore = Some(Restore::Account(r.key));
        self.start_connect(instance, None, credential, Mode::Restore, restore);
    }

    /// After the active account left: the first connected background account becomes
    /// active. Returns whether there was one.
    fn promote_connected(&mut self) -> bool {
        match self.background.first().map(|r| r.key.clone()) {
            Some(key) => self.promote(&key),
            None => false,
        }
    }

    /// Like [`Backend::promote_connected`], or else a stored account that is still
    /// waiting for its background start, connected now.
    fn promote_next(&mut self) -> bool {
        if self.promote_connected() {
            return true;
        }
        let Some(key) = self.starts.keys().min().cloned() else {
            return false;
        };
        if let Some(start) = self.starts.remove(&key) {
            start.task.abort();
            self.start_foreground(start.restored);
            return true;
        }
        false
    }

    /// Stops the requests for what the active account has on screen.
    fn stop_screen_tasks(&mut self) {
        for task in [self.open_guild.take(), self.open_channel.take()]
            .into_iter()
            .flatten()
        {
            task.abort();
        }
        self.older = Default::default();
        self.threads.stop();
    }

    /// Stops an account's gateway and its tasks.
    fn close_engine(r: &RealActive) {
        r.gateway.close();
        r.forward.abort();
        if let Some(t) = &r.restart_timer {
            t.abort();
        }
        for t in &r.settings_writes {
            t.abort();
        }
    }

    /// Stops the active account (gateway, tasks and what is loading for the screen).
    pub(super) fn teardown(&mut self, r: &RealActive) {
        Self::close_engine(r);
        self.stop_screen_tasks();
        self.uploads.forget_gen(r.gen);
    }

    /// Stops an account wherever it runs or waits (active, background, waiting start).
    fn drop_engine(&mut self, key: &AccountKey) {
        if matches!(&self.active, Some(Active::Real(r)) if &r.key == key) {
            if let Some(Active::Real(r)) = self.active.take() {
                self.teardown(&r);
            }
            self.events = self.global.clone();
        }
        if let Some(ix) = self.background.iter().position(|r| &r.key == key) {
            let r = self.background.remove(ix);
            Self::close_engine(&r);
            self.uploads.forget_gen(r.gen);
        }
        if let Some(start) = self.starts.remove(key) {
            start.task.abort();
        }
    }

    /// Removes an account from the store (ending its token on the server when `revoke`
    /// is set and it was not pasted) and tells the UI the new list.
    fn forget_account(&mut self, key: AccountKey, revoke: bool) {
        let legacy = self.engine_of(&key).and_then(|r| r.legacy.clone());
        self.forget_account_with(key, revoke, legacy);
    }

    /// [`Backend::forget_account`] for an account whose old entry (`legacy`, the
    /// origin) is known.
    fn forget_account_with(&mut self, key: AccountKey, revoke: bool, legacy: Option<String>) {
        self.drop_engine(&key);
        let store = self.store.clone();
        let internal = self.internal.clone();
        self.store_job(async move {
            let mut accounts = store.lock().await;
            accounts.remove(&key, revoke).await;
            if let Some(origin) = legacy {
                // The old entry of this account, not moved yet: it goes too (already
                // ended with the live session above when that was allowed).
                accounts.forget_legacy(&origin, false).await;
            }
            let _ = internal.send(Internal::ListChanged(accounts.list()));
        });
    }

    /// The server ended session `gen` (token rejected or revoked).
    fn session_ended(&mut self, gen: u64, message: String) {
        let Some(r) = self.engine(gen) else {
            return;
        };
        let key = r.key.clone();
        let was_active = same_session(self.active_gen(), gen);
        self.forget_account(key.clone(), false);
        if !was_active {
            self.tell(Ev::AccountRemoved {
                key,
                message: Some(message),
            });
            return;
        }
        if self.promote_next() {
            self.tell(Ev::AccountRemoved {
                key,
                message: Some(message),
            });
        } else {
            crate::theme::set_accent(None);
            self.tell(Ev::SessionEnded(message));
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

    /// Recovery-screen sign-out: no session is running. Ends the active stored session on
    /// the server if it can be reached within the timeouts, then removes it. The other
    /// accounts start again afterwards.
    pub(super) fn forget_stored(&mut self) {
        let store = self.store.clone();
        let internal = self.internal.clone();
        self.store_job(async move {
            let mut accounts = store.lock().await;
            accounts.restore_all().await;
            let active = accounts.active().cloned();
            let (_, legacy) = accounts.take_restored();
            if let Some(key) = active {
                accounts.remove(&key, true).await;
            } else if let Some(legacy) = legacy {
                accounts.forget_legacy(&legacy.origin, true).await;
            }
            let _ = internal.send(Internal::ListChanged(accounts.list()));
            let _ = internal.send(Internal::Forgotten);
        });
    }

    /// Signs the active account out. Another signed-in account becomes active, or the
    /// sign-in screen shows when there is none.
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
        self.events = self.global.clone();
        let mut legacy = None;
        let restore = match active {
            Some(Active::Real(r)) => {
                // Close the gateway first, so the server's reaction to the revoked token
                // cannot be read as "the session ended" (the session is gone as well).
                self.teardown(&r);
                legacy = r.legacy.clone();
                Some(Restore::Account(r.key.clone()))
            }
            #[cfg(feature = "demo")]
            Some(Active::Demo(_)) => None,
            None => offline.and_then(|o| o.restore),
        };
        let gone = match restore {
            Some(Restore::Account(key)) => {
                self.forget_account_with(key.clone(), true, legacy);
                Some(key)
            }
            Some(Restore::Legacy(origin)) => {
                let store = self.store.clone();
                self.store_job(async move {
                    store.lock().await.forget_legacy(&origin, true).await;
                });
                None
            }
            None => None,
        };
        if !self.promote_next() {
            crate::theme::set_accent(None);
            self.tell(Ev::SignedOut);
        }
        // Off the switcher and the tray at once, not only once the server was told.
        if let Some(key) = gone {
            self.tell(Ev::AccountRemoved { key, message: None });
        }
    }

    /// Signs one account out from the account switcher.
    fn remove_account(&mut self, key: AccountKey) {
        if matches!(&self.active, Some(Active::Real(r)) if r.key == key) {
            return self.sign_out();
        }
        self.forget_account(key.clone(), true);
        self.tell(Ev::AccountRemoved { key, message: None });
    }
}

/// The instance's accent colour (its branding), as Fluxer's own client applies it.
fn apply_accent(session: &p::Session) {
    let accent = session
        .instance()
        .discovery()
        .app_public
        .get("branding")
        .and_then(|b| b.get("theme_color"))
        .and_then(|c| c.as_str())
        .and_then(crate::theme::parse_hex);
    crate::theme::set_accent(accent);
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

/// Whether signing out may end a token on the server. A pasted token belongs to another
/// client and is only forgotten here.
pub fn should_revoke(pasted: bool) -> bool {
    !pasted
}

pub(super) fn notice_forgotten(events: &Ui, deleted: bool) {
    let mut text = t!("notice-pasted-forgotten");
    if !deleted {
        text.push(' ');
        text.push_str(&t!("notice-not-deleted"));
    }
    let _ = events.unbounded_send(Ev::Notice(text));
}

pub(super) fn notice_sign_out(events: &Ui, told_server: bool, deleted: bool) {
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
