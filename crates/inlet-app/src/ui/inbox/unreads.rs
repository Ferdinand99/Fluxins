// SPDX-License-Identifier: AGPL-3.0-or-later
//! The inbox's unread channels: every channel with unread messages or mentions, newest
//! first, from the one account state, with "mark as read" per channel and for all.
//!
//! Deviation: Fluxer shows the unread messages of each channel under it, loaded in
//! batches; Inlet lists the channels with their mention count only, so opening the tab
//! costs no requests. A click opens the channel at its "NEW" line.
//!
//! "Mark as read" here is the person's own click in the panel over the chat, so it is
//! sent although the chat is covered (`reading.rs`, `mark_listed_read`).

use gpui::{div, prelude::*, px, AnyElement, Context, FontWeight, SharedString};
use inlet_state::{AccountState, ChannelId, GuildId, MessageId};

use super::{card_button, list_note, panel_body, panel_title, InboxIcon};
use crate::app::Inlet;
use crate::theme;
use crate::ui::notifications::badge::Badge;
use crate::ui::notifications::channel_badge;

/// Channels the tab lists at most.
pub const MAX_UNREAD_ROWS: usize = 100;

/// One unread channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnreadRow {
    pub channel: ChannelId,
    pub guild: Option<GuildId>,
    pub mentions: u32,
    /// The channel's newest message: where "mark as read" reads up to.
    pub last: MessageId,
}

/// The unread channels as Fluxer's inbox lists them: unread and not muted, or with
/// mentions; the most recent first.
pub fn unread_rows(state: &AccountState) -> Vec<UnreadRow> {
    let mut channels: Vec<(ChannelId, Option<GuildId>)> = Vec::new();
    for g in state.guilds() {
        channels.extend(
            state
                .visible_channels(g.id)
                .iter()
                .filter_map(|c| ChannelId::parse(&c.id))
                .map(|ch| (ch, Some(g.id))),
        );
    }
    channels.extend(
        state
            .private_channels()
            .iter()
            .filter_map(|c| ChannelId::parse(&c.id))
            .map(|ch| (ch, None)),
    );
    let mut rows: Vec<UnreadRow> = channels
        .into_iter()
        .filter_map(|(ch, guild)| {
            let badge: Badge = channel_badge(state, ch);
            let shows = badge.mentions > 0 || (badge.unread && !badge.muted);
            if !shows {
                return None;
            }
            let last = state.read_state(ch)?.last_message_id?;
            Some(UnreadRow {
                channel: ch,
                guild,
                mentions: badge.mentions,
                last,
            })
        })
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.last));
    rows.truncate(MAX_UNREAD_ROWS);
    rows
}

impl Inlet {
    /// The check in the inbox's tab column: every listed channel is read.
    pub(super) fn mark_inbox_unreads_read(&mut self, cx: &mut Context<Self>) {
        let Some(account) = self.account.clone() else {
            return;
        };
        let targets: Vec<(ChannelId, MessageId)> = unread_rows(account.read(cx))
            .into_iter()
            .map(|r| (r.channel, r.last))
            .collect();
        self.mark_listed_read(targets, cx);
    }

    fn mark_unread_row_read(&mut self, row: &UnreadRow, cx: &mut Context<Self>) {
        self.mark_listed_read(vec![(row.channel, row.last)], cx);
    }

    pub(super) fn render_unreads(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let rows = match (&self.inbox.unread_rows, &self.account) {
            (Some(rows), _) => rows.clone(),
            (None, Some(account)) => {
                let rows = std::rc::Rc::new(unread_rows(account.read(cx)));
                self.inbox.unread_rows = Some(rows.clone());
                rows
            }
            (None, None) => std::rc::Rc::new(Vec::new()),
        };
        let mut items = Vec::with_capacity(rows.len());
        for row in rows.iter().cloned() {
            let (channel, guild) = self.channel_and_guild_names(&row.channel.to_string(), cx);
            let check = {
                let row = row.clone();
                card_button(
                    SharedString::from(format!("unread-read-{}", row.channel)),
                    InboxIcon::Check,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.mark_unread_row_read(&row, cx)))
            };
            let (g, ch) = (row.guild, row.channel);
            items.push(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(12.))
                    .py(px(10.))
                    .border_b_1()
                    .border_color(theme::preview_card_divider())
                    .child(
                        div()
                            .id(SharedString::from(format!("unread-open-{}", row.channel)))
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .cursor_pointer()
                            .child(
                                div()
                                    .text_size(px(14.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme::text_strong())
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .whitespace_nowrap()
                                    .child(channel),
                            )
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(theme::text_muted())
                                    .child(guild.unwrap_or_else(|| t!("inbox-direct-messages"))),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_from_panel(g, ch, window, cx)
                            })),
                    )
                    .when(row.mentions > 0, |d| {
                        d.child(crate::ui::notifications::badge::mention_badge(row.mentions))
                    })
                    .child(check)
                    .into_any_element(),
            );
        }
        let empty = items
            .is_empty()
            .then(|| list_note(t!("inbox-unreads-empty")).into_any_element());
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(panel_title(
                InboxIcon::Bell,
                t!("inbox-unreads"),
                Vec::new(),
            ))
            .child(panel_body("unreads-list").children(items).children(empty))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use inlet_protocol as p;
    use serde_json::json;

    #[test]
    fn the_unread_list_skips_read_and_muted_channels() {
        let ready: p::Ready = serde_json::from_value(json!({
            "session_id": "s",
            "user": {"id": "10", "username": "me"},
            "guilds": [],
            "private_channels": [
                {"id": "901", "type": 1, "last_message_id": "50", "recipients": [{"id": "20", "username": "ada"}]},
                {"id": "902", "type": 1, "last_message_id": "60", "recipients": [{"id": "21", "username": "bo"}]},
                {"id": "903", "type": 1, "last_message_id": "70", "recipients": [{"id": "22", "username": "cy"}]}
            ],
            "users": [],
            "read_states": [
                {"id": "901", "last_message_id": "40", "mention_count": 0},
                {"id": "902", "last_message_id": "60", "mention_count": 0},
                {"id": "903", "last_message_id": "10", "mention_count": 2}
            ],
        }))
        .expect("ready");
        let state = AccountState::from_ready_at(&ready, 1);
        let rows = unread_rows(&state);
        // 902 is read; the others come newest first, with their mentions.
        let listed: Vec<(u64, u32)> = rows.iter().map(|r| (r.channel.0, r.mentions)).collect();
        assert_eq!(listed, vec![(903, 2), (901, 0)]);
        assert_eq!(rows[0].last, MessageId(70));
    }
}
