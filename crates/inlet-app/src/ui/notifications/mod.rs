// SPDX-License-Identifier: AGPL-3.0-or-later
//! Unread state, mention badges, muting and notification levels, as the UI holds and
//! draws them. The backend (`backend/notifications`) applies Fluxer's rules and sends
//! plain [`Badge`]s keyed by the channel and guild IDs the views already use; the
//! channel list, the guild rail and the settings window read them from here.
//!
//! For the message view: tell this domain what is being read with [`Inlet::set_scrolled_up`]
//! (the person left the bottom of the list, so new messages stay unread) and read where the
//! account had read to when a channel was opened from `State::opened`. Opening a channel in
//! a window that is in front reads it up to its newest message; messages that arrive while
//! it is open and at the bottom are read as they come, and acknowledged at most once every
//! three seconds.

pub mod badge;

use std::collections::HashMap;

use gpui::{actions, div, prelude::*, px, AnyElement, App, Context, KeyBinding, Window};

use crate::app::{Inlet, Screen};
use crate::assets::Icon;
use crate::backend::notifications::{
    self as backend, Badge, BadgeUpdate, Change, SettingsView, Target,
};
use crate::theme;

actions!(notifications, [MarkChannelRead, MarkGuildRead]);

/// Escape reads the open channel; Shift+Escape reads every channel of the open server
/// (or every direct message). Bound for the whole window, so a more specific binding
/// (a text field's own Escape) wins where there is one.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("escape", MarkChannelRead, None),
        KeyBinding::new("shift-escape", MarkGuildRead, None),
    ]);
}

/// Where the account had read to when a channel was opened.
#[allow(dead_code)] // read by the message view for the "new messages" divider
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenedRead {
    pub channel_id: String,
    /// The last message the account had read, before opening read it.
    pub last_read: Option<String>,
    /// Unread mentions there were.
    pub mentions: u32,
}

/// The settings window: for which guild and channel, and their settings once they arrive.
pub struct SettingsWindow {
    pub guild_id: Option<String>,
    pub channel_id: Option<String>,
    pub view: Option<SettingsView>,
}

/// This domain's view state.
#[derive(Default)]
pub struct State {
    channels: HashMap<String, Badge>,
    guilds: HashMap<String, Badge>,
    dm: Badge,
    /// Set when a channel is opened.
    #[allow(dead_code)] // read by the message view for the "new messages" divider
    pub opened: Option<OpenedRead>,
    /// The channel the backend was last told is being read.
    watching: Option<String>,
    /// The message list is not at the bottom.
    scrolled_up: bool,
    pub settings: Option<SettingsWindow>,
}

impl State {
    /// Applies what the backend says changed.
    pub fn apply(&mut self, update: BadgeUpdate) {
        if update.reset {
            self.channels.clear();
            self.guilds.clear();
            self.dm = Badge::default();
        }
        for (id, badge) in update.channels {
            if badge.is_default() {
                self.channels.remove(&id);
            } else {
                self.channels.insert(id, badge);
            }
        }
        for (id, badge) in update.guilds {
            match id {
                None => self.dm = badge,
                Some(id) if badge.is_default() => {
                    self.guilds.remove(&id);
                }
                Some(id) => {
                    self.guilds.insert(id, badge);
                }
            }
        }
    }

    /// Forgets everything (signed out, or a new backend).
    pub fn clear(&mut self) {
        *self = State::default();
    }
}

/// The notification settings button in the channel header (see `ui/chat/header_buttons.rs`).
pub fn header_button(this: &Inlet, cx: &mut Context<Inlet>) -> Option<AnyElement> {
    let channel = this.selected_channel.as_deref()?;
    let icon = if this.badge(channel).muted {
        Icon::BellSlash
    } else {
        Icon::Bell
    };
    Some(
        div()
            .id("notification-settings")
            .flex_none()
            .p(px(6.))
            .rounded(px(6.))
            .cursor_pointer()
            .hover(|s| s.bg(theme::hover_bg()))
            .child(icon.el(px(24.), theme::text_muted()))
            .on_click(cx.listener(|this, _, _, cx| this.open_notification_settings(cx)))
            .into_any_element(),
    )
}

impl Inlet {
    /// What a channel, category or direct message shows. Nothing for one that is read.
    pub fn badge(&self, channel_id: &str) -> Badge {
        self.notifications
            .channels
            .get(channel_id)
            .copied()
            .unwrap_or_default()
    }

    /// What a guild's rail entry shows; `None` is the direct messages button.
    pub fn guild_badge(&self, guild_id: Option<&str>) -> Badge {
        match guild_id {
            Some(id) => self
                .notifications
                .guilds
                .get(id)
                .copied()
                .unwrap_or_default(),
            None => self.notifications.dm,
        }
    }

    /// Events from the backend's notifications domain.
    pub(crate) fn on_notifications_event(
        &mut self,
        event: backend::Ev,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        match event {
            backend::Ev::Badges(update) => self.notifications.apply(update),
            backend::Ev::Opened {
                channel_id,
                last_read,
                mentions,
            } => {
                self.notifications.opened = Some(OpenedRead {
                    channel_id,
                    last_read,
                    mentions,
                })
            }
            backend::Ev::Settings(view) => {
                if let Some(w) = &mut self.notifications.settings {
                    if w.guild_id == view.guild_id && w.channel_id == view.channel_id {
                        w.view = Some(view);
                    }
                }
            }
        }
    }

    /// Tells the backend which channel is being read: the open one while the window is in
    /// front and the message list is at the bottom. Called every frame; it only sends when
    /// the answer changes.
    pub(crate) fn sync_watching(&mut self, window: &Window) {
        let want = (self.screen == Screen::Main
            && window.is_window_active()
            && !self.notifications.scrolled_up)
            .then(|| self.selected_channel.clone())
            .flatten();
        if want != self.notifications.watching {
            self.notifications.watching = want.clone();
            self.backend.send(backend::Cmd::Watch { channel_id: want });
        }
    }

    /// The message list left (or came back to) the bottom. While it is away, new messages
    /// stay unread.
    #[allow(dead_code)] // called by the message view
    pub fn set_scrolled_up(&mut self, scrolled_up: bool, cx: &mut Context<Self>) {
        if self.notifications.scrolled_up != scrolled_up {
            self.notifications.scrolled_up = scrolled_up;
            cx.notify();
        }
    }

    /// "Mark as read" for the open channel.
    pub(crate) fn mark_channel_read(&mut self) {
        if let Some(channel_id) = self.selected_channel.clone() {
            self.backend
                .send(backend::Cmd::MarkChannelRead { channel_id });
        }
    }

    /// "Mark as read" for every channel of the open server, or every direct message.
    pub(crate) fn mark_guild_read(&mut self) {
        self.backend.send(backend::Cmd::MarkGuildRead {
            guild_id: self.selected_guild.clone(),
        });
    }

    /// "Mark as read" for everything.
    pub(crate) fn mark_all_read(&mut self) {
        self.backend.send(backend::Cmd::MarkAllRead);
    }

    /// Escape (`shift` false) or Shift+Escape. A window over the app takes the key first.
    fn on_escape(&mut self, shift: bool, cx: &mut Context<Self>) {
        if self.notifications.settings.is_some() {
            self.close_notification_settings(cx);
            return;
        }
        if self.screen != Screen::Main || self.overlays.pending_link.is_some() {
            return;
        }
        if shift {
            self.mark_guild_read();
        } else {
            self.mark_channel_read();
        }
    }

    /// Adds the Escape handlers to the window's root element.
    pub(crate) fn notification_keys<E: InteractiveElement>(
        &self,
        el: E,
        cx: &mut Context<Self>,
    ) -> E {
        el.on_action(cx.listener(|this, _: &MarkChannelRead, _, cx| this.on_escape(false, cx)))
            .on_action(cx.listener(|this, _: &MarkGuildRead, _, cx| this.on_escape(true, cx)))
    }

    /// Opens the settings window for the open channel and its server.
    pub(crate) fn open_notification_settings(&mut self, cx: &mut Context<Self>) {
        let (guild_id, channel_id) = (self.selected_guild.clone(), self.selected_channel.clone());
        self.notifications.settings = Some(SettingsWindow {
            guild_id: guild_id.clone(),
            channel_id: channel_id.clone(),
            view: None,
        });
        self.backend.send(backend::Cmd::ShowSettings {
            guild_id,
            channel_id,
        });
        cx.notify();
    }

    pub(crate) fn close_notification_settings(&mut self, cx: &mut Context<Self>) {
        if self.notifications.settings.take().is_some() {
            self.backend.send(backend::Cmd::HideSettings);
            cx.notify();
        }
    }

    /// A choice in the settings window.
    pub(crate) fn notification_change(&mut self, target: Target, change: Change) {
        self.backend.send(backend::Cmd::Change { target, change });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unread() -> Badge {
        Badge {
            unread: true,
            ..Default::default()
        }
    }

    #[test]
    fn updates_set_replace_and_remove_badges() {
        let mut s = State::default();
        s.apply(BadgeUpdate {
            reset: false,
            channels: vec![("1".into(), unread()), ("2".into(), unread())],
            guilds: vec![(Some("9".into()), unread()), (None, unread())],
        });
        assert_eq!(s.channels.len(), 2);
        assert!(s.guilds.contains_key("9") && s.dm.unread);

        // A default badge is a removal.
        s.apply(BadgeUpdate {
            reset: false,
            channels: vec![("1".into(), Badge::default())],
            guilds: vec![
                (Some("9".into()), Badge::default()),
                (None, Badge::default()),
            ],
        });
        assert!(!s.channels.contains_key("1") && s.channels.contains_key("2"));
        assert!(s.guilds.is_empty() && !s.dm.unread);

        // A reset drops everything first.
        s.apply(BadgeUpdate {
            reset: true,
            channels: vec![("3".into(), unread())],
            guilds: vec![],
        });
        assert_eq!(s.channels.len(), 1);
        assert!(s.channels.contains_key("3"));
    }

    #[test]
    fn clear_forgets_everything() {
        let mut s = State::default();
        s.apply(BadgeUpdate {
            reset: false,
            channels: vec![("1".into(), unread())],
            guilds: vec![],
        });
        s.watching = Some("1".into());
        s.clear();
        assert!(s.channels.is_empty() && s.watching.is_none());
    }
}
