// SPDX-License-Identifier: AGPL-3.0-or-later
//! Everything drawn on top of the window content: modals, context menus, tooltips.

mod context_menu;
mod link_confirm;
mod link_prompt;
pub mod modal;
mod notification_settings;
mod tooltip;

use gpui::{AnyElement, Context, FocusHandle};

use crate::app::Inlet;
use crate::backend::platform::links::Prompt;

#[derive(Default)]
pub struct OverlayState {
    /// A masked link waiting for confirmation.
    pub pending_link: Option<String>,
    /// A deep link or invite link waiting for confirmation (see `desktop`).
    pub link_prompt: Option<Prompt>,
    /// The channel a confirmed link opens once its guild's channel list is in.
    pub link_channel: Option<String>,
    /// The keyboard focus of the topmost layer over the chat (link prompt, settings).
    pub layer_focus: Option<FocusHandle>,
    /// Whether a layer took the focus, so it goes back to the composer on close.
    pub layer_had_focus: bool,
}

impl Inlet {
    /// The overlays to draw over the current screen, bottom first.
    pub(crate) fn render_overlays(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut out = Vec::new();
        if self.settings.view.is_some() {
            out.push(self.render_settings_overlay(cx));
        }
        if let Some(url) = self.overlays.pending_link.clone() {
            out.push(self.render_link_confirm(&url, cx));
        }
        if self.notifications.settings.is_some() {
            out.push(self.render_notification_settings(cx));
        }
        if let Some(prompt) = self.overlays.link_prompt.clone() {
            out.push(self.render_link_prompt(prompt, cx));
        }
        out
    }
}
