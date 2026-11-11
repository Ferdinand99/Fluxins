// SPDX-License-Identifier: AGPL-3.0-or-later
//! The pinned messages of the open channel: the panel behind the pin in the channel
//! header (Fluxer's `ChannelPinsPopout`), newest pin first, each with "Jump" and, for an
//! account that may pin, "Unpin".
//!
//! The list belongs to one channel. It loads when the panel opens and nothing fresh is
//! kept, and again after CHANNEL_PINS_UPDATE for that channel (one request at a time,
//! at most one a second; see `ui/inbox`). Unpinning is the message actions' request.

use gpui::{div, prelude::*, px, AnyElement, Context, SharedString};
use inlet_protocol as p;
use inlet_state::{ChannelId, MessageId};

use crate::app::Inlet;
use crate::backend::messages::actions::{self, Kind};
use crate::backend::messages::inbox::Item;
use crate::model::Message;
use crate::ui::inbox::{
    card_button, list_footer, panel_body, panel_title, text_button, InboxIcon, Paged,
};

/// Pins kept in the list (five pages).
pub const MAX_PINS: usize = 250;

/// The pins of one channel.
pub struct Pins {
    /// The channel the list belongs to.
    pub channel: Option<ChannelId>,
    pub list: Paged<Item>,
    /// The `before` of the next page (the last pin's `pinned_at`).
    pub next: Option<String>,
}

impl Default for Pins {
    fn default() -> Self {
        Pins {
            channel: None,
            list: Paged::new(MAX_PINS),
            next: None,
        }
    }
}

impl Pins {
    /// The panel opens on `ch`: another channel's list is dropped; a failed load may try
    /// again.
    pub fn open(&mut self, ch: ChannelId) {
        if self.channel != Some(ch) {
            self.channel = Some(ch);
            self.list.reset();
            self.next = None;
        }
        self.list.error = None;
    }

    /// Gateway events. CHANNEL_PINS_UPDATE for the list's channel makes it load again
    /// (now if the panel is open, else when it opens); a deleted message leaves it, an
    /// edited one is redrawn. Whether the list changed.
    pub fn on_dispatch(&mut self, d: &p::Dispatch, rendered: &[Message]) -> bool {
        let Some(own) = self.channel else {
            return false;
        };
        match d {
            p::Dispatch::ChannelPinsUpdate(u) => {
                if ChannelId::parse(&u.channel_id) != Some(own) {
                    return false;
                }
                self.list.stale = true;
                true
            }
            p::Dispatch::MessageDelete(m) => {
                let before = self.list.items.len();
                self.list.items.retain(|i| i.message.id != m.id);
                before != self.list.items.len()
            }
            p::Dispatch::MessageUpdate(m) => {
                let Some(new) = rendered.iter().find(|r| r.id == m.id) else {
                    return false;
                };
                match self.list.items.iter_mut().find(|i| i.message.id == m.id) {
                    Some(item) => {
                        item.message = new.clone();
                        true
                    }
                    None => false,
                }
            }
            p::Dispatch::ChannelDelete(c) if ChannelId::parse(&c.id) == Some(own) => {
                *self = Pins::default();
                true
            }
            _ => false,
        }
    }
}

impl Inlet {
    /// "Unpin" in the panel: the message actions send it (once per message at a time);
    /// CHANNEL_PINS_UPDATE then reloads the list. The right to pin is checked again
    /// when clicked (it may have gone since the panel was drawn).
    fn unpin_from_panel(&mut self, item: &Item, cx: &mut Context<Self>) {
        let (Some(id), Some(ch)) = (
            MessageId::parse(&item.message.id),
            ChannelId::parse(&item.message.channel_id),
        ) else {
            return;
        };
        let allowed = self
            .account
            .as_ref()
            .is_some_and(|a| a.read(cx).channel_permissions(ch).can_pin());
        if !allowed {
            return;
        }
        self.send_once(
            Kind::Unpin,
            actions::Cmd::Pin {
                channel_id: item.message.channel_id.clone(),
                message_id: item.message.id.clone(),
                pin: false,
            },
            id,
        );
        cx.notify();
    }

    pub(crate) fn render_pins_panel(&mut self, cx: &mut Context<Self>) -> AnyElement {
        // Unpinning needs the right to pin, read from the state when drawn (fails closed).
        let can_pin = match (self.inbox.pins.channel, &self.account) {
            (Some(ch), Some(account)) => account.read(cx).channel_permissions(ch).can_pin(),
            _ => false,
        };
        let items = self.inbox.pins.list.items.clone();
        let mut cards = Vec::with_capacity(items.len());
        for item in &items {
            let id = item.message.id.clone();
            let jump = {
                let (channel_id, message_id) = (item.message.channel_id.clone(), id.clone());
                text_button(
                    SharedString::from(format!("pin-jump-{id}")),
                    t!("inbox-jump"),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.jump_to(&channel_id, &message_id, window, cx)
                }))
                .into_any_element()
            };
            let unpin = can_pin.then(|| {
                let item = item.clone();
                card_button(
                    SharedString::from(format!("pin-remove-{id}")),
                    InboxIcon::Close,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.unpin_from_panel(&item, cx)))
                .into_any_element()
            });
            let actions = div()
                .flex()
                .flex_row()
                .justify_end()
                .items_center()
                .gap(px(6.))
                .pb(px(6.))
                .child(jump)
                .children(unpin)
                .into_any_element();
            cards.push(self.message_card("pin", item, Some(actions), cx));
        }
        let footer = list_footer("pins", &self.inbox.pins.list, t!("inbox-pins-empty"), cx);
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(panel_title(
                InboxIcon::PushPin,
                t!("inbox-pins"),
                Vec::new(),
            ))
            .child(panel_body("pins-list").children(cards).children(footer))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::inbox::tests::item;
    use serde_json::json;

    fn update(channel: &str) -> p::Dispatch {
        p::Dispatch::parse(
            "CHANNEL_PINS_UPDATE",
            json!({"channel_id": channel, "last_pin_timestamp": "2026-01-01T10:00:00Z"}),
        )
    }

    #[test]
    fn pins_update_on_channel_pins_update() {
        let (a, b) = (ChannelId(301), ChannelId(302));
        let mut pins = Pins::default();
        pins.open(a);
        assert!(pins.list.wants_first());
        pins.list.start(1, true, 0);
        assert!(pins.list.land(1, vec![item(10, "301")], false, 10));
        assert!(!pins.list.wants_first(), "loaded: nothing to ask");

        // Another channel's update changes nothing.
        assert!(!pins.on_dispatch(&update("302"), &[]));
        assert!(!pins.list.wants_first());

        // The list's own channel: its pins load again, within the budget.
        assert!(pins.on_dispatch(&update("301"), &[]));
        assert!(pins.list.wants_first());
        assert!(!pins.list.may_start(500), "at most one request a second");
        assert!(pins.list.may_start(1_000));
        pins.list.start(2, true, 1_000);

        // A second update while that load is on its way: no second request now, one
        // after it, because the answer may be older than the update.
        assert!(pins.on_dispatch(&update("301"), &[]));
        assert!(!pins.list.wants_first());
        assert!(pins
            .list
            .land(2, vec![item(11, "301"), item(10, "301")], false, 1_200));
        assert_eq!(pins.list.items.len(), 2);
        assert!(pins.list.wants_first());
        // An answer to a request no one waits for is dropped.
        assert!(!pins.list.land(1, Vec::new(), false, 1_300));
        assert_eq!(pins.list.items.len(), 2);

        // Unpinned and deleted messages leave; opening another channel starts over.
        let deleted =
            p::Dispatch::parse("MESSAGE_DELETE", json!({"id": "10", "channel_id": "301"}));
        assert!(pins.on_dispatch(&deleted, &[]));
        assert_eq!(pins.list.items.len(), 1);
        pins.open(b);
        assert!(pins.list.items.is_empty() && pins.list.wants_first());
        assert!(!pins.on_dispatch(&update("301"), &[]));
    }
}
