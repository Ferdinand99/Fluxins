// SPDX-License-Identifier: AGPL-3.0-or-later
//! Everything drawn on top of the window content: modals, context menus, tooltips.

mod context_menu;
mod link_confirm;
pub mod modal;
mod tooltip;

use gpui::{AnyElement, Context};

use crate::app::Inlet;

#[derive(Default)]
pub struct OverlayState {
    /// A masked link waiting for confirmation.
    pub pending_link: Option<String>,
}

impl Inlet {
    /// The overlays to draw over the current screen, bottom first.
    pub(crate) fn render_overlays(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut out = Vec::new();
        out.extend(self.render_image_viewer(cx));
        if let Some(url) = self.overlays.pending_link.clone() {
            out.push(self.render_link_confirm(&url, cx));
        }
        out
    }
}
