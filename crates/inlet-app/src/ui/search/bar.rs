// SPDX-License-Identifier: AGPL-3.0-or-later
//! The search field in the channel header and the search panel it opens.
//!
//! The header shows Fluxer's search field (244 × 36, the magnifying glass on the right,
//! the search typed last). A click opens the search panel, docked on the right under the
//! header (Fluxer's results column is 420 px). Its top holds the field itself; under it
//! are either the search options (the filter words, and suggestions for the one being
//! typed: people, channels, kinds, a day) or the results (`results.rs`).
//!
//! Deviation: Fluxer types into the field in the header and opens its results beside the
//! chat, which narrows. Inlet types in the panel's own field and lays the panel over the
//! right of the chat, so the panel is one layer (`desktop::Layer::Search`) that takes the
//! keyboard: nothing typed reaches the composer and nothing is read behind it.
//!
//! A search covers the open guild, or the open direct message. It starts on Enter only.

use gpui::{
    div, prelude::*, px, AnyElement, Context, Entity, Focusable, FontWeight, SharedString,
    Subscription, Window,
};
use inlet_state::{AccountState, ChannelId, UserId};

use super::filters::{self, channel_id_in, typing_filter, user_id_in, Has, Key, Problem, Scope};
use super::results::Ask;
use crate::app::{Inlet, Screen};
use crate::backend::messages::parse::to_channel;
use crate::desktop::Layer;
use crate::model::{clean_name, ChannelKind};
use crate::text_input::{InputEvent, TextInput};
use crate::theme;
use crate::ui::inbox::{card_button, InboxIcon};
use crate::ui::HEADER_HEIGHT;

/// Fluxer's search field in the header.
pub const FIELD_WIDTH: f32 = 244.;
pub const FIELD_HEIGHT: f32 = 36.;
/// Fluxer's results column.
pub const PANEL_WIDTH: f32 = 420.;
/// Suggestions listed for the filter being typed.
const SUGGESTIONS: usize = 8;

const MAGNIFIER: &[u8] = include_bytes!("../../../assets/icons/magnifying-glass.svg");

/// The search field's state.
#[derive(Default)]
pub struct Bar {
    /// The search panel is open.
    pub open: bool,
    input: Option<Entity<TextInput>>,
    _subscriptions: Vec<Subscription>,
    /// The field's text as last seen, to tell typing from other redraws.
    text: String,
    /// Show the search options rather than the results (something was typed since).
    pub editing: bool,
    /// Filters of the last Enter that could not be used.
    pub problems: Vec<Problem>,
    /// The field still holds the text of an account left; it empties when it opens.
    stale: bool,
}

impl Bar {
    pub fn input(&self) -> Option<&Entity<TextInput>> {
        self.input.as_ref()
    }

    /// Another account, or signed out: the panel closes and the field empties.
    pub fn clear(&mut self) {
        self.open = false;
        self.editing = false;
        self.problems.clear();
        self.stale = self.input.is_some();
    }
}

/// A user of the search's scope by mention, ID or name (username, display name or the
/// nickname in the guild), from what the account state holds.
fn find_user(state: &AccountState, scope: Scope, value: &str) -> Option<UserId> {
    if let Some(id) = user_id_in(value) {
        return Some(id);
    }
    let wanted = value.trim_start_matches('@').to_lowercase();
    people(state, scope)
        .into_iter()
        .find(|(_, names)| names.iter().any(|n| n.to_lowercase() == wanted))
        .map(|(id, _)| id)
}

/// The people a search of `scope` can name: the guild's cached members and the account
/// itself, or the direct message's recipients. Each with every name it goes by, the
/// username first.
fn people(state: &AccountState, scope: Scope) -> Vec<(UserId, Vec<String>)> {
    let names = |u: &inlet_protocol::models::User, nick: Option<&str>| {
        let mut v = vec![u.username.clone()];
        v.extend(u.global_name.clone());
        v.extend(nick.map(str::to_string));
        v
    };
    let mut out: Vec<(UserId, Vec<String>)> = Vec::new();
    let me = &state.me().user;
    if let Some(id) = UserId::parse(&me.id) {
        out.push((id, names(me, None)));
    }
    match scope {
        Scope::Guild(g) => {
            if let Some(guild) = state.guild(g) {
                for (id, m) in guild.members.others() {
                    if let Some(u) = &m.user {
                        out.push((id, names(u, m.nick.as_deref())));
                    }
                }
            }
        }
        Scope::Channel(ch) => {
            if let Some(c) = state.channel(ch) {
                for u in &c.recipients {
                    if let Some(id) = UserId::parse(&u.id) {
                        out.push((id, names(u, None)));
                    }
                }
            }
        }
    }
    out
}

/// The text channels of the guild a search may narrow to, by name.
fn guild_channels(state: &AccountState, scope: Scope) -> Vec<(ChannelId, String)> {
    let Scope::Guild(g) = scope else {
        return Vec::new();
    };
    state
        .visible_channels(g)
        .into_iter()
        .filter(|c| !c.is_thread())
        .filter_map(|c| {
            let ch = ChannelId::parse(&c.id)?;
            let channel = to_channel(c);
            matches!(
                channel.kind,
                ChannelKind::Text | ChannelKind::Announcement | ChannelKind::Forum
            )
            .then_some((ch, channel.name))
        })
        .collect()
}

fn find_channel(state: &AccountState, scope: Scope, value: &str) -> Option<ChannelId> {
    let list = guild_channels(state, scope);
    if let Some(id) = channel_id_in(value) {
        return list.iter().any(|(ch, _)| *ch == id).then_some(id);
    }
    let wanted = value.trim_start_matches('#').to_lowercase();
    list.into_iter()
        .find(|(_, name)| name.to_lowercase() == wanted)
        .map(|(ch, _)| ch)
}

/// A value as typed after a filter word: quoted when it has spaces.
fn typed_value(value: &str) -> String {
    if value.contains(char::is_whitespace) {
        format!("\"{value}\"")
    } else {
        value.to_string()
    }
}

/// The search field in the channel header (see `ui/chat/header_buttons.rs`).
pub fn header_button(this: &Inlet, cx: &mut Context<Inlet>) -> Option<AnyElement> {
    this.account.as_ref()?;
    this.search_scope()?;
    let typed = this
        .search
        .results
        .shown
        .as_ref()
        .and(this.search.bar.input())
        .map(|i| i.read(cx).text().to_string())
        .filter(|t| !t.trim().is_empty());
    let open = this.search_panel_open();
    let clear = typed.is_some().then(|| {
        div()
            .id("header-search-clear")
            .flex_none()
            .cursor_pointer()
            .child(InboxIcon::Close.el(16., theme::text_muted()))
            .on_click(cx.listener(|this, _, _, cx| {
                cx.stop_propagation();
                this.clear_search(cx);
            }))
    });
    let has_query = typed.is_some();
    Some(
        div()
            .id("header-search")
            .flex_none()
            .ml(px(8.))
            .w(px(FIELD_WIDTH))
            .h(px(FIELD_HEIGHT))
            .px(px(8.))
            .rounded(px(6.))
            .bg(theme::bg_app())
            .when(open, |d| d.border_1().border_color(theme::focus_ring()))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.))
            .cursor_pointer()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .text_size(px(14.))
                    .text_color(if has_query {
                        theme::text()
                    } else {
                        theme::text_muted()
                    })
                    .child(typed.unwrap_or_else(|| t!("search-placeholder"))),
            )
            .children(clear)
            .when(!has_query, |d| {
                d.child(
                    gpui::svg()
                        .data(MAGNIFIER)
                        .size(px(16.))
                        .flex_none()
                        .text_color(theme::text_muted()),
                )
            })
            .on_click(cx.listener(|this, _, window, cx| this.open_search(window, cx)))
            .into_any_element(),
    )
}

impl Inlet {
    /// Where a search looks now: the open guild, or the open direct message.
    pub(crate) fn search_scope(&self) -> Option<Scope> {
        match self.selected_guild.as_deref() {
            Some(g) => inlet_state::GuildId::parse(g).map(Scope::Guild),
            None => self.open_channel_id().map(Scope::Channel),
        }
    }

    /// The search field, made on first use.
    fn search_input(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<TextInput> {
        if let Some(input) = self.search.bar.input.clone() {
            return input;
        }
        let input = cx.new(|cx| TextInput::new(t!("search-placeholder"), cx).max_chars(1_500));
        let submit = cx.subscribe_in(&input, window, |this, _, _: &InputEvent, _, cx| {
            this.submit_search(cx)
        });
        let changed = cx.observe(&input, |this, input, cx| {
            let text = input.read(cx).text().to_string();
            if text != this.search.bar.text {
                this.search.bar.text = text;
                this.search.bar.editing = true;
                this.search.bar.problems.clear();
                cx.notify();
            }
        });
        self.search.bar._subscriptions = vec![submit, changed];
        self.search.bar.input = Some(input.clone());
        input
    }

    /// Opens the search panel with the keyboard in its field.
    pub(crate) fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.screen != Screen::Main || self.search_scope().is_none() {
            return;
        }
        let input = self.search_input(window, cx);
        if std::mem::take(&mut self.search.bar.stale) {
            input.update(cx, |i, cx| i.clear(cx));
        }
        // The inbox or pins panel would lie over it; one panel at a time.
        self.inbox.open = None;
        self.search.bar.open = true;
        self.search.bar.editing = self.search.results.shown.is_none();
        self.focus_layer_field(&input.focus_handle(cx), window, cx);
        cx.notify();
    }

    /// Clears the search (the cross in the header field) and closes the panel.
    pub(crate) fn clear_search(&mut self, cx: &mut Context<Self>) {
        self.search.bar.clear();
        self.search.results.clear();
        cx.notify();
    }

    /// Puts `text` into the field as if it had been typed (the demo scene).
    #[cfg(feature = "demo")]
    pub(crate) fn type_search(&mut self, text: &str, cx: &mut Context<Self>) {
        if let Some(input) = self.search.bar.input.clone() {
            input.update(cx, |i, cx| i.set_text(text.to_string(), cx));
            self.search.bar.text = text.to_string();
        }
    }

    /// Enter in the field: the typed filters are resolved and the search starts.
    pub(crate) fn submit_search(&mut self, cx: &mut Context<Self>) {
        let (Some(input), Some(scope), Some(account), Some(key)) = (
            self.search.bar.input.clone(),
            self.search_scope(),
            self.account.clone(),
            self.switcher.active.clone(),
        ) else {
            return;
        };
        let parsed = filters::parse(input.read(cx).text());
        let resolved = {
            let state = account.read(cx);
            filters::resolve(
                &parsed,
                |v| find_user(state, scope, v),
                |v| find_channel(state, scope, v),
            )
        };
        match resolved {
            Err(problems) => {
                self.search.bar.problems = problems;
                self.search.bar.editing = true;
            }
            Ok(f) if f.is_empty() => {}
            Ok(f) => {
                self.search.bar.problems.clear();
                self.search.bar.editing = false;
                let sort = self
                    .search
                    .results
                    .shown
                    .as_ref()
                    .map(|s| s.sort)
                    .unwrap_or_default();
                self.ask_search(
                    Ask {
                        account: key,
                        scope,
                        filters: f,
                        sort,
                        page: 1,
                    },
                    cx,
                );
            }
        }
        cx.notify();
    }

    /// Puts a suggestion into the field: in place of the filter being typed (`from`),
    /// or after what is there.
    fn put_search_text(
        &mut self,
        from: Option<usize>,
        insert: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(input) = self.search.bar.input.clone() else {
            return;
        };
        let current = input.read(cx).text().to_string();
        let mut base = match from {
            Some(i) if current.is_char_boundary(i) => current[..i].to_string(),
            _ => current,
        };
        if from.is_none() && !base.is_empty() && !base.ends_with(' ') {
            base.push(' ');
        }
        base.push_str(&insert);
        input.update(cx, |i, cx| i.set_text(base, cx));
        input.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    /// The search panel, docked under the header on the right, when it is open.
    pub(crate) fn render_search_panel(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.search_panel_open() {
            return None;
        }
        let input = self.search.bar.input.clone()?;
        let body = if self.search.bar.editing || self.search.results.shown.is_none() {
            self.render_search_options(cx)
        } else {
            self.render_search_results(cx)
        };
        let close = card_button("search-close".into(), InboxIcon::Close).on_click(cx.listener(
            |this, _, _, cx| {
                this.search.bar.open = false;
                cx.notify();
            },
        ));
        let frame = div()
            .id("search-panel")
            .occlude()
            .absolute()
            .top(px(HEADER_HEIGHT))
            .right_0()
            .bottom_0()
            .w(px(PANEL_WIDTH))
            .max_w_full()
            .flex()
            .flex_col()
            .bg(theme::bg_app())
            .border_l_1()
            .border_color(theme::divider())
            .child(
                div()
                    .flex_none()
                    .p(px(12.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .child(div().flex_1().min_w_0().child(input))
                    .child(close),
            )
            .child(body);
        Some(if self.is_top_layer(Layer::Search) {
            self.layer_keys(frame, cx).into_any_element()
        } else {
            frame.into_any_element()
        })
    }

    /// The search options: what went wrong with the last Enter, then suggestions for
    /// the filter being typed, or the list of filter words.
    fn render_search_options(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let text = self
            .search
            .bar
            .input
            .as_ref()
            .map(|i| i.read(cx).text().to_string())
            .unwrap_or_default();
        let mut rows: Vec<AnyElement> = self
            .search
            .bar
            .problems
            .iter()
            .map(|p| {
                let line = match p {
                    Problem::UnknownUser(v) => t!("search-problem-user", name = v.as_str()),
                    Problem::UnknownChannel(v) => {
                        t!("search-problem-channel", name = v.as_str())
                    }
                    Problem::UnknownHas(v) => t!("search-problem-has", name = v.as_str()),
                    Problem::BadDate(v) => t!("search-problem-date", name = v.as_str()),
                };
                div()
                    .px(px(8.))
                    .py(px(4.))
                    .text_size(px(13.))
                    .text_color(theme::danger())
                    .child(line)
                    .into_any_element()
            })
            .collect();
        match typing_filter(&text) {
            Some((key, partial, start)) => {
                let partial = partial.to_lowercase();
                let picks = self.suggestions(key, &partial, cx);
                rows.push(options_heading(t!("search-suggestions")));
                if picks.is_empty() {
                    rows.push(option_note(t!("search-no-suggestions")));
                }
                for (ix, (label, detail, value)) in picks.into_iter().enumerate() {
                    let insert = format!("{}:{} ", key.word(), typed_value(&value));
                    rows.push(
                        option_row(
                            SharedString::from(format!("search-pick-{ix}")),
                            label,
                            detail,
                        )
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.put_search_text(Some(start), insert.clone(), window, cx)
                        }))
                        .into_any_element(),
                    );
                }
            }
            None => {
                rows.push(options_heading(t!("search-options")));
                for key in Key::ALL {
                    let insert = format!("{}:", key.word());
                    rows.push(
                        option_row(
                            SharedString::from(format!("search-option-{}", key.word())),
                            insert.clone(),
                            option_hint(key),
                        )
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.put_search_text(None, insert.clone(), window, cx)
                        }))
                        .into_any_element(),
                    );
                }
            }
        }
        div()
            .id("search-options")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px(px(8.))
            .pb(px(12.))
            .flex()
            .flex_col()
            .children(rows)
            .into_any_element()
    }

    /// Suggestions for the filter `key` with `partial` typed: (label, detail, value).
    fn suggestions(
        &self,
        key: Key,
        partial: &str,
        cx: &mut Context<Self>,
    ) -> Vec<(String, Option<String>, String)> {
        let (Some(scope), Some(account)) = (self.search_scope(), self.account.as_ref()) else {
            return Vec::new();
        };
        let state = account.read(cx);
        let mut out = Vec::new();
        match key {
            Key::From | Key::Mentions => {
                for (_, names) in people(state, scope) {
                    let hit = names.iter().any(|n| n.to_lowercase().contains(partial));
                    if hit {
                        let username = names[0].clone();
                        let shown = clean_name(names.get(1).unwrap_or(&names[0]));
                        out.push((shown, Some(clean_name(&username)), username));
                    }
                }
            }
            Key::In => {
                for (_, name) in guild_channels(state, scope) {
                    if name.to_lowercase().contains(partial) {
                        out.push((format!("#{name}"), None, name));
                    }
                }
            }
            Key::Has => {
                for h in Has::ALL {
                    if h.word().starts_with(partial) {
                        out.push((h.word().to_string(), None, h.word().to_string()));
                    }
                }
            }
            Key::Before | Key::After => {
                let today = chrono::Local::now().date_naive().to_string();
                out.push((today.clone(), Some(t!("search-date-hint")), today));
            }
            Key::Pinned => {
                for v in ["true", "false"] {
                    if v.starts_with(partial) {
                        out.push((v.to_string(), None, v.to_string()));
                    }
                }
            }
        }
        out.truncate(SUGGESTIONS);
        out
    }
}

fn option_hint(key: Key) -> Option<String> {
    Some(match key {
        Key::From => t!("search-option-from"),
        Key::Mentions => t!("search-option-mentions"),
        Key::In => t!("search-option-in"),
        Key::Has => t!("search-option-has"),
        Key::Before => t!("search-option-before"),
        Key::After => t!("search-option-after"),
        Key::Pinned => t!("search-option-pinned"),
    })
}

fn options_heading(label: String) -> AnyElement {
    div()
        .px(px(8.))
        .pt(px(8.))
        .pb(px(4.))
        .text_size(px(12.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme::text_muted())
        .child(label)
        .into_any_element()
}

fn option_note(label: String) -> AnyElement {
    div()
        .px(px(8.))
        .py(px(6.))
        .text_size(px(13.))
        .text_color(theme::text_muted())
        .child(label)
        .into_any_element()
}

fn option_row(
    id: SharedString,
    label: String,
    detail: Option<String>,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_none()
        .min_h(px(32.))
        .px(px(8.))
        .rounded(px(4.))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.))
        .cursor_pointer()
        .hover(|s| s.bg(theme::hover_bg()))
        .child(
            div()
                .flex_none()
                .text_size(px(14.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme::text_strong())
                .child(label),
        )
        .children(detail.map(|d| {
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .text_size(px(13.))
                .text_color(theme::text_muted())
                .child(d)
        }))
}
