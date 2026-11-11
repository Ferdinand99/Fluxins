// SPDX-License-Identifier: AGPL-3.0-or-later
//! Everything drawn on top of the window content: modals, context menus, tooltips.

pub mod account_switcher;
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
    /// Whether a layer took the focus, so it is given back when the layers close.
    pub layer_had_focus: bool,
    /// What had the focus before the first layer took it.
    pub focus_before: Option<FocusHandle>,
}

impl Inlet {
    /// The overlays to draw over the current screen, bottom first.
    pub(crate) fn render_overlays(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut out = Vec::new();
        // The viewer lies under the settings view and the prompts (Escape closes the
        // topmost layer first).
        out.extend(self.render_image_viewer(cx));
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
        if self.switcher.open && self.screen == crate::app::Screen::Main {
            out.push(self.render_account_switcher(cx));
        }
        out
    }
}
