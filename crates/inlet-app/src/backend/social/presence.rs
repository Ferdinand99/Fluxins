// SPDX-License-Identifier: AGPL-3.0-or-later
//! Presence and status: this session's presence (Presence Update, op 3), the status and
//! custom status saved on the account, and what an account in the background shows.
//! Typing is sent by the composer (`backend/composer`), at most once per channel every
//! 8 s through the protocol.
//!
//! Budget (Fluxer drops more than 5 Presence Updates per 20 s on a connection, and
//! allows 20 `PATCH /users/@me/settings` per 10 s):
//! - op 3: the UI asks for one only when the status it shows changes (a pick, idle or
//!   back, an account switch). The protocol sends at most 5 per 20 s per connection and
//!   replaces one that waits with the newest, so a burst ends with the last status.
//! - `PATCH /users/@me/settings` (status, custom status): only the newest wish is
//!   written, at most 5 per 20 s per account ([`SAVES_PER_WINDOW`]), through the
//!   protocol's limiter. Every pick is a new wish, even one equal to what Inlet wrote
//!   before (another client may have changed the status since). A failed write is
//!   reported once and not retried by itself. A wish that waits for the budget when the
//!   account goes to the background is still written from there.
//! - When the last wish is written, the server's answer goes to the UI
//!   ([`Ev::Saved`]), which shows the pick until then; a failure or a pick that cannot
//!   run (no session, another account) answers [`Ev::SaveFailed`].
//! - An account in the background is `invisible` and `afk`: its gateway starts with
//!   that (`gateway_options`), and an account that goes to the background sends it once.
//!   After that it sends nothing: no idle changes and no picks reach it, because a
//!   command for an account that is not active is dropped.

use std::collections::VecDeque;
use std::time::Duration;

use inlet_protocol as p;
use p::models::CustomStatus;
use tokio::sync::mpsc::UnboundedSender as TokioSender;
use tokio::sync::watch;
use tokio::task::AbortHandle;
use tokio::time::Instant;

use super::super::{report, Active, Backend, Internal, RealActive, Ui};

/// The status an account in the background shows (Fluxer's background sessions do the same).
pub const BACKGROUND_STATUS: &str = "invisible";
/// An account in the background reports `afk`: nobody looks at it, so Fluxer still
/// sends its mentions to the phone.
// Deviation: Fluxer's client sends `afk: false` when it retires a session on an account
// switch. With `afk`, a mention in a background account can notify both the phone and
// the desktop (Inlet's own notification); without it the phone gets nothing while Inlet
// runs.
pub const BACKGROUND_AFK: bool = true;
/// Settings writes per window and account.
pub const SAVES_PER_WINDOW: usize = 5;
/// The window of [`SAVES_PER_WINDOW`].
pub const SAVE_WINDOW: Duration = Duration::from_secs(20);

/// Commands of the presence part of the social domain.
#[derive(Clone, Debug, PartialEq)]
pub enum Cmd {
    /// This session's presence: the status shown (the chosen one, or `idle` while the
    /// person is away) and whether they are away (`afk`).
    Presence { status: String, afk: bool },
    /// Saves the chosen status on the account (`online`, `idle`, `dnd`, `invisible`).
    SaveStatus { status: String },
    /// Saves the custom status, or clears it with `None`.
    SaveCustomStatus { custom: Option<CustomStatus> },
}

/// Events of the presence part of the social domain.
#[derive(Clone, Debug, PartialEq)]
pub enum Ev {
    /// A status or custom status was not saved (the reason is already in the notice
    /// line), or the pick could not run. The UI shows the saved status again.
    SaveFailed,
    /// Every wish is written; the server's settings after the last write.
    Saved(Box<p::UserSettings>),
}

/// The presence state of one account, kept with the account (see `RealActive`).
#[derive(Default)]
pub struct Presence {
    /// The presence the UI asked for last while the account was active; a new gateway
    /// of the active account starts with it.
    wanted: Option<(String, bool)>,
    /// The newest settings wish, read by the account's write task.
    saves: Option<watch::Sender<Save>>,
    task: Option<AbortHandle>,
}

impl Drop for Presence {
    fn drop(&mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
        }
    }
}

impl Presence {
    /// The account went to the background: it shows [`BACKGROUND_STATUS`]. Sent once;
    /// the gateway remembers it for reconnects.
    pub fn to_background(&self, gateway: &p::GatewayHandle) {
        gateway.set_presence(BACKGROUND_STATUS, BACKGROUND_AFK);
    }

    /// The account is in front again, or has a new gateway: the status the UI asked for
    /// last, if any (a new gateway without one identifies with the account's saved
    /// status; a gateway that was in the background leaves `invisible` this way).
    pub fn to_front(&self, gateway: &p::GatewayHandle) {
        if let Some((status, afk)) = &self.wanted {
            gateway.set_presence(status, *afk);
        }
    }
}

/// What the account's settings should say. `None` fields are not asked for. Each pick
/// counts up its field's number, so a pick equal to an earlier one is still a new wish.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Save {
    status: Option<String>,
    custom: Option<Option<CustomStatus>>,
    status_pick: u64,
    custom_pick: u64,
}

impl Save {
    fn merge(&mut self, other: Save) {
        if other.status.is_some() {
            self.status = other.status;
            self.status_pick += 1;
        }
        if other.custom.is_some() {
            self.custom = other.custom;
            self.custom_pick += 1;
        }
    }
}

/// What the write task does next.
#[derive(Debug, PartialEq)]
pub enum Next {
    /// Nothing waits.
    Idle,
    /// A write waits for the budget this long.
    Wait(Duration),
    /// Write this now.
    Write(p::UserSettingsUpdate),
}

/// Writes only the newest wish, at most [`SAVES_PER_WINDOW`] per [`SAVE_WINDOW`].
#[derive(Default)]
pub struct Coalescer {
    wanted: Save,
    written: Save,
    /// What counted as written before the write in flight (see [`Coalescer::failed`]).
    before: Save,
    sent: VecDeque<Instant>,
}

impl Coalescer {
    /// The newest wish (it replaces anything not written yet).
    pub fn want(&mut self, wanted: Save) {
        self.wanted = wanted;
    }

    /// The fields picked since the last write.
    fn unwritten(&self) -> p::UserSettingsUpdate {
        let mut update = p::UserSettingsUpdate::default();
        if self.wanted.status.is_some() && self.wanted.status_pick != self.written.status_pick {
            update.status = self.wanted.status.clone();
        }
        if self.wanted.custom.is_some() && self.wanted.custom_pick != self.written.custom_pick {
            update.custom_status = self.wanted.custom.clone();
        }
        update
    }

    /// Whether a picked field waits to be written.
    pub fn has_unwritten(&self) -> bool {
        self.unwritten() != p::UserSettingsUpdate::default()
    }

    /// The write due at `now`, if any. Taking a write uses a slot of the budget, and the
    /// wish counts as written (a failed write is not retried by itself).
    pub fn next(&mut self, now: Instant) -> Next {
        let update = self.unwritten();
        if update == p::UserSettingsUpdate::default() {
            return Next::Idle;
        }
        while self
            .sent
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) >= SAVE_WINDOW)
        {
            self.sent.pop_front();
        }
        if self.sent.len() >= SAVES_PER_WINDOW {
            let free_at = self.sent[0] + SAVE_WINDOW;
            return Next::Wait(free_at.saturating_duration_since(now));
        }
        self.sent.push_back(now);
        self.before = std::mem::replace(&mut self.written, self.wanted.clone());
        Next::Write(update)
    }

    /// The last write failed: what it carried does not count as written, so the next
    /// wish writes it again, even when it asks for the same. It is not retried on its
    /// own.
    pub fn failed(&mut self) {
        self.written = std::mem::take(&mut self.before);
    }
}

/// The write task of one account: waits for wishes and writes them within the budget.
async fn write_task(
    client: p::Client,
    events: Ui,
    internal: TokioSender<Internal>,
    gen: u64,
    mut rx: watch::Receiver<Save>,
) {
    let mut c = Coalescer::default();
    loop {
        c.want(rx.borrow_and_update().clone());
        match c.next(Instant::now()) {
            Next::Write(update) => match client.update_user_settings(&update).await {
                Ok(settings) => {
                    // A newer wish may have come meanwhile: the UI keeps showing it
                    // until that one is written too.
                    c.want(rx.borrow().clone());
                    if !c.has_unwritten() {
                        let _ = events
                            .unbounded_send(super::Ev::Presence(Ev::Saved(Box::new(settings))));
                    }
                }
                Err(err) => {
                    c.failed();
                    report(
                        &events,
                        &internal,
                        gen,
                        |reason| t!("presence-save-failed", reason = reason),
                        err,
                    );
                    let _ = events.unbounded_send(super::Ev::Presence(Ev::SaveFailed));
                    // No retry of its own: wait for the next wish.
                    if rx.changed().await.is_err() {
                        return;
                    }
                }
            },
            Next::Wait(wait) => {
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {}
                    changed = rx.changed() => if changed.is_err() { return },
                }
            }
            Next::Idle => {
                if rx.changed().await.is_err() {
                    return;
                }
            }
        }
    }
}

pub(in crate::backend) fn handle(backend: &mut Backend, cmd: Cmd) {
    let internal = backend.internal.clone();
    #[cfg(feature = "demo")]
    if let Some(Active::Demo(_)) = &backend.active {
        // The demo has no server: the UI already shows the change.
        return;
    }
    let Some(Active::Real(r)) = &mut backend.active else {
        // Not signed in, or the command was for an account that is not active: nothing
        // is sent (an account in the background keeps its status). A pick is answered,
        // tagged with its account, so its state shows the saved status again.
        if !matches!(cmd, Cmd::Presence { .. }) {
            let _ = backend
                .events
                .unbounded_send(super::Ev::Presence(Ev::SaveFailed));
        }
        return;
    };
    match cmd {
        Cmd::Presence { status, afk } => {
            let status = inlet_state::presence::normalize(&status).to_owned();
            r.gateway.set_presence(&status, afk);
            r.presence.wanted = Some((status, afk));
        }
        Cmd::SaveStatus { status } => {
            let status = inlet_state::presence::normalize(&status).to_owned();
            save(
                r,
                internal,
                Save {
                    status: Some(status),
                    ..Save::default()
                },
            );
        }
        Cmd::SaveCustomStatus { custom } => save(
            r,
            internal,
            Save {
                custom: Some(custom.map(clean_custom)),
                ..Save::default()
            },
        ),
    }
}

/// Hands a wish to the account's write task (started on the first one).
fn save(r: &mut RealActive, internal: TokioSender<Internal>, wish: Save) {
    let p = &mut r.presence;
    match &p.saves {
        Some(tx) => tx.send_modify(|s| s.merge(wish)),
        None => {
            let mut first = Save::default();
            first.merge(wish);
            let (tx, rx) = watch::channel(first);
            let task = tokio::spawn(write_task(
                r.session.client().clone(),
                r.events.clone(),
                internal,
                r.gen,
                rx,
            ));
            p.task = Some(task.abort_handle());
            p.saves = Some(tx);
        }
    }
}

/// A custom status as Fluxer takes it: trimmed text of at most 128 UTF-16 units, and no
/// empty emoji name.
fn clean_custom(mut c: CustomStatus) -> CustomStatus {
    use inlet_state::presence::{truncate_utf16, CUSTOM_STATUS_TEXT_LIMIT};
    c.text = c
        .text
        .map(|t| truncate_utf16(t.trim(), CUSTOM_STATUS_TEXT_LIMIT).to_owned())
        .filter(|t| !t.is_empty());
    c.emoji_name = c.emoji_name.filter(|e| !e.trim().is_empty());
    c
}

#[cfg(test)]
mod tests;
