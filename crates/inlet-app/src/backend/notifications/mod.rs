// SPDX-License-Identifier: AGPL-3.0-or-later
//! The network side of unread state and notification settings: sending read
//! acknowledgements and writing settings. What is read, what is unread and what the
//! settings are is decided on the UI thread, from the one account state (`ui/notifications`);
//! this domain only talks to the server.
//!
//! Request budget (all through the protocol's limiter, which also coalesces
//! acknowledgements into one batched request per second for the account, up to 100
//! channels per request):
//!
//! - Acknowledgements: `Cmd::Ack` hands each channel and message to `Client::ack`. The UI
//!   side decides when (at most one per channel per 3 s for reading, at once for "mark as
//!   read"), and never sends one for a channel that is already read.
//! - Settings: one `PATCH` per `Cmd::WriteSettings`; the UI keeps at most one in flight
//!   per guild. Each write carries a [`WriteId`] that its answer echoes, so the UI can
//!   tell an answer to the write it waits for from a late one. Signing out aborts the
//!   writes still running (`Accounts::teardown`), so no answer reaches the next account.

pub mod alerts;
pub mod sound;

use inlet_protocol as p;
use inlet_state::GuildId;
use serde_json::Value;

use super::session::Failure;
use super::{Active, Backend};

/// Which settings write an answer belongs to: the UI's sign-in session (counted up at
/// every sign-out) and the write's number within it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WriteId {
    pub session: u64,
    pub seq: u64,
}

/// Commands of this domain.
pub enum Cmd {
    /// Channels read up to a message: `(channel ID, message ID)`.
    Ack { items: Vec<(String, String)> },
    /// A settings write for a guild (`None`: the direct messages).
    WriteSettings {
        id: WriteId,
        guild_id: Option<String>,
        body: Value,
    },
}

/// Events of this domain.
pub enum Ev {
    /// A settings write ended: the settings the server answered with, or why it failed.
    SettingsWritten {
        id: WriteId,
        guild_id: Option<String>,
        result: Result<Value, String>,
    },
}

/// The gateway event that carries a guild's notification settings.
pub const SETTINGS_UPDATE: &str = "USER_GUILD_SETTINGS_UPDATE";

/// The route of a guild's (or the direct messages') settings.
pub fn settings_route(guild: Option<GuildId>) -> String {
    match guild {
        Some(g) => format!("/users/@me/guilds/{g}/settings"),
        None => "/users/@me/guilds/@me/settings".to_string(),
    }
}

/// Hands the acknowledgements to the protocol, which sends them as batches.
pub fn send_acks(client: &p::Client, items: &[(String, String)]) {
    for (channel, message) in items {
        client.ack(channel, message);
    }
}

/// One settings `PATCH`.
pub async fn write_settings(
    client: &p::Client,
    guild: Option<GuildId>,
    body: &Value,
) -> Result<Value, String> {
    client
        .request_json(p::Method::PATCH, &settings_route(guild), Some(body))
        .await
        .map_err(|err| Failure::from(err).message)
}

pub(super) fn handle(backend: &mut Backend, cmd: Cmd) {
    match cmd {
        Cmd::Ack { items } => {
            if let Some(Active::Real(r)) = &backend.active {
                send_acks(r.session.client(), &items);
            }
        }
        Cmd::WriteSettings { id, guild_id, body } => {
            // Only a guild ID that is one; anything else is answered with a failure, as
            // the UI waits for an answer to every write.
            let guild = match guild_id.as_deref() {
                None => Some(None),
                Some(g) => GuildId::parse(g).map(Some),
            };
            let events = backend.events.clone();
            let answer = move |result: Result<Value, String>| {
                let _ = events.unbounded_send(Ev::SettingsWritten {
                    id,
                    guild_id,
                    result,
                });
            };
            match (&backend.active, guild) {
                (Some(Active::Real(r)), Some(guild)) => {
                    let client = r.session.client().clone();
                    let task = backend.spawn_guarded("write settings", None, async move {
                        answer(write_settings(&client, guild, &body).await);
                    });
                    // Kept with the session, so signing out aborts it before it answers.
                    if let Some(Active::Real(r)) = &mut backend.active {
                        r.settings_writes.retain(|t| !t.is_finished());
                        r.settings_writes.push(task);
                    }
                }
                #[cfg(feature = "demo")]
                (Some(Active::Demo(_)), Some(_)) => answer(Ok(Value::Null)),
                _ => answer(Err(t!("error-settings-not-saved"))),
            }
        }
    }
}

#[cfg(test)]
mod tests;
