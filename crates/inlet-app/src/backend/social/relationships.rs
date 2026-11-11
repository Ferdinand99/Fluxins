// SPDX-License-Identifier: AGPL-3.0-or-later
//! Friends, friend requests, blocks and the DM list's actions: open a DM, close it, pin
//! it, and create, edit, add to and leave group DMs. The results come back as gateway
//! events (RELATIONSHIP_*, CHANNEL_*, USER_PINNED_DMS_UPDATE) that the account state
//! applies; this domain only answers whether a call went through.
//!
//! Budget (every call also goes through the protocol's limiter):
//! - One call per target at a time ([`Gate`]): a second click on the same person or DM
//!   while the first call runs sends nothing.
//! - At most [`WRITES_PER_WINDOW`] relationship and DM writes per [`WRITE_WINDOW`] per
//!   account; more are refused at once, without a request.
//! - Group DMs: Fluxer allows 10 creations and 10 additions per hour per account, every
//!   attempt counts, and one call can use up to three (the CAPTCHA). Inlet makes at most
//!   [`GROUP_CALLS_PER_HOUR`] create calls and as many add calls per hour per account
//!   (at most 9 requests of each), and none at all while the server's `retry_after`
//!   runs. A CAPTCHA it cannot pass or a rate limit ends the call: the UI shows a
//!   lasting message, and nothing is tried again on its own.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use inlet_protocol as p;
use tokio::sync::mpsc::UnboundedSender as TokioSender;

use super::super::accounts::AccountKey;
use super::super::session::{Failure, FailureKind};
use super::super::{Active, Backend, Internal};

/// Relationship and DM writes per window and account.
pub const WRITES_PER_WINDOW: usize = 10;
/// The window of [`WRITES_PER_WINDOW`].
pub const WRITE_WINDOW: Duration = Duration::from_secs(10);
/// Group DM create calls (and, apart, add calls) per hour and account.
pub const GROUP_CALLS_PER_HOUR: usize = 3;
/// The window of [`GROUP_CALLS_PER_HOUR`].
pub const GROUP_WINDOW: Duration = Duration::from_secs(3600);
/// Largest group DM icon read from disk (Fluxer takes up to 10 MiB).
pub const MAX_ICON_BYTES: u64 = 10 * 1024 * 1024;

/// A new icon for a group DM.
#[derive(Clone, Debug, PartialEq)]
pub enum Icon {
    /// The picture in this file (picked by the person).
    File(PathBuf),
    Remove,
}

/// Commands of the relationships part of the social domain.
#[derive(Clone, Debug, PartialEq)]
pub enum Cmd {
    /// A friend request by username (and tag, on instances that have them).
    AddFriend {
        username: String,
        discriminator: Option<String>,
    },
    /// Accepts a friend request.
    Accept {
        user_id: String,
    },
    /// Ends a friendship, ignores or cancels a request, or unblocks.
    Remove {
        user_id: String,
    },
    Block {
        user_id: String,
    },
    /// Opens (or finds) the DM with someone; [`Ev::DmOpened`] answers.
    OpenDm {
        user_id: String,
    },
    /// A new group DM with these friends.
    CreateGroupDm {
        recipients: Vec<String>,
    },
    AddRecipient {
        channel_id: String,
        user_id: String,
    },
    RemoveRecipient {
        channel_id: String,
        user_id: String,
    },
    /// Closes a DM or leaves a group DM (`kind` is the channel's type).
    CloseDm {
        channel_id: String,
        kind: i64,
    },
    /// Renames a group DM (`Some(None)` clears the name) or changes its icon.
    EditGroupDm {
        channel_id: String,
        name: Option<Option<String>>,
        icon: Option<Icon>,
    },
    PinDm {
        channel_id: String,
        pin: bool,
    },
}

impl Cmd {
    /// What the UI marks as busy while the call runs, and what the answer names.
    pub fn key(&self) -> String {
        match self {
            Cmd::AddFriend { .. } => "add-friend".into(),
            Cmd::Accept { user_id } | Cmd::Remove { user_id } | Cmd::Block { user_id } => {
                format!("user:{user_id}")
            }
            Cmd::OpenDm { user_id } => format!("open:{user_id}"),
            Cmd::CreateGroupDm { .. } => "group-dm".into(),
            Cmd::AddRecipient { channel_id, .. }
            | Cmd::RemoveRecipient { channel_id, .. }
            | Cmd::EditGroupDm { channel_id, .. } => format!("group:{channel_id}"),
            Cmd::CloseDm { channel_id, .. } | Cmd::PinDm { channel_id, .. } => {
                format!("dm:{channel_id}")
            }
        }
    }

    fn budget(&self) -> Budget {
        match self {
            Cmd::CreateGroupDm { .. } => Budget::GroupCreate,
            Cmd::AddRecipient { .. } => Budget::GroupAdd,
            _ => Budget::Write,
        }
    }
}

/// How a group DM create or add ended.
#[derive(Clone, Debug, PartialEq)]
pub enum GroupDm {
    /// Done (a created group opens through [`Ev::DmOpened`]).
    Done,
    /// The instance wants a CAPTCHA Inlet could not pass: do it in the browser.
    Captcha,
    /// Over Fluxer's limit (or Inlet's): wait this long.
    RateLimited {
        retry_after: Duration,
    },
    /// These people cannot be added (user ID and Fluxer's reason).
    NotAddable(Vec<(String, String)>),
    Failed(String),
}

/// Events of the relationships part of the social domain.
#[derive(Clone, Debug, PartialEq)]
pub enum Ev {
    /// The call named `key` went through.
    Done { key: String },
    /// The call named `key` failed; `reason` says why.
    Failed { key: String, reason: String },
    /// A DM is open (or a group DM was created): the UI shows it. The channel comes with
    /// it, so the state and the list know it before the gateway's CHANNEL_CREATE.
    DmOpened {
        key: String,
        channel: Box<p::Channel>,
    },
    /// The answer to a group DM create or add (`key` as in [`Cmd::key`]).
    GroupDm { key: String, result: GroupDm },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Budget {
    Write,
    GroupCreate,
    GroupAdd,
}

/// Why a call may not start now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The same call runs already.
    Busy,
    /// Too many writes just now.
    TooFast,
    /// The group DM budget is used up, or the server asked to wait: free in `wait`.
    GroupLimit { wait: Duration },
}

/// One account's budget for these calls.
#[derive(Default, Debug)]
pub struct Gate {
    busy: HashSet<String>,
    writes: VecDeque<Instant>,
    creates: VecDeque<Instant>,
    adds: VecDeque<Instant>,
    /// The server's `retry_after` for group DM calls ends here.
    group_wait_until: Option<Instant>,
}

fn take_slot(
    sent: &mut VecDeque<Instant>,
    max: usize,
    window: Duration,
    now: Instant,
) -> Result<(), Duration> {
    while sent
        .front()
        .is_some_and(|t| now.saturating_duration_since(*t) >= window)
    {
        sent.pop_front();
    }
    if sent.len() >= max {
        return Err((sent[0] + window).saturating_duration_since(now));
    }
    sent.push_back(now);
    Ok(())
}

impl Gate {
    /// Whether the call named `key` may start at `now`; taking it uses a slot.
    fn admit(&mut self, key: &str, budget: Budget, now: Instant) -> Result<(), Refusal> {
        if self.busy.contains(key) {
            return Err(Refusal::Busy);
        }
        match budget {
            Budget::Write => take_slot(&mut self.writes, WRITES_PER_WINDOW, WRITE_WINDOW, now)
                .map_err(|_| Refusal::TooFast)?,
            Budget::GroupCreate | Budget::GroupAdd => {
                if let Some(until) = self.group_wait_until.filter(|u| *u > now) {
                    return Err(Refusal::GroupLimit {
                        wait: until.saturating_duration_since(now),
                    });
                }
                let sent = if budget == Budget::GroupCreate {
                    &mut self.creates
                } else {
                    &mut self.adds
                };
                take_slot(sent, GROUP_CALLS_PER_HOUR, GROUP_WINDOW, now)
                    .map_err(|wait| Refusal::GroupLimit { wait })?;
            }
        }
        self.busy.insert(key.to_owned());
        Ok(())
    }

    fn finish(&mut self, key: &str) {
        self.busy.remove(key);
    }

    /// The server said to wait `wait` before the next group DM call.
    fn group_rate_limited(&mut self, now: Instant, wait: Duration) {
        let until = now + wait;
        self.group_wait_until = Some(self.group_wait_until.map_or(until, |u| u.max(until)));
    }
}

/// The gates of every account, by account (at most a handful of entries).
static GATES: LazyLock<Mutex<HashMap<AccountKey, Gate>>> = LazyLock::new(Default::default);

fn with_gate<T>(account: &AccountKey, f: impl FnOnce(&mut Gate) -> T) -> T {
    let mut gates = GATES.lock().unwrap_or_else(|e| e.into_inner());
    f(gates.entry(account.clone()).or_default())
}

/// The sentence for a refusal.
pub fn refusal_text(r: &Refusal) -> String {
    match r {
        Refusal::Busy => t!("social-busy"),
        Refusal::TooFast => t!("social-too-fast"),
        Refusal::GroupLimit { wait } => t!("social-group-dm-limit", wait = wait_text(*wait)),
    }
}

/// A wait as people say it: seconds, minutes or hours, rounded up.
pub fn wait_text(wait: Duration) -> String {
    let secs = wait.as_secs().max(1);
    if secs < 60 {
        t!("social-wait-seconds", count = secs)
    } else if secs < 3600 {
        t!("social-wait-minutes", count = secs.div_ceil(60))
    } else {
        t!("social-wait-hours", count = secs.div_ceil(3600))
    }
}

/// A failed call: the session ends on an auth failure (`None`), anything else is the
/// sentence to show.
fn failure(internal: &TokioSender<Internal>, gen: u64, err: p::Error) -> Option<String> {
    let failure = Failure::from(err);
    if failure.kind == FailureKind::SessionEnded {
        let _ = internal.send(Internal::Ended {
            gen,
            message: failure.message,
        });
        return None;
    }
    Some(failure.message)
}

/// A group DM call's outcome as the UI shows it; other errors stay errors.
pub fn group_result<T>(
    result: p::Result<p::GroupDmOutcome<T>>,
) -> Result<(GroupDm, Option<T>), p::Error> {
    match result {
        Ok(p::GroupDmOutcome::Done(v)) => Ok((GroupDm::Done, Some(v))),
        Ok(p::GroupDmOutcome::Captcha) => Ok((GroupDm::Captcha, None)),
        Ok(p::GroupDmOutcome::NotAddable { unaddable, .. }) => Ok((
            GroupDm::NotAddable(
                unaddable
                    .into_iter()
                    .map(|u| (u.user_id, u.reason))
                    .collect(),
            ),
            None,
        )),
        Ok(_) => Ok((GroupDm::Failed(t!("social-group-dm-failed")), None)),
        Err(p::Error::RateLimited { retry_after }) => {
            Ok((GroupDm::RateLimited { retry_after }, None))
        }
        Err(err) => Err(err),
    }
}

/// Reads a picked icon file, within [`MAX_ICON_BYTES`], with its type from its bytes.
fn read_icon(path: &PathBuf) -> Result<p::ImageData, String> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    file.take(MAX_ICON_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_ICON_BYTES {
        return Err(t!("social-icon-too-large"));
    }
    let kind = if bytes.starts_with(b"\x89PNG") {
        "image/png"
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF8") {
        "image/gif"
    } else if bytes.len() > 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else {
        return Err(t!("social-icon-not-an-image"));
    };
    p::ImageData::new(kind, bytes).map_err(|e| e.to_string())
}

/// A channel the protocol's DM calls take: its ID and type are all they check.
fn channel_of(id: &str, kind: i64) -> Option<p::Channel> {
    serde_json::from_value(serde_json::json!({"id": id, "type": kind})).ok()
}

pub(in crate::backend) fn handle(backend: &mut Backend, cmd: Cmd) {
    let key = cmd.key();
    let (client, gen, account, events) = match &backend.active {
        Some(Active::Real(r)) => (
            r.session.client().clone(),
            r.gen,
            r.key.clone(),
            r.events.clone(),
        ),
        #[cfg(feature = "demo")]
        Some(Active::Demo(_)) => {
            // The demo has no server: nothing changes, the call just ends.
            let _ = backend
                .events
                .unbounded_send(super::Ev::Relationships(Ev::Done { key }));
            return;
        }
        _ => {
            let _ = backend
                .events
                .unbounded_send(super::Ev::Relationships(Ev::Failed {
                    key,
                    reason: t!("social-not-connected"),
                }));
            return;
        }
    };
    if let Err(refusal) = with_gate(&account, |g| g.admit(&key, cmd.budget(), Instant::now())) {
        let ev = match refusal {
            // A second click while the first runs: the first one answers.
            Refusal::Busy => return,
            Refusal::GroupLimit { wait } => Ev::GroupDm {
                key,
                result: GroupDm::RateLimited { retry_after: wait },
            },
            other => Ev::Failed {
                key,
                reason: refusal_text(&other),
            },
        };
        let _ = events.unbounded_send(super::Ev::Relationships(ev));
        return;
    }
    let internal = backend.internal.clone();
    backend.spawn_guarded("social action", None, async move {
        let ev = run(&client, &internal, gen, &account, &key, cmd).await;
        with_gate(&account, |g| g.finish(&key));
        let _ = events.unbounded_send(super::Ev::Relationships(ev));
    });
}

/// Runs one admitted call and says how it ended.
async fn run(
    client: &p::Client,
    internal: &TokioSender<Internal>,
    gen: u64,
    account: &AccountKey,
    key: &str,
    cmd: Cmd,
) -> Ev {
    let key = key.to_owned();
    let failed = |err: p::Error, key: String| match failure(internal, gen, err) {
        Some(reason) => Ev::Failed { key, reason },
        // The session ended: the sign-in screen says why.
        None => Ev::Done { key },
    };
    let done = |r: p::Result<()>, key: String| match r {
        Ok(()) => Ev::Done { key },
        Err(err) => failed(err, key),
    };
    let opened = |channel: p::Channel, key: String| Ev::DmOpened {
        key,
        channel: Box::new(channel),
    };
    match cmd {
        Cmd::AddFriend {
            username,
            discriminator,
        } => done(
            client
                .send_friend_request_by_tag(&username, discriminator.as_deref())
                .await
                .map(drop),
            key,
        ),
        Cmd::Accept { user_id } => {
            done(client.accept_friend_request(&user_id).await.map(drop), key)
        }
        Cmd::Remove { user_id } => done(client.remove_relationship(&user_id).await, key),
        Cmd::Block { user_id } => done(client.block_user(&user_id).await.map(drop), key),
        Cmd::OpenDm { user_id } => match client.open_dm(&user_id).await {
            Ok(channel) => opened(channel, key),
            Err(err) => failed(err, key),
        },
        Cmd::CreateGroupDm { recipients } => {
            match group_result(client.create_group_dm(&recipients).await) {
                Ok((_, Some(channel))) => opened(channel, key),
                Ok((result, None)) => group_answer(account, key, result),
                Err(err) => group_failed(internal, gen, key, err),
            }
        }
        Cmd::AddRecipient {
            channel_id,
            user_id,
        } => match group_result(client.add_group_dm_recipient(&channel_id, &user_id).await) {
            Ok((result, _)) => group_answer(account, key, result),
            Err(err) => group_failed(internal, gen, key, err),
        },
        Cmd::RemoveRecipient {
            channel_id,
            user_id,
        } => done(
            client
                .remove_group_dm_recipient(&channel_id, &user_id, false)
                .await,
            key,
        ),
        Cmd::CloseDm { channel_id, kind } => match channel_of(&channel_id, kind) {
            Some(ch) => done(client.close_dm(&ch, false).await, key),
            None => Ev::Failed {
                key,
                reason: t!("social-not-a-dm"),
            },
        },
        Cmd::EditGroupDm {
            channel_id,
            name,
            icon,
        } => {
            let icon = match icon {
                None => None,
                Some(Icon::Remove) => Some(None),
                Some(Icon::File(path)) => {
                    match tokio::task::spawn_blocking(move || read_icon(&path)).await {
                        Ok(Ok(data)) => Some(Some(data)),
                        Ok(Err(reason)) => return Ev::Failed { key, reason },
                        Err(_) => {
                            return Ev::Failed {
                                key,
                                reason: t!("social-icon-not-an-image"),
                            }
                        }
                    }
                }
            };
            let edit = p::GroupDmEdit {
                name,
                icon,
                owner_id: None,
            };
            match channel_of(&channel_id, p::models::channel_type::GROUP_DM) {
                Some(ch) => done(client.edit_group_dm(&ch, &edit).await.map(drop), key),
                None => Ev::Failed {
                    key,
                    reason: t!("social-not-a-dm"),
                },
            }
        }
        Cmd::PinDm { channel_id, pin } => done(
            if pin {
                client.pin_dm(&channel_id).await
            } else {
                client.unpin_dm(&channel_id).await
            },
            key,
        ),
    }
}

/// A group DM answer; a rate limit also closes the account's group DM calls until the
/// wait is over.
fn group_answer(account: &AccountKey, key: String, result: GroupDm) -> Ev {
    if let GroupDm::RateLimited { retry_after } = &result {
        with_gate(account, |g| {
            g.group_rate_limited(Instant::now(), *retry_after)
        });
    }
    Ev::GroupDm { key, result }
}

fn group_failed(internal: &TokioSender<Internal>, gen: u64, key: String, err: p::Error) -> Ev {
    let reason = failure(internal, gen, err).unwrap_or_default();
    Ev::GroupDm {
        key,
        result: GroupDm::Failed(reason),
    }
}

#[cfg(test)]
mod tests;
