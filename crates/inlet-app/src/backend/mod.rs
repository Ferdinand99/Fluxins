// SPDX-License-Identifier: AGPL-3.0-or-later
//! The bridge between the UI and the network.
//!
//! The UI runs on gpui's main thread. Everything that talks to a Fluxer instance runs
//! on a background thread with its own tokio runtime. The UI sends [`Command`]s, the
//! backend answers with [`Event`]s. Both are split by domain: every domain module owns
//! its `Cmd` and `Ev` types and a `handle` function for its commands, so tasks that work
//! on different domains do not edit the same file. A domain that has nothing yet has
//! empty enums.
//!
//! The backend loop only holds state and starts tasks; it never waits on the network
//! itself. Each REST call runs in its own task with a clone of the session, so a slow
//! request never holds up sign-out, channel switches or the end of a session. Opening a
//! channel or guild aborts the previous open. Task panics are caught and reported.
//!
//! The token never travels to the UI. It goes from the browser handoff (or the paste
//! field) to the keychain and the protocol session, nowhere else.
//!
//! Several accounts can be signed in at once. The active one is `Backend::active`, as
//! before; the others run in `Backend::background` with their own session, limiter and
//! gateway, passive (see `accounts`). Every event an account's tasks send carries the
//! account ([`Event::Tagged`]), so the UI can tell the active account's events from the
//! others' and from late answers of an account it switched away from.

pub mod accounts;
pub mod composer;
mod gateway;
pub mod media;
pub mod messages;
pub mod notifications;
pub mod platform;
pub mod search;
mod session;
pub mod settings;
pub mod social;
pub mod voice;

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::channel::mpsc::UnboundedSender;
use inlet_protocol as p;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender as TokioSender};
use tokio::task::AbortHandle;

use crate::model::Guild;
use crate::reconnect::ReconnectPolicy;
use accounts::{AccountInfo, AccountKey, Accounts};
use gateway::GatewayEnd;
use messages::parse::{ChannelCache, SharedNames};
use session::{Credential, Failure, FailureKind};

/// What the UI asks of the backend, by domain.
// Domains without commands yet have uninhabited `Cmd` types.
#[allow(dead_code)]
pub enum Command {
    Accounts(accounts::Cmd),
    Messages(messages::Cmd),
    Composer(composer::Cmd),
    Media(media::Cmd),
    Social(social::Cmd),
    Settings(settings::Cmd),
    Search(search::Cmd),
    Notifications(notifications::Cmd),
    Voice(voice::Cmd),
    Platform(platform::Cmd),
}

/// What the backend tells the UI, by domain.
// Domains without events yet have uninhabited `Ev` types. Events are moved once through
// a channel, so the size of the largest domain's event does not matter.
#[allow(dead_code, clippy::large_enum_variant)]
pub enum Event {
    Accounts(accounts::Ev),
    Messages(messages::Ev),
    Composer(composer::Ev),
    Media(media::Ev),
    Social(social::Ev),
    Settings(settings::Ev),
    Search(search::Ev),
    Notifications(notifications::Ev),
    Voice(voice::Ev),
    Platform(platform::Ev),
    /// An event sent by one account's tasks (its gateway, its REST calls). The UI hands
    /// it on as usual when `account` is the active account, and to that account's
    /// passive state otherwise.
    Tagged {
        account: AccountKey,
        event: Box<Event>,
    },
}

/// `From` for each domain's `Cmd` and `Ev`, so callers write `send(accounts::Cmd::SignOut)`.
macro_rules! domains {
    ($($variant:ident => $module:ident),* $(,)?) => {
        $(
            impl From<$module::Cmd> for Command {
                fn from(cmd: $module::Cmd) -> Self {
                    Command::$variant(cmd)
                }
            }
            impl From<$module::Ev> for Event {
                fn from(ev: $module::Ev) -> Self {
                    Event::$variant(ev)
                }
            }
        )*
    };
}

domains! {
    Accounts => accounts,
    Messages => messages,
    Composer => composer,
    Media => media,
    Social => social,
    Settings => settings,
    Search => search,
    Notifications => notifications,
    Voice => voice,
    Platform => platform,
}

/// The sender for UI events. Errors and status lines are coalesced: the same text again
/// within a few seconds is dropped, and at most a handful of errors pass per window, so
/// no loop can flood the UI.
///
/// A sender made with [`Ui::for_account`] tags every event with its account.
#[derive(Clone)]
pub struct Ui {
    tx: UnboundedSender<Event>,
    throttle: Arc<Mutex<Throttle>>,
    account: Option<AccountKey>,
}

#[derive(Default)]
struct Throttle {
    last_error: Option<(String, Instant)>,
    last_status: Option<String>,
    window_start: Option<Instant>,
    errors_in_window: u32,
}

const ERROR_REPEAT: Duration = Duration::from_secs(5);
const ERROR_WINDOW: Duration = Duration::from_secs(10);
const ERRORS_PER_WINDOW: u32 = 5;

impl Ui {
    pub fn new(tx: UnboundedSender<Event>) -> Self {
        Self {
            tx,
            throttle: Arc::default(),
            account: None,
        }
    }

    /// A sender for one account's tasks: same channel, own error throttle, and every
    /// event wrapped in [`Event::Tagged`].
    pub fn for_account(&self, account: AccountKey) -> Self {
        Self {
            tx: self.tx.clone(),
            throttle: Arc::default(),
            account: Some(account),
        }
    }

    /// Sends an event unless it is a repeated or excess error or an unchanged status.
    pub fn unbounded_send(&self, event: impl Into<Event>) -> Result<(), ()> {
        let event = event.into();
        let now = Instant::now();
        {
            let mut t = self.throttle.lock().unwrap();
            if !t.admit(&event, now) {
                return Ok(());
            }
        }
        let event = match &self.account {
            Some(account) => Event::Tagged {
                account: account.clone(),
                event: Box::new(event),
            },
            None => event,
        };
        self.tx.unbounded_send(event).map_err(|_| ())
    }
}

impl Throttle {
    fn admit(&mut self, event: &Event, now: Instant) -> bool {
        match event {
            Event::Tagged { event, .. } => self.admit(event, now),
            Event::Accounts(accounts::Ev::Error(message)) => {
                if let Some((last, at)) = &self.last_error {
                    if last == message && now.saturating_duration_since(*at) < ERROR_REPEAT {
                        return false;
                    }
                }
                match self.window_start {
                    Some(start) if now.saturating_duration_since(start) < ERROR_WINDOW => {
                        if self.errors_in_window >= ERRORS_PER_WINDOW {
                            return false;
                        }
                        self.errors_in_window += 1;
                    }
                    _ => {
                        self.window_start = Some(now);
                        self.errors_in_window = 1;
                    }
                }
                self.last_error = Some((message.clone(), now));
                true
            }
            Event::Accounts(accounts::Ev::Status(text)) => {
                if self.last_status.as_deref() == Some(text) {
                    return false;
                }
                self.last_status = Some(text.clone());
                true
            }
            _ => true,
        }
    }
}

/// The UI's way to the backend. Every command except the accounts domain's goes out
/// with the account the UI shows ([`BackendHandle::set_account`]); the backend runs it
/// only while that account is the active one, so no command meant for one account can
/// ever run with another account's session and token.
#[derive(Clone)]
pub struct BackendHandle {
    tx: TokioSender<(Option<AccountKey>, Command)>,
    account: Arc<Mutex<Option<AccountKey>>>,
}

impl BackendHandle {
    /// Sends a command. If the backend thread has died, the UI notices because the
    /// event channel closes; nothing to do here.
    pub fn send(&self, command: impl Into<Command>) {
        let command = command.into();
        let account = match command {
            Command::Accounts(_) => None,
            _ => self
                .account
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        };
        let _ = self.tx.send((account, command));
    }

    /// The account the UI shows from now on; the commands sent after this are for it.
    pub fn set_account(&self, account: Option<AccountKey>) {
        *self.account.lock().unwrap_or_else(|e| e.into_inner()) = account;
    }

    /// A handle with no backend behind it: commands go nowhere. For the layout tests,
    /// which feed the view its events themselves.
    #[cfg(feature = "demo")]
    pub fn detached() -> Self {
        let (tx, _rx) = unbounded_channel();
        Self {
            tx,
            account: Arc::default(),
        }
    }
}

/// Starts the backend thread. Events arrive on `events`. When the thread ends (a panic in
/// the loop itself), `events` is dropped and the UI's receiver ends.
pub fn start(events: UnboundedSender<Event>) -> BackendHandle {
    start_with(events, Accounts::system)
}

/// Starts the backend thread with the stored accounts `store` makes (the system
/// keychain in the app, an in-memory one in tests).
pub fn start_with(
    events: UnboundedSender<Event>,
    store: impl FnOnce(Ui) -> Accounts + Send + 'static,
) -> BackendHandle {
    let events = Ui::new(events);
    let (tx, rx) = unbounded_channel();
    std::thread::Builder::new()
        .name("inlet-backend".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("tokio runtime");
            let store = store(events.clone());
            runtime.block_on(Backend::with_store(events, store).run(rx));
        })
        .expect("backend thread");
    BackendHandle {
        tx,
        account: Arc::default(),
    }
}

/// Messages from tasks back to the loop.
enum Internal {
    /// The stored accounts were read (start-up).
    Restored {
        attempt: u64,
        restored: Vec<accounts::Restored>,
        legacy: Option<accounts::Legacy>,
        list: Vec<AccountInfo>,
        last_instance: Option<String>,
    },
    /// The list of stored accounts changed.
    ListChanged(Vec<AccountInfo>),
    Connected {
        attempt: u64,
        mode: Mode,
        instance: String,
        credential: Credential,
        /// Which stored account this restore is for (`None` for a new sign-in).
        restore: Option<accounts::Restore>,
        result: Result<session::Connected, Failure>,
    },
    /// A background account's session is built (or failed); its gateway starts next.
    BackgroundReady {
        key: AccountKey,
        result: Result<p::Session, Failure>,
    },
    /// An account was signed out from the recovery screen; start again with the rest.
    Forgotten,
    /// A stored account read again to start it in the background.
    BackgroundLoaded(Option<accounts::Restored>),
    /// A stored account read again to make it the active one (`None`: not stored).
    ForegroundLoaded(Option<accounts::Restored>),
    /// The move of an older entry found a newer token in the account's own entry: the
    /// account goes on with that one, and the older session (`old`) is ended on the
    /// server unless it was pasted.
    NewerEntry {
        key: AccountKey,
        old: p::Session,
        old_pasted: bool,
    },
    HandoffDone {
        id: u64,
        result: Result<(p::Instance, p::SignedIn), String>,
    },
    /// The session `gen` ended for good (auth failure).
    Ended {
        gen: u64,
        message: String,
    },
    /// The gateway of session `gen` stopped without an auth failure.
    GatewayEnded {
        gen: u64,
        end: GatewayEnd,
    },
    /// The gateway of session `gen` reached READY or RESUMED.
    GatewayConnected {
        gen: u64,
    },
    /// Time to start the gateway of session `gen` again.
    GatewayRestart {
        gen: u64,
    },
    Retry {
        attempt: u64,
    },
    TaskPanicked {
        what: &'static str,
        gateway_of: Option<u64>,
    },
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
    /// Which account this is.
    key: AccountKey,
    /// Display name, for the UI when the account becomes the active one.
    name: String,
    /// Sends events tagged with this account.
    events: Ui,
    /// Whether this is the active account. The gateway task reads it: a background
    /// account's events are not rendered and its lists are not sent to the UI.
    foreground: Arc<AtomicBool>,
    /// The gateway was started for the background (it ignores typing events); it is
    /// started again when the account becomes the active one.
    ignores_typing: bool,
    /// The gateway stopped and waits for the person (no automatic restart).
    down: bool,
    /// The account was restored from an older Inlet's entry (this origin) that may not
    /// be moved yet; signing it out removes that entry too.
    legacy: Option<String>,
    /// The composer's state of this account: its ID and limit, sends that may be
    /// retried, open member searches. Kept per account, so switching back finds it.
    composer: composer::State,
    /// This account's presence: the status it shows and its settings writes.
    presence: Box<social::presence::Presence>,
    session: p::Session,
    gateway: p::GatewayHandle,
    forward: AbortHandle,
    guilds: Arc<Mutex<Vec<Guild>>>,
    cache: ChannelCache,
    names: SharedNames,
    active_guild: Option<String>,
    /// Restart bookkeeping; lives with the session so spacing survives restarts.
    policy: ReconnectPolicy,
    restart_timer: Option<AbortHandle>,
    /// Notification settings writes still running; aborted when the session ends.
    settings_writes: Vec<AbortHandle>,
}

/// A new sign-in waiting for "Continue".
struct PendingConfirm {
    connected: session::Connected,
    /// Issued to Inlet by the browser handoff. A pasted token belongs to another client
    /// and is only forgotten, never signed out.
    from_handoff: bool,
}

enum Active {
    Real(RealActive),
    #[cfg(feature = "demo")]
    Demo(crate::demo::MockSession),
}

struct Offline {
    instance: String,
    credential: Credential,
    restore: Option<accounts::Restore>,
    failures: u32,
    timer: Option<AbortHandle>,
}

struct Backend {
    /// The active account's sender (tagged with it), or `global` with none active.
    events: Ui,
    /// The untagged sender: sign-in, the account list, which account is active.
    global: Ui,
    internal: TokioSender<Internal>,
    next_gen: u64,
    active: Option<Active>,
    /// The other signed-in accounts, passive.
    background: Vec<RealActive>,
    /// Stored accounts whose background start is waiting (start-up spacing, retries).
    starts: HashMap<AccountKey, accounts::PendingStart>,
    /// The stored accounts as last read or written.
    known: Vec<AccountInfo>,
    /// The keychain entries and the account list; tasks lock it in turn.
    store: Arc<tokio::sync::Mutex<Accounts>>,
    /// The last task on the store; the next one waits for it (see `store_job`).
    store_tail: Option<tokio::task::JoinHandle<()>>,
    /// The account the UI last switched to (a stored account read again is started only
    /// when it still is).
    switch_target: Option<AccountKey>,
    attempt: u64,
    connecting: Option<(u64, AbortHandle, Mode)>,
    offline: Option<Offline>,
    handoff_seq: u64,
    handoff: Option<(u64, tokio::sync::oneshot::Sender<()>)>,
    /// A new sign-in waiting for the person to confirm the account.
    pending_confirm: Option<PendingConfirm>,
    open_guild: Option<AbortHandle>,
    open_channel: Option<AbortHandle>,
    /// The request for older messages of the open channel.
    older: messages::OlderPages,
    /// Requests of the thread panel, thread lists and forums.
    threads: messages::threads::State,
    /// The member list subscription and the profile fetch of the active account.
    social: social::State,
}

impl Backend {
    fn with_store(events: Ui, store: Accounts) -> Self {
        let (internal, _) = unbounded_channel();
        Self {
            global: events.clone(),
            events,
            internal,
            next_gen: 1,
            active: None,
            background: Vec::new(),
            starts: HashMap::new(),
            known: Vec::new(),
            store: Arc::new(tokio::sync::Mutex::new(store)),
            store_tail: None,
            switch_target: None,
            attempt: 0,
            connecting: None,
            offline: None,
            handoff_seq: 0,
            handoff: None,
            pending_confirm: None,
            open_guild: None,
            open_channel: None,
            older: Default::default(),
            threads: Default::default(),
            social: Default::default(),
        }
    }

    fn emit(&self, event: impl Into<Event>) {
        let _ = self.events.unbounded_send(event);
    }

    fn active_gen(&self) -> Option<u64> {
        match &self.active {
            Some(Active::Real(r)) => Some(r.gen),
            _ => None,
        }
    }

    /// The account (active or background) whose session generation is `gen`.
    fn engine(&self, gen: u64) -> Option<&RealActive> {
        match &self.active {
            Some(Active::Real(r)) if r.gen == gen => Some(r),
            _ => self.background.iter().find(|r| r.gen == gen),
        }
    }

    fn engine_mut(&mut self, gen: u64) -> Option<&mut RealActive> {
        match &mut self.active {
            Some(Active::Real(r)) if r.gen == gen => Some(r),
            _ => self.background.iter_mut().find(|r| r.gen == gen),
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

    async fn run(mut self, mut rx: UnboundedReceiver<(Option<AccountKey>, Command)>) {
        let (internal, mut internal_rx) = unbounded_channel();
        self.internal = internal;
        loop {
            tokio::select! {
                // Internal results first: session end must not wait behind commands.
                biased;
                Some(msg) = internal_rx.recv() => self.internal(msg),
                command = rx.recv() => match command {
                    Some((account, command)) => self.command(account, command),
                    None => break,
                },
            }
        }
    }

    /// A command from the UI, for `account` (`None` for the accounts domain). A command
    /// for an account that is not the active one is dropped: it was sent before a
    /// switch reached the backend, and running it would use the wrong session.
    fn command(&mut self, account: Option<AccountKey>, command: Command) {
        if !matches!(command, Command::Accounts(_)) && !self.acts_for(account.as_ref()) {
            log::debug!("a command for an account that is not the active one was not run");
            return self.answer_without_session(account, command);
        }
        self.handle(command);
    }

    /// Runs a command for an account that is not the active one as if no account were
    /// signed in: every domain then answers "not connected" without using any token,
    /// tagged with the command's account, so a caller that waits for an answer (a page,
    /// a settings write, a send) never hangs. Nothing of the active account is touched.
    fn answer_without_session(&mut self, account: Option<AccountKey>, command: Command) {
        let answer_to = match account {
            Some(k) => self.global.for_account(k),
            None => self.global.clone(),
        };
        let active = self.active.take();
        let events = std::mem::replace(&mut self.events, answer_to);
        let open = (self.open_guild.take(), self.open_channel.take());
        let older = std::mem::take(&mut self.older);
        self.handle(command);
        (self.open_guild, self.open_channel) = open;
        self.older = older;
        self.events = events;
        self.active = active;
    }

    /// Whether a command sent for `account` may run now.
    fn acts_for(&self, account: Option<&AccountKey>) -> bool {
        match &self.active {
            Some(Active::Real(r)) => account == Some(&r.key),
            #[cfg(feature = "demo")]
            Some(Active::Demo(_)) => true,
            // No session: the domains answer "not connected" without a token.
            None => true,
        }
    }

    /// Hands a command to the domain it belongs to.
    fn handle(&mut self, command: Command) {
        match command {
            Command::Accounts(cmd) => accounts::handle(self, cmd),
            Command::Messages(cmd) => messages::handle(self, cmd),
            Command::Composer(cmd) => composer::handle(self, cmd),
            Command::Media(cmd) => media::handle(self, cmd),
            Command::Social(cmd) => social::handle(self, cmd),
            Command::Settings(cmd) => settings::handle(self, cmd),
            Command::Search(cmd) => search::handle(self, cmd),
            Command::Notifications(cmd) => notifications::handle(self, cmd),
            Command::Voice(cmd) => voice::handle(self, cmd),
            Command::Platform(cmd) => platform::handle(self, cmd),
        }
    }
}

/// Sends a REST failure to the UI (`what` turns the reason into the sentence), or ends
/// the session when the token is gone.
fn report(
    events: &Ui,
    internal: &TokioSender<Internal>,
    gen: u64,
    what: impl FnOnce(String) -> String,
    err: p::Error,
) {
    let failure = Failure::from(err);
    if failure.kind == FailureKind::SessionEnded {
        let _ = internal.send(Internal::Ended {
            gen,
            message: failure.message,
        });
    } else {
        let _ = events.unbounded_send(accounts::Ev::Error(what(failure.message)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_errors_are_coalesced() {
        let mut t = Throttle::default();
        let now = Instant::now();
        let err = |m: &str| Event::from(accounts::Ev::Error(m.to_string()));
        assert!(t.admit(&err("a"), now));
        assert!(!t.admit(&err("a"), now + Duration::from_secs(1)));
        assert!(t.admit(&err("a"), now + ERROR_REPEAT));
        // A loop producing new texts is capped per window.
        let later = now + Duration::from_secs(60);
        let passed = (0..100)
            .filter(|i| t.admit(&err(&format!("e{i}")), later))
            .count();
        assert_eq!(passed as u32, ERRORS_PER_WINDOW);
        assert!(t.admit(&accounts::Ev::Status("Connected".into()).into(), later));
        assert!(!t.admit(&accounts::Ev::Status("Connected".into()).into(), later));
    }

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
