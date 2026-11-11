// SPDX-License-Identifier: AGPL-3.0-or-later
//! The inbox's saved messages (Fluxer's "Bookmarks"): newest first, each under its
//! channel with "remove", and a card for a message the account can no longer read.
//!
//! Saving and removing are the message actions' (`backend/messages/actions.rs`), which
//! also know which messages are saved; the list feeds them what it loads. The list
//! follows SAVED_MESSAGE_DELETE at once; SAVED_MESSAGE_CREATE makes it load again the
//! next time it is shown, since the event's message is not converted for drawing.

use gpui::{div, prelude::*, px, AnyElement, Context, SharedString};
use inlet_protocol as p;
use inlet_state::MessageId;

use super::{card_button, list_footer, panel_body, panel_title, InboxIcon, Paged};
use crate::app::Inlet;
use crate::backend::messages::actions::{self, Kind};
use crate::backend::messages::inbox::SavedEntry;
use crate::model::Message;
use crate::theme;

/// Saved messages kept in the list (Fluxer allows 50 per account by default).
pub const MAX_SAVED: usize = 200;

pub struct Saved {
    pub list: Paged<SavedEntry>,
}

impl Default for Saved {
    fn default() -> Self {
        Saved {
            list: Paged::new(MAX_SAVED),
        }
    }
}

impl Saved {
    /// Gateway events. Whether the list changed (or needs loading again).
    pub fn on_dispatch(&mut self, d: &p::Dispatch, rendered: &[Message]) -> bool {
        match d {
            p::Dispatch::SavedMessageDelete(s) => self.drop_row(&s.message_id),
            p::Dispatch::SavedMessageCreate(m) => {
                if self.list.loaded && !self.list.items.iter().any(|e| e.message_id() == m.id) {
                    self.list.stale = true;
                    return true;
                }
                false
            }
            p::Dispatch::MessageUpdate(m) => {
                let Some(new) = rendered.iter().find(|r| r.id == m.id) else {
                    return false;
                };
                let mut changed = false;
                for e in &mut self.list.items {
                    if let SavedEntry::Shown(item) = e {
                        if item.message.id == m.id {
                            item.message = new.clone();
                            changed = true;
                        }
                    }
                }
                changed
            }
            _ => false,
        }
    }

    /// Removes a row. Whether it was there.
    pub fn drop_row(&mut self, message_id: &str) -> bool {
        let before = self.list.items.len();
        self.list.items.retain(|e| e.message_id() != message_id);
        before != self.list.items.len()
    }
}

impl Inlet {
    /// "Remove from saved": the row goes at once, and the message actions send the
    /// request (one at a time per message) and keep what they know as saved.
    fn unsave(&mut self, channel_id: String, message_id: String, cx: &mut Context<Self>) {
        let Some(id) = MessageId::parse(&message_id) else {
            return;
        };
        self.inbox.saved.drop_row(&message_id);
        self.send_once(
            Kind::Unsave,
            actions::Cmd::Save {
                channel_id,
                message_id,
                save: false,
            },
            id,
        );
        cx.notify();
    }

    pub(super) fn render_saved(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let entries = self.inbox.saved.list.items.clone();
        let mut cards = Vec::with_capacity(entries.len());
        for entry in &entries {
            let (channel_id, message_id) = match entry {
                SavedEntry::Shown(item) => {
                    (item.message.channel_id.clone(), item.message.id.clone())
                }
                SavedEntry::Missing {
                    channel_id,
                    message_id,
                } => (channel_id.clone(), message_id.clone()),
            };
            let remove = {
                let (channel_id, message_id) = (channel_id.clone(), message_id.clone());
                card_button(
                    SharedString::from(format!("saved-remove-{message_id}")),
                    InboxIcon::Close,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.unsave(channel_id.clone(), message_id.clone(), cx)
                }))
                .into_any_element()
            };
            match entry {
                SavedEntry::Shown(item) => {
                    let header = self.card_header("saved", item, vec![remove], cx);
                    cards.push(self.message_card("saved", item, Some(header), cx));
                }
                SavedEntry::Missing { .. } => {
                    let (channel, guild) = self.channel_and_guild_names(&channel_id, cx);
                    let place = match guild {
                        Some(g) => format!("{channel} \u{00b7} {g}"),
                        None => channel,
                    };
                    cards.push(
                        div()
                            .mb(px(16.))
                            .p(px(12.))
                            .rounded(px(8.))
                            .border_1()
                            .border_color(theme::preview_card_border())
                            .bg(theme::preview_card_bg())
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(12.))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .child(
                                        div()
                                            .text_size(px(14.))
                                            .text_color(theme::text())
                                            .child(t!("inbox-saved-missing")),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(12.))
                                            .text_color(theme::text_muted())
                                            .child(place),
                                    ),
                            )
                            .child(remove)
                            .into_any_element(),
                    );
                }
            }
        }
        let footer = list_footer("saved", &self.inbox.saved.list, t!("inbox-saved-empty"), cx);
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(panel_title(
                InboxIcon::Bookmark,
                t!("inbox-saved"),
                Vec::new(),
            ))
            .child(panel_body("saved-list").children(cards).children(footer))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::item;
    use super::*;
    use serde_json::json;

    #[test]
    fn saved_rows_follow_the_gateway() {
        let mut s = Saved::default();
        s.list.start(1, true, 0);
        s.list.land(
            1,
            vec![
                SavedEntry::Shown(Box::new(item(2, "301"))),
                SavedEntry::Missing {
                    channel_id: "302".into(),
                    message_id: "1".into(),
                },
            ],
            false,
            0,
        );
        let delete = p::Dispatch::parse("SAVED_MESSAGE_DELETE", json!({"message_id": "1"}));
        assert!(s.on_dispatch(&delete, &[]));
        assert_eq!(s.list.items.len(), 1);
        // A message saved elsewhere: the list loads again when it is shown.
        let raw = super::super::tests::raw(5, "301", json!({}));
        let create = p::Dispatch::SavedMessageCreate(Box::new(raw));
        assert!(s.on_dispatch(&create, &[]));
        assert!(s.list.stale && s.list.wants_first());
    }
}
