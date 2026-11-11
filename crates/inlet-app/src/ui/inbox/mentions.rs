// SPDX-License-Identifier: AGPL-3.0-or-later
//! The inbox's recent mentions: messages in servers that mentioned the account, newest
//! first, each under its channel with "remove" (Fluxer's `RecentMentionsContent`).
//!
//! Removing a row hides it at once and sends one request; RECENT_MENTION_DELETE (from
//! this or another session) removes it for good, and a failed request shows it again.
//! New messages that mention the account are added at the top while the list is loaded.

use gpui::{div, prelude::*, px, AnyElement, Context, SharedString};
use inlet_protocol as p;
use inlet_state::{AccountState, ChannelId, MessageId, UserId};

use super::{card_button, list_footer, panel_body, panel_title, InboxIcon, Paged};
use crate::app::Inlet;
use crate::backend::messages::inbox::{Cmd, Item};
use crate::backend::messages::Cmd as MessagesCmd;
use crate::model::Message;
use crate::theme;
use crate::ui::chat::messages::mentions_me;

/// Mentions kept in the list (the server keeps them for a while; Fluxer shows pages of
/// 25 as the person scrolls).
pub const MAX_MENTIONS: usize = 200;
/// Rows hidden by a removal on its way, kept to show them again if it fails.
const MAX_REMOVING: usize = 50;

/// The recent mentions and their filters.
pub struct Mentions {
    pub list: Paged<Item>,
    /// Include `@everyone` and `@here`.
    pub everyone: bool,
    /// Include mentions of the account's roles.
    pub roles: bool,
    /// Rows removed by a click whose request is on its way, with where they were.
    removing: Vec<(usize, Item)>,
}

impl Default for Mentions {
    fn default() -> Self {
        Mentions {
            list: Paged::new(MAX_MENTIONS),
            everyone: true,
            roles: true,
            removing: Vec::new(),
        }
    }
}

impl Mentions {
    /// A page came. Rows removed meanwhile stay out.
    pub fn landed(&mut self, request: u64, items: Vec<Item>, more: bool, now: i64) -> bool {
        let removing: Vec<String> = self
            .removing
            .iter()
            .map(|(_, i)| i.message.id.clone())
            .collect();
        let items = items
            .into_iter()
            .filter(|i| !removing.contains(&i.message.id))
            .collect();
        self.list.land(request, items, more, now)
    }

    /// "Remove" on a row: it goes at once. Whether it was there.
    pub fn remove(&mut self, message_id: &str) -> bool {
        let Some(ix) = self
            .list
            .items
            .iter()
            .position(|i| i.message.id == message_id)
        else {
            return false;
        };
        let item = self.list.items.remove(ix);
        if self.removing.len() >= MAX_REMOVING {
            self.removing.remove(0);
        }
        self.removing.push((ix, item));
        true
    }

    /// Removing a row failed: it shows again where it was. Whether it did.
    pub fn delete_failed(&mut self, message_id: &str) -> bool {
        let Some(pos) = self
            .removing
            .iter()
            .position(|(_, i)| i.message.id == message_id)
        else {
            return false;
        };
        let (ix, item) = self.removing.remove(pos);
        let ix = ix.min(self.list.items.len());
        self.list.items.insert(ix, item);
        true
    }

    /// Other filters: the list loads again from the top.
    pub fn set_filters(&mut self, everyone: bool, roles: bool) {
        self.everyone = everyone;
        self.roles = roles;
        self.removing.clear();
        self.list.reset();
    }

    /// Gateway events: RECENT_MENTION_DELETE and MESSAGE_DELETE remove a row, an edit
    /// updates it, a new message that mentions the account comes first. Whether the
    /// list changed.
    pub fn on_dispatch(
        &mut self,
        d: &p::Dispatch,
        state: &AccountState,
        rendered: &[Message],
    ) -> bool {
        match d {
            p::Dispatch::RecentMentionDelete(r) => {
                self.removing.retain(|(_, i)| i.message.id != r.message_id);
                self.drop_row(&r.message_id)
            }
            p::Dispatch::MessageDelete(m) => {
                self.removing.retain(|(_, i)| i.message.id != m.id);
                self.drop_row(&m.id)
            }
            p::Dispatch::MessageDeleteBulk(b) => {
                let before = self.list.items.len();
                self.list.items.retain(|i| !b.ids.contains(&i.message.id));
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
            p::Dispatch::MessageCreate(m) => {
                if !self.list.loaded || !self.counts(m, state) {
                    return false;
                }
                let Some(new) = rendered.iter().find(|r| r.id == m.id) else {
                    return false;
                };
                if self.list.items.iter().any(|i| i.message.id == m.id) {
                    return false;
                }
                self.list.items.insert(
                    0,
                    Item {
                        message: new.clone(),
                    },
                );
                self.list.items.truncate(MAX_MENTIONS);
                true
            }
            _ => false,
        }
    }

    /// Whether a new message belongs in the list: in a server channel, not the account's
    /// own, and it mentions the account in a way the filters include.
    fn counts(&self, m: &p::Message, state: &AccountState) -> bool {
        let Some(ch) = ChannelId::parse(&m.channel_id) else {
            return false;
        };
        let me = state.me_id();
        if state.channels().guild_of(ch).is_none() || UserId::parse(&m.author.id) == Some(me) {
            return false;
        }
        if m.mentions.iter().any(|u| UserId::parse(&u.id) == Some(me)) {
            return true;
        }
        // An `@everyone` or role mention: only with both filters on, as the server
        // cannot be asked which of the two it was here.
        self.everyone && self.roles && mentions_me(state, ch, m)
    }

    fn drop_row(&mut self, message_id: &str) -> bool {
        let before = self.list.items.len();
        self.list.items.retain(|i| i.message.id != message_id);
        before != self.list.items.len()
    }
}

impl Inlet {
    /// "Remove" on a mention: gone at once, one request.
    fn remove_mention(&mut self, message_id: String, cx: &mut Context<Self>) {
        if MessageId::parse(&message_id).is_none() {
            return;
        }
        if self.inbox.mentions.remove(&message_id) {
            self.backend
                .send(MessagesCmd::Inbox(Cmd::DeleteMention { message_id }));
        }
        cx.notify();
    }

    fn toggle_mention_filter(&mut self, everyone: bool, cx: &mut Context<Self>) {
        let m = &self.inbox.mentions;
        let (e, r) = if everyone {
            (!m.everyone, m.roles)
        } else {
            (m.everyone, !m.roles)
        };
        self.inbox.mentions.set_filters(e, r);
        self.pump_panels(cx);
        cx.notify();
    }

    pub(super) fn render_mentions(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let chip = |id: &'static str, label: String, on: bool| {
            div()
                .id(id)
                .px(px(8.))
                .py(px(3.))
                .rounded(px(6.))
                .cursor_pointer()
                .text_size(px(12.))
                .text_color(if on {
                    theme::text_strong()
                } else {
                    theme::text_muted()
                })
                .when(on, |d| d.bg(theme::selected_bg()))
                .hover(|s| s.bg(theme::hover_bg()))
                .child(label)
        };
        let (everyone, roles) = (self.inbox.mentions.everyone, self.inbox.mentions.roles);
        let filters = vec![
            chip("mentions-everyone", t!("inbox-filter-everyone"), everyone)
                .on_click(cx.listener(|this, _, _, cx| this.toggle_mention_filter(true, cx)))
                .into_any_element(),
            chip("mentions-roles", t!("inbox-filter-roles"), roles)
                .on_click(cx.listener(|this, _, _, cx| this.toggle_mention_filter(false, cx)))
                .into_any_element(),
        ];
        let items = self.inbox.mentions.list.items.clone();
        let mut cards = Vec::with_capacity(items.len());
        for item in &items {
            let id = item.message.id.clone();
            let remove = card_button(
                SharedString::from(format!("mention-remove-{id}")),
                InboxIcon::Close,
            )
            .on_click(cx.listener(move |this, _, _, cx| this.remove_mention(id.clone(), cx)))
            .into_any_element();
            let header = self.card_header("mention", item, vec![remove], cx);
            cards.push(self.message_card("mention", item, Some(header), cx));
        }
        let footer = list_footer(
            "mentions",
            &self.inbox.mentions.list,
            t!("inbox-mentions-empty"),
            cx,
        );
        // The server keeps mentions for a week; the note says so, as Fluxer's does.
        let note = self.inbox.mentions.list.loaded.then(|| {
            div()
                .px(px(16.))
                .pb(px(8.))
                .text_size(px(12.))
                .text_color(theme::text_muted())
                .child(t!("inbox-mentions-note"))
        });
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(panel_title(InboxIcon::At, t!("inbox-mentions"), filters))
            .children(note)
            .child(panel_body("mentions-list").children(cards).children(footer))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::item;
    use super::*;
    use serde_json::json;

    fn state() -> AccountState {
        let ready: p::Ready = serde_json::from_value(json!({
            "session_id": "s",
            "user": {"id": "10", "username": "me"},
            "guilds": [],
            "private_channels": [],
            "users": [],
            "read_states": [],
        }))
        .expect("ready");
        AccountState::from_ready_at(&ready, 1)
    }

    fn ids(m: &Mentions) -> Vec<String> {
        m.list.items.iter().map(|i| i.message.id.clone()).collect()
    }

    #[test]
    fn recent_mention_delete_removes_the_row() {
        let state = state();
        let mut m = Mentions::default();
        m.list.start(1, true, 0);
        assert!(m.landed(
            1,
            vec![item(3, "301"), item(2, "301"), item(1, "301")],
            false,
            0
        ));

        // Removed in another session (or the echo of a removal here): the row goes.
        let event = p::Dispatch::parse("RECENT_MENTION_DELETE", json!({"message_id": "2"}));
        assert!(m.on_dispatch(&event, &state, &[]));
        assert_eq!(ids(&m), vec!["3", "1"]);
        assert!(!m.on_dispatch(&event, &state, &[]), "already gone");

        // "Remove" here hides the row at once; a failure shows it again where it was.
        assert!(m.remove("3"));
        assert_eq!(ids(&m), vec!["1"]);
        assert!(m.delete_failed("3"));
        assert_eq!(ids(&m), vec!["3", "1"]);
        // A confirmed removal stays removed, also when a page brings it back.
        assert!(m.remove("1"));
        let echo = p::Dispatch::parse("RECENT_MENTION_DELETE", json!({"message_id": "1"}));
        m.on_dispatch(&echo, &state, &[]);
        assert!(!m.delete_failed("1"));
        assert_eq!(ids(&m), vec!["3"]);

        // A deleted message leaves the list too.
        let deleted = p::Dispatch::parse("MESSAGE_DELETE", json!({"id": "3", "channel_id": "301"}));
        assert!(m.on_dispatch(&deleted, &state, &[]));
        assert!(m.list.items.is_empty());
    }

    #[test]
    fn a_removal_on_its_way_keeps_the_row_out_of_a_new_page() {
        let mut m = Mentions::default();
        m.list.start(1, true, 0);
        m.landed(1, vec![item(2, "301"), item(1, "301")], false, 0);
        assert!(m.remove("2"));
        m.list.stale = true;
        m.list.start(2, true, 2_000);
        m.landed(2, vec![item(2, "301"), item(1, "301")], false, 2_000);
        assert_eq!(ids(&m), vec!["1"]);
    }

    #[test]
    fn fast_filter_clicks_keep_the_pace() {
        let mut m = Mentions::default();
        assert!(m.list.wants_first() && m.list.may_start(0));
        m.list.start(1, true, 0);
        // Three filter changes within a second: no request while the first is on its
        // way, and none before the second is over.
        m.set_filters(false, true);
        m.set_filters(false, false);
        m.set_filters(true, true);
        assert!(
            !m.list.wants_first() && !m.list.may_start(300),
            "the old request is still on its way"
        );
        // The old filters' answer is dropped, not shown; then the new filters want
        // their first page.
        assert!(!m.landed(1, vec![item(5, "301")], false, 400));
        assert!(m.list.items.is_empty() && !m.list.loaded);
        assert!(m.list.wants_first());
        assert!(!m.list.may_start(999), "at most one request a second");
        assert!(m.list.may_start(1_000));
        m.list.start(2, true, 1_000);
        assert!(m.landed(2, vec![item(6, "301")], false, 1_100));
        assert_eq!(ids(&m), vec!["6"]);
        // A failure of a dropped request shows nothing.
        m.set_filters(false, true);
        m.list.start(3, true, 2_000);
        m.set_filters(true, true);
        assert!(m.list.fail(3, "offline".into()));
        assert!(m.list.error.is_none() && m.list.wants_first());
    }
}
