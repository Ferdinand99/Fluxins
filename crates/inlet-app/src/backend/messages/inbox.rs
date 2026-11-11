// SPDX-License-Identifier: AGPL-3.0-or-later
//! The panels over the chat on the network side: the inbox (recent mentions and saved
//! messages), the pinned messages of a channel (`pins.rs`), and the page around a message
//! that a jump loads. Every request goes through the account's limiter, and every load is
//! answered exactly once ([`Answer`]), so the UI never waits for a lost request.
//!
//! Request budget (on top of the protocol's limiter), kept by the UI (`ui/inbox`):
//!
//! - A list (pins of a channel, mentions, saved messages) loads its first page when its
//!   panel opens and nothing fresh is cached; one request per list at a time and at most
//!   one a second, also for "load more" and for reloads after gateway events.
//! - A jump loads one page around the message when it is not in the open window; one
//!   jump request at a time.
//! - Removing a mention: one request per click, none while the same one is on its way.
//! - Marking pins as seen: one request when the pins panel opens on unseen pins.

use inlet_protocol as p;

use super::parse::{self, SharedNames};
use super::Ev as MessagesEv;
use crate::backend::session::{Failure, FailureKind};
use crate::backend::{Active, Backend, Internal, Ui};
use crate::model::Message;

/// Recent mentions per page (Fluxer's inbox asks for 25).
pub const MENTIONS_PAGE: u8 = 25;
/// Saved messages per page.
pub const SAVED_PAGE: u8 = 25;
/// Pins per page (the most the API gives).
pub const PINS_PAGE: u8 = 50;
/// Messages in the page around a message a jump loads (half before, half after).
pub const AROUND_PAGE: u8 = 50;

pub enum Cmd {
    /// A page of the channel's pins, newest first; `before` is the `pinned_at` of the
    /// last pin seen.
    Pins {
        channel_id: String,
        before: Option<String>,
        request: u64,
    },
    /// The account looked at the channel's pins.
    AckPins { channel_id: String },
    /// A page of recent mentions, below the message `before`.
    Mentions {
        before: Option<String>,
        everyone: bool,
        roles: bool,
        request: u64,
    },
    /// Removes a message from the recent mentions.
    DeleteMention { message_id: String },
    /// A page of saved messages, below the message `before`.
    Saved {
        before: Option<String>,
        request: u64,
    },
    /// The page around a message, for a jump.
    Around {
        channel_id: String,
        message_id: String,
        request: u64,
    },
}

/// A message in a panel, converted and parsed here, off the UI thread.
#[derive(Clone, Debug)]
pub struct Item {
    pub message: Message,
}

/// A saved message, or one the account can no longer read.
#[derive(Clone, Debug)]
pub enum SavedEntry {
    Shown(Box<Item>),
    Missing {
        channel_id: String,
        message_id: String,
    },
}

impl SavedEntry {
    pub fn message_id(&self) -> &str {
        match self {
            SavedEntry::Shown(item) => &item.message.id,
            SavedEntry::Missing { message_id, .. } => message_id,
        }
    }
}

pub enum Ev {
    Pins {
        channel_id: String,
        request: u64,
        items: Vec<Item>,
        /// The `before` for the next page, when there are older pins.
        next: Option<String>,
    },
    Mentions {
        request: u64,
        items: Vec<Item>,
        more: bool,
    },
    Saved {
        request: u64,
        entries: Vec<SavedEntry>,
        more: bool,
    },
    /// The page around a message, with its messages converted for the message list.
    Around {
        channel_id: String,
        request: u64,
        messages: Vec<p::Message>,
        rendered: Vec<Message>,
    },
    /// A load failed (`reason`), or was given up without one (no session, aborted, the
    /// session ended).
    Failed {
        request: u64,
        reason: Option<String>,
    },
    /// Removing a mention failed: it shows again.
    MentionDeleteFailed { message_id: String, reason: String },
}

/// The one answer to a load. Sending the result uses it up; dropped unused (an error, a
/// panicking or aborted task, no session) it sends `Failed`.
pub struct Answer {
    events: Ui,
    request: u64,
    reason: Option<String>,
    answered: bool,
}

impl Answer {
    pub fn new(events: &Ui, request: u64) -> Self {
        Answer {
            events: events.clone(),
            request,
            reason: None,
            answered: false,
        }
    }

    pub fn request(&self) -> u64 {
        self.request
    }

    pub fn send(mut self, ev: Ev) {
        self.answered = true;
        let _ = self.events.unbounded_send(MessagesEv::Inbox(ev));
    }

    /// The load failed with `reason` (`None`: the session ended).
    pub fn fail(mut self, reason: Option<String>) {
        self.reason = reason;
    }
}

impl Drop for Answer {
    fn drop(&mut self) {
        if !self.answered {
            let _ = self.events.unbounded_send(MessagesEv::Inbox(Ev::Failed {
                request: self.request,
                reason: self.reason.take(),
            }));
        }
    }
}

/// The reason to show for a failed request, or `None` when the session ended (the loop
/// is told, and signs out).
pub(super) fn failure(
    internal: &tokio::sync::mpsc::UnboundedSender<Internal>,
    gen: u64,
    err: p::Error,
) -> Option<String> {
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

/// A message as a panel shows it.
pub(crate) fn item(m: &p::Message, names: &SharedNames) -> Item {
    Item {
        message: parse::to_message(m, names),
    }
}

/// A saved message for the list; one the account cannot read any more stays as a row
/// that can be removed (Fluxer's "missing" card).
pub(crate) fn saved_entry(s: &p::SavedMessage, names: &SharedNames) -> SavedEntry {
    match (&s.message, s.is_available()) {
        (Some(m), true) => SavedEntry::Shown(Box::new(item(m, names))),
        _ => SavedEntry::Missing {
            channel_id: s.channel_id.clone(),
            message_id: s.message_id.clone(),
        },
    }
}

/// The request number of a load, for its answer.
fn request_of(cmd: &Cmd) -> Option<u64> {
    match cmd {
        Cmd::Pins { request, .. }
        | Cmd::Mentions { request, .. }
        | Cmd::Saved { request, .. }
        | Cmd::Around { request, .. } => Some(*request),
        Cmd::AckPins { .. } | Cmd::DeleteMention { .. } => None,
    }
}

impl Backend {
    pub(super) fn inbox(&mut self, cmd: Cmd) {
        let answer = request_of(&cmd).map(|r| Answer::new(&self.events, r));
        let (client, gen, names) = match &self.active {
            Some(Active::Real(r)) => (r.session.client().clone(), r.gen, r.names.clone()),
            #[cfg(feature = "demo")]
            Some(Active::Demo(m)) => return m.inbox(cmd, answer),
            // No session: a load is answered as failed when its answer drops; removing a
            // mention shows the row again.
            _ => {
                if let Cmd::DeleteMention { message_id } = cmd {
                    self.emit(MessagesEv::Inbox(Ev::MentionDeleteFailed {
                        message_id,
                        reason: t!("composer-not-connected"),
                    }));
                }
                return;
            }
        };
        let events = self.events.clone();
        let internal = self.internal.clone();
        let fail = move |err: p::Error| failure(&internal, gen, err);
        let what = match &cmd {
            Cmd::Pins { .. } | Cmd::AckPins { .. } => "pins",
            Cmd::Mentions { .. } | Cmd::DeleteMention { .. } => "mentions",
            Cmd::Saved { .. } => "saved messages",
            Cmd::Around { .. } => "jump to message",
        };
        self.spawn_guarded(what, None, async move {
            match cmd {
                Cmd::Pins {
                    channel_id, before, ..
                } => {
                    if let Some(answer) = answer {
                        super::pins::load(&client, &names, channel_id, before, answer, &fail).await;
                    }
                }
                Cmd::AckPins { channel_id } => {
                    if let Err(err) = client.ack_pins(&channel_id).await {
                        // Only the end of the session matters: a lost ack is sent again
                        // the next time the pins are looked at.
                        let _ = fail(err);
                    }
                }
                Cmd::Mentions {
                    before,
                    everyone,
                    roles,
                    ..
                } => {
                    let Some(answer) = answer else { return };
                    let query = p::MentionsQuery {
                        limit: MENTIONS_PAGE,
                        roles,
                        everyone,
                        guilds: true,
                        before,
                    };
                    match client.recent_mentions(&query).await {
                        Ok(list) => {
                            let request = answer.request();
                            answer.send(Ev::Mentions {
                                request,
                                more: list.len() >= usize::from(MENTIONS_PAGE),
                                items: list.iter().map(|m| item(m, &names)).collect(),
                            });
                        }
                        Err(err) => answer.fail(fail(err)),
                    }
                }
                Cmd::DeleteMention { message_id } => {
                    if let Err(err) = client.delete_recent_mention(&message_id).await {
                        if let Some(reason) = fail(err) {
                            let _ =
                                events.unbounded_send(MessagesEv::Inbox(Ev::MentionDeleteFailed {
                                    message_id,
                                    reason,
                                }));
                        }
                    }
                }
                Cmd::Saved { before, .. } => {
                    let Some(answer) = answer else { return };
                    match client.saved_messages(before.as_deref(), SAVED_PAGE).await {
                        Ok(list) => {
                            let request = answer.request();
                            answer.send(Ev::Saved {
                                request,
                                more: list.len() >= usize::from(SAVED_PAGE),
                                entries: list.iter().map(|s| saved_entry(s, &names)).collect(),
                            });
                        }
                        Err(err) => answer.fail(fail(err)),
                    }
                }
                Cmd::Around {
                    channel_id,
                    message_id,
                    ..
                } => {
                    let Some(answer) = answer else { return };
                    match client
                        .messages_around(&channel_id, &message_id, AROUND_PAGE)
                        .await
                    {
                        Ok(messages) => {
                            let request = answer.request();
                            let rendered = messages
                                .iter()
                                .map(|m| parse::to_message(m, &names))
                                .collect();
                            answer.send(Ev::Around {
                                channel_id,
                                request,
                                messages,
                                rendered,
                            });
                        }
                        Err(err) => answer.fail(fail(err)),
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Event;
    use futures::StreamExt;

    fn failed(ev: Option<Event>) -> Option<(u64, Option<String>)> {
        match ev {
            Some(Event::Messages(MessagesEv::Inbox(Ev::Failed { request, reason }))) => {
                Some((request, reason))
            }
            _ => None,
        }
    }

    #[tokio::test]
    async fn every_panel_load_gets_one_answer() {
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let events = Ui::new(tx);
        // Dropped without a result (no session, an aborted or panicking task).
        drop(Answer::new(&events, 7));
        assert_eq!(failed(rx.next().await), Some((7, None)));
        // A failure with its reason.
        Answer::new(&events, 8).fail(Some("offline".into()));
        assert_eq!(failed(rx.next().await), Some((8, Some("offline".into()))));
        // A result: no failure follows it.
        Answer::new(&events, 9).send(Ev::Mentions {
            request: 9,
            items: Vec::new(),
            more: false,
        });
        drop(Answer::new(&events, 10));
        assert!(matches!(
            rx.next().await,
            Some(Event::Messages(MessagesEv::Inbox(Ev::Mentions {
                request: 9,
                ..
            })))
        ));
        assert_eq!(failed(rx.next().await), Some((10, None)));
    }
}
