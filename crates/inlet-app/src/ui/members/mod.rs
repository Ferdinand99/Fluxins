// SPDX-License-Identifier: AGPL-3.0-or-later
//! The member list beside the chat (Fluxer's `ChannelMembers`): 264 px wide, the
//! members of the open channel in groups (hoisted roles, then online, then offline,
//! `{name}—{count}`), offline members at 30 %, a crown for the guild's owner, and a
//! member search on top.
//!
//! The list shows in guild text and announcement channels while the header's members
//! button has not hidden it. As in Fluxer it gives way to the thread panel (the panel
//! takes the right side while it is open) and has no place in forums and voice
//! channels. Only the active account has one: an account switch drops it, and the
//! backend unsubscribes the account that went to the background.
//!
//! What is asked of the server: the rows on screen ([`list::window`]), sent when that
//! changes and coalesced by the backend (`backend::social::members`). It is asked
//! only when the account may see the channel's members (`VIEW_CHANNEL` and
//! `VIEW_CHANNEL_MEMBERS`); until the roles are in, nothing is (fails closed).

mod group_header;
pub mod list;
mod row;

pub use row::status_badge;
mod search;

use std::ops::Range;

use gpui::{
    div, list as list_view, prelude::*, px, AnyElement, App, Context, ListAlignment,
    ListScrollEvent, ListState, Window,
};
use inlet_protocol as p;
use inlet_state::{ChannelId, GuildId};

use crate::app::{Inlet, Screen};
use crate::backend::social::members::Cmd;
use crate::model::ChannelKind;
use crate::theme;
use list::MemberList;

/// Width of the member list (`--layout-member-list-width`).
pub const WIDTH: f32 = 264.;
/// Height of a member row, the 2 px under it included (`MemberListMetrics`).
pub const ROW_HEIGHT: f32 = 44.;
/// Height of a group header (12 px above the text, 4 px below).
pub const HEADER_HEIGHT: f32 = 32.;

/// Icons of this domain (Phosphor, fill).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemberIcon {
    /// The header button (`users`).
    Users,
    /// The guild's owner (`crown`).
    Crown,
    /// The search field (`magnifying-glass`).
    Search,
}

impl MemberIcon {
    fn data(self) -> &'static [u8] {
        match self {
            MemberIcon::Users => include_bytes!("../../../assets/icons/users.svg"),
            MemberIcon::Crown => include_bytes!("../../../assets/icons/crown.svg"),
            MemberIcon::Search => include_bytes!("../../../assets/icons/magnifying-glass.svg"),
        }
    }

    pub fn el(self, size: gpui::Pixels, color: gpui::Hsla) -> gpui::Svg {
        gpui::svg()
            .data(self.data())
            .size(size)
            .flex_none()
            .text_color(color)
    }
}

/// What the UI last asked the backend for: the channel's list and its ranges, or no
/// member list (`None`).
type Wish = Option<(String, String, Vec<[u32; 2]>)>;

/// The member list's view state.
pub struct State {
    /// Hidden with the header's members button (Fluxer shows it by default).
    pub hidden: bool,
    /// The list of the channel on screen.
    pub list: Option<MemberList>,
    pub list_state: ListState,
    /// Rows on screen, from the list's last scroll.
    visible: Range<usize>,
    scroll_hooked: bool,
    /// What was asked last (`None`: nothing yet, ask now).
    sent: Option<Wish>,
    pub search: search::Search,
}

impl Default for State {
    fn default() -> Self {
        State {
            hidden: false,
            list: None,
            list_state: ListState::new(0, ListAlignment::Top, px(ROW_HEIGHT * 10.)),
            visible: 0..0,
            scroll_hooked: false,
            sent: None,
            search: Default::default(),
        }
    }
}

impl State {
    /// Sign-out, an account switch or a backend restart: the list was the old
    /// account's. The next frame asks again for the new one.
    pub fn reset(&mut self) {
        self.list = None;
        self.visible = 0..0;
        self.sent = None;
        self.search.clear();
        let n = self.list_state.item_count();
        self.list_state.splice(0..n, 0);
    }
}

/// The members button in the channel header: shows and hides the list.
// Deviation: Fluxer remembers the choice per channel (and hides the list in voice
// channels by default); here one choice holds for every channel until Inlet restarts.
pub fn header_button(this: &Inlet, cx: &mut Context<Inlet>) -> Option<AnyElement> {
    this.member_list_channel()?;
    let shown = !this.members.hidden && this.threads.panel.is_none();
    let color = if shown {
        theme::text_strong()
    } else {
        theme::text_muted()
    };
    Some(
        div()
            .id("members-button")
            .flex_none()
            .p(px(6.))
            .rounded(px(6.))
            .cursor_pointer()
            .when(shown, |d| d.bg(theme::selected_bg()))
            .hover(|s| s.bg(theme::hover_bg()))
            .child(MemberIcon::Users.el(px(24.), color))
            .on_click(cx.listener(|this, _, _, cx| {
                this.members.hidden = !this.members.hidden;
                cx.notify();
            }))
            .into_any_element(),
    )
}

impl Inlet {
    /// The guild and channel whose member list may show: a guild text or announcement
    /// channel on the main screen.
    pub(crate) fn member_list_channel(&self) -> Option<(GuildId, ChannelId)> {
        if self.screen != Screen::Main || self.account.is_none() {
            return None;
        }
        let guild = GuildId::parse(self.selected_guild.as_deref()?)?;
        let id = self.selected_channel.as_deref()?;
        let channel = self.channels.iter().find(|c| c.id == id)?;
        if !matches!(channel.kind, ChannelKind::Text | ChannelKind::Announcement) {
            return None;
        }
        Some((guild, ChannelId::parse(id)?))
    }

    /// The member list on screen, if one shows: not hidden, and not giving way to the
    /// thread panel.
    pub(crate) fn member_list_shown(&self) -> Option<(GuildId, ChannelId)> {
        if self.members.hidden || self.threads.panel.is_some() {
            return None;
        }
        self.member_list_channel()
    }

    /// Whether the account may see the members of a channel. Fails closed: nothing
    /// until the account state has the roles.
    fn may_list_members(&self, g: GuildId, ch: ChannelId, cx: &App) -> bool {
        let Some(account) = &self.account else {
            return false;
        };
        account
            .read(cx)
            .permissions(g, ch)
            .has(p::permissions::VIEW_CHANNEL | p::permissions::VIEW_CHANNEL_MEMBERS)
    }

    /// Once per frame: the list follows the channel on screen, and the backend is
    /// told when the rows to ask for change.
    pub(crate) fn members_frame(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let shown = self.member_list_shown();
        let same = match (&self.members.list, shown) {
            (Some(l), Some((g, ch))) => l.guild == g && l.channel == ch,
            (None, None) => true,
            _ => false,
        };
        if !same {
            self.members.list = shown.map(|(g, ch)| MemberList::new(g, ch));
            self.members.visible = 0..0;
            self.members.search.clear();
            let n = self.members.list_state.item_count();
            self.members.list_state.splice(0..n, 0);
        }
        let wish: Wish = match shown {
            Some((g, ch)) if self.may_list_members(g, ch, cx) => {
                let list = self.members.list.as_ref();
                let total = list.filter(|l| l.loaded).map(MemberList::len);
                let visible = &self.members.visible;
                let ranges = list::window(
                    visible.start as u32,
                    visible.end.saturating_sub(1) as u32,
                    total,
                );
                Some((g.to_string(), ch.to_string(), ranges))
            }
            _ => None,
        };
        if self.members.sent.as_ref() != Some(&wish) {
            match &wish {
                Some((guild_id, channel_id, ranges)) => {
                    if let Some(l) = self.members.list.as_mut() {
                        l.set_ranges(ranges);
                    }
                    self.backend.send(Cmd::Window {
                        guild_id: guild_id.clone(),
                        channel_id: channel_id.clone(),
                        ranges: ranges.clone(),
                    });
                }
                None => self.backend.send(Cmd::Close),
            }
            self.members.sent = Some(wish);
        }
        self.sync_member_rows();
        self.search_frame(cx);
    }

    /// Keeps the list view's row count equal to the list's.
    fn sync_member_rows(&mut self) {
        let n = match self.members.list.as_ref() {
            Some(l) if !l.is_empty() => l.len() as usize,
            _ => 0,
        };
        let old = self.members.list_state.item_count();
        if n > old {
            self.members.list_state.splice(old..old, n - old);
        } else if n < old {
            self.members.list_state.splice(n..old, 0);
        }
    }

    /// Gateway events for the member list: its updates, the answers to member
    /// searches, and a new READY (everything is asked again).
    pub(crate) fn members_on_dispatch(&mut self, dispatch: &p::Dispatch, cx: &mut Context<Self>) {
        match dispatch {
            p::Dispatch::Ready(_) => {
                self.members.sent = None;
                if let Some(l) = self.members.list.as_mut() {
                    *l = MemberList::new(l.guild, l.channel);
                }
                cx.notify();
            }
            p::Dispatch::GuildMemberListUpdate(u)
                if self.members.list.as_mut().is_some_and(|l| l.apply(u)) =>
            {
                self.sync_member_rows();
                cx.notify();
            }
            p::Dispatch::GuildMembersChunk(chunk) if self.members.search.on_chunk(chunk) => {
                cx.notify();
            }
            _ => {}
        }
    }

    fn hook_member_scroll(&mut self, cx: &mut Context<Self>) {
        if self.members.scroll_hooked {
            return;
        }
        self.members.scroll_hooked = true;
        let weak = cx.entity().downgrade();
        self.members.list_state.set_scroll_handler(
            move |event: &ListScrollEvent, _window: &mut Window, cx: &mut App| {
                let range = event.visible_range.clone();
                // Inside the list's borrow: a plain field of the view only.
                let _ = weak.update(cx, |this, cx| {
                    if this.members.visible != range {
                        this.members.visible = range;
                        cx.notify();
                    }
                });
            },
        );
    }

    /// The member list column, when one shows.
    pub(crate) fn render_member_list(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (g, _) = self.member_list_shown()?;
        self.hook_member_scroll(cx);
        let body = if self.members.search.active() {
            self.render_member_search_results(g, cx)
        } else {
            list_view(
                self.members.list_state.clone(),
                cx.processor(|this, ix: usize, _window, cx| this.render_member_item(ix, cx)),
            )
            .flex_1()
            .w_full()
            .into_any_element()
        };
        Some(
            div()
                .id("member-list")
                .debug_selector(|| "member-list".into())
                .w(px(WIDTH))
                .h_full()
                .flex_none()
                .flex()
                .flex_col()
                .bg(theme::bg_chat())
                .child(self.render_member_search(cx))
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .px(px(8.))
                        .flex()
                        .flex_col()
                        .child(body),
                )
                .into_any_element(),
        )
    }

    /// One row of the list: a group header, a member, or a placeholder for a row
    /// that is not loaded yet.
    fn render_member_item(&mut self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(list) = self.members.list.as_ref() else {
            return div().into_any_element();
        };
        let guild = list.guild;
        let header = list.is_header(ix as u32);
        match list.row(ix as u32).cloned() {
            Some(list::Row::Group { id, count }) => {
                group_header::render(&self.group_label(guild, &id, cx), count)
            }
            Some(list::Row::Member(m)) => self.render_member_row(&format!("m{ix}"), guild, &m, cx),
            None if header => div().h(px(HEADER_HEIGHT)).into_any_element(),
            None => row::placeholder(ix),
        }
    }
}
