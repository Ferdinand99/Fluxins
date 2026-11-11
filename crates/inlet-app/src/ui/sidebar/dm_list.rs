// SPDX-License-Identifier: AGPL-3.0-or-later
//! The list of direct messages, shown when no guild is selected.

use gpui::{div, prelude::*, px, AnyElement, Context};
use inlet_state::{AccountState, ChannelId, UserId};

use crate::app::Inlet;
use crate::model::{Channel, ChannelKind};
use crate::theme;
use crate::ui::social::presence::{status_dot, DOT};

impl Inlet {
    /// The rows of the direct message list. A direct message with one person shows
    /// their status on its icon.
    pub(super) fn dm_rows(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let selected = self.selected_channel.clone();
        // One lookup per row: the channel's first recipient and their presence.
        let statuses: Vec<Option<&'static str>> = {
            let state = self.account.as_ref().map(|a| a.read(cx));
            self.channels.iter().map(|c| dm_status(state, c)).collect()
        };
        self.channels
            .iter()
            .zip(statuses)
            .map(|(c, status)| {
                let row = self.channel_row(c, selected.as_deref() == Some(c.id.as_str()), cx);
                match status {
                    None => row,
                    Some(status) => div()
                        .relative()
                        .flex_none()
                        .child(row)
                        .child(
                            // On the bottom-right of the 20 px icon (16 px from the
                            // row's left edge, centred in its 32 px).
                            div()
                                .absolute()
                                .left(px(36. - DOT + 2.))
                                .top(px(26. - DOT + 2.))
                                .p(px(2.))
                                .rounded_full()
                                .bg(theme::bg_app())
                                .child(status_dot(status, DOT)),
                        )
                        .into_any_element(),
                }
            })
            .collect()
    }
}

/// The status of the person in a one-to-one direct message (`offline` when unknown);
/// `None` for anything else.
pub(crate) fn dm_status(state: Option<&AccountState>, c: &Channel) -> Option<&'static str> {
    if c.kind != ChannelKind::Dm {
        return None;
    }
    let state = state?;
    let channel = state.channel(ChannelId::parse(&c.id)?)?;
    let user = UserId::parse(&channel.recipients.first()?.id)?;
    Some(match state.presences().status(user) {
        "online" => "online",
        "idle" => "idle",
        "dnd" => "dnd",
        _ => "offline",
    })
}
