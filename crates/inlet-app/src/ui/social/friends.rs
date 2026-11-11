// SPDX-License-Identifier: AGPL-3.0-or-later
//! The friends page in the middle while the DM list is up and no DM is open, as
//! Fluxer's `DMFriendsView`: a header with the title and the tabs Online, All, Pending
//! (with the count of incoming requests), Blocked and the Add Friend button; then a
//! search field and the list.
//!
//! Each row: avatar with status, name and a line under it, and round buttons on the
//! right (`FriendListItem`). A friend row opens the DM on a click.
// Deviation: Fluxer keeps the blocked list in its privacy settings; here it is a tab of
// the friends page, so blocks can be undone where they are listed.
// Deviation: Fluxer's friend row opens the profile on a click and has a menu (⋯) for
// remove and block; here the click opens the DM, and remove and block are buttons.

use gpui::{div, prelude::*, px, AnyElement, Context, Entity, FontWeight, SharedString};
use inlet_state::relationships::Kind;
use inlet_state::{AccountState, UserId};

use super::{
    person_name, person_status, person_tag, person_user, round_action, user_tag, SocialIcon,
};
use crate::app::Inlet;
use crate::backend::social::relationships::Cmd;
use crate::text_input::TextInput;
use crate::theme;
use crate::ui::notifications::badge::mention_badge;
use crate::ui::social::presence::{avatar_status, status_label};
use crate::ui::HEADER_HEIGHT;

/// Height of a person row.
pub const ROW_HEIGHT: f32 = 62.;
/// Most rows drawn in one list (the rest are reached through the search).
pub const MAX_ROWS: usize = 500;

/// The tabs of the friends page.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tab {
    #[default]
    Online,
    All,
    Pending,
    Blocked,
}

/// The friends page's own state.
#[derive(Default)]
pub struct FriendsPage {
    pub tab: Tab,
    /// The search field (made the first time the page is drawn).
    pub search: Option<Entity<TextInput>>,
}

/// One row of a list on the friends page.
#[derive(Clone, Debug, PartialEq)]
pub struct PersonRow {
    pub user: UserId,
    pub kind: Kind,
    pub name: String,
    pub tag: String,
    pub status: &'static str,
}

/// Whether `query` (lower case) matches a row's name or tag.
fn matches(row: &PersonRow, query: &str) -> bool {
    query.is_empty()
        || row.name.to_lowercase().contains(query)
        || row.tag.to_lowercase().contains(query)
}

/// The rows of a tab, sorted by name, filtered by `query` (the page draws at most
/// [`MAX_ROWS`] of them).
pub fn rows(state: &AccountState, tab: Tab, query: &str) -> Vec<PersonRow> {
    let query = query.trim().to_lowercase();
    let kinds: &[Kind] = match tab {
        Tab::Online | Tab::All => &[Kind::Friend],
        Tab::Pending => &[Kind::Incoming, Kind::Outgoing],
        Tab::Blocked => &[Kind::Blocked],
    };
    let mut out: Vec<PersonRow> = state
        .relationships()
        .iter()
        .filter(|p| kinds.contains(&p.kind))
        .map(|p| PersonRow {
            user: p.id,
            kind: p.kind,
            name: person_name(state, p.id),
            tag: person_tag(state, p.id),
            status: person_status(state, p.id),
        })
        .filter(|r| tab != Tab::Online || r.status != "offline")
        .filter(|r| matches(r, &query))
        .collect();
    // Incoming requests first on the pending tab, then by name.
    out.sort_by(|a, b| {
        (a.kind != Kind::Incoming)
            .cmp(&(b.kind != Kind::Incoming))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then(a.user.cmp(&b.user))
    });
    out
}

impl Inlet {
    /// The friends page.
    pub(crate) fn render_friends_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let search = self.friends_search(cx);
        let tab = self.social.friends.tab;
        let query = search.read(cx).text().to_owned();
        let (mut list, pending) = match self.account.as_ref().map(|a| a.read(cx)) {
            Some(state) => (
                match tab {
                    Tab::Pending => super::pending::pending_rows(state, &query),
                    _ => rows(state, tab, &query),
                },
                super::pending::incoming(state),
            ),
            None => (Vec::new(), 0),
        };
        // The count is of everyone in the tab, before the drawn rows are cut.
        let total = list.len();
        list.truncate(MAX_ROWS);
        let title = match tab {
            Tab::Online => t!("social-online-count", count = total),
            Tab::All => t!("social-all-count", count = total),
            Tab::Pending => t!("social-pending-count", count = total),
            Tab::Blocked => t!("social-blocked-count", count = total),
        };
        let empty = list.is_empty().then(|| {
            div()
                .pt(px(48.))
                .flex()
                .justify_center()
                .text_color(theme::text_muted())
                .child(match tab {
                    Tab::Online => t!("social-empty-online"),
                    Tab::All => t!("social-empty-all"),
                    Tab::Pending => t!("social-empty-pending"),
                    Tab::Blocked => t!("social-empty-blocked"),
                })
        });
        let rows: Vec<AnyElement> = list.iter().map(|r| self.render_person_row(r, cx)).collect();
        div()
            .debug_selector(|| "friends-page".into())
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(theme::bg_chat())
            .child(self.render_friends_header(tab, pending, cx))
            .children(self.social.notice.clone().map(|n| {
                div()
                    .id("social-notice")
                    .mx_4()
                    .mt_3()
                    .px_3()
                    .py_2()
                    .rounded(px(6.))
                    .bg(theme::danger())
                    .text_color(theme::on_danger())
                    .cursor_pointer()
                    .child(t!("chat-dismissable", text = n))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.social.notice = None;
                        cx.notify();
                    }))
            }))
            .child(div().px_4().pt_3().child(search))
            .child(
                div()
                    .id("friends-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_4()
                    .pt_4()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .pb_4()
                            .text_size(px(12.))
                            .line_height(px(16.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::text_muted())
                            .child(title.to_uppercase()),
                    )
                    .children(rows)
                    .children(empty),
            )
            .into_any_element()
    }

    /// The search field, made once.
    fn friends_search(&mut self, cx: &mut Context<Self>) -> Entity<TextInput> {
        if let Some(s) = &self.social.friends.search {
            return s.clone();
        }
        let input = cx.new(|cx| TextInput::new(t!("social-search"), cx).max_chars(100));
        cx.observe(&input, |_, _, cx| cx.notify()).detach();
        self.social.friends.search = Some(input.clone());
        input
    }

    fn render_friends_header(
        &mut self,
        tab: Tab,
        pending: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tab_button = |id: &'static str, label: String, this_tab: Tab, badge: usize| {
            let active = tab == this_tab;
            div()
                .id(id)
                .flex_none()
                .px(px(12.))
                .py(px(4.))
                .rounded(px(6.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .cursor_pointer()
                .text_size(px(16.))
                .line_height(px(24.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(if active {
                    theme::text_strong()
                } else {
                    theme::text_muted()
                })
                .when(active, |d| d.bg(theme::selected_bg()))
                .hover(|s| s.bg(theme::hover_bg()).text_color(theme::text_strong()))
                .child(label)
                .when(badge > 0, |d| d.child(mention_badge(badge as u32)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.social.friends.tab = this_tab;
                    cx.notify();
                }))
        };
        div()
            .debug_selector(|| "friends-header".into())
            .h(px(HEADER_HEIGHT))
            .flex_none()
            .px_4()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(16.))
            .border_b_1()
            .border_color(theme::divider())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .flex_none()
                    .child(SocialIcon::Friends.el(24., theme::text_muted()))
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::text_strong())
                            .child(t!("social-friends")),
                    ),
            )
            .child(div().flex_none().w(px(1.)).h(px(24.)).bg(theme::divider()))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .overflow_hidden()
                    .child(tab_button(
                        "friends-tab-online",
                        t!("social-tab-online"),
                        Tab::Online,
                        0,
                    ))
                    .child(tab_button(
                        "friends-tab-all",
                        t!("social-tab-all"),
                        Tab::All,
                        0,
                    ))
                    .child(tab_button(
                        "friends-tab-pending",
                        t!("social-tab-pending"),
                        Tab::Pending,
                        pending,
                    ))
                    .child(tab_button(
                        "friends-tab-blocked",
                        t!("social-tab-blocked"),
                        Tab::Blocked,
                        0,
                    ))
                    .child(
                        div()
                            .id("friends-add")
                            .flex_none()
                            .px(px(12.))
                            .py(px(4.))
                            .rounded(px(6.))
                            .cursor_pointer()
                            .text_size(px(16.))
                            .line_height(px(24.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .bg(theme::brand())
                            .text_color(theme::on_brand())
                            .hover(|s| s.opacity(0.9))
                            .child(t!("social-add-friend"))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.open_add_friend(window, cx)),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// One person: avatar with status, name, the line under it, and the buttons.
    pub(crate) fn render_person_row(
        &mut self,
        row: &PersonRow,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let user = row.user;
        let id = user.to_string();
        let user_record = self
            .account
            .as_ref()
            .and_then(|a| person_user(a.read(cx), user));
        let avatar = user_record
            .as_ref()
            .and_then(crate::backend::media::user_avatar);
        let tag = user_record.as_ref().and_then(user_tag);
        let picture = self.user_avatar(avatar.as_ref(), &row.name, 32., false, cx);
        let busy = self.social_busy(&format!("user:{id}"));
        let sub = match row.kind {
            Kind::Friend => status_label(row.status),
            Kind::Incoming => t!("social-incoming-request"),
            Kind::Outgoing => t!("social-outgoing-request"),
            Kind::Blocked => t!("social-blocked"),
        };
        let buttons = self.person_buttons(row, busy, cx);
        let name = row.name.clone();
        div()
            .id(SharedString::from(format!("person-{id}")))
            .debug_selector(|| "person-row".into())
            .group("person-row")
            .flex_none()
            .h(px(ROW_HEIGHT))
            .px(px(10.))
            .mx(px(-10.))
            .rounded(px(8.))
            .border_t_1()
            .border_color(theme::divider())
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.))
            .hover(|s| s.bg(theme::hover_bg()))
            .when(row.kind == Kind::Friend, |d| {
                d.cursor_pointer().on_click(
                    cx.listener(move |this, _, window, cx| this.open_dm_with(user, window, cx)),
                )
            })
            .child(
                div()
                    .relative()
                    .size(px(32.))
                    .flex_none()
                    .child(picture)
                    .when(row.kind != Kind::Blocked, |d| {
                        d.child(avatar_status(row.status, theme::bg_chat()))
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(1.))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.))
                            .min_w_0()
                            .child(
                                div()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme::text_strong())
                                    .child(name),
                            )
                            .children(tag)
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(px(12.))
                                    .text_color(theme::text_tertiary())
                                    .opacity(0.)
                                    .group_hover("person-row", |s| s.opacity(1.))
                                    .child(row.tag.clone()),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme::text_muted())
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(sub),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_row()
                    .gap(px(10.))
                    .children(buttons),
            )
            .into_any_element()
    }

    /// The round buttons of a row; none while its call runs.
    fn person_buttons(
        &mut self,
        row: &PersonRow,
        busy: bool,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        if busy {
            return Vec::new();
        }
        let user = row.user;
        let id = user.to_string();
        let name = row.name.clone();
        let mut out = Vec::new();
        let send = |cmd: fn(String) -> Cmd| {
            let id = id.clone();
            cx.listener(move |this: &mut Inlet, _: &gpui::ClickEvent, _, cx| {
                cx.stop_propagation();
                this.social_action(cmd(id.clone()), cx)
            })
        };
        match row.kind {
            Kind::Friend => {
                out.push(
                    round_action(format!("friend-message-{id}"), SocialIcon::Message, false)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.open_dm_with(user, window, cx)
                        }))
                        .into_any_element(),
                );
                let n = name.clone();
                out.push(
                    round_action(format!("friend-remove-{id}"), SocialIcon::UserMinus, false)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.social.confirm = Some(super::Confirm::RemoveFriend {
                                user,
                                name: n.clone(),
                            });
                            cx.notify();
                        }))
                        .into_any_element(),
                );
                out.push(
                    round_action(format!("friend-block-{id}"), SocialIcon::Block, true)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.social.confirm = Some(super::Confirm::Block {
                                user,
                                name: name.clone(),
                            });
                            cx.notify();
                        }))
                        .into_any_element(),
                );
            }
            Kind::Incoming => {
                out.push(
                    round_action(format!("request-accept-{id}"), SocialIcon::Check, false)
                        .on_click(send(|user_id| Cmd::Accept { user_id }))
                        .into_any_element(),
                );
                out.push(
                    round_action(format!("request-ignore-{id}"), SocialIcon::Close, true)
                        .on_click(send(|user_id| Cmd::Remove { user_id }))
                        .into_any_element(),
                );
            }
            Kind::Outgoing => out.push(
                round_action(format!("request-cancel-{id}"), SocialIcon::Close, true)
                    .on_click(send(|user_id| Cmd::Remove { user_id }))
                    .into_any_element(),
            ),
            Kind::Blocked => out.push(
                round_action(
                    format!("blocked-unblock-{id}"),
                    SocialIcon::UserMinus,
                    false,
                )
                .on_click(send(|user_id| Cmd::Remove { user_id }))
                .into_any_element(),
            ),
        }
        out
    }
}
