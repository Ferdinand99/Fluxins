// SPDX-License-Identifier: AGPL-3.0-or-later
//! The message composer under the chat, built on `inlet-editor`.
//!
//! [`ComposerView`] (in `editor`) is its own entity: the text field with pills, the status
//! row above it, the reply bar, the edit box and the autocomplete list. This file connects
//! it to the app: it opens channels in it, hands its requests to the backend and passes the
//! backend's answers back. The pickers (emoji, GIF, stickers) and attachments have their
//! own modules.

mod attachments;
pub mod autocomplete;
pub mod commands;
pub mod drafts;
pub mod edit_inline;
pub mod editor;
mod pickers;
pub mod reply_bar;
pub mod slowmode;
mod status_row;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gpui::{prelude::*, App, Context, Entity, Subscription, Window};
use inlet_editor::{Id, Resolve};

use crate::app::Inlet;
use crate::backend::composer::{Cmd, Ev};
use crate::model::Message;
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

/// Names of message authors, for the pills of a message being edited.
struct AuthorNames(HashMap<u64, String>);

impl Resolve for AuthorNames {
    fn user(&self, id: Id) -> Option<String> {
        self.0.get(&id.0).cloned()
    }
    fn role(&self, _: Id) -> Option<String> {
        None
    }
    fn channel(&self, _: Id) -> Option<String> {
        None
    }
}

/// Up in an empty composer: opens the account's newest message among `shown` (oldest
/// first) for editing. This is the one place that reads the message list for it.
pub(crate) fn edit_last<'a>(
    view: &Entity<ComposerView>,
    shown: impl DoubleEndedIterator<Item = &'a Message> + Clone,
    me: &str,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(message) = edit_inline::last_editable(shown.clone(), me).cloned() else {
        return;
    };
    let names = AuthorNames(
        shown
            .filter_map(|m| Some((m.author_id.parse().ok()?, m.author.clone())))
            .collect(),
    );
    let target = edit_inline::EditTarget {
        message_id: message.id,
        channel_id: message.channel_id,
        content: message.content,
        // Whether the original reply notified its author is not in the list's model
        // yet; unknown keeps the author from being added by the edit.
        replied_user: None,
    };
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
        self.composer
            .view
            .update(cx, |v, cx| v.set_channel(ctx, window, cx));
    }

    fn on_composer_view_event(
        &mut self,
        event: ComposerEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            ComposerEvent::Send(message) => self.backend.send(Cmd::Send(message)),
            ComposerEvent::SendUnlessSent(message) => {
                self.backend.send(Cmd::SendUnlessSent(message))
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
            ComposerEvent::Directory { guild_id } => self.backend.send(Cmd::Directory { guild_id }),
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
                let me = self.composer.user_id.clone().unwrap_or_default();
                let shown = self.chat.messages.iter().map(|m| &m.message);
                edit_last(&self.composer.view, shown, &me, window, cx);
            }
        }
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
        let view = self.composer.view.clone();
        match event {
            Ev::Session {
                token,
                user_id,
                max_message_length,
            } => {
                // An answer for an account that has signed out since is dropped.
                if !self.composer.session_asked || token != self.composer.session_token {
                    return;
                }
                // Local, not roaming: drafts are message text and stay on this machine.
                let file = dirs::data_local_dir()
                    .and_then(|dir| drafts::path_for(&dir, &self.instance, &user_id));
                self.composer.user_id = Some(user_id);
                view.update(cx, |v, cx| {
                    v.set_session(max_message_length, file, window, cx)
                });
            }
            Ev::Sent { nonce } => view.update(cx, |v, cx| v.on_sent(&nonce, cx)),
            Ev::SendFailed {
                nonce,
                reason,
                retry_after_secs,
                ambiguous,
            } => view.update(cx, |v, cx| {
                v.on_send_failed(&nonce, reason, retry_after_secs, ambiguous, cx)
            }),
            Ev::DirectMessageFailed { nonce, reason } => view.update(cx, |v, cx| {
                v.on_direct_message_failed(&nonce, reason, window, cx)
            }),
            Ev::EditFailed { reason } => view.update(cx, |v, cx| v.on_edit_failed(reason, cx)),
            Ev::Directory {
                guild_id,
                directory,
            } => view.update(cx, |v, cx| v.on_directory(guild_id, directory, cx)),
            Ev::Members { guild_id, members } => {
                view.update(cx, |v, cx| v.on_members(guild_id, members, cx))
            }
            Ev::Notice(text) => self.chat.notice = Some(text),
        }
    }
}
