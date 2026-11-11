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
    /// Moves the clock of the passive states once a minute (timed mutes end).
    ticker: Option<gpui::Task<()>>,
}

/// How often the passive states' clocks move: often enough for a timed mute to end
/// within a minute in the switcher and the tray, cheap for ten accounts.
const BACKGROUND_TICK: std::time::Duration = std::time::Duration::from_secs(60);

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
        self.background_marks()
            .fold(0u32, |n, s| n.saturating_add(s.mentions))
    }

    /// The unread marks of the accounts that are not active.
    pub fn background_marks(&self) -> impl Iterator<Item = Summary> + '_ {
        self.summaries
            .iter()
            .filter(|(k, _)| !self.is_active(k))
            .map(|(_, s)| *s)
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
        self.ticker = None;
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
                let mut state = crate::ui::chat::messages::account_state_from(ready);
                state.set_passive(true);
                self.switcher
                    .background
                    .insert(account.clone(), cx.new(|_| state));
                self.start_background_ticker(cx);
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

    /// Starts the once-a-minute clock of the passive states, once.
    fn start_background_ticker(&mut self, cx: &mut Context<Self>) {
        if self.switcher.ticker.is_some() {
            return;
        }
        self.switcher.ticker = Some(cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(BACKGROUND_TICK).await;
            if this
                .update(cx, |this, cx| this.tick_background(cx))
                .is_err()
            {
                break;
            }
        }));
    }

    /// Moves every passive state's clock; marks whose mutes or settings changed are
    /// worked out again.
    pub(crate) fn tick_background(&mut self, cx: &mut Context<Self>) {
        let now = chrono::Utc::now().timestamp_millis();
        let keys: Vec<AccountKey> = self.switcher.background.keys().cloned().collect();
        for key in keys {
            let Some(state) = self.switcher.background.get(&key).cloned() else {
                continue;
            };
            let changes = state.update(cx, |s, _| s.tick(now));
            if changes.settings || changes.read_state {
                let s = summary(state.read(cx));
                self.switcher.summaries.insert(key, s);
            }
        }
    }

    /// Shows `key` as the active account: the old account's state goes to the
    /// background (without its message windows) and its view state is cleared, then the
    /// new account's state, if it ran in the background, comes back with its last place.
    pub(crate) fn switch_ui_to(
        &mut self,
        key: AccountKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
                // Passive: no message windows, no members or presences, few users.
                state.update(cx, |s, _| {
                    s.set_passive(true);
                });
                self.start_background_ticker(cx);
                let s = summary(state.read(cx));
                self.switcher.summaries.insert(old.clone(), s);
                self.switcher.background.insert(old, state);
            }
        }
        // Nothing of the old account stays on screen or on its way: the message view
        // with its outbox, the unread gate and settings writes (a new write session, so
        // late answers are ignored), the composer (its text is saved with the old
        // account's drafts, which are then forgotten here; the account ID is asked for
        // again, so the next account's drafts file is used) and any open window.
        self.reset_messages();
        self.notifications.clear();
        self.composer
            .view
            .update(cx, |v, cx| v.switch_account(window, cx));
        self.composer.reset_session();
        // The old account's image client is not used for the next one; the backend
        // sends the next account's own (its media endpoint) right after the switch.
        self.media.reset();
        // Links: the prompt, a link waiting for the session and a channel a link wants to
        // open were all for the account before (as at sign-out).
        self.overlays.pending_link = None;
        self.overlays.link_prompt = None;
        self.overlays.link_channel = None;
        if cx.has_global::<crate::desktop::Desktop>() {
            cx.global_mut::<crate::desktop::Desktop>()
                .forget_waiting_link();
        }
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
        // Every command from here on is for the new account; the backend drops one that
        // reaches it while another account is active.
        self.backend.set_account(Some(key.clone()));
        self.switcher.summaries.remove(&key);
        if let Some(state) = self.switcher.background.remove(&key).or(first) {
            state.update(cx, |s, _| {
                s.set_passive(false);
            });
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
    pub(crate) fn switch_account(
        &mut self,
        key: AccountKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.switcher.open = false;
        if !self.switcher.is_active(&key) {
            // The backend switches first, so the commands that open the new account's
            // place reach it with that account active.
            self.backend.send(Cmd::SwitchAccount(key.clone()));
            self.switch_ui_to(key, window, cx);
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
    pub(crate) fn back_from_sign_in(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.backend.send(Cmd::CancelSignIn);
        self.switcher.adding = false;
        self.sign_in.busy = false;
        self.sign_in.handoff = None;
        self.sign_in.error = None;
        if self.switcher.active.is_some() {
            self.screen = Screen::Main;
        } else if let Some(first) = self.switcher.accounts.first().map(|a| a.key.clone()) {
            self.backend.send(Cmd::SwitchAccount(first.clone()));
            self.switch_ui_to(first, window, cx);
        }
        cx.notify();
    }

    /// The stored accounts changed.
    pub(crate) fn on_account_list(&mut self, list: Vec<AccountInfo>, cx: &mut Context<Self>) {
        let gone: Vec<AccountKey> = self
            .switcher
            .accounts
            .iter()
            .map(|a| a.key.clone())
            .filter(|k| !list.iter().any(|a| &a.key == k) && !self.switcher.is_active(k))
            .collect();
        for k in gone {
            self.forget_background(&k, cx);
        }
        self.switcher.accounts = list;
    }

    /// An account was signed out or ended by the server.
    pub(crate) fn on_account_removed(
        &mut self,
        key: AccountKey,
        message: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.switcher.accounts.retain(|a| a.key != key);
        if !self.switcher.is_active(&key) {
            self.forget_background(&key, cx);
        }
        if let Some(message) = message {
            let name = self.account_label(&key);
            self.chat.notice = Some(t!("accounts-ended", account = name, message = message));
        }
    }

    /// An account that is not the active one is gone: its passive state, its marks,
    /// its last place and its drafts file (no draft outlives a sign-out).
    fn forget_background(&mut self, key: &AccountKey, cx: &mut Context<Self>) {
        self.switcher.background.remove(key);
        self.switcher.summaries.remove(key);
        self.switcher.places.forget(key);
        let path = dirs::data_local_dir().and_then(|dir| {
            crate::ui::composer::drafts::path_for(
                &dir,
                key.origin.as_str(),
                &key.user_id.to_string(),
            )
        });
        if let Some(path) = path {
            let forget = self.composer.shared.borrow().drafts.forget_file(path);
            cx.background_spawn(async move { forget() }).detach();
        }
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

    /// Whether the account switcher is up (a layer: it has the keyboard and Escape).
    pub(crate) fn switcher_layer_open(&self) -> bool {
        self.switcher.open && self.screen == Screen::Main
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
        let element = modal(
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
        );
        // While topmost it has the keyboard, so typing never reaches the composer behind
        // it, and Escape closes it.
        if self.is_top_layer(crate::desktop::Layer::AccountSwitcher) {
            self.layer_keys(element, cx).into_any_element()
        } else {
            element.into_any_element()
        }
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
        let main =
            div()
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
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.switch_account(key.clone(), window, cx)
                }))
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

    /// A window for the test: `Inlet` lives in it as an entity, not drawn.
    struct Probe;

    impl gpui::Render for Probe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    /// The real switch path, A → B → A → B, against a mock instance (REST) and a mock
    /// gateway: the switcher's own `switch_account`, the real backend thread with two
    /// stored accounts (A active, B started in the background). Every request carries
    /// the token of the account whose server or channel it is for, and each switch
    /// opens the channel that account had open, the second and later switches too.
    #[gpui::test]
    fn switching_account_keeps_the_last_channel(cx: &mut gpui::TestAppContext) {
        use std::collections::HashMap;
        use std::sync::Arc;

        use crate::backend::accounts::testkit::{
            by_token, mock_instance, ready_json, requests, serve_gateway,
        };
        use crate::backend::accounts::Accounts;
        use crate::token_store::{self, AccountList, AccountRecord, MemoryStore};

        cx.executor().allow_parking();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let listener = rt
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let (server, instance) = rt.block_on(mock_instance(
            &format!("ws://127.0.0.1:{port}/gateway"),
            &[("tok-a", "11"), ("tok-b", "12")],
        ));
        {
            let _guard = rt.enter();
            serve_gateway(
                listener,
                HashMap::from([
                    ("tok-a".to_string(), ready_json("11", "600", "601")),
                    ("tok-b".to_string(), ready_json("12", "700", "701")),
                ]),
            );
        }
        let origin = instance.origin().to_string();
        let ka = key(&origin, "11");
        let kb = key(&origin, "12");
        // Both stored, A the active one.
        let store = Arc::new(MemoryStore::default());
        for (k, token) in [(&ka, "tok-a"), (&kb, "tok-b")] {
            token_store::save_session(
                &*store,
                &k.entry(),
                &inlet_protocol::Token::new(token),
                &instance.session_pins(),
                false,
            )
            .unwrap();
        }
        let list = std::env::temp_dir()
            .join(format!("inlet-switch-{}", std::process::id()))
            .join("accounts.json");
        let record = |k: &AccountKey, name: &str| AccountRecord {
            origin: origin.clone(),
            user_id: k.user_id.to_string(),
            name: name.into(),
            handle: name.into(),
        };
        token_store::save_list(
            &list,
            &AccountList {
                accounts: vec![record(&ka, "a"), record(&kb, "b")],
                active: Some(ka.entry()),
            },
        )
        .unwrap();

        let window = cx.add_window(|_, _| Probe);
        let (store2, list2) = (store.clone(), list.clone());
        let inlet = window
            .update(cx, |_, window, cx| {
                cx.new(|cx| {
                    Inlet::new_with(window, cx, move |tx| {
                        crate::backend::start_with(tx, move |ui| {
                            Accounts::new(store2, Some(list2), Some(ui))
                        })
                    })
                })
            })
            .unwrap();

        let wait = |cx: &mut gpui::TestAppContext,
                    what: &str,
                    done: &dyn Fn(&Inlet, &[wiremock::Request]) -> bool| {
            for _ in 0..600 {
                cx.run_until_parked();
                let reqs = rt.block_on(requests(&server));
                if inlet.read_with(cx, |i, _| done(i, &reqs)) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            panic!("timed out waiting for: {what}");
        };
        let loads = |reqs: &[wiremock::Request], channel: &str| {
            let path = format!("/api/v1/channels/{channel}/messages");
            reqs.iter().filter(|r| r.url.path() == path).count()
        };
        let act = |cx: &mut gpui::TestAppContext,
                   f: &dyn Fn(&mut Inlet, &mut Window, &mut Context<Inlet>)| {
            window
                .update(cx, |_, window, cx| {
                    inlet.update(cx, |this, cx| f(this, window, cx))
                })
                .unwrap();
        };

        // A is up, B runs in the background.
        wait(cx, "A signed in, B in the background", &|i, _| {
            i.screen == Screen::Main
                && i.switcher.is_active(&ka)
                && i.account.is_some()
                && i.switcher.background.contains_key(&kb)
        });
        // A opens guild 600; its first channel 601 loads.
        act(cx, &|this, _, cx| this.select_guild(Some("600".into()), cx));
        wait(cx, "A opens #601", &|i, r| {
            i.selected_channel.as_deref() == Some("601") && loads(r, "601") >= 1
        });

        // To B, which opens guild 700 and its channel 701.
        let k = kb.clone();
        act(cx, &move |this, window, cx| {
            this.switch_account(k.clone(), window, cx)
        });
        wait(cx, "B is shown", &|i, _| i.account.is_some());
        act(cx, &|this, _, cx| this.select_guild(Some("700".into()), cx));
        wait(cx, "B opens #701", &|i, r| {
            i.selected_channel.as_deref() == Some("701") && loads(r, "701") >= 1
        });

        // Back to A: #601 again, without a click.
        let k = ka.clone();
        act(cx, &move |this, window, cx| {
            this.switch_account(k.clone(), window, cx)
        });
        wait(cx, "A is back in #601", &|i, r| {
            i.switcher.is_active(&ka)
                && i.selected_guild.as_deref() == Some("600")
                && i.selected_channel.as_deref() == Some("601")
                && loads(r, "601") >= 2
        });
        // And B again (the second switch to it): #701.
        let k = kb.clone();
        act(cx, &move |this, window, cx| {
            this.switch_account(k.clone(), window, cx)
        });
        wait(cx, "B is back in #701", &|i, r| {
            i.switcher.is_active(&kb)
                && i.selected_guild.as_deref() == Some("700")
                && i.selected_channel.as_deref() == Some("701")
                && loads(r, "701") >= 2
        });

        // Every request went out with the token of the account it was for.
        let reqs = rt.block_on(requests(&server));
        let seen = by_token(&reqs);
        let a_paths = seen.get("tok-a").cloned().unwrap_or_default();
        let b_paths = seen.get("tok-b").cloned().unwrap_or_default();
        assert!(
            !a_paths
                .iter()
                .any(|p| p.contains("/700") || p.contains("/701")),
            "A's token on B's server: {a_paths:?}"
        );
        assert!(
            !b_paths
                .iter()
                .any(|p| p.contains("/600") || p.contains("/601")),
            "B's token on A's server: {b_paths:?}"
        );
        assert!(a_paths
            .iter()
            .any(|p| p.ends_with("/channels/601/messages")));
        assert!(b_paths
            .iter()
            .any(|p| p.ends_with("/channels/701/messages")));
        let _ = std::fs::remove_dir_all(list.parent().unwrap());
        drop(rt);
    }

    #[test]
    fn places_are_kept_per_account() {
        let a = key("https://one.example", "1");
        let b = key("https://one.example", "2");
        let c = key("https://two.example", "1");
        let mut places = Places::default();
        let at_a = Place {
            guild: Some("10".into()),
            channel: Some("100".into()),
        };
        places.leave(a.clone(), at_a.clone());
        assert_eq!(places.arrive(&b), None);
        places.leave(c.clone(), Place::default());
        assert_eq!(places.arrive(&a), Some(at_a));
        // The same user ID on another instance is another account.
        assert_eq!(places.arrive(&c), Some(Place::default()));
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
