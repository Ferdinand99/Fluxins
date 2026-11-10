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

use gpui::{prelude::*, Context, Entity, Subscription, Window};
use inlet_editor::{Id, Resolve};

use crate::app::Inlet;
use crate::backend::composer::{Cmd, Ev};
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
        }
    }

    /// Sign-out: the account's drafts are deleted at once; the view forgets the rest
    /// (reply, edit, failed sends) when it opens the next channel.
    pub fn sign_out(&mut self) {
        self.shared.borrow_mut().drafts.sign_out();
        self.user_id = None;
        self.session_asked = false;
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

impl Inlet {
    /// Opens `channel_id` in the composer: its draft, placeholder and slowmode.
    pub(crate) fn composer_open_channel(
        &mut self,
        channel_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.composer.session_asked {
            self.composer.session_asked = true;
            self.backend.send(Cmd::Session);
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
                let Some(message) = edit_inline::last_editable(shown, &me).cloned() else {
                    return;
                };
                let names = AuthorNames(
                    self.chat
                        .messages
                        .iter()
                        .filter_map(|m| {
                            Some((m.message.author_id.parse().ok()?, m.message.author.clone()))
                        })
                        .collect(),
                );
                self.composer.view.update(cx, |v, cx| {
                    v.start_edit(
                        message.id,
                        message.channel_id,
                        &message.content,
                        &names,
                        window,
                        cx,
                    )
                });
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
                user_id,
                max_message_length,
            } => {
                // An answer for a session that has signed out since is dropped.
                if !self.composer.session_asked {
                    return;
                }
                let file = dirs::config_dir()
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
            } => view.update(cx, |v, cx| {
                v.on_send_failed(&nonce, reason, retry_after_secs, cx)
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
