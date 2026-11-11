// SPDX-License-Identifier: AGPL-3.0-or-later
//! Message actions on the network side: reactions, delete, pin, mark unread, forward,
//! save, hide embeds, the message link and who reacted. What may be done, and when, is
//! decided on the UI thread (`ui/chat/action_bar.rs`, from the account's permissions);
//! this module only sends the requests, each through the account's limiter, and
//! reports failures so the UI can undo what it showed early.
//!
//! Request budget (on top of the protocol's limiter):
//!
//! - Reactions: the UI collects clicks per message for 500 ms and sends only the net
//!   change per emoji (`ui/chat/reactions.rs`), so clicking a reaction on and off any
//!   number of times sends at most one request per emoji per message per 500 ms, and
//!   none when it ends where it started.
//! - Who reacted: one `GET` per opened list, at most one a second and one at a time,
//!   with answers kept for 30 s (`ui/chat/reactors.rs`).
//! - Everything else: one request per click. The UI drops a second click on the same
//!   action of the same message while the first is on its way.
//! - Mark unread goes through the protocol's coalesced acknowledgements (one batched
//!   request per second for the account).

use inlet_protocol as p;

use super::Ev as MessagesEv;
use crate::backend::session::{Failure, FailureKind};
use crate::backend::{Active, Backend, Internal};
use crate::model;

/// Users shown in the "who reacted" list (one page).
pub const REACTORS_PAGE: u8 = 25;

/// What an action did, for its answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Delete,
    Pin,
    Unpin,
    Forward,
    Save,
    Unsave,
    HideEmbeds,
    ShowEmbeds,
}

/// Commands of the message actions.
pub enum Cmd {
    /// Adds (`add`) or removes the account's reaction.
    React {
        channel_id: String,
        message_id: String,
        emoji: p::Emoji,
        add: bool,
    },
    Delete {
        channel_id: String,
        message_id: String,
    },
    Pin {
        channel_id: String,
        message_id: String,
        pin: bool,
    },
    /// Marks the messages after `after` unread, keeping `mentions` mentions.
    MarkUnread {
        channel_id: String,
        after: String,
        mentions: u64,
    },
    /// Copies a message into `to_channel`.
    Forward {
        source: p::ForwardSource,
        to_channel: String,
        nonce: String,
    },
    Save {
        channel_id: String,
        message_id: String,
        save: bool,
    },
    /// Hides (`hide`) or shows the embeds of a message.
    HideEmbeds {
        message: Box<p::Message>,
        hide: bool,
    },
    /// The message's link in the instance's web app, for the clipboard.
    Link {
        guild_id: Option<String>,
        channel_id: String,
        message_id: String,
    },
    /// The first page of who reacted with `emoji`.
    Reactors {
        channel_id: String,
        message_id: String,
        emoji: p::Emoji,
        request: u64,
    },
}

/// Events of the message actions.
pub enum Ev {
    /// A reaction request failed: what the UI showed early is undone.
    ReactFailed {
        channel_id: String,
        message_id: String,
        emoji: p::Emoji,
        add: bool,
        reason: String,
    },
    /// An action went through.
    Done {
        message_id: String,
        kind: Kind,
    },
    /// An action failed.
    Failed {
        message_id: String,
        kind: Kind,
        reason: String,
    },
    /// The link of a message.
    Link(String),
    /// Who reacted: cleaned display names, and whether there are more.
    Reactors {
        request: u64,
        names: Vec<String>,
        more: bool,
    },
    ReactorsFailed {
        request: u64,
        reason: String,
    },
}

/// A message's link in the web app: `{webapp}/channels/{guild or @me}/{channel}/{message}`.
pub fn message_link(
    webapp: &str,
    guild_id: Option<&str>,
    channel_id: &str,
    message_id: &str,
) -> String {
    format!(
        "{}/channels/{}/{}/{}",
        webapp.trim_end_matches('/'),
        guild_id.unwrap_or("@me"),
        channel_id,
        message_id
    )
}

/// What the UI is told when a request fails, or `None` when the session ended (the
/// loop is told instead, and signs out).
fn failure(
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

impl Backend {
    pub(super) fn message_action(&mut self, cmd: Cmd) {
        let (client, gen) = match &self.active {
            Some(Active::Real(r)) => (r.session.client().clone(), r.gen),
            #[cfg(feature = "demo")]
            Some(Active::Demo(_)) => return self.demo_action(cmd),
            _ => return self.no_session(cmd),
        };
        let events = self.events.clone();
        let internal = self.internal.clone();
        let fail = move |err: p::Error| failure(&internal, gen, err);
        let what = action_name(&cmd);
        self.spawn_guarded(what, None, async move {
            let send = |ev: Ev| {
                let _ = events.unbounded_send(MessagesEv::Action(ev));
            };
            match cmd {
                Cmd::React {
                    channel_id,
                    message_id,
                    emoji,
                    add,
                } => {
                    let result = if add {
                        client.add_reaction(&channel_id, &message_id, &emoji).await
                    } else {
                        client
                            .remove_reaction(&channel_id, &message_id, &emoji)
                            .await
                    };
                    if let Err(err) = result {
                        if let Some(reason) = fail(err) {
                            send(Ev::ReactFailed {
                                channel_id,
                                message_id,
                                emoji,
                                add,
                                reason,
                            });
                        }
                    }
                }
                Cmd::Delete {
                    channel_id,
                    message_id,
                } => {
                    let r = client.delete_message(&channel_id, &message_id).await;
                    answer(&send, &fail, message_id, Kind::Delete, r);
                }
                Cmd::Pin {
                    channel_id,
                    message_id,
                    pin,
                } => {
                    let (r, kind) = if pin {
                        (
                            client.pin_message(&channel_id, &message_id).await,
                            Kind::Pin,
                        )
                    } else {
                        (
                            client.unpin_message(&channel_id, &message_id).await,
                            Kind::Unpin,
                        )
                    };
                    answer(&send, &fail, message_id, kind, r);
                }
                Cmd::MarkUnread {
                    channel_id,
                    after,
                    mentions,
                } => client.mark_unread(&channel_id, &after, mentions),
                Cmd::Forward {
                    source,
                    to_channel,
                    nonce,
                } => {
                    let message_id = source.message_id.clone();
                    let r = client
                        .forward(&to_channel, &source, Some(&nonce), false)
                        .await
                        .map(|_| ());
                    answer(&send, &fail, message_id, Kind::Forward, r);
                }
                Cmd::Save {
                    channel_id,
                    message_id,
                    save,
                } => {
                    let (r, kind) = if save {
                        (
                            client.save_message(&channel_id, &message_id).await,
                            Kind::Save,
                        )
                    } else {
                        (client.unsave_message(&message_id).await, Kind::Unsave)
                    };
                    answer(&send, &fail, message_id, kind, r);
                }
                Cmd::HideEmbeds { message, hide } => {
                    let kind = if hide {
                        Kind::HideEmbeds
                    } else {
                        Kind::ShowEmbeds
                    };
                    let r = client
                        .set_embeds_suppressed(&message, hide)
                        .await
                        .map(|_| ());
                    answer(&send, &fail, message.id.clone(), kind, r);
                }
                Cmd::Link {
                    guild_id,
                    channel_id,
                    message_id,
                } => send(Ev::Link(message_link(
                    client.instance().webapp(),
                    guild_id.as_deref(),
                    &channel_id,
                    &message_id,
                ))),
                Cmd::Reactors {
                    channel_id,
                    message_id,
                    emoji,
                    request,
                } => match client
                    .reaction_users(&channel_id, &message_id, &emoji, None, REACTORS_PAGE)
                    .await
                {
                    Ok(page) => send(Ev::Reactors {
                        request,
                        names: page
                            .items
                            .iter()
                            .take(usize::from(REACTORS_PAGE))
                            .map(|u| model::clean_name(u.display_name()))
                            .collect(),
                        more: page.has_more,
                    }),
                    // A session that ended signs out, which forgets the request; no
                    // answer then, like every other action.
                    Err(err) => {
                        if let Some(reason) = fail(err) {
                            send(Ev::ReactorsFailed { request, reason });
                        }
                    }
                },
            }
        });
    }

    /// No session: every action fails at once, so the UI undoes what it showed.
    fn no_session(&self, cmd: Cmd) {
        if let Some(ev) = refused(cmd, t!("composer-not-connected")) {
            self.emit(MessagesEv::Action(ev));
        }
    }

    /// The demo has no server: actions succeed, and a delete removes the message.
    #[cfg(feature = "demo")]
    fn demo_action(&self, cmd: Cmd) {
        let ev = match cmd {
            Cmd::React { .. } | Cmd::MarkUnread { .. } => return,
            Cmd::Delete {
                channel_id,
                message_id,
            } => {
                let dispatch = p::Dispatch::parse(
                    "MESSAGE_DELETE",
                    serde_json::json!({"id": message_id, "channel_id": channel_id}),
                );
                self.emit(MessagesEv::Dispatch {
                    dispatch: Box::new(dispatch),
                    rendered: Vec::new(),
                });
                Ev::Done {
                    message_id,
                    kind: Kind::Delete,
                }
            }
            Cmd::Pin {
                message_id, pin, ..
            } => Ev::Done {
                message_id,
                kind: if pin { Kind::Pin } else { Kind::Unpin },
            },
            Cmd::Forward { source, .. } => Ev::Done {
                message_id: source.message_id,
                kind: Kind::Forward,
            },
            Cmd::Save {
                message_id, save, ..
            } => Ev::Done {
                message_id,
                kind: if save { Kind::Save } else { Kind::Unsave },
            },
            Cmd::HideEmbeds { message, hide } => Ev::Done {
                message_id: message.id,
                kind: if hide {
                    Kind::HideEmbeds
                } else {
                    Kind::ShowEmbeds
                },
            },
            Cmd::Link {
                guild_id,
                channel_id,
                message_id,
            } => Ev::Link(message_link(
                "https://demo.invalid",
                guild_id.as_deref(),
                &channel_id,
                &message_id,
            )),
            Cmd::Reactors { request, .. } => Ev::Reactors {
                request,
                names: vec!["ada".into(), "linus".into(), "grace".into()],
                more: false,
            },
        };
        self.emit(MessagesEv::Action(ev));
    }
}

/// The answer of an action that has one.
fn answer(
    send: &impl Fn(Ev),
    fail: &impl Fn(p::Error) -> Option<String>,
    message_id: String,
    kind: Kind,
    result: p::Result<()>,
) {
    match result {
        Ok(()) => send(Ev::Done { message_id, kind }),
        Err(err) => {
            if let Some(reason) = fail(err) {
                send(Ev::Failed {
                    message_id,
                    kind,
                    reason,
                });
            }
        }
    }
}

/// The failure event for a command that could not be sent at all.
fn refused(cmd: Cmd, reason: String) -> Option<Ev> {
    Some(match cmd {
        Cmd::React {
            channel_id,
            message_id,
            emoji,
            add,
        } => Ev::ReactFailed {
            channel_id,
            message_id,
            emoji,
            add,
            reason,
        },
        Cmd::Delete { message_id, .. } => Ev::Failed {
            message_id,
            kind: Kind::Delete,
            reason,
        },
        Cmd::Pin {
            message_id, pin, ..
        } => Ev::Failed {
            message_id,
            kind: if pin { Kind::Pin } else { Kind::Unpin },
            reason,
        },
        Cmd::Forward { source, .. } => Ev::Failed {
            message_id: source.message_id,
            kind: Kind::Forward,
            reason,
        },
        Cmd::Save {
            message_id, save, ..
        } => Ev::Failed {
            message_id,
            kind: if save { Kind::Save } else { Kind::Unsave },
            reason,
        },
        Cmd::HideEmbeds { message, hide } => Ev::Failed {
            message_id: message.id,
            kind: if hide {
                Kind::HideEmbeds
            } else {
                Kind::ShowEmbeds
            },
            reason,
        },
        Cmd::Reactors { request, .. } => Ev::ReactorsFailed { request, reason },
        Cmd::MarkUnread { .. } | Cmd::Link { .. } => return None,
    })
}

/// The task name, for panic reports.
fn action_name(cmd: &Cmd) -> &'static str {
    match cmd {
        Cmd::React { .. } => "react",
        Cmd::Delete { .. } => "delete message",
        Cmd::Pin { .. } => "pin message",
        Cmd::MarkUnread { .. } => "mark unread",
        Cmd::Forward { .. } => "forward message",
        Cmd::Save { .. } => "save message",
        Cmd::HideEmbeds { .. } => "hide embeds",
        Cmd::Link { .. } => "message link",
        Cmd::Reactors { .. } => "reactors",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_point_at_the_web_app() {
        assert_eq!(
            message_link("https://web.example/", Some("1"), "2", "3"),
            "https://web.example/channels/1/2/3"
        );
        assert_eq!(
            message_link("https://web.example", None, "2", "3"),
            "https://web.example/channels/@me/2/3"
        );
    }
}
