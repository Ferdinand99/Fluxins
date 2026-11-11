// SPDX-License-Identifier: AGPL-3.0-or-later
//! The message composer under the chat, built on `inlet-editor`.
//!
//! [`ComposerView`] (in `editor`) is its own entity: the text field with pills, the status
//! row above it, the reply bar, the edit box and the autocomplete list. This file connects
//! it to the app: it opens channels in it, hands its requests to the backend and passes the
//! backend's answers back. The pickers (emoji, GIF, stickers) and attachments have their
//! own modules; a message with files goes the same way as any other send (one nonce, one
//! pending row, Retry), as [`Cmd::Upload`].

pub mod attachments;
pub mod autocomplete;
pub mod commands;
pub mod drafts;
pub mod edit_inline;
pub mod editor;
mod pickers;
pub mod reply_bar;
pub mod slowmode;
mod status_row;
pub mod upload_queue;

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{prelude::*, App, Context, Entity, Subscription, Window};
use inlet_state::{AccountState, ChannelId};

use crate::app::Inlet;
use crate::backend::composer::{Cmd, Ev, Outgoing, Upload};
use crate::backend::messages;
use crate::ui::chat::messages::MessageView;
use crate::ui::sidebar::channel_list::channel_label;
pub use editor::{bind_keys, ChannelContext, ComposerEvent, ComposerView, Shared};

/// The message length limit until the instance's own is known: the smaller of Fluxer's
/// two, so nothing typed before can be too long for the server.
const DEFAULT_MAX_LEN: usize = 2000;

pub struct ComposerState {
    pub view: Entity<ComposerView>,
    pub shared: Rc<RefCell<Shared>>,
    /// The signed-in account's ID, once the backend has told.
    pub user_id: Option<String>,
    /// The account's upload limits, for the thread panel's composer too.
    pub upload_limits: crate::backend::composer::Limits,
    session_asked: bool,
    /// Tags each request for the account ID; answers with an older tag are dropped, so
    /// a slow answer for an account that signed out cannot pick the drafts file.
    session_token: u64,
}

impl ComposerState {
    pub fn new(
        window: &mut Window,
        cx: &mut Context<Inlet>,
        subscriptions: &mut Vec<Subscription>,
    ) -> Self {
        let shared: Rc<RefCell<Shared>> = Rc::default();
        let view = {
            let shared = shared.clone();
            cx.new(|cx| ComposerView::new(shared, DEFAULT_MAX_LEN, window, cx))
        };
        subscriptions.push(cx.subscribe_in(
            &view,
            window,
            |this, _, event: &ComposerEvent, window, cx| {
                this.on_composer_view_event(event.clone(), window, cx)
            },
        ));
        Self {
            view,
            shared,
            user_id: None,
            upload_limits: Default::default(),
            session_asked: false,
            session_token: 0,
        }
    }

    /// The session is gone (sign-out, or the backend restarts): ask again next time.
    pub fn reset_session(&mut self) {
        self.user_id = None;
        self.session_asked = false;
        self.session_token += 1;
    }

    /// Sign-out: the account's drafts are deleted at once; the view forgets the rest
    /// (reply, edit, failed sends) when it opens the next channel.
    pub fn sign_out(&mut self) {
        self.shared.borrow_mut().sign_out();
        self.reset_session();
    }
}

/// A send from the composer: the message list shows its pending row first, then the
/// returned command sends it, so no answer can come before the row exists.
pub fn begin_send(view: &mut MessageView, message: Outgoing) -> Cmd {
    view.upsert_pending(pending_of(&message));
    Cmd::Send(message)
}

/// The pending row a send shows in the message list until the server took it.
pub fn pending_of(message: &Outgoing) -> messages::Pending {
    messages::Pending {
        channel_id: message.channel_id.clone(),
        nonce: message.nonce.clone(),
        text: message.content.clone(),
        reply_to: message.reply_to.clone(),
        files: Vec::new(),
        failed: None,
    }
}

/// A message with files: the same pending row (with the file names) before the command
/// goes out, so a failed upload shows Retry like any failed send.
pub fn begin_upload(view: &mut MessageView, upload: Upload) -> Cmd {
    let mut pending = pending_of(&upload.message);
    pending.files = upload.files.iter().map(|f| f.name.clone()).collect();
    view.upsert_pending(pending);
    Cmd::Upload(upload)
}

/// Up in an empty composer: opens the account's newest message in `channel` for editing,
/// read from the account state's window (the message list's source).
pub(crate) fn edit_last(
    view: &Entity<ComposerView>,
    account: &Entity<AccountState>,
    channel: ChannelId,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(target) = edit_inline::last_own(account.read(cx), channel) else {
        return;
    };
    start_edit_of(view, account, channel, target, window, cx);
}

/// "Edit" in a message's actions: opens that message for editing, if it is one of the
/// account's own in `channel`'s window.
pub(crate) fn edit_message(
    view: &Entity<ComposerView>,
    account: &Entity<AccountState>,
    channel: ChannelId,
    message: inlet_state::MessageId,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(target) = edit_inline::own(account.read(cx), channel, message) else {
        return;
    };
    start_edit_of(view, account, channel, target, window, cx);
}

fn start_edit_of(
    view: &Entity<ComposerView>,
    account: &Entity<AccountState>,
    channel: ChannelId,
    target: edit_inline::EditTarget,
    window: &mut Window,
    cx: &mut App,
) {
    let state = account.read(cx);
    let guild = state.channels().guild_of(channel);
    // The labels are worked out now; the edit box keeps them.
    let draft = {
        let names = edit_inline::StateNames { state, guild };
        let buffer = inlet_editor::Buffer::from_wire(&target.content, &names);
        drafts::Draft::of(&buffer)
    };
    let names = draft.unwrap_or_default();
    view.update(cx, |v, cx| v.start_edit(target, &names, window, cx));
}

impl Inlet {
    /// Opens `channel_id` in the composer: its draft, placeholder and slowmode. Opening
    /// the open channel again takes its new settings (for example a changed slowmode).
    pub(crate) fn composer_open_channel(
        &mut self,
        channel_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.composer.session_asked {
            self.composer.session_asked = true;
            self.backend.send(Cmd::Session {
                token: self.composer.session_token,
            });
        }
        let channel = self.channels.iter().find(|c| c.id == channel_id);
        let ctx = ChannelContext {
            channel_id: channel_id.to_owned(),
            guild_id: self.selected_guild.clone(),
            placeholder: match channel {
                Some(c) => t!("composer-placeholder", channel = channel_label(c)),
                None => t!("composer-placeholder-empty"),
            },
            slowmode_secs: channel.map_or(0, |c| c.slowmode_secs),
        };
        let account = self.account.as_ref().map(Entity::downgrade);
        // The account first: opening the channel looks in its messages (a draft of a
        // message that was sent after all goes).
        self.composer.view.update(cx, |v, cx| {
            v.set_account(account, cx);
            v.set_channel(ctx, window, cx);
        });
    }

    fn on_composer_view_event(
        &mut self,
        event: ComposerEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            ComposerEvent::Send(message) => {
                // The row shows "sending" at once; the backend reports the outcome to it.
                let cmd = begin_send(&mut self.chat.view, message);
                self.refresh_rows(cx);
                self.backend.send(cmd);
            }
            ComposerEvent::Upload(upload) => {
                let cmd = begin_upload(&mut self.chat.view, upload);
                self.refresh_rows(cx);
                self.backend.send(cmd);
            }
            ComposerEvent::CancelUpload { nonce } => {
                // The message is not sent: its pending row goes with the upload.
                self.backend.send(Cmd::CancelUpload {
                    nonce: nonce.clone(),
                });
                if self.chat.view.sent(&nonce) {
                    self.refresh_rows(cx);
                }
            }
            ComposerEvent::Edit {
                channel_id,
                message_id,
                content,
                mentions,
            } => self.backend.send(Cmd::Edit {
                channel_id,
                message_id,
                content,
                mentions,
            }),
            ComposerEvent::Typing { channel_id } => self.backend.send(Cmd::Typing { channel_id }),
            ComposerEvent::SearchMembers {
                guild_id,
                query,
                nonce,
            } => self.backend.send(Cmd::SearchMembers {
                guild_id,
                query,
                nonce,
            }),
            ComposerEvent::SetNick { guild_id, nick } => {
                self.backend.send(Cmd::SetNick { guild_id, nick })
            }
            ComposerEvent::DirectMessage { user_id, message } => {
                self.backend.send(Cmd::DirectMessage { user_id, message })
            }
            ComposerEvent::EditLast { channel_id } => {
                if self.selected_channel.as_ref() != Some(&channel_id) {
                    return;
                }
                let (Some(account), Some(channel)) =
                    (self.account.clone(), ChannelId::parse(&channel_id))
                else {
                    return;
                };
                edit_last(&self.composer.view, &account, channel, window, cx);
            }
        }
    }

    /// Retry in the message list sends again: the composer's slowmode counts it.
    pub(crate) fn composer_resent(&mut self, channel_id: &str, cx: &mut Context<Self>) {
        self.composer
            .view
            .update(cx, |v, cx| v.on_resend(channel_id, cx));
    }

    pub(crate) fn render_composer(&self) -> impl IntoElement {
        self.composer.view.clone()
    }

    /// Events from the backend's composer domain.
    pub(crate) fn on_composer_event(
        &mut self,
        event: Ev,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The thread panel's composer takes its share first.
        let Some(event) = self.panel_composer_event(event, window, cx) else {
            return;
        };
        let view = self.composer.view.clone();
        match event {
            Ev::Session {
                token,
                user_id,
                max_message_length,
                uploads,
            } => {
                // An answer for an account that has signed out since is dropped.
                if !self.composer.session_asked || token != self.composer.session_token {
                    return;
                }
                // Local, not roaming: drafts are message text and stay on this machine.
                let file = dirs::data_local_dir()
                    .and_then(|dir| drafts::path_for(&dir, &self.instance, &user_id));
                self.composer.user_id = Some(user_id);
                self.composer.upload_limits = uploads;
                view.update(cx, |v, cx| {
                    v.set_session(max_message_length, file, window, cx);
                    v.set_upload_limits(uploads);
                });
                self.panel_session(max_message_length, window, cx);
                if let Some(panel) = self.threads.composer.clone() {
                    panel.update(cx, |v, _| v.set_upload_limits(uploads));
                }
            }
            Ev::SlowmodeWait { channel_id, secs } => {
                view.update(cx, |v, cx| v.on_slowmode_wait(&channel_id, secs, cx))
            }
            Ev::DirectMessageFailed { nonce, reason } => view.update(cx, |v, cx| {
                v.on_direct_message_failed(&nonce, reason, window, cx)
            }),
            Ev::Edited { message_id } => {
                view.update(cx, |v, cx| v.on_edited(&message_id, window, cx))
            }
            Ev::EditFailed { message_id, reason } => {
                let shown = view.update(cx, |v, cx| v.on_edit_failed(&message_id, &reason, cx));
                if !shown {
                    // The edit box is gone (another channel): say it in the notice bar.
                    self.chat.notice = Some(t!("composer-edit-failed", error = reason));
                }
            }
            Ev::Members { guild_id, members } => {
                view.update(cx, |v, cx| v.on_members(guild_id, members, cx))
            }
            Ev::Notice(text) => self.chat.notice = Some(text),
            Ev::UploadStarted {
                nonce,
                channel_id,
                names,
                total,
            } => view.update(cx, |v, cx| {
                v.on_upload_started(nonce, channel_id, names, total, cx)
            }),
            Ev::UploadProgress { nonce, sent, total } => {
                view.update(cx, |v, cx| v.on_upload_progress(&nonce, sent, total, cx))
            }
            Ev::UploadStuck { nonce, name } => {
                if self.chat.view.sent(&nonce) {
                    self.refresh_rows(cx);
                }
                view.update(cx, |v, cx| v.on_upload_stuck(&nonce, &name, window, cx))
            }
            Ev::UploadDone { nonce, sent } => {
                view.update(cx, |v, cx| v.on_upload_done(&nonce, sent, cx))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::composer::Mentions;
    use crate::ui::chat::messages::{MessageView, Row};

    fn outgoing() -> Outgoing {
        Outgoing {
            channel_id: "301".into(),
            nonce: "777".into(),
            content: "hello".into(),
            mentions: Mentions::default(),
            reply_to: None,
            silent: false,
        }
    }

    fn pending_rows(view: &mut MessageView, state: &AccountState) -> Vec<(String, Option<String>)> {
        let ch = ChannelId::parse("301").unwrap();
        view.build_rows(state, ch)
            .into_iter()
            .filter_map(|r| match r {
                Row::Pending(p) => Some((p.nonce.clone(), p.failed.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_send_shows_a_pending_row_until_the_server_took_it() {
        let (state, _) = edit_inline::tests::state_with(serde_json::json!([]));
        let mut view = MessageView::new();
        // What the app does for the composer's send.
        view.upsert_pending(pending_of(&outgoing()));
        assert_eq!(pending_rows(&mut view, &state), [("777".to_owned(), None)]);
        // The backend's `Sent` with the nonce.
        assert!(view.sent("777"));
        assert!(pending_rows(&mut view, &state).is_empty());
    }

    #[test]
    fn a_failed_send_offers_retry_with_the_same_nonce() {
        let (state, _) = edit_inline::tests::state_with(serde_json::json!([]));
        let mut view = MessageView::new();
        view.upsert_pending(pending_of(&outgoing()));
        // The backend reports the failure on the same row.
        let mut failed = pending_of(&outgoing());
        failed.failed = Some("offline".into());
        view.upsert_pending(failed);
        assert_eq!(
            pending_rows(&mut view, &state),
            [("777".to_owned(), Some("offline".to_owned()))],
            "the row shows the reason (and Retry)"
        );
        match view.retry("777") {
            Some(messages::Cmd::Resend {
                channel_id, nonce, ..
            }) => {
                assert_eq!(channel_id, "301");
                assert_eq!(
                    nonce, "777",
                    "the first nonce, so the server can drop a copy"
                );
            }
            _ => panic!("Retry sends the message again"),
        }
        assert_eq!(pending_rows(&mut view, &state), [("777".to_owned(), None)]);
        assert!(view.retry("777").is_none(), "only a failed row retries");
    }

    #[test]
    fn a_page_clears_a_row_by_its_own_text_not_by_nonce() {
        use crate::backend::messages::outbox::tests::listed;
        let (state, _) = edit_inline::tests::state_with(serde_json::json!([]));
        let mut view = MessageView::new();
        let mut failed = pending_of(&outgoing());
        failed.failed = Some("timed out".into());
        view.upsert_pending(failed);
        let t = chrono::Utc::now().timestamp_millis() as u64;
        // Lists carry no nonce. Someone else's message, other text, or an own message
        // from before the send do not clear the row.
        let page = [
            listed(t + 1_000, "20", "hello", 0),
            listed(t + 1_000, "10", "hello again", 0),
            listed(t - 60_000, "10", "hello", 0),
        ];
        assert!(!view.clear_arrived("10", "301", &page));
        assert!(!view.clear_arrived("10", "302", &[listed(t + 1_000, "10", "hello", 0)]));
        assert_eq!(pending_rows(&mut view, &state).len(), 1);
        // The own copy, sent after the row was shown, does.
        assert!(view.clear_arrived("10", "301", &[listed(t + 1_000, "10", "hello", 0)]));
        assert!(pending_rows(&mut view, &state).is_empty());
    }
}
