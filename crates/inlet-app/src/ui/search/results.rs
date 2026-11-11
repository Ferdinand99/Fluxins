// SPDX-License-Identifier: AGPL-3.0-or-later
//! The results of a message search and the pacing of its requests.
//!
//! The results show in the search panel as the inbox shows messages: each under its
//! channel, drawn with the message markdown and attachments (spoilers and media covers
//! stay closed), and a click jumps to the message through the inbox's jump (the page
//! around it, a detached window, the highlight). Results from channels the account
//! cannot view are never shown: the threads of a page are stored first (with the
//! account's membership, so private threads fail closed), then every result is checked
//! against the account state, also again when it is drawn.
//!
//! Budget ([`Budget`]): one search at a time and at most one a second; a newer search
//! waits for its turn and replaces one that waits. A search on its way that never
//! answers stops counting after [`SEARCH_TIMEOUT_MS`].

use std::time::Duration;

use gpui::{div, prelude::*, px, AnyElement, Context, FontWeight, SharedString, Task, Window};
use inlet_state::ChannelId;

use super::filters::{Filters, Scope, Sort};
use crate::app::Inlet;
use crate::backend::accounts::AccountKey;
use crate::backend::messages::inbox::Item;
use crate::backend::search::{Cmd, Ev, Page};
use crate::theme;
use crate::ui::inbox::{list_note, text_button};

/// Shortest time between the starts of two searches of the account.
pub const SEARCH_INTERVAL_MS: i64 = 1_000;
/// A search on its way longer than this no longer holds the next one back.
pub const SEARCH_TIMEOUT_MS: i64 = 15_000;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// One search at a time, at most one a second; the newest waiting search wins.
pub struct Budget<T> {
    in_flight: Option<(u64, i64)>,
    last_start: Option<i64>,
    waiting: Option<T>,
}

impl<T> Default for Budget<T> {
    fn default() -> Self {
        Budget {
            in_flight: None,
            last_start: None,
            waiting: None,
        }
    }
}

impl<T> Budget<T> {
    /// A search is wanted; it replaces one that still waits for its turn.
    pub fn ask(&mut self, what: T) {
        self.waiting = Some(what);
    }

    /// The search whose turn has come, if any.
    pub fn take(&mut self, now: i64) -> Option<T> {
        if self
            .in_flight
            .is_some_and(|(_, at)| now.saturating_sub(at) >= SEARCH_TIMEOUT_MS)
        {
            self.in_flight = None;
        }
        if self.in_flight.is_some() || self.wait_ms(now) > 0 {
            return None;
        }
        self.waiting.take()
    }

    /// The search taken last went out as `request` at `now`.
    pub fn started(&mut self, request: u64, now: i64) {
        self.in_flight = Some((request, now));
        self.last_start = Some(now);
    }

    /// The answer to `request` came: the next search may go (after the interval).
    pub fn answered(&mut self, request: u64) {
        if self.in_flight.is_some_and(|(r, _)| r == request) {
            self.in_flight = None;
        }
    }

    /// How long a waiting search still has to wait for the interval.
    pub fn wait_ms(&self, now: i64) -> i64 {
        self.last_start
            .map_or(0, |t| (t + SEARCH_INTERVAL_MS).saturating_sub(now).max(0))
    }

    pub fn has_waiting(&self) -> bool {
        self.waiting.is_some()
    }

    pub fn busy(&self) -> bool {
        self.in_flight.is_some()
    }
}

/// What a search asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ask {
    pub account: AccountKey,
    pub scope: Scope,
    pub filters: Filters,
    pub sort: Sort,
    pub page: u16,
}

/// The search on screen and its results.
#[derive(Default)]
pub struct Results {
    /// The search shown (what the next page or sort asks again).
    pub shown: Option<Ask>,
    pub items: Vec<Item>,
    pub total: u64,
    pub more: bool,
    /// The answer wanted: the newest search that went out, unless a newer one waits.
    want: Option<u64>,
    pub error: Option<String>,
    pub indexing: bool,
    budget: Budget<Ask>,
    next_request: u64,
    /// Wakes the waiting search when its turn comes.
    timer: Option<Task<()>>,
}

impl Results {
    /// Whether the shown search waits for its results.
    pub fn loading(&self) -> bool {
        self.want.is_some() || self.budget.has_waiting()
    }

    /// Another account, or signed out: nothing of the old one stays. Request numbers
    /// go on counting, so a late answer never fits a new request.
    pub fn clear(&mut self) {
        let next_request = self.next_request;
        *self = Results {
            next_request,
            ..Results::default()
        };
    }
}

impl Inlet {
    /// Searches `ask` (a new search, another page or another sort), within the budget.
    pub(crate) fn ask_search(&mut self, ask: Ask, cx: &mut Context<Self>) {
        let r = &mut self.search.results;
        if r.shown
            .as_ref()
            .is_none_or(|s| s.filters != ask.filters || s.scope != ask.scope)
        {
            r.items.clear();
            r.total = 0;
            r.more = false;
        }
        r.shown = Some(ask.clone());
        r.error = None;
        r.indexing = false;
        // An answer on its way is for an older search now.
        r.want = None;
        r.budget.ask(ask);
        self.pump_search(cx);
        cx.notify();
    }

    /// Sends the waiting search if its turn has come; otherwise wakes up for it.
    fn pump_search(&mut self, cx: &mut Context<Self>) {
        let now = now_ms();
        let r = &mut self.search.results;
        if let Some(ask) = r.budget.take(now) {
            // Only for the account on screen; a search of an account left is dropped.
            if !self.switcher.is_active(&ask.account) {
                return;
            }
            r.next_request += 1;
            let request = r.next_request;
            r.budget.started(request, now);
            r.want = Some(request);
            let search = ask
                .filters
                .query(ask.scope, ask.sort, ask.page, &chrono::Local);
            self.backend.send(Cmd::Messages {
                account: ask.account,
                request,
                search: Box::new(search),
            });
            return;
        }
        if r.budget.has_waiting() && !r.budget.busy() {
            let wait = r.budget.wait_ms(now).max(1) as u64;
            r.timer = Some(cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(wait))
                    .await;
                let _ = this.update(cx, |this, cx| this.pump_search(cx));
            }));
        }
    }

    /// Events from the backend's search domain.
    pub(crate) fn on_search_results(&mut self, ev: Ev, cx: &mut Context<Self>) {
        let request = match &ev {
            Ev::Results { request, .. } | Ev::Indexing { request } | Ev::Failed { request, .. } => {
                *request
            }
        };
        self.search.results.budget.answered(request);
        if self.search.results.want == Some(request) {
            self.search.results.want = None;
            match ev {
                Ev::Results { page, .. } => self.land_results(page, cx),
                Ev::Indexing { .. } => self.search.results.indexing = true,
                Ev::Failed { reason, .. } => {
                    self.search.results.error = Some(reason.unwrap_or_else(|| t!("search-failed")));
                }
            }
        }
        self.pump_search(cx);
        cx.notify();
    }

    /// A page of results: its threads go into the account state first (with the
    /// account's membership), then only results the account can view are kept.
    fn land_results(&mut self, page: Page, cx: &mut Context<Self>) {
        let Some(account) = self.account.clone() else {
            return;
        };
        if !page.threads.is_empty() {
            let changes = account.update(cx, |s, _| {
                let mut c = inlet_state::Changes::default();
                for t in &page.threads {
                    c.merge(s.upsert_thread(t));
                }
                c
            });
            self.after_changes(changes, false, cx);
        }
        let state = account.read(cx);
        let items: Vec<Item> = page
            .items
            .into_iter()
            .filter(|i| can_view(state, &i.message.channel_id))
            .collect();
        let r = &mut self.search.results;
        r.items = items;
        r.total = page.total;
        r.more = page.more;
        if let Some(shown) = r.shown.as_mut() {
            shown.page = page.page;
        }
    }

    /// Another page of the shown search (`by` pages on).
    pub(crate) fn search_page(&mut self, by: i32, cx: &mut Context<Self>) {
        if let Some(mut ask) = self.search.results.shown.clone() {
            let page = i32::from(ask.page) + by;
            if !(1..=400).contains(&page) {
                return;
            }
            ask.page = page as u16;
            self.ask_search(ask, cx);
        }
    }

    /// The shown search in the next order.
    pub(crate) fn next_search_sort(&mut self, cx: &mut Context<Self>) {
        if let Some(mut ask) = self.search.results.shown.clone() {
            ask.sort = ask.sort.next();
            ask.page = 1;
            self.ask_search(ask, cx);
        }
    }

    /// A click on a result: the inbox's jump to the message (its channel opens, the page
    /// around it loads when it is not in the list, and it is highlighted). The results
    /// stay open beside it. A channel the account cannot view is refused by the jump.
    pub(crate) fn open_result(&mut self, item: &Item, window: &mut Window, cx: &mut Context<Self>) {
        let (channel_id, message_id) = (item.message.channel_id.clone(), item.message.id.clone());
        self.jump_to(&channel_id, &message_id, window, cx);
    }

    /// "Try again" after a failure or while the instance indexes.
    pub(crate) fn retry_search(&mut self, cx: &mut Context<Self>) {
        if let Some(ask) = self.search.results.shown.clone() {
            self.ask_search(ask, cx);
        }
    }

    /// The results part of the search panel.
    pub(crate) fn render_search_results(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let r = &self.search.results;
        let sort = r.shown.as_ref().map(|s| s.sort).unwrap_or_default();
        let page = r.shown.as_ref().map_or(1, |s| s.page);
        let title = if r.loading() && r.items.is_empty() {
            t!("search-searching")
        } else {
            t!("search-results-count", count = r.total)
        };
        let sort_label = match sort {
            Sort::Newest => t!("search-sort-newest"),
            Sort::Oldest => t!("search-sort-oldest"),
            Sort::Relevance => t!("search-sort-relevance"),
        };
        let header = div()
            .flex_none()
            .h(px(48.))
            .px(px(16.))
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .border_b_1()
            .border_color(theme::divider())
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::text_strong())
                    .child(title),
            )
            .child(
                text_button("search-sort".into(), sort_label)
                    .on_click(cx.listener(|this, _, _, cx| this.next_search_sort(cx))),
            );

        let error = r.error.clone();
        let indexing = r.indexing;
        let (more, loading, empty) = (r.more, r.loading(), r.items.is_empty());
        let items = r.items.clone();
        let mut body = Vec::new();
        if let Some(error) = error {
            body.push(self.search_retry(error, cx));
        } else if indexing {
            body.push(self.search_retry(t!("search-indexing"), cx));
        } else if loading && empty {
            body.push(list_note(t!("search-searching")).into_any_element());
        } else if empty {
            body.push(list_note(t!("search-no-results")).into_any_element());
        }
        let state = self.account.clone();
        for item in &items {
            // Checked again as drawn: access can go while the results are open.
            let visible = state
                .as_ref()
                .is_some_and(|s| can_view(s.read(cx), &item.message.channel_id));
            if !visible {
                continue;
            }
            let header = self.result_header(item, cx);
            body.push(self.message_card("search", item, Some(header), cx));
        }
        let pager = (!empty && (page > 1 || more)).then(|| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_center()
                .gap(px(12.))
                .py(px(8.))
                .when(page > 1, |d| {
                    d.child(
                        text_button("search-prev".into(), t!("search-page-previous"))
                            .on_click(cx.listener(|this, _, _, cx| this.search_page(-1, cx))),
                    )
                })
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme::text_muted())
                        .child(t!("search-page", page = page)),
                )
                .when(more, |d| {
                    d.child(
                        text_button("search-next".into(), t!("search-page-next"))
                            .on_click(cx.listener(|this, _, _, cx| this.search_page(1, cx))),
                    )
                })
                .into_any_element()
        });
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(header)
            .child(
                div()
                    .id("search-results")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(px(12.))
                    .pt(px(8.))
                    .flex()
                    .flex_col()
                    .children(body)
                    .children(pager),
            )
            .into_any_element()
    }

    fn search_retry(&self, note: String, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(8.))
            .child(list_note(note))
            .child(
                text_button("search-retry".into(), t!("inbox-retry"))
                    .on_click(cx.listener(|this, _, _, cx| this.retry_search(cx))),
            )
            .into_any_element()
    }

    /// The channel and server over a result; a click jumps to the message.
    fn result_header(&self, item: &Item, cx: &mut Context<Self>) -> AnyElement {
        let (channel, guild) = self.channel_and_guild_names(&item.message.channel_id, cx);
        let message_id = item.message.id.clone();
        let clicked = item.clone();
        let label = match guild {
            Some(g) => format!("{channel} · {g}"),
            None => channel,
        };
        div()
            .id(SharedString::from(format!("search-jump-{message_id}")))
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(8.))
            .py(px(6.))
            .cursor_pointer()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .text_size(px(12.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::text_muted())
                    .child(label),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(px(12.))
                    .text_color(theme::link())
                    .child(t!("search-jump")),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_result(&clicked, window, cx)
            }))
            .into_any_element()
    }
}

/// Whether the account may see the channel of a result (fail closed: unknown channels
/// are hidden).
fn can_view(state: &inlet_state::AccountState, channel_id: &str) -> bool {
    ChannelId::parse(channel_id).is_some_and(|ch| state.channel_permissions(ch).can_view())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::app::Screen;
    use crate::backend::accounts::Accounts;
    use crate::token_store::MemoryStore;
    use crate::ui::inbox::tests::item;
    use gpui::{Render, TestAppContext};
    use inlet_protocol as p;
    use inlet_state::AccountState;

    const GUILD: &str = "100";
    const GENERAL: &str = "301";
    const OTHER: &str = "302";
    const SECRET: &str = "303";

    fn state() -> AccountState {
        let ready: p::Ready = serde_json::from_value(serde_json::json!({
            "session_id": "s",
            "user": {"id": "10", "username": "me", "mfa_enabled": false},
            "guilds": [{
                "id": GUILD,
                "properties": {"id": GUILD, "name": "g", "owner_id": "1"},
                "roles": [{"id": GUILD, "name": "@everyone", "position": 0,
                           "permissions": "274877975552"}],
                "channels": [
                    {"id": GENERAL, "type": 0, "name": "general", "position": 0},
                    {"id": OTHER, "type": 0, "name": "other", "position": 1},
                    {"id": SECRET, "type": 0, "name": "secret", "position": 2,
                     "permission_overwrites": [
                         {"id": GUILD, "type": 0, "allow": "0", "deny": "1024"}
                     ]}
                ],
                "members": [{"user": {"id": "10", "username": "me"}, "roles": [],
                             "joined_at": "2026-01-01T00:00:00Z"}]
            }],
            "private_channels": [], "users": [], "read_states": [], "user_guild_settings": []
        }))
        .unwrap();
        AccountState::from_ready_at(&ready, 1_800_000_000_000)
    }

    struct Probe;

    impl Render for Probe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    /// A click on a result goes through the inbox's jump: the jump starts for the
    /// result's message, its channel is asked to open, and the results stay open. A
    /// result in a channel the account cannot view never starts a jump.
    #[gpui::test]
    fn result_click_jumps(cx: &mut TestAppContext) {
        let window = cx.add_window(|_, _| Probe);
        let inlet = window
            .update(cx, |_, window, cx| {
                cx.new(|cx| {
                    Inlet::new_with(window, cx, |tx| {
                        crate::backend::start_with(tx, |ui| {
                            Accounts::new(Arc::new(MemoryStore::default()), None, Some(ui))
                        })
                    })
                })
            })
            .unwrap();
        window
            .update(cx, |_, window, cx| {
                inlet.update(cx, |this, cx| {
                    let s = state();
                    let general = ChannelId::parse(GENERAL).unwrap();
                    let secret = ChannelId::parse(SECRET).unwrap();
                    assert!(s.channel_permissions(general).can_view());
                    assert!(!s.channel_permissions(secret).can_view());
                    this.screen = Screen::Main;
                    this.account = Some(cx.new(|_| s));
                    this.selected_guild = Some(GUILD.into());
                    this.selected_channel = Some(OTHER.into());
                    this.search.bar.open = true;
                    let now = now_ms();

                    this.open_result(&item(5_000, GENERAL), window, cx);
                    assert!(this.inbox.jump.is_active(now));
                    assert_eq!(this.chat.pending_channel_jump.as_deref(), Some(GENERAL));
                    assert!(this.search.bar.open);
                    // Once the channel is open and its page is in without the message,
                    // the jump asks for the page around it.
                    let step = this.inbox.jump.step(Some(general), true, |_| false, now);
                    assert_eq!(
                        step,
                        crate::ui::inbox::JumpStep::Fetch(crate::ui::inbox::Target {
                            channel: general,
                            message: inlet_state::MessageId::parse("5000").unwrap(),
                        })
                    );

                    this.chat.pending_channel_jump = None;
                    this.open_result(&item(5_001, SECRET), window, cx);
                    assert!(!this.inbox.jump.is_active(now));
                    assert_eq!(this.chat.pending_channel_jump, None);
                })
            })
            .unwrap();
    }

    /// The worst case: Enter every 100 ms for five seconds, answers 50 ms after each
    /// request (and one that never comes). Requests start at least a second apart,
    /// never two at once, and the last search asked for is the last one sent.
    #[test]
    fn search_is_budgeted_one_per_second() {
        let mut b: Budget<u32> = Budget::default();
        let mut starts: Vec<(i64, u32)> = Vec::new();
        let mut on_way: Option<(u64, i64)> = None;
        let mut request = 0u64;
        for tick in 0..=500 {
            let now = tick * 50;
            if now % 100 == 0 && now <= 5_000 {
                b.ask((now / 100) as u32);
            }
            if let Some((r, at)) = on_way {
                // The third request is never answered.
                if r != 3 && now >= at + 50 {
                    b.answered(r);
                    on_way = None;
                }
            }
            if let Some(what) = b.take(now) {
                request += 1;
                b.started(request, now);
                on_way = Some((request, now));
                starts.push((now, what));
            }
        }
        assert!(starts.len() >= 2);
        for pair in starts.windows(2) {
            assert!(pair[1].0 - pair[0].0 >= SEARCH_INTERVAL_MS);
        }
        // The lost answer held the next search back until it timed out, no longer.
        let third = starts[2].0;
        assert!(starts[3].0 >= third + SEARCH_TIMEOUT_MS);
        assert!(starts[3].0 <= third + SEARCH_TIMEOUT_MS + 50);
        // The newest search wins: the last one asked for (at 5 s) went out last.
        assert_eq!(starts.last().map(|s| s.1), Some(50));
        // Within the five seconds of typing, at most one search a second.
        assert!(starts.iter().filter(|s| s.0 <= 5_000).count() <= 6);
    }
}
