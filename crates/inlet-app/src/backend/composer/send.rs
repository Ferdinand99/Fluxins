// SPDX-License-Identifier: AGPL-3.0-or-later
//! Sending and editing messages, typing, and the commands that call the server.
//!
//! Every message and every edit carries `allowed_mentions` built from the mentions the
//! person picked in the editor ([`Mentions`]). The editor also escapes typed text that
//! looks like a mention, but that depends on how the server parses today;
//! `allowed_mentions` is the guard that does not: the server drops every user and role
//! mention that is not on the list, and `@everyone`/`@here` unless one was picked.
//! The protocol's `send` cannot carry such a list yet, so the body is built here and goes
//! through `request_json`, which uses the same rate limiter.

use inlet_protocol as p;
use reqwest::Method;
use serde_json::{json, Map, Value};

use super::Ev;
use crate::backend::messages::{self, parse};
use crate::backend::session::{Failure, FailureKind};
use crate::backend::{Active, Backend, Internal, Ui};

/// `SUPPRESS_NOTIFICATIONS`: the message is sent without a notification (`@silent`).
const SILENT_FLAG: u64 = p::models::message_flags::SUPPRESS_NOTIFICATIONS;

/// The longest nickname Fluxer takes.
const MAX_NICK_CHARS: usize = 32;

/// Who a message may notify: the mentions picked in the editor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Mentions {
    pub users: Vec<u64>,
    pub roles: Vec<u64>,
    /// An `@everyone` or `@here` pill was picked.
    pub everyone: bool,
    /// For a reply: whether the author of the message replied to is notified.
    pub replied_user: bool,
}

impl Mentions {
    /// From the editor's [`inlet_editor::AllowedMentions`] (always build it from the
    /// editor's buffer, never from the text).
    pub fn from_editor(allowed: &inlet_editor::AllowedMentions, replied_user: bool) -> Self {
        Mentions {
            users: allowed.users.iter().map(|id| id.0).collect(),
            roles: allowed.roles.iter().map(|id| id.0).collect(),
            everyone: allowed.everyone,
            replied_user,
        }
    }

    /// The `allowed_mentions` object: the picked lists with `parse: []`, or
    /// `parse: ["everyone"]` alone (never with lists) when an everyone pill was picked.
    /// The server refuses a non-empty `parse` together with lists, so with an everyone pill
    /// and picked users or roles at once it gets all three kinds
    /// (the editor then escapes every typed look-alike).
    pub fn to_json(&self, reply: bool) -> Value {
        let ids = |list: &[u64]| -> Vec<String> { list.iter().map(u64::to_string).collect() };
        let mut out = Map::new();
        if self.everyone && (!self.users.is_empty() || !self.roles.is_empty()) {
            out.insert("parse".into(), json!(["everyone", "users", "roles"]));
        } else if self.everyone {
            out.insert("parse".into(), json!(["everyone"]));
        } else {
            // An empty `parse` is sent as `[]`: nothing is parsed, only the lists count.
            out.insert("parse".into(), json!([]));
            out.insert("users".into(), json!(ids(&self.users)));
            out.insert("roles".into(), json!(ids(&self.roles)));
        }
        if reply {
            out.insert("replied_user".into(), json!(self.replied_user));
        }
        Value::Object(out)
    }
}

/// A message to send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outgoing {
    pub channel_id: String,
    /// From [`p::new_nonce`]; reused when the person retries a failed send.
    pub nonce: String,
    /// The wire text (pills as `<@id>` and so on), after commands and `@silent`.
    pub content: String,
    pub mentions: Mentions,
    /// The message this one replies to.
    pub reply_to: Option<String>,
    /// Sent without a notification.
    pub silent: bool,
}

impl Outgoing {
    /// The body of `POST /channels/{id}/messages`.
    pub fn to_json(&self) -> Value {
        let mut body = Map::new();
        body.insert("content".into(), json!(self.content));
        body.insert("nonce".into(), json!(self.nonce));
        body.insert(
            "allowed_mentions".into(),
            self.mentions.to_json(self.reply_to.is_some()),
        );
        if let Some(id) = &self.reply_to {
            body.insert(
                "message_reference".into(),
                json!({ "message_id": id, "type": 0 }),
            );
        }
        if self.silent {
            body.insert("flags".into(), json!(SILENT_FLAG));
        }
        Value::Object(body)
    }
}

/// The body of `PATCH /channels/{id}/messages/{id}`.
pub fn edit_json(content: &str, mentions: &Mentions) -> Value {
    json!({ "content": content, "allowed_mentions": mentions.to_json(false) })
}

/// A snowflake as it may go into a path: 1 to 20 digits, no leading zero.
pub fn snowflake(s: &str) -> Option<&str> {
    let ok = !s.is_empty()
        && s.len() <= 20
        && s.bytes().all(|b| b.is_ascii_digit())
        && !(s.len() > 1 && s.starts_with('0'));
    ok.then_some(s)
}

/// A snowflake as a number, for pills.
#[cfg_attr(not(test), allow(dead_code))] // used by the account-state source
pub fn snowflake_u64(s: &str) -> Option<u64> {
    snowflake(s)?.parse().ok()
}

/// What a failed request means for the UI: the reason and how long the server asked to
/// wait, or `None` when the session has ended (the loop is told).
fn failed(
    internal: &tokio::sync::mpsc::UnboundedSender<Internal>,
    gen: u64,
    err: p::Error,
) -> Option<(String, Option<u64>)> {
    let retry = match &err {
        p::Error::RateLimited { retry_after } => Some(retry_after.as_secs().max(1)),
        _ => None,
    };
    let failure = Failure::from(err);
    if failure.kind == FailureKind::SessionEnded {
        let _ = internal.send(Internal::Ended {
            gen,
            message: failure.message,
        });
        return None;
    }
    Some((failure.message, retry))
}

fn path_for(channel_id: &str) -> Result<String, p::Error> {
    snowflake(channel_id)
        .map(|id| format!("/channels/{id}/messages"))
        .ok_or_else(|| p::Error::InvalidInput("an ID is not a snowflake".into()))
}

/// Posts `message` and reports the result.
async fn post(
    session: p::Session,
    message: Outgoing,
    names: parse::SharedNames,
    events: Ui,
    internal: tokio::sync::mpsc::UnboundedSender<Internal>,
    gen: u64,
) {
    let result = match path_for(&message.channel_id) {
        Ok(path) => {
            session
                .client()
                .request_json(Method::POST, &path, Some(&message.to_json()))
                .await
        }
        Err(err) => Err(err),
    };
    match result.and_then(|v| {
        serde_json::from_value::<p::Message>(v).map_err(|e| p::Error::Decode(e.to_string()))
    }) {
        Ok(sent) => {
            let _ = events.unbounded_send(Ev::Sent {
                nonce: message.nonce.clone(),
            });
            let _ = events.unbounded_send(messages::Ev::MessageCreated(parse::to_message(
                &sent, &names,
            )));
        }
        Err(err) => {
            if let Some((reason, retry_after_secs)) = failed(&internal, gen, err) {
                let _ = events.unbounded_send(Ev::SendFailed {
                    nonce: message.nonce,
                    reason,
                    retry_after_secs,
                });
            }
        }
    }
}

impl Backend {
    /// The account ID and message length limit, fetched once per session.
    pub(super) fn composer_session(&mut self) {
        match &self.active {
            #[cfg(feature = "demo")]
            Some(Active::Demo(_)) => self.emit(Ev::Session {
                user_id: crate::demo::composer::DEMO_USER_ID.into(),
                max_message_length: 2000,
            }),
            Some(Active::Real(r)) => {
                if let Some((user_id, max)) = self.composer.session(r.gen) {
                    self.emit(Ev::Session {
                        user_id,
                        max_message_length: max,
                    });
                    return;
                }
                let (session, events, internal, gen) = (
                    r.session.clone(),
                    self.events.clone(),
                    self.internal.clone(),
                    r.gen,
                );
                let state = self.composer.clone();
                self.spawn_guarded("composer session", None, async move {
                    match session.client().me().await {
                        Ok(me) => {
                            let max = session
                                .instance()
                                .discovery()
                                .resolve_limit(p::limits::MAX_MESSAGE_LENGTH, &me.traits())
                                .unwrap_or(2000)
                                .clamp(1, 100_000) as usize;
                            state.set_session(gen, me.id.clone(), max);
                            let _ = events.unbounded_send(Ev::Session {
                                user_id: me.id.clone(),
                                max_message_length: max,
                            });
                        }
                        Err(err) => {
                            // The composer keeps the safe default limit (2000).
                            let _ = failed(&internal, gen, err);
                        }
                    }
                });
            }
            None => {}
        }
    }

    pub(super) fn send(&mut self, message: Outgoing) {
        match &mut self.active {
            #[cfg(feature = "demo")]
            Some(Active::Demo(m)) => {
                let nonce = message.nonce.clone();
                m.send(message.channel_id, message.content);
                let _ = self.events.unbounded_send(Ev::Sent { nonce });
            }
            Some(Active::Real(r)) => {
                let (session, events, internal, gen, names) = (
                    r.session.clone(),
                    self.events.clone(),
                    self.internal.clone(),
                    r.gen,
                    r.names.clone(),
                );
                self.spawn_guarded(
                    "send",
                    None,
                    post(session, message, names, events, internal, gen),
                );
            }
            None => {}
        }
    }

    pub(super) fn edit(
        &mut self,
        channel_id: String,
        message_id: String,
        content: String,
        mentions: Mentions,
    ) {
        match &mut self.active {
            #[cfg(feature = "demo")]
            Some(Active::Demo(m)) => {
                m.edit(&channel_id, &message_id, &content);
            }
            Some(Active::Real(r)) => {
                let (session, events, internal, gen, names) = (
                    r.session.clone(),
                    self.events.clone(),
                    self.internal.clone(),
                    r.gen,
                    r.names.clone(),
                );
                self.spawn_guarded("edit", None, async move {
                    let path = match (snowflake(&channel_id), snowflake(&message_id)) {
                        (Some(c), Some(m)) => format!("/channels/{c}/messages/{m}"),
                        _ => return,
                    };
                    let body = edit_json(&content, &mentions);
                    let result = session
                        .client()
                        .request_json(Method::PATCH, &path, Some(&body))
                        .await
                        .and_then(|v| {
                            serde_json::from_value::<p::Message>(v)
                                .map_err(|e| p::Error::Decode(e.to_string()))
                        });
                    match result {
                        Ok(edited) => {
                            let _ = events.unbounded_send(messages::Ev::MessageUpdated(
                                parse::to_message(&edited, &names),
                            ));
                        }
                        Err(err) => {
                            if let Some((reason, _)) = failed(&internal, gen, err) {
                                let _ = events.unbounded_send(Ev::EditFailed { reason });
                            }
                        }
                    }
                });
            }
            None => {}
        }
    }

    /// The protocol sends at most one typing request per channel every 8 s; the other
    /// calls return at once without a request.
    pub(super) fn typing(&mut self, channel_id: String) {
        if let Some(Active::Real(r)) = &self.active {
            let session = r.session.clone();
            self.spawn_guarded("typing", None, async move {
                if let Err(err) = session.client().trigger_typing(&channel_id).await {
                    log::debug!("typing was not sent: {err}");
                }
            });
        }
    }

    pub(super) fn set_nick(&mut self, guild_id: String, nick: Option<String>) {
        let nick = nick
            .map(|n| crate::model::clean_name(n.trim()))
            .filter(|n| !n.is_empty())
            .map(|n| n.chars().take(MAX_NICK_CHARS).collect::<String>());
        match &self.active {
            #[cfg(feature = "demo")]
            Some(Active::Demo(_)) => self.emit(Ev::Notice(nick_notice(nick.as_deref()))),
            Some(Active::Real(r)) => {
                let Some(guild) = snowflake(&guild_id).map(str::to_owned) else {
                    return;
                };
                let (session, events, internal, gen) = (
                    r.session.clone(),
                    self.events.clone(),
                    self.internal.clone(),
                    r.gen,
                );
                self.spawn_guarded("nick", None, async move {
                    let body = json!({ "nick": nick });
                    let path = format!("/guilds/{guild}/members/@me");
                    match session
                        .client()
                        .request_json(Method::PATCH, &path, Some(&body))
                        .await
                    {
                        Ok(_) => {
                            let _ = events.unbounded_send(Ev::Notice(nick_notice(nick.as_deref())));
                        }
                        Err(err) => {
                            if let Some((reason, _)) = failed(&internal, gen, err) {
                                let _ = events.unbounded_send(Ev::Notice(t!(
                                    "composer-command-failed",
                                    error = reason
                                )));
                            }
                        }
                    }
                });
            }
            None => {}
        }
    }

    /// `/msg`: opens (or finds) the DM with `user_id` and sends `message` there.
    pub(super) fn direct_message(&mut self, user_id: String, mut message: Outgoing) {
        match &mut self.active {
            #[cfg(feature = "demo")]
            Some(Active::Demo(_)) => self.emit(Ev::Notice(t!("composer-msg-sent"))),
            Some(Active::Real(r)) => {
                let Some(user) = snowflake(&user_id).map(str::to_owned) else {
                    return;
                };
                let (session, events, internal, gen, names) = (
                    r.session.clone(),
                    self.events.clone(),
                    self.internal.clone(),
                    r.gen,
                    r.names.clone(),
                );
                self.spawn_guarded("direct message", None, async move {
                    let body = json!({ "recipient_id": user });
                    let channel = session
                        .client()
                        .request_json(Method::POST, "/users/@me/channels", Some(&body))
                        .await;
                    let channel_id = match channel {
                        Ok(v) => v
                            .get("id")
                            .and_then(Value::as_str)
                            .and_then(snowflake)
                            .map(str::to_owned),
                        Err(err) => {
                            if let Some((reason, _)) = failed(&internal, gen, err) {
                                let _ = events.unbounded_send(Ev::Notice(t!(
                                    "composer-msg-failed",
                                    error = reason
                                )));
                            }
                            return;
                        }
                    };
                    let Some(channel_id) = channel_id else {
                        let _ = events.unbounded_send(Ev::Notice(t!(
                            "composer-msg-failed",
                            error = t!("composer-bad-answer")
                        )));
                        return;
                    };
                    message.channel_id = channel_id;
                    let notice = events.clone();
                    post(session, message, names, events, internal, gen).await;
                    let _ = notice.unbounded_send(Ev::Notice(t!("composer-msg-sent")));
                });
            }
            None => {}
        }
    }
}

fn nick_notice(nick: Option<&str>) -> String {
    match nick {
        Some(nick) => t!("composer-nick-changed", nick = nick),
        None => t!("composer-nick-reset"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outgoing(mentions: Mentions, reply_to: Option<&str>, silent: bool) -> Outgoing {
        Outgoing {
            channel_id: "10".into(),
            nonce: "99".into(),
            content: "hi <@1>".into(),
            mentions,
            reply_to: reply_to.map(Into::into),
            silent,
        }
    }

    #[test]
    fn every_send_carries_allowed_mentions_from_the_picked_pills() {
        let plain = outgoing(Mentions::default(), None, false).to_json();
        assert_eq!(
            plain["allowed_mentions"],
            json!({ "parse": [], "users": [], "roles": [] }),
            "nothing picked: nothing may ping, whatever the text says"
        );
        let picked = Mentions {
            users: vec![1, 2],
            roles: vec![3],
            ..Default::default()
        };
        let body = outgoing(picked, None, false).to_json();
        assert_eq!(
            body["allowed_mentions"],
            json!({ "parse": [], "users": ["1", "2"], "roles": ["3"] })
        );
        assert!(body.get("flags").is_none());
        assert!(body.get("message_reference").is_none());
        assert_eq!(body["nonce"], "99");
    }

    #[test]
    fn everyone_is_parsed_only_when_picked() {
        let only = Mentions {
            everyone: true,
            ..Default::default()
        };
        assert_eq!(only.to_json(false), json!({ "parse": ["everyone"] }));
        let both = Mentions {
            users: vec![1],
            everyone: true,
            ..Default::default()
        };
        assert_eq!(
            both.to_json(false),
            json!({ "parse": ["everyone", "users", "roles"] })
        );
    }

    #[test]
    fn a_reply_says_whether_the_author_is_notified_and_silent_sets_the_flag() {
        let quiet = Mentions {
            replied_user: false,
            ..Default::default()
        };
        let body = outgoing(quiet, Some("55"), true).to_json();
        assert_eq!(body["allowed_mentions"]["replied_user"], false);
        assert_eq!(body["message_reference"]["message_id"], "55");
        assert_eq!(body["flags"], 4096);
        let loud = Mentions {
            replied_user: true,
            ..Default::default()
        };
        let body = outgoing(loud, Some("55"), false).to_json();
        assert_eq!(body["allowed_mentions"]["replied_user"], true);
    }

    #[test]
    fn edits_carry_allowed_mentions_too() {
        let m = Mentions {
            users: vec![7],
            ..Default::default()
        };
        let body = edit_json("new <@7>", &m);
        assert_eq!(
            body["allowed_mentions"],
            json!({ "parse": [], "users": ["7"], "roles": [] })
        );
        assert!(body["allowed_mentions"].get("replied_user").is_none());
    }

    #[test]
    fn only_snowflakes_go_into_paths() {
        assert_eq!(snowflake("123"), Some("123"));
        for bad in [
            "",
            "0123",
            "12a",
            "../1",
            "1/2",
            "123456789012345678901",
            "@me",
        ] {
            assert_eq!(snowflake(bad), None, "{bad:?}");
        }
    }
}
