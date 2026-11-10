// SPDX-License-Identifier: AGPL-3.0-or-later
//! Several accounts: which one is active, the passive state of the others, and the
//! account switcher (a window opened from the user panel, as in Fluxer: every signed-in
//! account with its unread mentions, switch, sign one out, add another).
//!
//! Events from the backend come tagged with their account. The active account's go to
//! the views as before; another account's gateway events only feed its own account
//! state, kept here without message windows, so the switcher can show its mentions.
//!
//! Switching is done here first: the view state of the old account (its account state,
//! the open channel, the outbox of the message view, the unread gate and settings writes,
//! the composer's text) is put away or cleared before the backend is told, so nothing of
//! one account is ever sent with the other's session. The old account's state keeps
//! running in the background; its message windows are dropped. Each account comes back
//! to the guild and channel it had open.

use std::collections::HashMap;

use gpui::{
    div, prelude::*, px, AnyElement, Context, Entity, Focusable, FontWeight, SharedString, Window,
};
use inlet_state::{AccountState, Changes};

use super::modal::modal;
use crate::app::{Inlet, Screen};
use crate::assets::Icon;
use crate::backend::accounts::{AccountInfo, AccountKey, Cmd, MAX_ACCOUNTS};
use crate::backend::{messages, Event};
use crate::model::Guild;
use crate::theme;
use crate::ui::notifications::badge::mention_badge;
use crate::ui::widgets::button;

/// Where an account was: the guild (`None` = direct messages) and channel on screen.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Place {
    pub guild: Option<String>,
    pub channel: Option<String>,
}

/// The last place of each account, so switching back opens it again.
#[derive(Default)]
pub struct Places {
    map: HashMap<AccountKey, Place>,
}

impl Places {
    /// `account` is left while `place` is on screen.
    pub fn leave(&mut self, account: AccountKey, place: Place) {
        self.map.insert(account, place);
    }

    /// Where `account` was when it was last left.
    pub fn arrive(&self, account: &AccountKey) -> Option<Place> {
        self.map.get(account).cloned()
    }

    pub fn forget(&mut self, account: &AccountKey) {
        self.map.remove(account);
    }
}

/// Unread marks of an account in the background.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub mentions: u32,
    pub unread: bool,
}

/// The unread marks of a whole account: every guild that is not muted, and the DMs.
pub fn summary(state: &AccountState) -> Summary {
    let mut out = Summary::default();
    for g in state.guilds() {
        let u = state.guild_unread(g.id);
        out.mentions = out.mentions.saturating_add(u.mentions);
        out.unread |= u.unread && !u.muted;
    }
    let dms = state.private_unread();
    out.mentions = out.mentions.saturating_add(dms.mentions);
    out.unread |= dms.unread;
    out
}

/// This domain's state.
#[derive(Default)]
pub struct Switcher {
    /// The switcher window is open.
    pub open: bool,
    /// The signed-in accounts, in order.
    pub accounts: Vec<AccountInfo>,
    /// The active account; `None` before the first sign-in.
    pub active: Option<AccountKey>,
    /// The other accounts' state, fed by their gateways, without message windows.
    background: HashMap<AccountKey, Entity<AccountState>>,
    summaries: HashMap<AccountKey, Summary>,
    pub places: Places,
    /// The sign-in screen is up to add an account (it has a way back).
    pub adding: bool,
    /// The channel to open once the selected guild's channels arrive.
    wanted_channel: Option<String>,
}

impl Switcher {
    pub fn is_active(&self, k: &AccountKey) -> bool {
        self.active.as_ref() == Some(k)
    }

    /// The channel to open in the guild that just loaded, once.
    pub fn take_wanted_channel(&mut self) -> Option<String> {
        self.wanted_channel.take()
    }

    /// Mentions waiting in the accounts that are not active.
    pub fn background_mentions(&self) -> u32 {
        self.summaries
            .iter()
            .filter(|(k, _)| !self.is_active(k))
            .fold(0u32, |n, (_, s)| n.saturating_add(s.mentions))
    }

    fn summary_of(&self, k: &AccountKey) -> Summary {
        self.summaries.get(k).copied().unwrap_or_default()
    }

    /// No account is signed in any more: nothing is active, nothing kept in the
    /// background. The list follows from the backend.
    pub fn signed_out(&mut self) {
        self.open = false;
        self.adding = false;
        self.active = None;
        self.background.clear();
        self.summaries.clear();
        self.wanted_channel = None;
    }
}

impl Inlet {
    /// An event one account's tasks sent: as usual for the active account, into its
    /// passive state for another one.
    pub(crate) fn on_tagged_event(
        &mut self,
        account: AccountKey,
        event: Event,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.switcher.is_active(&account) {
            return self.handle_event(event, window, cx);
        }
        self.on_background_event(account, event, cx);
    }

    /// A background account's event. Only its gateway events matter (they keep its
    /// unread counts); page answers, statuses and errors of an account that is not on
    /// screen are dropped.
    fn on_background_event(&mut self, account: AccountKey, event: Event, cx: &mut Context<Self>) {
        let Event::Messages(messages::Ev::Dispatch { dispatch, .. }) = event else {
            return;
        };
        let changes = match (&*dispatch, self.switcher.background.get(&account)) {
            (inlet_protocol::Dispatch::Ready(ready), None) => {
                let state = crate::ui::chat::messages::account_state_from(ready);
                self.switcher
                    .background
                    .insert(account.clone(), cx.new(|_| state));
                let mut c = Changes::default();
                c.reset = true;
                c
            }
            (_, Some(state)) => state.update(cx, |s, _| {
                let mut c = s.apply(&dispatch);
                // Nothing on screen: a window the state opened is not kept.
                if !c.messages.is_empty() {
                    c.merge(s.drop_message_windows());
                }
                c
            }),
            // Events before READY: nothing to apply them to yet.
            (_, None) => return,
        };
        if changes.reset || changes.read_state || changes.settings || changes.guilds {
            if let Some(state) = self.switcher.background.get(&account) {
                let s = summary(state.read(cx));
                self.switcher.summaries.insert(account, s);
            }
        }
    }

    /// Shows `key` as the active account: the old account's state goes to the
    /// background (without its message windows) and its view state is cleared, then the
    /// new account's state, if it ran in the background, comes back with its last place.
    pub(crate) fn switch_ui_to(&mut self, key: AccountKey, cx: &mut Context<Self>) {
        if self.switcher.is_active(&key) {
            return;
        }
        // Before the first account is shown, a state built from untagged events is that
        // account's own (the demo sends its READY first).
        let first = match self.switcher.active {
            None => self.account.take(),
            Some(_) => None,
        };
        if let Some(old) = self.switcher.active.take() {
            self.switcher.places.leave(
                old.clone(),
                Place {
                    guild: self.selected_guild.clone(),
                    channel: self.selected_channel.clone(),
                },
            );
            if let Some(state) = self.account.take() {
                state.update(cx, |s, _| {
                    s.drop_message_windows();
                });
                let s = summary(state.read(cx));
                self.switcher.summaries.insert(old.clone(), s);
                self.switcher.background.insert(old, state);
            }
        }
        // Nothing of the old account stays on screen or on its way: the message view
        // with its outbox, the unread gate and settings writes (a new write session, so
        // late answers are ignored), the composer's draft and any open window.
        self.reset_messages();
        self.notifications.clear();
        self.composer.input.update(cx, |c, cx| c.clear(cx));
        self.overlays.pending_link = None;
        self.guilds.clear();
        self.channels.clear();
        self.selected_guild = None;
        self.selected_channel = None;
        self.chat.status.clear();
        self.chat.error = None;
        self.chat.notice = None;
        self.chat.gateway_down = None;
        if let Some(a) = self.switcher.accounts.iter().find(|a| a.key == key) {
            self.username = a.name.clone();
            self.instance = a.key.origin.to_string();
        }
        self.switcher.active = Some(key.clone());
        self.switcher.summaries.remove(&key);
        if let Some(state) = self.switcher.background.remove(&key).or(first) {
            self.account = Some(state);
            self.start_ticker(cx);
            let mut changes = Changes::default();
            changes.reset = true;
            self.after_changes(changes, false, cx);
        }
        let place = self.switcher.places.arrive(&key).unwrap_or_default();
        self.switcher.wanted_channel = place.channel;
        self.select_guild(place.guild, cx);
    }

    /// Loads the guild on screen again and opens the channel that was open in it (the
    /// account's session was replaced, or connected after a switch).
    pub(crate) fn reopen_place(&mut self, cx: &mut Context<Self>) {
        self.switcher.wanted_channel = self.selected_channel.clone();
        self.select_guild(self.selected_guild.clone(), cx);
    }

    /// The person picked an account in the switcher.
    pub(crate) fn switch_account(&mut self, key: AccountKey, cx: &mut Context<Self>) {
        self.switcher.open = false;
        if !self.switcher.is_active(&key) {
            self.switch_ui_to(key.clone(), cx);
            self.backend.send(Cmd::SwitchAccount(key));
        }
        cx.notify();
    }

    /// Opens the sign-in screen to add an account; the active one keeps running.
    pub(crate) fn begin_add_account(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.switcher.open = false;
        self.switcher.adding = true;
        self.sign_in.busy = false;
        self.sign_in.error = None;
        self.sign_in.notice = None;
        self.sign_in.handoff = None;
        self.screen = Screen::SignIn;
        self.sign_in
            .instance_input
            .focus_handle(cx)
            .focus(window, cx);
        cx.notify();
    }

    /// Whether the sign-in screen offers a way back to an account.
    pub(crate) fn sign_in_has_way_back(&self) -> bool {
        self.switcher.active.is_some() || !self.switcher.accounts.is_empty()
    }

    /// "Back" on the sign-in screen: to the active account, or to another signed-in one.
    pub(crate) fn back_from_sign_in(&mut self, cx: &mut Context<Self>) {
        self.backend.send(Cmd::CancelSignIn);
        self.switcher.adding = false;
        self.sign_in.busy = false;
        self.sign_in.handoff = None;
        self.sign_in.error = None;
        if self.switcher.active.is_some() {
            self.screen = Screen::Main;
        } else if let Some(first) = self.switcher.accounts.first().map(|a| a.key.clone()) {
            self.switch_ui_to(first.clone(), cx);
            self.backend.send(Cmd::SwitchAccount(first));
        }
        cx.notify();
    }

    /// The stored accounts changed.
    pub(crate) fn on_account_list(&mut self, list: Vec<AccountInfo>) {
        let gone: Vec<AccountKey> = self
            .switcher
            .accounts
            .iter()
            .map(|a| a.key.clone())
            .filter(|k| !list.iter().any(|a| &a.key == k) && !self.switcher.is_active(k))
            .collect();
        for k in gone {
            self.forget_background(&k);
        }
        self.switcher.accounts = list;
    }

    /// An account was signed out or ended by the server.
    pub(crate) fn on_account_removed(&mut self, key: AccountKey, message: Option<String>) {
        self.switcher.accounts.retain(|a| a.key != key);
        if !self.switcher.is_active(&key) {
            self.forget_background(&key);
        }
        if let Some(message) = message {
            let name = self.account_label(&key);
            self.chat.notice = Some(t!("accounts-ended", account = name, message = message));
        }
    }

    fn forget_background(&mut self, key: &AccountKey) {
        self.switcher.background.remove(key);
        self.switcher.summaries.remove(key);
        self.switcher.places.forget(key);
    }

    /// The name to show for an account: from its state when it has one, else the list.
    fn account_label(&self, key: &AccountKey) -> String {
        self.switcher
            .accounts
            .iter()
            .find(|a| &a.key == key)
            .map(|a| a.name.clone())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| key.origin.host().to_string())
    }

    pub(crate) fn render_account_switcher(&self, cx: &mut Context<Self>) -> AnyElement {
        let rows: Vec<AnyElement> = self
            .switcher
            .accounts
            .iter()
            .enumerate()
            .map(|(ix, a)| self.account_row(ix, a, cx))
            .collect();
        let full = self.switcher.accounts.len() >= MAX_ACCOUNTS;
        let add = div()
            .id("account-add")
            .h(px(44.))
            .px_2()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .rounded(px(6.))
            .text_color(if full {
                theme::text_muted()
            } else {
                theme::text()
            })
            .when(!full, |d| {
                d.cursor_pointer()
                    .hover(|s| s.bg(theme::hover_bg()))
                    .on_click(cx.listener(|this, _, window, cx| this.begin_add_account(window, cx)))
            })
            .child(
                div()
                    .size(px(32.))
                    .flex_none()
                    .rounded_full()
                    .border_1()
                    .border_color(theme::ring())
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(18.))
                    .child("+"),
            )
            .child(if full {
                t!("accounts-full", max = MAX_ACCOUNTS)
            } else {
                t!("accounts-add")
            });
        modal(
            "account-switcher",
            440.,
            [
                div()
                    .text_size(px(16.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::text_strong())
                    .child(t!("accounts-title"))
                    .into_any_element(),
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .children(rows)
                    .child(add)
                    .into_any_element(),
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .child(
                        button("accounts-close", t!("accounts-close"), false).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.switcher.open = false;
                                cx.notify();
                            }),
                        ),
                    )
                    .into_any_element(),
            ],
        )
        .into_any_element()
    }

    fn account_row(&self, ix: usize, a: &AccountInfo, cx: &mut Context<Self>) -> AnyElement {
        let active = self.switcher.is_active(&a.key);
        let marks = self.switcher.summary_of(&a.key);
        let name = if a.name.is_empty() {
            a.handle.clone()
        } else {
            a.name.clone()
        };
        let initials = Guild {
            id: String::new(),
            name: name.clone(),
        }
        .initials();
        let detail = if a.handle.is_empty() {
            a.key.origin.host().to_string()
        } else {
            t!(
                "accounts-handle-at",
                handle = a.handle.clone(),
                instance = a.key.origin.host().to_string()
            )
        };
        let key = a.key.clone();
        let remove_key = a.key.clone();
        let main = div()
            .id(SharedString::from(format!("account-row-{ix}")))
            .flex_1()
            .min_w_0()
            .h(px(52.))
            .px_2()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .rounded(px(6.))
            .cursor_pointer()
            .when(active, |d| d.bg(theme::selected_bg()))
            .hover(|s| s.bg(theme::hover_bg()))
            .on_click(cx.listener(move |this, _, _, cx| this.switch_account(key.clone(), cx)))
            .child(
                div()
                    .relative()
                    .size(px(36.))
                    .flex_none()
                    .rounded_full()
                    .bg(if active {
                        theme::brand()
                    } else {
                        theme::guild_item()
                    })
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(13.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::text_strong())
                    .child(initials),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(
                        div()
                            .text_size(px(15.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme::text_strong())
                            .text_ellipsis()
                            .child(name),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme::text_muted())
                            .text_ellipsis()
                            .child(detail),
                    ),
            )
            .when(active, |d| {
                d.child(
                    div()
                        .flex_none()
                        .text_size(px(12.))
                        .text_color(theme::text_muted())
                        .child(t!("accounts-active")),
                )
            })
            .when(!active && marks.mentions > 0, |d| {
                d.child(mention_badge(marks.mentions))
            })
            .when(!active && marks.mentions == 0 && marks.unread, |d| {
                d.child(
                    div()
                        .flex_none()
                        .size(px(8.))
                        .rounded_full()
                        .bg(theme::text_strong()),
                )
            });
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .child(main)
            .child(
                div()
                    .id(SharedString::from(format!("account-sign-out-{ix}")))
                    .size(px(32.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(6.))
                    .cursor_pointer()
                    .hover(|s| s.bg(theme::hover_bg()))
                    .child(Icon::SignOut.el(px(18.), theme::text_muted()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.backend.send(Cmd::RemoveAccount(remove_key.clone()));
                        cx.notify();
                    })),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(origin: &str, user: &str) -> AccountKey {
        AccountKey::new(origin, user).unwrap()
    }

    #[test]
    fn switching_account_keeps_the_last_channel() {
        let a = key("https://one.example", "1");
        let b = key("https://one.example", "2");
        let c = key("https://two.example", "1");
        let mut places = Places::default();
        // A has #general open in guild 10; switch to B, which was never shown.
        let at_a = Place {
            guild: Some("10".into()),
            channel: Some("100".into()),
        };
        places.leave(a.clone(), at_a.clone());
        assert_eq!(places.arrive(&b), None);
        // B opens a DM, then C (another instance), then back to A, then to B.
        let at_b = Place {
            guild: None,
            channel: Some("200".into()),
        };
        places.leave(b.clone(), at_b.clone());
        places.leave(c.clone(), Place::default());
        assert_eq!(places.arrive(&a), Some(at_a.clone()));
        assert_eq!(places.arrive(&b), Some(at_b));
        // The same user ID on another instance is another account.
        assert_eq!(places.arrive(&c), Some(Place::default()));
        // A signed-out account starts over.
        places.forget(&a);
        assert_eq!(places.arrive(&a), None);
    }

    #[test]
    fn the_summary_adds_mentions_and_skips_muted_unread() {
        let ready: inlet_protocol::Ready = serde_json::from_value(serde_json::json!({
            "session_id": "s",
            "user": {"id": "1", "username": "me"},
            "guilds": [{
                "id": "10",
                "properties": {"id": "10", "name": "G", "owner_id": "1"},
                "channels": [{"id": "100", "type": 0, "guild_id": "10", "name": "general",
                              "last_message_id": "900"}],
                "roles": [{"id": "10", "name": "@everyone", "permissions": "2251799813685247", "position": 0}],
                "members": [{"user": {"id": "1", "username": "me"}, "roles": [],
                             "joined_at": "2020-01-01T00:00:00+00:00"}]
            }],
            "private_channels": [{"id": "300", "type": 1, "last_message_id": "950",
                                  "recipients": [{"id": "2", "username": "ada"}]}],
            "read_states": [
                {"id": "100", "last_message_id": "800", "mention_count": 2},
                {"id": "300", "last_message_id": "900", "mention_count": 1}
            ],
            "user_guild_settings": []
        }))
        .unwrap();
        let state = AccountState::from_ready_at(&ready, 1);
        let s = summary(&state);
        assert!(s.unread);
        assert_eq!(s.mentions, 3);
    }
}
