// SPDX-License-Identifier: AGPL-3.0-or-later
//! The quick switcher (Ctrl+K, Cmd+K on macOS): a window to jump to a channel, a direct
//! message or a server of any signed-in account, as Fluxer's.
//!
//! It needs no requests. The candidates come from the account states: the active one,
//! and the passive states of the accounts in the background, which keep their guilds,
//! channels and read states (not their members). Only channels the account can view are
//! offered. Without a query it lists the channels visited last, then those with mentions,
//! then the unread ones, over every account; with a query it ranks the names that match,
//! visited and unread ones first among equals. `#`, `!`, `@` and `*` narrow it to text
//! channels, voice channels, direct messages or servers, as in Fluxer.
//!
//! Choosing an entry of another account switches to that account through the switcher's
//! own path (`switch_account`), which then opens the entry. The visits are kept in
//! memory only, for this run.

use std::collections::VecDeque;
use std::rc::Rc;

use gpui::{
    div, prelude::*, px, AnyElement, Context, Entity, Focusable, FontWeight, SharedString,
    Subscription, Window,
};
use inlet_state::{AccountState, ChannelId, GuildId};

use crate::app::Inlet;
use crate::backend::accounts::AccountKey;
use crate::backend::messages::parse::to_channel;
use crate::model::{clean_name, ChannelKind};
use crate::text_input::{InputEvent, TextInput};
use crate::theme;
use crate::ui::notifications::badge::mention_badge;
use crate::ui::notifications::channel_badge;
use crate::ui::overlays::account_switcher::Place;
use crate::ui::sidebar::channel_list::channel_icon;

/// Channels remembered as visited, per account (Fluxer keeps 20).
const MAX_VISITS: usize = 20;
/// Fluxer's history: the newest visits shown, fewer when other sections follow.
const HISTORY_LIMIT: usize = 8;
const PREVIOUS_WITH_SECTIONS: usize = 3;
const PREVIOUS_ALONE: usize = 7;
/// The first visits are not listed again under mentions or unread.
const HISTORY_HIDDEN_FROM_SECTIONS: usize = 3;
/// Rows per section without a query, and results with one.
const SECTION_LIMIT: usize = 20;
const RESULT_LIMIT: usize = 50;
/// The switcher's size, from Fluxer's `QuickSwitcherModal.module.css`.
pub const SWITCHER_WIDTH: f32 = 576.;
const LIST_HEIGHT: f32 = 320.;
const ROW_HEIGHT: f32 = 36.;

/// What an entry opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Channel(ChannelKind),
    Guild,
}

/// One entry the switcher can offer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub account: AccountKey,
    pub guild: Option<GuildId>,
    /// `None` for a server.
    pub channel: Option<ChannelId>,
    pub kind: Kind,
    pub title: String,
    pub guild_name: Option<String>,
    pub mentions: u32,
    /// Unread and not muted.
    pub unread: bool,
    /// The newest message's ID (0 when unknown): newer activity sorts first.
    pub activity: u64,
    /// When it was last opened (Unix ms), in this run.
    pub visited: Option<i64>,
    /// The channel on screen now.
    pub current: bool,
}

/// A heading of the list without a query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    Previous,
    Mentions,
    Unread,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Row {
    Header(Section),
    Item(Candidate),
}

/// The channels opened, newest first, per account.
///
/// Deviation: Fluxer stores the visits on disk; Inlet keeps them in memory for this run
/// only, because a log of which channels were opened when is not among the data kept
/// on disk.
#[derive(Default)]
pub struct Visits {
    list: VecDeque<(AccountKey, ChannelId, i64)>,
}

impl Visits {
    pub fn note(&mut self, account: AccountKey, channel: ChannelId, at: i64) {
        self.list
            .retain(|(a, c, _)| !(a == &account && *c == channel));
        self.list.push_front((account.clone(), channel, at));
        // At most MAX_VISITS per account: that account's oldest go.
        let mut seen = 0;
        self.list.retain(|(a, _, _)| {
            if a != &account {
                return true;
            }
            seen += 1;
            seen <= MAX_VISITS
        });
    }

    /// An account was signed out: its visits go.
    pub fn forget(&mut self, account: &AccountKey) {
        self.list.retain(|(a, _, _)| a != account);
    }

    pub fn last(&self, account: &AccountKey, channel: ChannelId) -> Option<i64> {
        self.list
            .iter()
            .find(|(a, c, _)| a == account && *c == channel)
            .map(|(_, _, at)| *at)
    }

    pub fn clear(&mut self) {
        self.list.clear();
    }
}

/// Every entry of one account: its servers, the channels it can view (no categories, no
/// threads) and its direct messages. Works on a passive state: it uses only guilds,
/// channels, permissions and read states.
pub fn candidates_of(
    account: &AccountKey,
    state: &AccountState,
    visits: &Visits,
    current: Option<ChannelId>,
) -> Vec<Candidate> {
    let mut out = Vec::new();
    let activity = |ch: ChannelId, c: &inlet_protocol::models::Channel| {
        state
            .read_state(ch)
            .and_then(|r| r.last_message_id)
            .map(|m| m.0)
            .or_else(|| c.last_message_id.as_deref().and_then(|m| m.parse().ok()))
            .unwrap_or(0)
    };
    for g in state.guilds() {
        if g.left || g.visiting {
            continue;
        }
        let name = g
            .properties
            .as_ref()
            .map(|p| clean_name(&p.name))
            .filter(|n| !n.is_empty());
        let u = state.guild_unread(g.id);
        out.push(Candidate {
            account: account.clone(),
            guild: Some(g.id),
            channel: None,
            kind: Kind::Guild,
            title: name.clone().unwrap_or_default(),
            guild_name: None,
            mentions: u.mentions,
            unread: u.unread && !u.muted,
            activity: 0,
            visited: None,
            current: false,
        });
        for c in state.visible_channels(g.id) {
            let Some(ch) = ChannelId::parse(&c.id) else {
                continue;
            };
            let kind = to_channel(c).kind;
            if matches!(kind, ChannelKind::Category | ChannelKind::Other) || c.is_thread() {
                continue;
            }
            // Fail closed: `visible_channels` only lists viewable ones; check again.
            if !state.channel_permissions(ch).can_view() {
                continue;
            }
            let badge = channel_badge(state, ch);
            out.push(Candidate {
                account: account.clone(),
                guild: Some(g.id),
                channel: Some(ch),
                kind: Kind::Channel(kind),
                title: to_channel(c).name,
                guild_name: name.clone(),
                mentions: badge.mentions,
                unread: badge.unread && !badge.muted,
                activity: activity(ch, c),
                visited: visits.last(account, ch),
                current: current == Some(ch),
            });
        }
    }
    for c in state.private_channels() {
        let Some(ch) = ChannelId::parse(&c.id) else {
            continue;
        };
        let channel = to_channel(c);
        if !matches!(channel.kind, ChannelKind::Dm | ChannelKind::GroupDm) {
            continue;
        }
        let badge = channel_badge(state, ch);
        out.push(Candidate {
            account: account.clone(),
            guild: None,
            channel: Some(ch),
            kind: Kind::Channel(channel.kind),
            title: channel.name,
            guild_name: None,
            mentions: badge.mentions,
            unread: badge.unread && !badge.muted,
            activity: activity(ch, c),
            visited: visits.last(account, ch),
            current: current == Some(ch),
        });
    }
    out
}

/// How well `title` matches `query` (both lower case), or `None`.
fn score(title: &str, query: &str) -> Option<u32> {
    if query.is_empty() {
        return Some(1);
    }
    if title == query {
        return Some(1000);
    }
    if title.starts_with(query) {
        return Some(800);
    }
    let word_start = title
        .match_indices(query)
        .any(|(i, _)| title[..i].ends_with([' ', '-', '_', '.', ',']));
    if word_start {
        return Some(600);
    }
    if title.contains(query) {
        return Some(400);
    }
    // Every character in order (a loose match).
    let mut chars = title.chars();
    query.chars().all(|q| chars.any(|t| t == q)).then_some(200)
}

/// A fixed order among entries that are otherwise equal (the background states come
/// from a map, so their order would change from run to run).
fn settle(a: &Candidate, b: &Candidate) -> std::cmp::Ordering {
    a.title
        .cmp(&b.title)
        .then_with(|| a.account.cmp(&b.account))
        .then(a.guild.cmp(&b.guild))
        .then(a.channel.cmp(&b.channel))
}

/// Newer activity first, then [`settle`].
fn by_activity(a: &Candidate, b: &Candidate) -> std::cmp::Ordering {
    b.activity.cmp(&a.activity).then_with(|| settle(a, b))
}

/// Visited first (newest visit first), then mentions, unread, newer activity, name.
fn by_interest(a: &Candidate, b: &Candidate) -> std::cmp::Ordering {
    b.visited
        .cmp(&a.visited)
        .then(b.mentions.cmp(&a.mentions))
        .then(b.unread.cmp(&a.unread))
        .then_with(|| by_activity(a, b))
}

/// The rows for `query` over the candidates of every account.
///
/// Deviation: Fluxer's `@` searches users (and opens a DM with them); here it narrows to
/// the direct messages that exist, since starting a DM is the DM list's job.
pub fn rank(candidates: &[Candidate], query: &str) -> Vec<Row> {
    let query = query.trim();
    let (mode, search) = match query.chars().next() {
        Some(c @ ('#' | '!' | '@' | '*')) => (Some(c), query[1..].trim()),
        _ => (None, query),
    };
    if mode.is_none() && search.is_empty() {
        return default_rows(candidates);
    }
    let fits = |c: &Candidate| match (mode, c.kind) {
        (None, _) => true,
        (Some('#'), Kind::Channel(k)) => matches!(
            k,
            ChannelKind::Text | ChannelKind::Announcement | ChannelKind::Forum | ChannelKind::Media
        ),
        (Some('!'), Kind::Channel(k)) => k == ChannelKind::Voice,
        (Some('@'), Kind::Channel(k)) => matches!(k, ChannelKind::Dm | ChannelKind::GroupDm),
        (Some('*'), Kind::Guild) => true,
        _ => false,
    };
    let search = search.to_lowercase();
    let mut found: Vec<(u32, &Candidate)> = candidates
        .iter()
        .filter(|c| fits(c) && !c.title.is_empty())
        .filter_map(|c| Some((score(&c.title.to_lowercase(), &search)?, c)))
        .collect();
    found.sort_by(|(sa, a), (sb, b)| sb.cmp(sa).then_with(|| by_interest(a, b)));
    found
        .into_iter()
        .take(RESULT_LIMIT)
        .map(|(_, c)| Row::Item(c.clone()))
        .collect()
}

/// Without a query: the channels visited last, then those with mentions, then the unread
/// ones, across every account (Fluxer's `generateDefaultResults`).
///
/// Deviation: Fluxer lists other accounts in sections of their own, and its unread
/// section holds the open community's text channels at the "all messages" level only.
/// Inlet ranks every account in the same sections and lists every unread channel and
/// direct message that is not muted, so one list shows where to go next across
/// accounts.
fn default_rows(candidates: &[Candidate]) -> Vec<Row> {
    let channels: Vec<&Candidate> = candidates
        .iter()
        .filter(|c| c.channel.is_some() && !c.current)
        .collect();
    let mut previous: Vec<&Candidate> = channels
        .iter()
        .copied()
        .filter(|c| c.visited.is_some())
        .collect();
    previous.sort_by(|a, b| b.visited.cmp(&a.visited).then_with(|| settle(a, b)));
    previous.truncate(HISTORY_LIMIT - 1);
    let seen = |c: &Candidate| {
        previous
            .iter()
            .take(HISTORY_HIDDEN_FROM_SECTIONS)
            .any(|p| p.account == c.account && p.channel == c.channel)
    };
    let mut mentions: Vec<&Candidate> = channels
        .iter()
        .copied()
        .filter(|c| c.mentions > 0 && !seen(c))
        .collect();
    mentions.sort_by(|a, b| by_activity(a, b));
    mentions.truncate(SECTION_LIMIT);
    let mut unread: Vec<&Candidate> = channels
        .iter()
        .copied()
        .filter(|c| c.unread && c.mentions == 0 && !seen(c))
        .collect();
    unread.sort_by(|a, b| by_activity(a, b));
    unread.truncate(SECTION_LIMIT);

    let mut rows = Vec::new();
    let cap = if mentions.is_empty() && unread.is_empty() {
        PREVIOUS_ALONE
    } else {
        PREVIOUS_WITH_SECTIONS
    };
    for (section, list) in [
        (Section::Previous, &previous[..previous.len().min(cap)]),
        (Section::Mentions, &mentions[..]),
        (Section::Unread, &unread[..]),
    ] {
        if !list.is_empty() {
            rows.push(Row::Header(section));
            rows.extend(list.iter().map(|c| Row::Item((*c).clone())));
        }
    }
    rows
}

/// The switcher's state.
#[derive(Default)]
pub struct QuickSwitcher {
    pub open: bool,
    input: Option<Entity<TextInput>>,
    _subscriptions: Vec<Subscription>,
    query: String,
    rows: Rc<Vec<Row>>,
    /// The highlighted entry (an index among the entries, headings not counted).
    selected: usize,
    pub visits: Visits,
    /// The last entry chosen (tests).
    #[cfg(test)]
    pub chosen: Option<Candidate>,
}

impl QuickSwitcher {
    fn items(&self) -> impl Iterator<Item = &Candidate> {
        self.rows.iter().filter_map(|r| match r {
            Row::Item(c) => Some(c),
            Row::Header(_) => None,
        })
    }

    pub fn input(&self) -> Option<&Entity<TextInput>> {
        self.input.as_ref()
    }

    pub fn move_selection(&mut self, by: i32) {
        let n = self.items().count();
        if n == 0 {
            return;
        }
        let i = self.selected as i64 + i64::from(by);
        self.selected = i.rem_euclid(n as i64) as usize;
    }
}

impl Inlet {
    /// Ctrl+K: opens the switcher, or closes it when it is open.
    pub(crate) fn toggle_quick_switcher(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.search.switcher.open {
            self.search.switcher.open = false;
            cx.notify();
            return;
        }
        // Not under the About view, which draws no layers.
        if self.screen != crate::app::Screen::Main || self.account.is_none() || self.show_about {
            return;
        }
        let input = match self.search.switcher.input.clone() {
            Some(input) => input,
            None => {
                let input = cx
                    .new(|cx| TextInput::new(t!("search-switcher-placeholder"), cx).max_chars(100));
                let submit =
                    cx.subscribe_in(&input, window, |this, _, _: &InputEvent, window, cx| {
                        this.choose_selected(window, cx)
                    });
                let changed = cx.observe(&input, |this, input, cx| {
                    let text = input.read(cx).text().to_string();
                    if text != this.search.switcher.query {
                        this.search.switcher.query = text;
                        this.rank_switcher(cx);
                    }
                });
                self.search.switcher._subscriptions = vec![submit, changed];
                self.search.switcher.input = Some(input.clone());
                input
            }
        };
        input.update(cx, |i, cx| i.clear(cx));
        self.search.switcher.query.clear();
        self.search.switcher.open = true;
        self.rank_switcher(cx);
        self.focus_layer_field(&input.focus_handle(cx), window, cx);
        cx.notify();
    }

    /// Works the rows out again from every account's state.
    pub(crate) fn rank_switcher(&mut self, cx: &mut Context<Self>) {
        let mut candidates = Vec::new();
        let visits = &self.search.switcher.visits;
        if let (Some(key), Some(state)) = (self.switcher.active.as_ref(), self.account.as_ref()) {
            candidates.extend(candidates_of(
                key,
                state.read(cx),
                visits,
                self.open_channel_id(),
            ));
        }
        for (key, state) in self.switcher.background_states() {
            candidates.extend(candidates_of(key, state.read(cx), visits, None));
        }
        let rows = rank(&candidates, &self.search.switcher.query);
        self.search.switcher.rows = Rc::new(rows);
        self.search.switcher.selected = 0;
        cx.notify();
    }

    fn choose_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Enter counts only while the switcher is the topmost layer.
        if !self.is_top_layer(crate::desktop::Layer::QuickSwitcher) {
            return;
        }
        let chosen = self
            .search
            .switcher
            .items()
            .nth(self.search.switcher.selected)
            .cloned();
        if let Some(c) = chosen {
            self.choose(c, window, cx);
        }
    }

    /// Opens an entry: on the active account at once; on another account by switching
    /// to it, which then opens the entry as its place.
    fn choose(&mut self, c: Candidate, window: &mut Window, cx: &mut Context<Self>) {
        // The rows are a snapshot: an account signed out since is not switched to.
        if !self.switcher.accounts.iter().any(|a| a.key == c.account) {
            self.rank_switcher(cx);
            return;
        }
        #[cfg(test)]
        {
            self.search.switcher.chosen = Some(c.clone());
        }
        self.search.switcher.open = false;
        let guild = c.guild.map(|g| g.to_string());
        let channel = c.channel.map(|ch| ch.to_string());
        if !self.switcher.is_active(&c.account) {
            self.switcher
                .places
                .leave(c.account.clone(), Place { guild, channel });
            self.switch_account(c.account, window, cx);
            return;
        }
        match channel {
            Some(ch) => {
                if self.selected_channel.as_deref() != Some(ch.as_str()) {
                    self.follow_channel_link(guild, ch, window, cx);
                }
            }
            None => {
                if self.selected_guild != guild {
                    self.select_guild(guild, cx);
                }
            }
        }
        cx.notify();
    }

    /// The name of another account, for the entries that belong to it.
    fn other_account_name(&self, key: &AccountKey) -> Option<String> {
        if self.switcher.is_active(key) {
            return None;
        }
        self.switcher
            .accounts
            .iter()
            .find(|a| &a.key == key)
            .map(|a| clean_name(&a.name))
    }

    /// The switcher window, when it is open.
    pub(crate) fn render_quick_switcher(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.quick_switcher_open() {
            return None;
        }
        let input = self.search.switcher.input.clone()?;
        let rows = self.search.switcher.rows.clone();
        let selected = self.search.switcher.selected;
        let mut list = Vec::with_capacity(rows.len());
        let mut item_ix = 0usize;
        for row in rows.iter() {
            match row {
                Row::Header(section) => list.push(section_header(*section)),
                Row::Item(c) => {
                    list.push(self.switcher_row(item_ix, c, item_ix == selected, cx));
                    item_ix += 1;
                }
            }
        }
        if list.is_empty() {
            list.push(
                div()
                    .py(px(24.))
                    .flex()
                    .justify_center()
                    .text_size(px(14.))
                    .text_color(theme::text_muted())
                    .child(t!("search-switcher-empty"))
                    .into_any_element(),
            );
        }
        let card = div()
            .w(px(SWITCHER_WIDTH))
            .max_w_full()
            .p(px(12.))
            .rounded(px(12.))
            .bg(theme::floating_bg())
            .border_1()
            .border_color(theme::accent_border())
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(input)
            .child(
                div()
                    .id("quick-switcher-list")
                    .h(px(LIST_HEIGHT))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .children(list),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(theme::text_muted())
                    .child(t!("search-switcher-tip")),
            );
        let frame = div()
            .id("quick-switcher")
            .occlude()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .pt(px(120.))
            .px(px(16.))
            .bg(gpui::rgba(0x000000a0))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.search.switcher.open = false;
                    cx.notify();
                }),
            )
            .child(
                // The switcher's own key context sits inside the frame: the frame gets
                // the layer's context (`layer_keys`), and an element has one context.
                div()
                    .key_context(super::SWITCHER_CONTEXT)
                    .on_action(cx.listener(|this, _: &super::SwitcherUp, _, cx| {
                        this.search.switcher.move_selection(-1);
                        cx.notify();
                    }))
                    .on_action(cx.listener(|this, _: &super::SwitcherDown, _, cx| {
                        this.search.switcher.move_selection(1);
                        cx.notify();
                    }))
                    .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(card),
            );
        Some(if self.is_top_layer(crate::desktop::Layer::QuickSwitcher) {
            self.layer_keys(frame, cx).into_any_element()
        } else {
            frame.into_any_element()
        })
    }

    fn switcher_row(
        &self,
        ix: usize,
        c: &Candidate,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let icon = match c.kind {
            Kind::Channel(kind) => channel_icon(kind)
                .el(px(20.), theme::text_muted())
                .into_any_element(),
            Kind::Guild => div()
                .size(px(20.))
                .flex_none()
                .rounded(px(6.))
                .bg(theme::guild_item())
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(9.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme::text_strong())
                .child(initials(&c.title))
                .into_any_element(),
        };
        let mut meta: Vec<String> = Vec::new();
        if let Some(g) = &c.guild_name {
            meta.push(g.clone());
        }
        if let Some(a) = self.other_account_name(&c.account) {
            meta.push(a);
        }
        let chosen = c.clone();
        div()
            .id(SharedString::from(format!("qs-row-{ix}")))
            .flex_none()
            .h(px(ROW_HEIGHT))
            .px(px(8.))
            .rounded(px(4.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.))
            .cursor_pointer()
            .when(selected, |d| d.bg(theme::selected_bg()))
            .when(!selected, |d| d.hover(|s| s.bg(theme::hover_bg())))
            .child(icon)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .text_size(px(14.))
                    .font_weight(if c.unread || c.mentions > 0 {
                        FontWeight::SEMIBOLD
                    } else {
                        FontWeight::MEDIUM
                    })
                    .text_color(if c.unread || c.mentions > 0 || selected {
                        theme::text_strong()
                    } else {
                        theme::text_secondary()
                    })
                    .child(c.title.clone()),
            )
            .when(!meta.is_empty(), |d| {
                d.child(
                    div()
                        .flex_none()
                        .max_w(px(200.))
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .text_size(px(12.))
                        .text_color(theme::text_muted())
                        .child(meta.join(" · ")),
                )
            })
            .when(c.mentions > 0, |d| d.child(mention_badge(c.mentions)))
            .on_click(
                cx.listener(move |this, _, window, cx| this.choose(chosen.clone(), window, cx)),
            )
            .into_any_element()
    }
}

fn section_header(section: Section) -> AnyElement {
    let label = match section {
        Section::Previous => t!("search-switcher-previous"),
        Section::Mentions => t!("search-switcher-mentions"),
        Section::Unread => t!("search-switcher-unread"),
    };
    div()
        .flex_none()
        .px(px(8.))
        .pt(px(8.))
        .pb(px(4.))
        .text_size(px(11.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme::text_muted())
        .child(label.to_uppercase())
        .into_any_element()
}

/// Up to two initials of a server's name.
fn initials(name: &str) -> String {
    name.split_whitespace()
        .filter_map(|w| w.chars().next())
        .take(2)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use inlet_protocol as p;
    use serde_json::{json, Value};

    fn state(me: &str, dms: Value, read_states: Value) -> AccountState {
        let ready: p::Ready = serde_json::from_value(json!({
            "session_id": "s",
            "user": {"id": me, "username": "me"},
            "guilds": [],
            "private_channels": dms,
            "users": [],
            "read_states": read_states,
        }))
        .expect("ready");
        AccountState::from_ready_at(&ready, 1)
    }

    fn dm(id: &str, last: &str, name: &str) -> Value {
        json!({"id": id, "type": 1, "last_message_id": last,
               "recipients": [{"id": format!("2{id}"), "username": name}]})
    }

    fn read(id: &str, last: &str, mentions: u64) -> Value {
        json!({"id": id, "last_message_id": last, "mention_count": mentions})
    }

    fn titles(rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|r| match r {
                Row::Header(s) => format!("section:{s:?}").to_lowercase(),
                Row::Item(c) => format!("{}:{}", c.account.user_id, c.title),
            })
            .collect()
    }

    #[test]
    fn quick_switcher_ranks_recent_and_unread_across_accounts() {
        let a = AccountKey::new("https://one.example", "10").unwrap();
        let b = AccountKey::new("https://two.example", "11").unwrap();
        // The active account: ada read and visited, bo with two mentions, cy open now.
        let active = state(
            "10",
            json!([
                dm("901", "50", "ada"),
                dm("902", "60", "bo"),
                dm("903", "70", "cy")
            ]),
            json!([
                read("901", "50", 0),
                read("902", "40", 2),
                read("903", "70", 0)
            ]),
        );
        // An account in the background (passive): dee visited, eve unread, fay with a
        // mention and the newest message.
        let mut background = state(
            "11",
            json!([
                dm("911", "55", "dee"),
                dm("912", "65", "eve"),
                dm("913", "80", "fay")
            ]),
            json!([
                read("911", "55", 0),
                read("912", "10", 0),
                read("913", "10", 1)
            ]),
        );
        background.set_passive(true);

        let ch = |id: &str| ChannelId::parse(id).unwrap();
        let mut visits = Visits::default();
        visits.note(a.clone(), ch("901"), 100);
        visits.note(b.clone(), ch("911"), 200);
        visits.note(a.clone(), ch("903"), 300);

        let mut all = candidates_of(&a, &active, &visits, Some(ch("903")));
        all.extend(candidates_of(&b, &background, &visits, None));

        // No query: visits newest first (the open channel left out), then mentions
        // (newest first), then unread, over both accounts.
        assert_eq!(
            titles(&rank(&all, "")),
            vec![
                "section:previous",
                "11:dee",
                "10:ada",
                "section:mentions",
                "11:fay",
                "10:bo",
                "section:unread",
                "11:eve",
            ]
        );

        // A query: the best match first; among equal matches the visited one first.
        assert_eq!(titles(&rank(&all, "e")), vec!["11:eve", "11:dee"]);
        assert_eq!(titles(&rank(&all, "y")), vec!["10:cy", "11:fay"]);
        // `@` keeps to direct messages; `*` to servers (there are none here).
        assert_eq!(rank(&all, "@").len(), 6);
        assert!(rank(&all, "*").is_empty());

        // Without visits, the one with a mention comes first among equal matches.
        let none = Visits::default();
        let mut fresh = candidates_of(&a, &active, &none, None);
        fresh.extend(candidates_of(&b, &background, &none, None));
        assert_eq!(titles(&rank(&fresh, "y")), vec!["11:fay", "10:cy"]);
    }

    #[test]
    fn matches_rank_exact_then_prefix_then_word_then_loose() {
        assert_eq!(score("general", "general"), Some(1000));
        assert_eq!(score("general", "gen"), Some(800));
        assert_eq!(score("dev-general", "gen"), Some(600));
        assert_eq!(score("degeneral", "gen"), Some(400));
        assert_eq!(score("general", "gnl"), Some(200));
        assert_eq!(score("general", "xyz"), None);
    }
}
