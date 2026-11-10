//! The bridge between the UI and the network.
//!
//! The UI runs on gpui's main thread. Everything that talks to a Fluxer instance runs
//! on a background thread with its own tokio runtime. The two sides talk through two
//! channels: the UI sends [`Command`]s, the backend answers with [`Event`]s.
//!
//! The token never travels to the UI. The backend reads it from and writes it to the
//! OS keychain (see [`crate::token_store`]) and keeps it in memory only.

use futures::channel::mpsc::UnboundedSender;
use inlet_protocol as p;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender as TokioSender, unbounded_channel};

use crate::model::{Channel, Guild, Message};
use crate::real::{self, Failure, RealSession};
use crate::{mock, token_store};

/// Default instance shown on the sign-in screen.
pub const DEFAULT_INSTANCE: &str = "fluxer.opland.net";

pub enum Command {
    /// Look for a saved session at start-up.
    Restore,
    SignInBrowser { instance: String },
    SignInToken { instance: String, token: String },
    /// Stop a browser sign-in that is waiting.
    CancelSignIn,
    SignOut,
    /// Load channels for a guild, or DMs when `guild_id` is `None`.
    OpenGuild { guild_id: Option<String> },
    OpenChannel { channel_id: String },
    Send { channel_id: String, text: String },
}

pub enum Event {
    /// No saved session; show the sign-in screen.
    NeedsSignIn { instance: Option<String> },
    /// Browser sign-in started: the page to open and the code to enter there.
    HandoffStarted { url: String, code: String },
    SignInFailed(String),
    SignedIn { username: String, instance: String },
    SignedOut,
    /// The session ended on the server side (token rejected or revoked). The token is
    /// already removed from the keychain; show sign-in with this message.
    SessionEnded(String),
    Guilds(Vec<Guild>),
    Channels { guild_id: Option<String>, channels: Vec<Channel> },
    Messages { channel_id: String, messages: Vec<Message> },
    MessageCreated(Message),
    MessageUpdated(Message),
    MessageDeleted { channel_id: String, message_id: String },
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
    pub fn send(&self, command: Command) {
        // The backend thread lives as long as the app; a send error means it panicked,
        // which the log already shows.
        let _ = self.tx.send(command);
    }
}

/// Starts the backend thread. Events arrive on `events`.
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

/// The signed-in connection. `Demo` is built-in sample data, only reachable with the
/// `INLET_DEMO` environment variable (for screenshots and UI checks without an account).
enum Active {
    Demo(mock::MockSession),
    Real(RealSession),
}

/// Result of a browser sign-in that ran as a background task.
type Finished = Result<(p::Instance, p::Token), String>;

struct Backend {
    events: UnboundedSender<Event>,
    active: Option<Active>,
    waiting: Option<tokio::task::JoinHandle<()>>,
    finished_tx: TokioSender<Finished>,
    ended_tx: TokioSender<String>,
}

impl Backend {
    fn new(events: UnboundedSender<Event>) -> Self {
        let (finished_tx, _) = unbounded_channel();
        let (ended_tx, _) = unbounded_channel();
        Self { events, active: None, waiting: None, finished_tx, ended_tx }
    }

    fn emit(&self, event: Event) {
        let _ = self.events.unbounded_send(event);
    }

    async fn run(mut self, mut rx: UnboundedReceiver<Command>) {
        let (finished_tx, mut finished_rx) = unbounded_channel();
        let (ended_tx, mut ended_rx) = unbounded_channel();
        self.finished_tx = finished_tx;
        self.ended_tx = ended_tx;
        loop {
            tokio::select! {
                command = rx.recv() => match command {
                    Some(command) => self.handle(command).await,
                    None => break,
                },
                Some(result) = finished_rx.recv() => {
                    self.waiting = None;
                    match result {
                        Ok((instance, token)) => self.finish(instance, token).await,
                        Err(message) => self.emit(Event::SignInFailed(message)),
                    }
                }
                Some(message) = ended_rx.recv() => self.session_ended(message),
            }
        }
    }

    /// The token is gone: forget it and send the person back to sign-in.
    fn session_ended(&mut self, message: String) {
        if let Some(Active::Real(session)) = self.active.take() {
            if let Err(err) = token_store::delete_token(session.instance()) {
                log::warn!("could not remove the token from the keychain: {err:#}");
            }
        }
        self.emit(Event::SessionEnded(message));
    }

    async fn handle(&mut self, command: Command) {
        match command {
            Command::Restore => {
                if std::env::var_os("INLET_DEMO").is_some() {
                    self.active = Some(Active::Demo(mock::MockSession::start(self.events.clone())));
                    return;
                }
                let instance = token_store::last_instance();
                let token = instance.as_deref().and_then(|i| match token_store::load_token(i) {
                    Ok(t) => t,
                    Err(err) => {
                        log::warn!("keychain read failed: {err:#}");
                        None
                    }
                });
                match (instance, token) {
                    (Some(instance), Some(token)) => {
                        let started = match real::discover(&instance).await {
                            Ok(inst) => {
                                RealSession::start(inst, p::Token::new(token), self.events.clone(), self.ended_tx.clone())
                                    .await
                            }
                            Err(err) => Err(err),
                        };
                        match started {
                            Ok(session) => self.active = Some(Active::Real(session)),
                            Err(Failure { message, session_ended }) => {
                                if session_ended {
                                    let _ = token_store::delete_token(&instance);
                                }
                                self.emit(Event::NeedsSignIn { instance: Some(instance) });
                                self.emit(Event::SignInFailed(message));
                            }
                        }
                    }
                    (instance, _) => self.emit(Event::NeedsSignIn { instance }),
                }
            }
            Command::SignInBrowser { instance } => match real::start_handoff(&instance).await {
                Ok((inst, handoff)) => {
                    self.emit(Event::HandoffStarted { url: handoff.browser_url(), code: handoff.display_code() });
                    if let Some(previous) = self.waiting.take() {
                        previous.abort();
                    }
                    let tx = self.finished_tx.clone();
                    self.waiting = Some(tokio::spawn(async move {
                        let _ = tx.send(real::wait_handoff(inst, handoff).await);
                    }));
                }
                Err(err) => self.emit(Event::SignInFailed(err.message)),
            },
            Command::SignInToken { instance, token } => match real::discover(&instance).await {
                Ok(inst) => self.finish(inst, p::Token::new(token.trim())).await,
                Err(err) => self.emit(Event::SignInFailed(err.message)),
            },
            Command::CancelSignIn => {
                if let Some(task) = self.waiting.take() {
                    task.abort();
                }
            }
            Command::SignOut => {
                if let Some(Active::Real(session)) = self.active.take() {
                    let instance = session.instance().to_string();
                    session.close().await;
                    if let Err(err) = token_store::delete_token(&instance) {
                        log::warn!("could not remove the token from the keychain: {err:#}");
                    }
                }
                self.active = None;
                self.emit(Event::SignedOut);
            }
            Command::OpenGuild { guild_id } => match &mut self.active {
                Some(Active::Demo(m)) => m.open_guild(guild_id),
                Some(Active::Real(r)) => r.open_guild(guild_id).await,
                None => {}
            },
            Command::OpenChannel { channel_id } => match &mut self.active {
                Some(Active::Demo(m)) => m.open_channel(channel_id),
                Some(Active::Real(r)) => r.open_channel(channel_id).await,
                None => {}
            },
            Command::Send { channel_id, text } => match &mut self.active {
                Some(Active::Demo(m)) => m.send(channel_id, text),
                Some(Active::Real(r)) => r.send(channel_id, text).await,
                None => {}
            },
        }
    }

    async fn finish(&mut self, instance: p::Instance, token: p::Token) {
        self.active = None;
        match RealSession::start(instance, token, self.events.clone(), self.ended_tx.clone()).await {
            Ok(session) => {
                if let Err(err) = token_store::save_token(session.instance(), session.token().expose_secret()) {
                    log::warn!("could not save the token in the keychain: {err:#}");
                    self.emit(Event::Error(
                        "Signed in, but the system keychain refused the token. You will have to sign in again next time."
                            .into(),
                    ));
                }
                token_store::set_last_instance(session.instance());
                self.active = Some(Active::Real(session));
            }
            Err(err) => self.emit(Event::SignInFailed(err.message)),
        }
    }
}
