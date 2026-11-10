// SPDX-License-Identifier: AGPL-3.0-or-later
//! The list of direct messages, shown when no guild is selected.

use gpui::{AnyElement, Context};

use crate::app::Inlet;

impl Inlet {
    /// The rows of the direct message list.
    pub(super) fn dm_rows(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let selected = self.selected_channel.clone();
        self.channels
            .iter()
            .map(|c| self.channel_row(c, selected.as_deref() == Some(c.id.as_str()), cx))
            .collect()
    }
}
