// SPDX-License-Identifier: AGPL-3.0-or-later
//! Unread state, mention badges, muting and notification levels, as the UI holds and
//! draws them. There is one account state (`Inlet.account`, on the UI thread): the unread
//! rules, the badges and the settings all read and change that, never a copy. The backend
//! (`backend/notifications`) only sends acknowledgements and settings writes.
//!
//! - `unread`: the badges the channel list and the rail show, brought up to date from the
//!   [`Changes`](inlet_state::Changes) of every update of the state.
//! - `reading`: what is read when, and the acknowledgements (at most one per channel per
//!   three seconds for reading, at once for "mark as read"). Nothing is read or
//!   acknowledged while the chat is out of sight.
//! - `writes`: notification settings. What the person asked for is kept apart from what
//!   the server confirmed, so an echo cannot undo a click and a failed write rolls back.
//! - `badge`: the pill and the count, and the rules for what a row shows.
//!
//! For the message view: its read marks call [`Inlet::read_channel`], which decides and
//! sends the acknowledgement; [`Inlet::read_if_watched`] reads the open channel once a
//! second when it is watched, and at once when the window gets focus or the list reports
//! the bottom. The "NEW" divider keeps using
//! the position the view captured when the channel was opened.

pub mod badge;
mod reading;
pub mod settings;
mod unread;
mod writes;

use gpui::{actions, div, prelude::*, px, AnyElement, App, Context, KeyBinding};

use crate::app::{Inlet, Screen};
use crate::assets::Icon;
use crate::backend::messages::ack::AckGate;
use crate::theme;
use badge::Badge;
use settings::SettingsBatch;
use unread::Badges;

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

/// The settings window: for which guild and channel (or category). Its values are read
/// from the account state when it is drawn.
pub struct SettingsWindow {
    pub guild_id: Option<String>,
    pub channel_id: Option<String>,
}

/// This domain's view state.
#[derive(Default)]
pub struct State {
    badges: Badges,
    /// When the open channel's acknowledgement may go out.
    gate: AckGate,
    /// Settings changes that are not confirmed yet.
    writes: SettingsBatch,
    pub settings: Option<SettingsWindow>,
    /// Whether the window has focus, kept by the window's activation observer.
    pub window_active: bool,
}

impl State {
    /// Nothing yet, in a window that has focus (`window_active`) or not.
    pub fn new(window_active: bool) -> Self {
        State {
            window_active,
            ..State::default()
        }
    }

    /// Brings the badges up to date after the account state changed.
    pub fn refresh_badges(
        &mut self,
        state: &inlet_state::AccountState,
        changes: &inlet_state::Changes,
    ) {
        self.badges.refresh(state, changes);
    }

    /// Drops the acknowledgement waiting for `ch`: "mark as unread" moved the read mark
    /// back, and the waiting one would move it forward again.
    pub fn drop_pending_ack(&mut self, ch: inlet_state::ChannelId) {
        self.gate.clear(ch, std::time::Instant::now());
    }

    /// Forgets everything (signed out). The settings writes start a new session, so a
    /// late answer to this one's writes is not taken for the next account's; whether
    /// the window has focus is not about the account and stays.
    pub fn clear(&mut self) {
        *self = State {
            writes: self.writes.next_session(),
            window_active: self.window_active,
            ..State::default()
        };
    }
}

/// What Escape and Shift+Escape do.
#[derive(Debug, PartialEq, Eq)]
enum EscapeAction {
    /// A layer is open (the link prompt or confirmation, the notification settings,
    /// the settings view, the image viewer) but the key reached the window: the topmost
    /// one closes.
    CloseLayer,
    /// A message menu or dialog is open (without the focus): it closes.
    CloseMessageOverlay,
    /// The thread list or the thread panel is open beside the chat: it closes.
    CloseThreads,
    MarkChannel,
    MarkGuild,
    Nothing,
}

/// What is over the chat takes the key first: the topmost layer (see
/// `desktop::OpenLayers::topmost`), then a message menu or dialog. With the chat out of
/// sight nothing is marked as read. Escape and Shift+Escape alike: neither marks
/// anything read while a layer or a message overlay is open.
fn escape_action(
    layer_open: bool,
    message_overlay_open: bool,
    threads_open: bool,
    chat_visible: bool,
    shift: bool,
) -> EscapeAction {
    if layer_open {
        EscapeAction::CloseLayer
    } else if message_overlay_open {
        EscapeAction::CloseMessageOverlay
    } else if threads_open {
        EscapeAction::CloseThreads
    } else if !chat_visible {
        EscapeAction::Nothing
    } else if shift {
        EscapeAction::MarkGuild
    } else {
        EscapeAction::MarkChannel
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
        self.notifications.badges.channel(channel_id)
    }

    /// What a guild's rail entry shows; `None` is the direct messages button.
    pub fn guild_badge(&self, guild_id: Option<&str>) -> Badge {
        self.notifications.badges.guild(guild_id)
    }

    /// Whether the chat is on screen: signed in, and not under the About view or the
    /// settings view. A view that replaces the chat must be added here, so nothing is
    /// read or marked as read behind it.
    pub(crate) fn chat_is_visible(&self) -> bool {
        self.screen == Screen::Main && !self.show_about && self.settings.view.is_none()
    }

    /// Escape (`shift` false) or Shift+Escape.
    fn on_escape(&mut self, shift: bool, window: &mut gpui::Window, cx: &mut Context<Self>) {
        let action = escape_action(
            self.layer_open(),
            self.chat.actions.overlay.is_some(),
            self.threads_open(),
            self.chat_is_visible(),
            shift,
        );
        match action {
            EscapeAction::CloseLayer => self.close_top_layer(cx),
            EscapeAction::CloseMessageOverlay => self.close_overlay(window, cx),
            EscapeAction::CloseThreads => self.close_threads_on_escape(window, cx),
            EscapeAction::MarkChannel => self.mark_channel_read(cx),
            EscapeAction::MarkGuild => self.mark_guild_read(cx),
            EscapeAction::Nothing => {}
        }
    }

    /// Adds the Escape handlers to the window's root element.
    pub(crate) fn notification_keys<E: InteractiveElement>(
        &self,
        el: E,
        cx: &mut Context<Self>,
    ) -> E {
        el.on_action(
            cx.listener(|this, _: &MarkChannelRead, window, cx| this.on_escape(false, window, cx)),
        )
        .on_action(
            cx.listener(|this, _: &MarkGuildRead, window, cx| this.on_escape(true, window, cx)),
        )
    }

    /// Opens the settings window for the open channel and its server.
    pub(crate) fn open_notification_settings(&mut self, cx: &mut Context<Self>) {
        let channel_id = self.selected_channel.clone();
        self.open_notification_settings_for(channel_id, cx);
    }

    /// Opens the settings window for a channel or category of the open server (or a
    /// direct message), and for the server itself.
    pub(crate) fn open_notification_settings_for(
        &mut self,
        channel_id: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.notifications.settings = Some(SettingsWindow {
            guild_id: self.selected_guild.clone(),
            channel_id,
        });
        cx.notify();
    }

    pub(crate) fn close_notification_settings(&mut self, cx: &mut Context<Self>) {
        if self.notifications.settings.take().is_some() {
            cx.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_marks_read_only_where_the_chat_is_seen() {
        use EscapeAction::*;
        // (layer open, message overlay open, threads open, chat visible, shift)
        assert_eq!(escape_action(false, false, false, true, false), MarkChannel);
        assert_eq!(escape_action(false, false, false, true, true), MarkGuild);
        // Under the About view: nothing, not the channel behind it.
        assert_eq!(escape_action(false, false, false, false, false), Nothing);
        assert_eq!(escape_action(false, false, false, false, true), Nothing);
        // A message menu or dialog closes, with or without Shift; nothing is read.
        assert_eq!(
            escape_action(false, true, false, true, false),
            CloseMessageOverlay
        );
        assert_eq!(
            escape_action(false, true, true, true, true),
            CloseMessageOverlay
        );
        // The thread list or panel closes before anything is marked read.
        assert_eq!(escape_action(false, false, true, true, false), CloseThreads);
        assert_eq!(escape_action(false, false, true, true, true), CloseThreads);
    }

    /// Any open layer (the image viewer, the settings view, the notification settings,
    /// a link prompt or confirmation) keeps Escape and Shift+Escape from marking
    /// anything read, also over a message overlay: the topmost layer closes instead.
    #[test]
    fn no_escape_marks_anything_read_while_a_layer_is_open() {
        use EscapeAction::*;
        for shift in [false, true] {
            for overlay in [false, true] {
                for visible in [false, true] {
                    for threads in [false, true] {
                        assert_eq!(
                            escape_action(true, overlay, threads, visible, shift),
                            CloseLayer
                        );
                    }
                }
            }
        }
    }
}
