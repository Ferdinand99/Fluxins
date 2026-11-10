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
use crate::{mock, real, token_store};

/// Default instance shown on the sign-in screen.
pub const DEFAULT_INSTANCE: &str = "fluxer.opland.net";

/// Origins that select the built-in demo data instead of a real instance.
pub fn is_demo_origin(origin: &str) -> bool {
    matches!(origin.trim(), "demo" | "mock")
}

pub enum Command {
    /// Look for a saved session at start-up.
    Restore,
    SignInBrowser { instance: String },
    SignInPassword { instance: String, email: String, password: String },
    SignInTotp { code: String },
    SignInToken { instance: String, token: String },
    /// Stop a browser sign-in or an email approval that is waiting.
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
    /// The password was right; a TOTP code is needed.
    NeedsTotp,
    /// Something to tell the person while sign-in waits (e.g. "check your email").
    SignInInfo(String),
    SignInFailed(String),
    SignedIn { username: String, instance: String },
    SignedOut,
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

/// The signed-in connection: either the demo data or a real instance.
enum Active {
    Mock(mock::MockSession),
    Real(real::RealSession),
}

/// Result of a sign-in that ran as a background task (browser, email approval).
type Finished = Result<(p::Instance, p::Token), String>;

struct Backend {
    events: UnboundedSender<Event>,
    active: Option<Active>,
    pending_totp: Option<real::PendingTotp>,
    waiting: Option<tokio::task::JoinHandle<()>>,
    finished_tx: TokioSender<Finished>,
    finished_rx: Option<UnboundedReceiver<Finished>>,
}

impl Backend {
    fn new(events: UnboundedSender<Event>) -> Self {
        let (finished_tx, finished_rx) = unbounded_channel();
        Self { events, active: None, pending_totp: None, waiting: None, finished_tx, finished_rx: Some(finished_rx) }
    }

    fn emit(&self, event: Event) {
        let _ = self.events.unbounded_send(event);
    }

    async fn run(mut self, mut rx: UnboundedReceiver<Command>) {
        let mut finished_rx = self.finished_rx.take().expect("receiver");
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
            }
        }
    }

    fn start_mock(&mut self) {
        self.active = Some(Active::Mock(mock::MockSession::start(self.events.clone())));
    }

    /// Runs a waiting sign-in in the background so the UI can still cancel it.
    fn spawn_wait(
        &mut self,
        fut: impl std::future::Future<Output = anyhow::Result<(p::Instance, p::Token)>> + Send + 'static,
    ) {
        if let Some(previous) = self.waiting.take() {
            previous.abort();
        }
        let tx = self.finished_tx.clone();
        self.waiting = Some(tokio::spawn(async move {
            let _ = tx.send(fut.await.map_err(|e| e.to_string()));
        }));
    }

    async fn handle(&mut self, command: Command) {
        match command {
            Command::Restore => {
                if std::env::var_os("INLET_DEMO").is_some() {
                    self.start_mock();
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
                        let started = match real::instance_for_token(&instance).await {
                            Ok(inst) => real::RealSession::start(inst, p::Token::new(token), self.events.clone()).await,
                            Err(err) => Err(err),
                        };
                        match started {
                            Ok(session) => self.active = Some(Active::Real(session)),
                            Err(err) => {
                                log::warn!("saved session did not work: {err:#}");
                                self.emit(Event::NeedsSignIn { instance: Some(instance) });
                                self.emit(Event::SignInFailed(format!("{err:#}")));
                            }
                        }
                    }
                    (instance, _) => self.emit(Event::NeedsSignIn { instance }),
                }
            }
            Command::SignInBrowser { instance } => {
                if is_demo_origin(&instance) {
                    self.start_mock();
                    return;
                }
                match real::start_handoff(&instance).await {
                    Ok((inst, handoff)) => {
                        self.emit(Event::HandoffStarted { url: handoff.browser_url(), code: handoff.display_code() });
                        self.spawn_wait(real::wait_handoff(inst, handoff));
                    }
                    Err(err) => self.emit(Event::SignInFailed(format!("{err:#}"))),
                }
            }
            Command::SignInPassword { instance, email, password } => {
                if is_demo_origin(&instance) {
                    self.start_mock();
                    return;
                }
                let result = real::sign_in_password(&instance, &email, &password).await;
                drop(password);
                self.password_outcome(result).await;
            }
            Command::SignInTotp { code } => match self.pending_totp.take() {
                Some(pending) => match real::sign_in_totp(pending, &code).await {
                    Ok(outcome) => self.password_outcome(Ok(outcome)).await,
                    Err((message, pending)) => {
                        self.pending_totp = pending;
                        self.emit(Event::SignInFailed(message));
                    }
                },
                None => self.emit(Event::SignInFailed("The sign-in expired. Start again.".into())),
            },
            Command::SignInToken { instance, token } => {
                if is_demo_origin(&instance) {
                    self.start_mock();
                    return;
                }
                match real::instance_for_token(&instance).await {
                    Ok(inst) => self.finish(inst, p::Token::new(token.trim())).await,
                    Err(err) => self.emit(Event::SignInFailed(format!("{err:#}"))),
                }
            }
            Command::CancelSignIn => {
                if let Some(task) = self.waiting.take() {
                    task.abort();
                }
                self.pending_totp = None;
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
                Some(Active::Mock(m)) => m.open_guild(guild_id),
                Some(Active::Real(r)) => r.open_guild(guild_id).await,
                None => {}
            },
            Command::OpenChannel { channel_id } => match &mut self.active {
                Some(Active::Mock(m)) => m.open_channel(channel_id),
                Some(Active::Real(r)) => r.open_channel(channel_id).await,
                None => {}
            },
            Command::Send { channel_id, text } => match &mut self.active {
                Some(Active::Mock(m)) => m.send(channel_id, text),
                Some(Active::Real(r)) => r.send(channel_id, text).await,
                None => {}
            },
        }
    }

    async fn password_outcome(&mut self, result: anyhow::Result<real::PasswordOutcome>) {
        match result {
            Ok(real::PasswordOutcome::Done(instance, token)) => self.finish(instance, token).await,
            Ok(real::PasswordOutcome::NeedsTotp(pending)) => {
                self.pending_totp = Some(pending);
                self.emit(Event::NeedsTotp);
            }
            Ok(real::PasswordOutcome::IpAuthorization(instance, ticket)) => {
                self.emit(Event::SignInInfo(
                    "New device: Fluxer sent you an email. Open the link in it to approve this sign-in. Inlet waits here."
                        .into(),
                ));
                self.spawn_wait(real::wait_ip_authorization(instance, ticket));
            }
            Err(err) => self.emit(Event::SignInFailed(format!("{err:#}"))),
        }
    }

    async fn finish(&mut self, instance: p::Instance, token: p::Token) {
        self.active = None;
        match real::RealSession::start(instance, token, self.events.clone()).await {
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
            Err(err) => self.emit(Event::SignInFailed(format!("{err:#}"))),
        }
    }
}
