// SPDX-License-Identifier: AGPL-3.0-or-later
//! Sending and editing messages, typing, and the commands that call the server.
//!
//! Every message and every edit carries `allowed_mentions` built from the mentions the
//! person picked in the editor ([`Mentions`]). The editor also escapes typed text that
//! looks like a mention, but that depends on how the server parses today;
//! `allowed_mentions` is the guard that does not: the server drops every user and role
//! mention that is not on the list, and `@everyone`/`@here` unless one was picked.
//! Sends and edits go through the protocol's `send` and `edit_message_with`.

use inlet_protocol as p;
use reqwest::Method;
use serde_json::{json, Value};

use super::Ev;
use crate::backend::messages::{self, parse};
use crate::backend::session::{Failure, FailureKind};
use crate::backend::{Active, Backend, Internal, Ui};

/// The longest nickname Fluxer takes.
const MAX_NICK_CHARS: usize = 32;
/// Recent messages read to find out whether a send went through after all.
const NONCE_LOOKBACK: u8 = 50;

/// Who a message may notify: the mentions picked in the editor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Mentions {
    pub users: Vec<u64>,
    pub roles: Vec<u64>,
    /// An `@everyone` or `@here` pill was picked.
    pub everyone: bool,
    /// Whether the author of the message replied to is notified. For a send it is the
    /// reply bar's switch (`None` for a message that is no reply); for an edit, what the
    /// original message did (`false` when that is not known).
    pub replied_user: Option<bool>,
}

impl Mentions {
    /// From the editor's [`inlet_editor::AllowedMentions`] (always build it from the
    /// editor's buffer, never from the text).
    pub fn from_editor(
        allowed: &inlet_editor::AllowedMentions,
        replied_user: Option<bool>,
    ) -> Self {
        Mentions {
            users: allowed.users.iter().map(|id| id.0).collect(),
            roles: allowed.roles.iter().map(|id| id.0).collect(),
            everyone: allowed.everyone,
            replied_user,
        }
    }

    /// The protocol's `allowed_mentions`: exactly the picked users, roles and everyone.
    /// Fluxer cannot express "these users and roles plus `@everyone`"; in that case
    /// every kind is allowed, which is safe because the editor then escapes every typed
    /// look-alike, so only the picked pills are mentions in the text.
    pub fn to_allowed(&self) -> Result<p::AllowedMentions, p::AllowedMentionsError> {
        let ids = |list: &[u64]| -> Vec<String> { list.iter().map(u64::to_string).collect() };
        let mut allowed =
            match p::AllowedMentions::only(&ids(&self.users), &ids(&self.roles), self.everyone) {
                Ok(a) => a,
                Err(p::AllowedMentionsError::ParseWithUsersOrRoles) => p::AllowedMentions::all(),
                Err(e) => return Err(e),
            };
        allowed.replied_user = self.replied_user;
        Ok(allowed)
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
    /// The protocol's message.
    pub fn to_message(&self) -> Result<p::OutgoingMessage, p::AllowedMentionsError> {
        let mut m = p::OutgoingMessage::text(self.content.clone())
            .with_nonce(self.nonce.clone())
            .with_allowed_mentions(self.mentions.to_allowed()?);
        if let Some(id) = &self.reply_to {
            m = m.reply_to(id.clone(), self.mentions.replied_user.unwrap_or(true));
        }
        if self.silent {
            m = m.silent();
        }
        Ok(m)
    }
}

/// The protocol's edit: new text and its `allowed_mentions`.
pub fn edit_of(
    content: &str,
    mentions: &Mentions,
) -> Result<p::MessageEdit, p::AllowedMentionsError> {
    Ok(p::MessageEdit {
        content: Some(content.to_owned()),
        flags: None,
        allowed_mentions: Some(mentions.to_allowed()?),
    })
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

/// Why a send failed, for the UI.
pub struct SendError {
    pub reason: String,
    /// The server asked to wait this long (slowmode or a rate limit).
    pub retry_after_secs: Option<u64>,
    /// The request may have reached the server (timeout, broken connection, an answer
    /// that could not be read, a server error): the message may exist.
    pub ambiguous: bool,
}

/// What a failed request means for the UI, or `None` when the session has ended (the
/// loop is told).
fn failed(
    internal: &tokio::sync::mpsc::UnboundedSender<Internal>,
    gen: u64,
    err: p::Error,
) -> Option<SendError> {
    let retry_after_secs = match &err {
        p::Error::RateLimited { retry_after } => Some(retry_after.as_secs().max(1)),
        _ => None,
    };
    let ambiguous = match &err {
        p::Error::Http(_) | p::Error::Decode(_) => true,
        p::Error::Api(api) => api.status >= 500 || api.status == 0,
        _ => false,
    };
    let failure = Failure::from(err);
    if failure.kind == FailureKind::SessionEnded {
        let _ = internal.send(Internal::Ended {
            gen,
            message: failure.message,
        });
        return None;
    }
    Some(SendError {
        reason: failure.message,
        retry_after_secs,
        ambiguous,
    })
}

/// The one place the composer hands a sent or edited message to the message list.
fn show_in_list(events: &Ui, names: &parse::SharedNames, m: &p::Message, edited: bool) {
    let shown = parse::to_message(m, names);
    let _ = if edited {
        events.unbounded_send(messages::Ev::MessageUpdated(shown))
    } else {
        events.unbounded_send(messages::Ev::MessageCreated(shown))
    };
}

/// The session's parts a send task needs.
#[derive(Clone)]
struct Ctx {
    session: p::Session,
    names: parse::SharedNames,
    events: Ui,
    internal: tokio::sync::mpsc::UnboundedSender<Internal>,
    gen: u64,
}

impl Ctx {
    /// Posts `message`. `Ok(())` when the server took it; `Err(None)` when the session
    /// ended.
    async fn post(&self, message: &Outgoing) -> Result<(), Option<SendError>> {
        let result = match message.to_message() {
            Ok(m) => self.session.client().send(&message.channel_id, &m).await,
            Err(e) => Err(e.into()),
        };
        match result {
            Ok(sent) => {
                show_in_list(&self.events, &self.names, &sent, false);
                Ok(())
            }
            Err(err) => Err(failed(&self.internal, self.gen, err)),
        }
    }

    /// Posts `message` and reports `Sent` or `SendFailed` with its nonce.
    async fn post_and_report(&self, message: Outgoing) {
        match self.post(&message).await {
            Ok(()) => {
                let _ = self.events.unbounded_send(Ev::Sent {
                    nonce: message.nonce,
                });
            }
            Err(Some(e)) => {
                let _ = self.events.unbounded_send(Ev::SendFailed {
                    nonce: message.nonce,
                    reason: e.reason,
                    retry_after_secs: e.retry_after_secs,
                    ambiguous: e.ambiguous,
                });
            }
            Err(None) => {}
        }
    }

    /// Whether a message with this nonce is among the channel's newest messages.
    async fn already_sent(&self, message: &Outgoing) -> Result<bool, Option<SendError>> {
        match self
            .session
            .client()
            .messages(&message.channel_id, None, NONCE_LOOKBACK)
            .await
        {
            Ok(list) => Ok(list
                .iter()
                .any(|m| m.nonce.as_deref() == Some(message.nonce.as_str()))),
            Err(err) => Err(failed(&self.internal, self.gen, err)),
        }
    }
}

impl Backend {
    fn send_ctx(&self) -> Option<Ctx> {
        match &self.active {
            Some(Active::Real(r)) => Some(Ctx {
                session: r.session.clone(),
                names: r.names.clone(),
                events: self.events.clone(),
                internal: self.internal.clone(),
                gen: r.gen,
            }),
            _ => None,
        }
    }

    /// The account ID and message length limit, fetched once per session. `token` is
    /// echoed so the UI can drop an answer meant for an account that signed out since.
    pub(super) fn composer_session(&mut self, token: u64) {
        match &self.active {
            #[cfg(feature = "demo")]
            Some(Active::Demo(_)) => self.emit(Ev::Session {
                token,
                user_id: crate::demo::composer::DEMO_USER_ID.into(),
                max_message_length: 2000,
            }),
            Some(Active::Real(r)) => {
                if let Some((user_id, max)) = self.composer.session(r.gen) {
                    self.emit(Ev::Session {
                        token,
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
                                token,
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
        #[cfg(feature = "demo")]
        if let Some(Active::Demo(m)) = &mut self.active {
            let nonce = message.nonce.clone();
            m.send(message.channel_id, message.content);
            let _ = self.events.unbounded_send(Ev::Sent { nonce });
            return;
        }
        if let Some(ctx) = self.send_ctx() {
            self.spawn_guarded("send", None, async move {
                ctx.post_and_report(message).await
            });
        }
    }

    /// A retry long after an unclear failure: the server only recognises a nonce for a
    /// few minutes, so first look whether the message arrived; send it only if not.
    pub(super) fn send_unless_sent(&mut self, message: Outgoing) {
        let Some(ctx) = self.send_ctx() else {
            self.send(message);
            return;
        };
        self.spawn_guarded("send unless sent", None, async move {
            match ctx.already_sent(&message).await {
                Ok(true) => {
                    let _ = ctx.events.unbounded_send(Ev::Sent {
                        nonce: message.nonce,
                    });
                }
                Ok(false) => ctx.post_and_report(message).await,
                Err(Some(e)) => {
                    let _ = ctx.events.unbounded_send(Ev::SendFailed {
                        nonce: message.nonce,
                        reason: e.reason,
                        retry_after_secs: e.retry_after_secs,
                        ambiguous: true,
                    });
                }
                Err(None) => {}
            }
        });
    }

    pub(super) fn edit(
        &mut self,
        channel_id: String,
        message_id: String,
        content: String,
        mentions: Mentions,
    ) {
        #[cfg(feature = "demo")]
        if let Some(Active::Demo(m)) = &mut self.active {
            m.edit(&channel_id, &message_id, &content);
            return;
        }
        let Some(ctx) = self.send_ctx() else {
            return;
        };
        self.spawn_guarded("edit", None, async move {
            let result = match edit_of(&content, &mentions) {
                Ok(edit) => {
                    ctx.session
                        .client()
                        .edit_message_with(&channel_id, &message_id, &edit)
                        .await
                }
                Err(e) => Err(e.into()),
            };
            match result {
                Ok(edited) => show_in_list(&ctx.events, &ctx.names, &edited, true),
                Err(err) => {
                    if let Some(e) = failed(&ctx.internal, ctx.gen, err) {
                        let _ = ctx
                            .events
                            .unbounded_send(Ev::EditFailed { reason: e.reason });
                    }
                }
            }
        });
    }

    /// The protocol sends at most one typing request per channel every 8 s; the other
    /// calls return at once without a request. A send resets it.
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
        #[cfg(feature = "demo")]
        if let Some(Active::Demo(_)) = &self.active {
            self.emit(Ev::Notice(nick_notice(nick.as_deref())));
            return;
        }
        let (Some(guild), Some(ctx)) = (snowflake(&guild_id).map(str::to_owned), self.send_ctx())
        else {
            return;
        };
        self.spawn_guarded("nick", None, async move {
            // The protocol has no method for this route yet.
            let body = json!({ "nick": nick });
            let path = format!("/guilds/{guild}/members/@me");
            match ctx
                .session
                .client()
                .request_json(Method::PATCH, &path, Some(&body))
                .await
            {
                Ok(_) => {
                    let _ = ctx
                        .events
                        .unbounded_send(Ev::Notice(nick_notice(nick.as_deref())));
                }
                Err(err) => {
                    if let Some(e) = failed(&ctx.internal, ctx.gen, err) {
                        let _ = ctx.events.unbounded_send(Ev::Notice(t!(
                            "composer-command-failed",
                            error = e.reason
                        )));
                    }
                }
            }
        });
    }

    /// `/msg`: opens (or finds) the DM with `user_id` and sends `message` there. Says
    /// "sent" only when the server took it; otherwise `DirectMessageFailed` lets the
    /// composer put the text back.
    pub(super) fn direct_message(&mut self, user_id: String, mut message: Outgoing) {
        #[cfg(feature = "demo")]
        if let Some(Active::Demo(_)) = &self.active {
            self.emit(Ev::Notice(t!("composer-msg-sent")));
            return;
        }
        let Some(ctx) = self.send_ctx() else {
            return;
        };
        let nonce = message.nonce.clone();
        let fail = move |events: &Ui, reason: String| {
            let _ = events.unbounded_send(Ev::DirectMessageFailed {
                nonce: nonce.clone(),
                reason,
            });
        };
        let Some(user) = snowflake(&user_id).map(str::to_owned) else {
            fail(&ctx.events, t!("composer-bad-answer"));
            return;
        };
        self.spawn_guarded("direct message", None, async move {
            // The protocol has no method for this route yet.
            let body = json!({ "recipient_id": user });
            let channel = ctx
                .session
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
                    if let Some(e) = failed(&ctx.internal, ctx.gen, err) {
                        fail(&ctx.events, e.reason);
                    }
                    return;
                }
            };
            let Some(channel_id) = channel_id else {
                fail(&ctx.events, t!("composer-bad-answer"));
                return;
            };
            message.channel_id = channel_id;
            match ctx.post(&message).await {
                Ok(()) => {
                    let _ = ctx
                        .events
                        .unbounded_send(Ev::Notice(t!("composer-msg-sent")));
                }
                Err(Some(e)) => fail(&ctx.events, e.reason),
                Err(None) => {}
            }
        });
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

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn a_send_becomes_the_protocols_message_with_the_picked_mentions() {
        let picked = Mentions {
            users: vec![1, 2],
            roles: vec![3],
            ..Default::default()
        };
        let m = outgoing(picked, None, false).to_message().unwrap();
        assert_eq!(m.content.as_deref(), Some("hi <@1>"));
        assert_eq!(m.nonce.as_deref(), Some("99"));
        let a = m.allowed_mentions.unwrap();
        assert!(a.parse.is_empty(), "nothing is parsed beyond the lists");
        assert_eq!(a.users, strings(&["1", "2"]));
        assert_eq!(a.roles, strings(&["3"]));
        assert!(m.reply_to.is_none() && m.flags.is_none());

        let none = outgoing(Mentions::default(), None, true)
            .to_message()
            .unwrap();
        let a = none.allowed_mentions.unwrap();
        assert!(a.parse.is_empty() && a.users.is_empty() && a.roles.is_empty());
        assert_eq!(none.flags, Some(4096), "@silent");
    }

    #[test]
    fn everyone_alone_is_parsed_and_with_lists_falls_back_to_all() {
        let only = Mentions {
            everyone: true,
            ..Default::default()
        };
        let a = only.to_allowed().unwrap();
        assert_eq!(a.parse, strings(&["everyone"]));
        assert!(a.users.is_empty());
        let both = Mentions {
            users: vec![1],
            everyone: true,
            ..Default::default()
        };
        assert_eq!(both.to_allowed().unwrap(), p::AllowedMentions::all());
    }

    #[test]
    fn a_reply_carries_the_mention_switch() {
        let quiet = Mentions {
            replied_user: Some(false),
            ..Default::default()
        };
        let m = outgoing(quiet, Some("55"), false).to_message().unwrap();
        let reply = m.reply_to.unwrap();
        assert_eq!(reply.message_id, "55");
        assert!(!reply.mention_author);
        assert_eq!(m.allowed_mentions.unwrap().replied_user, Some(false));
    }

    #[test]
    fn an_edit_never_adds_the_replied_author() {
        // The original is not known to have mentioned its reply's author: false, so the
        // server cannot add that author as a mention the person never picked.
        let m = Mentions::from_editor(
            &inlet_editor::AllowedMentions {
                users: vec![inlet_editor::Id(7)],
                ..Default::default()
            },
            Some(false),
        );
        let edit = edit_of("new <@7>", &m).unwrap();
        assert_eq!(edit.content.as_deref(), Some("new <@7>"));
        let a = edit.allowed_mentions.unwrap();
        assert_eq!(a.users, strings(&["7"]));
        assert!(a.parse.is_empty());
        assert_eq!(a.replied_user, Some(false));
    }

    #[test]
    fn only_snowflakes_go_into_paths() {
        assert_eq!(snowflake("123"), Some("123"));
        for bad in ["", "0123", "12a", "../1", "1/2", "123456789012345678901", "@me"] {
            assert_eq!(snowflake(bad), None, "{bad:?}");
        }
    }
}
