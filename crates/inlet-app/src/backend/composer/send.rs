// SPDX-License-Identifier: AGPL-3.0-or-later
//! Sending and editing messages, typing, and the commands that call the server.
//!
//! Every message and every edit carries `allowed_mentions` built from the mentions the
//! person picked in the editor ([`Mentions`]). The editor also escapes typed text that
//! looks like a mention, but that depends on how the server parses today;
//! `allowed_mentions` is the guard that does not: the server drops every user and role
//! mention that is not on the list, and `@everyone`/`@here` unless one was picked.
//! Sends and edits go through the protocol's `send` and `edit_message_with`.

use std::future::Future;
use std::time::Instant;

use inlet_protocol as p;
use reqwest::Method;
use serde_json::{json, Value};

use super::directory::RetryPlan;
use super::Ev;
use super::State;
use crate::backend::messages;
use crate::backend::messages::outbox::{is_own_copy, Sent, CLOCK_SLACK_MS};
use crate::backend::session::{Failure, FailureKind};
use crate::backend::{Active, Backend, Internal, Ui};

/// The longest nickname Fluxer takes.
const MAX_NICK_CHARS: usize = 32;
/// Messages read to find out whether a send went through after all (the most one
/// request gives).
const COPY_LOOKBACK: u8 = 100;

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

/// A send failed: the message list's row shows why (with Retry), and a wait the server
/// asked for goes to the composer's slowmode countdown.
fn report_failure(events: &Ui, message: &Outgoing, e: SendError) {
    if let Some(secs) = e.retry_after_secs {
        let _ = events.unbounded_send(Ev::SlowmodeWait {
            channel_id: message.channel_id.clone(),
            secs,
        });
    }
    let _ = events.unbounded_send(messages::Ev::Pending(messages::Pending {
        channel_id: message.channel_id.clone(),
        nonce: message.nonce.clone(),
        text: message.content.clone(),
        reply_to: message.reply_to.clone(),
        failed: Some(e.reason),
    }));
}

/// The session's parts a send task needs.
#[derive(Clone)]
struct Ctx {
    client: p::Client,
    events: Ui,
    internal: tokio::sync::mpsc::UnboundedSender<Internal>,
    gen: u64,
    state: State,
}

impl Ctx {
    /// Posts `message`. `Ok(())` when the server took it; `Err(None)` when the session
    /// ended.
    async fn post(&self, message: &Outgoing) -> Result<(), Option<SendError>> {
        let result = match message.to_message() {
            Ok(m) => self.client.send(&message.channel_id, &m).await,
            Err(e) => Err(e.into()),
        };
        match result {
            // The gateway's MESSAGE_CREATE brings the message into the list.
            Ok(_) => Ok(()),
            Err(err) => Err(failed(&self.internal, self.gen, err)),
        }
    }

    /// The server took the message: its pending row goes.
    fn sent(&self, nonce: String) {
        self.state.forget_send(self.gen, &nonce);
        let _ = self.events.unbounded_send(messages::Ev::Sent { nonce });
    }

    fn failed(&self, message: &Outgoing, e: SendError) {
        self.state
            .mark_failed(self.gen, &message.nonce, e.ambiguous);
        report_failure(&self.events, message, e);
    }

    /// Posts `message` and reports the outcome to the message list.
    async fn post_and_report(&self, message: Outgoing) {
        match self.post(&message).await {
            Ok(()) => self.sent(message.nonce),
            Err(Some(e)) => self.failed(&message, e),
            Err(None) => {}
        }
    }

    /// A send from the composer: kept for Retry, then posted.
    fn send(self, message: Outgoing) -> impl Future<Output = ()> + Send {
        self.state.remember_send(self.gen, &message);
        async move { self.post_and_report(message).await }
    }

    /// How a Retry at `now` goes out: as the composer first sent it, or, for a message
    /// it did not send, with no mentions allowed.
    fn retry_plan(
        &self,
        channel_id: String,
        nonce: String,
        text: String,
        now: Instant,
    ) -> RetryPlan {
        self.state
            .retry_of(self.gen, &nonce, now)
            .unwrap_or_else(|| RetryPlan {
                // Not sent from this session's composer: nothing it says may notify
                // anyone.
                message: Outgoing {
                    channel_id,
                    nonce,
                    content: text,
                    mentions: Mentions::default(),
                    reply_to: None,
                    silent: false,
                },
                look_since_ms: None,
            })
    }

    /// Sends a Retry with its first nonce. A late one after an unclear failure first
    /// looks whether the message arrived.
    fn resend(self, plan: RetryPlan) -> impl Future<Output = ()> + Send {
        self.state.remember_send(self.gen, &plan.message);
        async move {
            match plan.look_since_ms {
                Some(since) => self.send_unless_sent(plan.message, since).await,
                None => self.post_and_report(plan.message).await,
            }
        }
    }

    /// Whether a copy of `message`, first sent at `since_ms`, is among the channel's
    /// messages since then. Lists carry no nonce, so it is matched on the author, the
    /// wire text, the reply and the time ([`is_own_copy`]).
    async fn already_sent(&self, message: &Outgoing, since_ms: u64) -> Result<bool, p::Error> {
        let me = match self.state.session(self.gen) {
            Some((id, _)) => id,
            None => self.client.me().await?.id.clone(),
        };
        let after = p::models::snowflake_at_ms(since_ms.saturating_sub(CLOCK_SLACK_MS));
        let list = self
            .client
            .messages_after(&message.channel_id, &after, COPY_LOOKBACK)
            .await?;
        let sent = Sent {
            me: &me,
            content: &message.content,
            reply_to: message.reply_to.as_deref(),
            attachments: 0,
            sent_ms: since_ms,
        };
        // A copy counts for one sent message only: two failed rows with the same text
        // cannot both take the same message.
        Ok(list
            .iter()
            .filter(|m| is_own_copy(m, &sent))
            .any(|m| self.state.claim_copy(self.gen, &m.id)))
    }

    /// A retry long after an unclear failure: the server only recognises a nonce for a
    /// few minutes, so first look whether the message arrived; send it (same nonce)
    /// only if not.
    async fn send_unless_sent(&self, message: Outgoing, since_ms: u64) {
        match self.already_sent(&message, since_ms).await {
            Ok(true) => self.sent(message.nonce),
            Ok(false) => self.post_and_report(message).await,
            Err(err) => {
                if let Some(mut e) = failed(&self.internal, self.gen, err) {
                    // Still not known whether the first attempt arrived.
                    e.ambiguous = true;
                    self.failed(&message, e);
                }
            }
        }
    }
}

/// Why a send, a Retry or an edit could not even start.
fn not_connected() -> SendError {
    SendError {
        reason: t!("composer-not-connected"),
        retry_after_secs: None,
        ambiguous: false,
    }
}

impl Backend {
    fn send_ctx(&self) -> Option<Ctx> {
        match &self.active {
            Some(Active::Real(r)) => Some(Ctx {
                client: r.session.client().clone(),
                events: self.events.clone(),
                internal: self.internal.clone(),
                gen: r.gen,
                state: self.composer.clone(),
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
                user_id: crate::demo::composer::demo_user_id(),
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

    /// Sends a message. The UI has shown it as pending; the outcome goes to that row.
    pub(super) fn send(&mut self, message: Outgoing) {
        #[cfg(feature = "demo")]
        if let Some(Active::Demo(m)) = &mut self.active {
            m.resend(message.channel_id, message.nonce, message.content);
            return;
        }
        match self.send_ctx() {
            Some(ctx) => {
                self.spawn_guarded("send", None, ctx.send(message));
            }
            None => report_failure(&self.events, &message, not_connected()),
        }
    }

    /// Retry of a failed message from the message list, with its first nonce. The
    /// message goes out as it was first sent (mentions, reply, `@silent`). A retry more
    /// than five minutes after an unclear failure first looks whether it arrived.
    pub(in crate::backend) fn composer_resend(
        &mut self,
        channel_id: String,
        nonce: String,
        text: String,
    ) {
        #[cfg(feature = "demo")]
        if let Some(Active::Demo(m)) = &mut self.active {
            m.resend(channel_id, nonce, text);
            return;
        }
        let Some(ctx) = self.send_ctx() else {
            let message = Outgoing {
                channel_id,
                nonce,
                content: text,
                mentions: Mentions::default(),
                reply_to: None,
                silent: false,
            };
            report_failure(&self.events, &message, not_connected());
            return;
        };
        let plan = ctx.retry_plan(channel_id, nonce, text, Instant::now());
        self.spawn_guarded("resend", None, ctx.resend(plan));
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
            self.emit(Ev::Edited { message_id });
            return;
        }
        let Some(ctx) = self.send_ctx() else {
            self.emit(Ev::EditFailed {
                message_id,
                reason: not_connected().reason,
            });
            return;
        };
        self.spawn_guarded("edit", None, async move {
            let result = match edit_of(&content, &mentions) {
                Ok(edit) => {
                    ctx.client
                        .edit_message_with(&channel_id, &message_id, &edit)
                        .await
                }
                Err(e) => Err(e.into()),
            };
            match result {
                // The gateway's MESSAGE_UPDATE brings the new text into the list.
                Ok(_) => {
                    let _ = ctx.events.unbounded_send(Ev::Edited { message_id });
                }
                Err(err) => {
                    if let Some(e) = failed(&ctx.internal, ctx.gen, err) {
                        let _ = ctx.events.unbounded_send(Ev::EditFailed {
                            message_id,
                            reason: e.reason,
                        });
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
                .client
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
                .client
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

    /// The send path against a mock Fluxer: the app's wiring for a send and a Retry,
    /// and the look for a copy before a late Retry.
    mod wire {
        use std::time::{Duration, Instant};

        use futures::channel::mpsc::{unbounded, UnboundedReceiver};
        use serde_json::{json, Value};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, Request, ResponseTemplate};

        use super::super::super::directory::unix_ms;
        use super::super::*;
        use crate::backend::composer::Cmd;
        use crate::backend::{Event, Ui};
        use crate::ui::chat::messages::{MessageView, Row};
        use crate::ui::composer::begin_send;
        use crate::ui::composer::edit_inline::tests::state_with;

        const POSTS: &str = "/api/v1/channels/301/messages";

        fn discovery(base: &str) -> Value {
            json!({
                "api_code_version": 1,
                "endpoints": {
                    "api": format!("{base}/api"), "api_client": format!("{base}/api"),
                    "api_public": format!("{base}/api"), "gateway": "ws://127.0.0.1:1/gateway",
                    "media": format!("{base}/media"), "webapp": base,
                    "invite": format!("{base}/invite")
                },
                "captcha": {"provider": "altcha"},
                "features": {"account_identity": "email", "voice_enabled": true},
                "limits": {"version": 2, "rules": [{"id": "default", "overrides": {}}]},
                "app_public": {"branding": {"product_name": "Mock"}},
                "sso": {"enabled": false},
                "domain_migration": {"enabled": false}
            })
        }

        /// A mock instance and a send context for account `10` on it.
        async fn fixture() -> (MockServer, Ctx, UnboundedReceiver<Event>) {
            static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/.well-known/fluxer"))
                .respond_with(ResponseTemplate::new(200).set_body_json(discovery(&server.uri())))
                .mount(&server)
                .await;
            let info = p::ClientInfo {
                installation_id: "00000000-0000-4000-a000-000000000000".into(),
                ..p::ClientInfo::default()
            };
            let instance = p::Instance::discover_with(&server.uri(), info)
                .await
                .expect("discovery");
            // One token per fixture: clients of one token share their rate state.
            let token = format!(
                "tok-composer-{}",
                NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            );
            let (tx, rx) = unbounded();
            let (internal, _) = tokio::sync::mpsc::unbounded_channel();
            let state = State::default();
            state.set_session(1, "10".into(), 2000);
            let ctx = Ctx {
                client: p::Client::new(instance, p::Token::new(token)),
                events: Ui::new(tx),
                internal,
                gen: 1,
                state,
            };
            (server, ctx, rx)
        }

        fn created(content: &str, nonce: &str) -> Value {
            json!({
                "id": p::models::snowflake_at_ms(unix_ms()), "channel_id": "301",
                "author": {"id": "10", "username": "me"}, "content": content, "nonce": nonce
            })
        }

        fn outgoing() -> Outgoing {
            Outgoing {
                channel_id: "301".into(),
                nonce: "777".into(),
                content: "hi <@7>".into(),
                mentions: Mentions {
                    users: vec![7],
                    ..Default::default()
                },
                reply_to: None,
                silent: false,
            }
        }

        async fn posts(server: &MockServer) -> Vec<Value> {
            server
                .received_requests()
                .await
                .unwrap_or_default()
                .into_iter()
                .filter(|r: &Request| r.method.as_str() == "POST" && r.url.path() == POSTS)
                .map(|r| serde_json::from_slice(&r.body).unwrap())
                .collect()
        }

        /// What the app does with the backend's events for the message list.
        fn apply(view: &mut MessageView, rx: &mut UnboundedReceiver<Event>) {
            while let Ok(event) = rx.try_recv() {
                match event {
                    Event::Messages(messages::Ev::Pending(p)) => view.upsert_pending(p),
                    Event::Messages(messages::Ev::Sent { nonce }) => {
                        view.sent(&nonce);
                    }
                    _ => {}
                }
            }
        }

        fn rows(view: &mut MessageView) -> Vec<(String, bool)> {
            let (state, ch) = state_with(json!([]));
            view.build_rows(&state, ch)
                .into_iter()
                .filter_map(|r| match r {
                    Row::Pending(p) => Some((p.nonce.clone(), p.failed.is_some())),
                    _ => None,
                })
                .collect()
        }

        /// A message whose first send failed unclearly more than five minutes ago.
        fn late_unclear(ctx: &Ctx) -> RetryPlan {
            ctx.state.remember_send(ctx.gen, &outgoing());
            ctx.state.mark_failed(ctx.gen, "777", true);
            let later = Instant::now() + Duration::from_secs(6 * 60);
            let plan = ctx.retry_plan("301".into(), "777".into(), "hi <@7>".into(), later);
            assert!(plan.look_since_ms.is_some(), "a late retry looks first");
            plan
        }

        /// A message list with the message's failed row.
        fn failed_row() -> MessageView {
            let mut view = MessageView::new();
            let mut failed = crate::ui::composer::pending_of(&outgoing());
            failed.failed = Some("timed out".into());
            view.upsert_pending(failed);
            view
        }

        #[tokio::test]
        async fn send_and_retry_go_through_the_app_wiring() {
            let (server, ctx, mut rx) = fixture().await;
            Mock::given(method("POST"))
                .and(path(POSTS))
                .respond_with(ResponseTemplate::new(503))
                .up_to_n_times(1)
                .with_priority(1)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path(POSTS))
                .respond_with(ResponseTemplate::new(200).set_body_json(created("hi <@7>", "777")))
                .mount(&server)
                .await;

            // The composer's send: the row is in the list before the command goes out.
            let mut view = MessageView::new();
            let Cmd::Send(message) = begin_send(&mut view, outgoing()) else {
                panic!("a send");
            };
            assert_eq!(rows(&mut view), [("777".to_owned(), false)]);
            // What `Backend::send` runs. The server fails: the row shows Retry.
            ctx.clone().send(message).await;
            apply(&mut view, &mut rx);
            assert_eq!(rows(&mut view), [("777".to_owned(), true)]);

            // Retry in the row: the message list's command, then the backend's resend.
            let Some(messages::Cmd::Resend {
                channel_id,
                nonce,
                text,
            }) = view.retry("777")
            else {
                panic!("a failed row retries");
            };
            assert_eq!(rows(&mut view), [("777".to_owned(), false)]);
            let plan = ctx.retry_plan(channel_id, nonce, text, Instant::now());
            assert!(plan.look_since_ms.is_none(), "within the nonce window");
            ctx.clone().resend(plan).await;
            apply(&mut view, &mut rx);
            assert!(rows(&mut view).is_empty(), "the server took it");

            let sent = posts(&server).await;
            assert_eq!(sent.len(), 2);
            for body in &sent {
                assert_eq!(body["nonce"], "777", "the same nonce both times");
                assert_eq!(body["allowed_mentions"]["users"], json!(["7"]));
                assert_eq!(body["allowed_mentions"]["parse"], json!([]));
            }
        }

        #[tokio::test]
        async fn a_late_retry_finds_the_message_by_its_text_and_does_not_send_it_again() {
            let (server, ctx, mut rx) = fixture().await;
            let plan = late_unclear(&ctx);
            // The first attempt did arrive. A list has no nonce.
            let copy = json!({
                "id": p::models::snowflake_at_ms(unix_ms() + 500), "channel_id": "301",
                "author": {"id": "10", "username": "me"}, "content": "hi <@7>"
            });
            Mock::given(method("GET"))
                .and(path(POSTS))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!([copy])))
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path(POSTS))
                .respond_with(ResponseTemplate::new(200).set_body_json(created("hi <@7>", "777")))
                .mount(&server)
                .await;
            let mut view = failed_row();
            ctx.clone().resend(plan).await;
            apply(&mut view, &mut rx);
            assert!(posts(&server).await.is_empty(), "not posted twice");
            assert!(rows(&mut view).is_empty(), "the row counts as sent");
        }

        #[tokio::test]
        async fn a_late_retry_without_a_copy_sends_with_the_same_nonce() {
            let (server, ctx, mut rx) = fixture().await;
            let plan = late_unclear(&ctx);
            let at = p::models::snowflake_at_ms(unix_ms() + 500);
            // Someone else wrote the same; this account wrote something else.
            let list = json!([
                {"id": at, "channel_id": "301", "author": {"id": "20", "username": "ada"},
                 "content": "hi <@7>"},
                {"id": at, "channel_id": "301", "author": {"id": "10", "username": "me"},
                 "content": "hi"}
            ]);
            Mock::given(method("GET"))
                .and(path(POSTS))
                .respond_with(ResponseTemplate::new(200).set_body_json(list))
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path(POSTS))
                .respond_with(ResponseTemplate::new(200).set_body_json(created("hi <@7>", "777")))
                .mount(&server)
                .await;

            let mut view = failed_row();
            ctx.clone().resend(plan).await;
            let sent = posts(&server).await;
            assert_eq!(sent.len(), 1, "sent again");
            assert_eq!(sent[0]["nonce"], "777");
            assert_eq!(sent[0]["allowed_mentions"]["users"], json!(["7"]));
            apply(&mut view, &mut rx);
            assert!(rows(&mut view).is_empty(), "the server took it");
            // The look asked only for messages since the first send.
            let lookups: Vec<_> = server
                .received_requests()
                .await
                .unwrap_or_default()
                .into_iter()
                .filter(|r| r.method.as_str() == "GET" && r.url.path() == POSTS)
                .collect();
            assert_eq!(lookups.len(), 1);
            assert!(lookups[0].url.query().is_some_and(|q| q.contains("after=")));
        }
    }
}
