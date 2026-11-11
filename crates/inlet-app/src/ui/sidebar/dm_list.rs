// SPDX-License-Identifier: AGPL-3.0-or-later
//! The list of direct messages, shown when no guild is selected, as Fluxer's
//! `DirectMessageList`: the quick switcher bar in the header, the Friends and Personal
//! notes rows, the "Direct messages" heading with its + (new DM or group DM), then the
//! DMs, pinned ones first, the rest newest activity first (`AccountState::dm_list`).
//!
//! A DM row (42 px): the other person's avatar with their status dot, or the group's
//! icon; the name with a BOT or SYSTEM tag; the mention count, and a close button on
//! hover (a DM closes, a group asks before it is left). Right click opens the DM menu:
//! pin or unpin, the group's settings, close or leave.
// Deviation: Fluxer's list is virtualised; Inlet draws the first `MAX_DM_ROWS` rows.

use gpui::{
    anchored, deferred, div, point, prelude::*, px, AnyElement, Context, FontWeight, Hsla,
    MouseButton, MouseDownEvent, SharedString,
};
use inlet_protocol::models::channel_type;
use inlet_state::{AccountState, ChannelId, UserId};

use crate::app::Inlet;
use crate::backend::social::relationships::Cmd;
use crate::model::{Channel, ChannelKind};
use crate::theme;
use crate::ui::notifications::badge::{mention_badge, RowMarks};
use crate::ui::search::sidebar_actions::OpenQuickSwitcher;
use crate::ui::social::group_dm::{group_name, Mode};
use crate::ui::social::presence::avatar_status;
use crate::ui::social::{person_status, user_tag, Confirm, DmMenu, SocialIcon};
use crate::ui::HEADER_HEIGHT;

/// Height of a DM row, and of the Friends and Personal notes rows.
pub const DM_ROW_HEIGHT: f32 = 42.;
/// Size of a DM row's avatar or icon.
pub const DM_AVATAR: f32 = 32.;
/// Most DM rows drawn.
pub const MAX_DM_ROWS: usize = 300;

/// What a DM row shows, worked out from the account state.
#[derive(Clone, Debug, PartialEq)]
pub struct DmRow {
    pub id: String,
    pub name: String,
    pub group: bool,
    /// The person in a one-to-one DM.
    pub user: Option<inlet_protocol::User>,
    /// Their shown status (one-to-one DMs only).
    pub status: Option<&'static str>,
    /// The group's icon hash.
    pub icon: Option<String>,
    pub pinned: bool,
}

/// The DM rows in Fluxer's order. `shown` are the channels the sidebar lists (their
/// names are already cleaned); a DM the state does not know yet goes last.
pub fn dm_rows_of(state: &AccountState, shown: &[Channel]) -> Vec<DmRow> {
    let pinned = state.relationships().pinned_dms();
    let mut out: Vec<DmRow> = state
        .dm_list()
        .into_iter()
        .take(MAX_DM_ROWS)
        .filter_map(|c| {
            let id = ChannelId::parse(&c.id)?;
            let group = c.kind == channel_type::GROUP_DM;
            let user = (!group).then(|| c.recipients.first().cloned()).flatten();
            let status = user
                .as_ref()
                .and_then(|u| UserId::parse(&u.id))
                .map(|u| person_status(state, u));
            let name = match (&user, group) {
                (_, true) => group_name(state, id),
                (Some(u), false) => UserId::parse(&u.id)
                    .map(|uid| crate::ui::social::person_name(state, uid))
                    .unwrap_or_else(|| crate::model::clean_name(u.display_name())),
                (None, false) => shown
                    .iter()
                    .find(|s| s.id == c.id)
                    .map(|s| s.name.clone())
                    .unwrap_or_default(),
            };
            Some(DmRow {
                id: c.id.clone(),
                name,
                group,
                user,
                status,
                icon: c
                    .extra
                    .get("icon")
                    .and_then(|v| v.as_str())
                    .filter(|h| !h.is_empty() && h.len() <= 128)
                    .map(str::to_owned),
                pinned: pinned.contains(&id),
            })
        })
        .collect();
    let notes = state.me_id().to_string();
    for c in shown {
        if out.len() >= MAX_DM_ROWS {
            break;
        }
        if c.id != notes
            && matches!(c.kind, ChannelKind::Dm | ChannelKind::GroupDm)
            && !out.iter().any(|r| r.id == c.id)
        {
            out.push(DmRow {
                id: c.id.clone(),
                name: c.name.clone(),
                group: c.kind == ChannelKind::GroupDm,
                user: None,
                status: None,
                icon: None,
                pinned: false,
            });
        }
    }
    out
}

/// The shortcut of the quick switcher, as the platform writes it.
fn switcher_keys() -> [String; 2] {
    let modifier = if cfg!(target_os = "macos") {
        t!("sidebar-key-command")
    } else {
        t!("sidebar-key-ctrl")
    };
    [modifier, t!("sidebar-key-k")]
}

impl Inlet {
    /// The header of the DM list: the quick switcher bar.
    pub(super) fn render_dm_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let kbd = |k: String| {
            div()
                .px(px(4.))
                .min_w(px(16.))
                .h(px(16.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(3.))
                .bg(theme::bg_app())
                .text_size(px(10.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme::text_muted())
                .child(k)
        };
        div()
            .debug_selector(|| "guild-header".into())
            .h(px(HEADER_HEIGHT))
            .flex_none()
            .px_2()
            .flex()
            .items_center()
            .border_b_1()
            .border_color(theme::divider())
            .child(
                div()
                    .id("quick-switcher")
                    .debug_selector(|| "quick-switcher".into())
                    .w_full()
                    .h(px(28.))
                    .px_3()
                    .rounded(px(6.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .cursor_pointer()
                    .bg(theme::guild_item())
                    .text_size(px(13.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme::text_muted())
                    .hover(|s| s.text_color(theme::text_strong()))
                    .child(t!("sidebar-quick-switcher"))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(2.))
                            .children(switcher_keys().map(kbd)),
                    )
                    .on_click(cx.listener(|_, _, window, cx| {
                        window.dispatch_action(Box::new(OpenQuickSwitcher), cx)
                    })),
            )
            .into_any_element()
    }

    /// The rows of the DM list.
    pub(super) fn dm_rows(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let cached = self.social.dm_rows.clone();
        let (rows, pending, notes_id) = match self.account.as_ref().map(|a| a.read(cx)) {
            Some(state) => (
                cached.unwrap_or_else(|| dm_rows_of(state, &self.channels)),
                state.relationships().incoming_count(),
                Some(state.me_id().to_string()),
            ),
            None => (Vec::new(), 0, None),
        };
        // Sorted and worked out again only when something they show changes.
        self.social.dm_rows = Some(rows.clone());
        let mut out = Vec::with_capacity(rows.len() + 3);
        let friends_selected = self.friends_page_shown();
        let open_friends =
            cx.listener(|this: &mut Inlet, _: &gpui::ClickEvent, _, cx| this.show_friends(cx));
        out.push(nav_row(
            "dm-friends",
            SocialIcon::Friends,
            t!("social-friends"),
            friends_selected,
            pending,
            open_friends,
        ));
        let notes_selected = notes_id.is_some() && self.selected_channel == notes_id;
        let open_notes = cx.listener(|this: &mut Inlet, _: &gpui::ClickEvent, window, cx| {
            this.open_personal_notes(window, cx)
        });
        out.push(nav_row(
            "dm-notes",
            SocialIcon::Notes,
            t!("social-personal-notes"),
            notes_selected,
            0,
            open_notes,
        ));
        out.push(
            div()
                .flex_none()
                .ml_2()
                .mt_2()
                .mb_1()
                .px_2()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .text_size(px(13.))
                        .line_height(px(16.))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme::text_muted())
                        .child(t!("sidebar-direct-messages")),
                )
                .child(
                    div()
                        .id("dm-new")
                        .debug_selector(|| "dm-new".into())
                        .size(px(18.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(4.))
                        .cursor_pointer()
                        .hover(|s| s.bg(theme::hover_bg()))
                        .child(SocialIcon::Plus.el(14., theme::text_muted()))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_group_dm(Mode::Create, window, cx)
                        })),
                )
                .into_any_element(),
        );
        let selected = self.selected_channel.clone();
        for row in rows {
            let active = selected.as_deref() == Some(row.id.as_str());
            out.push(self.dm_row(&row, active, cx));
        }
        out
    }

    /// The avatar of a DM row: the person's picture (or initials) with their status, or
    /// the group's icon (or a round icon in the brand colour).
    fn dm_avatar(&self, row: &DmRow, ring: Hsla, cx: &mut Context<Self>) -> AnyElement {
        let picture = if row.group {
            match row
                .icon
                .as_deref()
                .and_then(|h| self.group_dm_icon_image(&row.id, h, DM_AVATAR, cx))
            {
                Some(img) => img.into_any_element(),
                None => div()
                    .size(px(DM_AVATAR))
                    .rounded_full()
                    .bg(theme::brand())
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(SocialIcon::Friends.el(18., theme::on_brand()))
                    .into_any_element(),
            }
        } else {
            let avatar = row
                .user
                .as_ref()
                .and_then(crate::backend::media::user_avatar);
            self.user_avatar(avatar.as_ref(), &row.name, DM_AVATAR, false, cx)
        };
        div()
            .relative()
            .size(px(DM_AVATAR))
            .flex_none()
            .child(picture)
            .children(row.status.map(|s| avatar_status(s, ring)))
            .into_any_element()
    }

    /// One DM row.
    fn dm_row(&mut self, row: &DmRow, active: bool, cx: &mut Context<Self>) -> AnyElement {
        let badge = self.badge(&row.id);
        let marks = RowMarks::of(badge, active);
        let color = if active {
            theme::text_strong()
        } else if marks.highlight {
            theme::text_secondary()
        } else {
            theme::text_channel()
        };
        // The dot's ring takes the row's colour (a selected row is lighter).
        let ring = if active {
            theme::bg_app().blend(theme::selected_bg())
        } else {
            theme::bg_app()
        };
        let avatar = self.dm_avatar(row, ring, cx);
        let tag = row.user.as_ref().and_then(user_tag);
        let id = row.id.clone();
        let group = SharedString::from(format!("dm-{}", row.id));
        let kind = if row.group {
            channel_type::GROUP_DM
        } else {
            channel_type::DM
        };
        let (close_id, menu_id, name) = (row.id.clone(), row.id.clone(), row.name.clone());
        let close = div()
            .id(SharedString::from(format!("dm-close-{}", row.id)))
            .size(px(18.))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(4.))
            .opacity(0.)
            .group_hover(group.clone(), |s| s.opacity(1.))
            .hover(|s| s.bg(theme::hover_bg()))
            .child(SocialIcon::Close.el(12., theme::text_muted()))
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                let Some(channel) = ChannelId::parse(&close_id) else {
                    return;
                };
                if kind == channel_type::GROUP_DM {
                    this.social.confirm = Some(Confirm::LeaveGroup {
                        channel,
                        name: name.clone(),
                    });
                    cx.notify();
                } else {
                    this.close_dm(channel, cx);
                }
            }));
        let row_el = div()
            .id(SharedString::from(format!("ch-{}", row.id)))
            .debug_selector(|| "dm-row".into())
            .group(group)
            .flex_none()
            .h(px(DM_ROW_HEIGHT))
            .ml_2()
            .px_2()
            .rounded(px(6.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.))
            .whitespace_nowrap()
            .overflow_hidden()
            .cursor_pointer()
            .font_weight(FontWeight::MEDIUM)
            .text_color(color)
            .when(active, |d| d.bg(theme::selected_bg()))
            .hover(|s| s.bg(theme::hover_bg()).text_color(theme::text()))
            .on_click(
                cx.listener(move |this, _, window, cx| this.select_channel(id.clone(), window, cx)),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, e: &MouseDownEvent, _, cx| {
                    if let Some(channel) = ChannelId::parse(&menu_id) {
                        this.social.dm_menu = Some(DmMenu {
                            channel,
                            at: e.position,
                        });
                        cx.notify();
                    }
                }),
            )
            .child(avatar)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_size(px(16.))
                            .line_height(px(20.))
                            .child(row.name.clone()),
                    )
                    .children(tag),
            )
            .when(marks.count, |d| d.child(mention_badge(badge.mentions)))
            .child(close);
        div()
            .relative()
            .flex_none()
            .flex()
            .flex_col()
            .when(marks.dimmed, |d| d.opacity(0.5))
            .when(marks.pill, |d| {
                d.child(
                    div()
                        .absolute()
                        .left(px(4.))
                        .top(px(17.))
                        .w(px(8.))
                        .h(px(8.))
                        .rounded_r(px(4.))
                        .bg(theme::text_strong())
                        .when(marks.pill_faded, |p| p.opacity(0.4)),
                )
            })
            .child(row_el)
            .into_any_element()
    }

    /// The DM menu: pin or unpin, the group's settings, close or leave.
    pub(crate) fn render_dm_menu(&mut self, menu: &DmMenu, cx: &mut Context<Self>) -> AnyElement {
        let channel = menu.channel;
        let (pinned, group, name) = match self.account.as_ref().map(|a| a.read(cx)) {
            Some(s) => (
                s.relationships().pinned_dms().contains(&channel),
                s.channel(channel)
                    .is_some_and(|c| c.kind == channel_type::GROUP_DM),
                group_name(s, channel),
            ),
            None => (false, false, String::new()),
        };
        let item = |id: &'static str, icon: SocialIcon, label: String, danger: bool| {
            let color = if danger {
                theme::danger()
            } else {
                theme::text_muted()
            };
            div()
                .id(id)
                .h(px(32.))
                .px(px(8.))
                .rounded(px(4.))
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .cursor_pointer()
                .text_size(px(14.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(color)
                .hover(|s| {
                    s.bg(theme::floating_hover())
                        .text_color(theme::text_strong())
                })
                .child(label)
                .child(icon.el(16., color))
        };
        let pin_label = if pinned {
            t!("social-unpin-dm")
        } else {
            t!("social-pin-dm")
        };
        let mut items = div().w(px(188.)).flex().flex_col().child(
            item("dm-menu-pin", SocialIcon::Pin, pin_label, false).on_click(cx.listener(
                move |this, _, _, cx| {
                    this.social.dm_menu = None;
                    this.social_action(
                        Cmd::PinDm {
                            channel_id: channel.to_string(),
                            pin: !pinned,
                        },
                        cx,
                    );
                },
            )),
        );
        if group {
            items = items.child(
                item(
                    "dm-menu-settings",
                    SocialIcon::Pencil,
                    t!("social-group-settings"),
                    false,
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_group_dm(Mode::Settings { channel }, window, cx)
                })),
            );
            items = items.child(
                item(
                    "dm-menu-leave",
                    SocialIcon::Leave,
                    t!("social-leave-group"),
                    true,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.social.dm_menu = None;
                    this.social.confirm = Some(Confirm::LeaveGroup {
                        channel,
                        name: name.clone(),
                    });
                    cx.notify();
                })),
            );
        } else {
            items = items.child(
                item(
                    "dm-menu-close",
                    SocialIcon::Close,
                    t!("social-close-dm"),
                    true,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.social.dm_menu = None;
                    this.close_dm(channel, cx);
                })),
            );
        }
        let card = div()
            .id("dm-menu")
            .occlude()
            .p(px(6.))
            .rounded(px(8.))
            .bg(theme::floating_bg())
            .border_1()
            .border_color(theme::accent_border())
            .shadow(vec![gpui::BoxShadow {
                color: theme::floating_shadow(),
                offset: point(px(0.), px(6.)),
                blur_radius: px(14.),
                spread_radius: px(0.),
                inset: false,
            }])
            .child(items)
            .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, _, cx| {
                this.social.dm_menu = None;
                cx.notify();
            }));
        let card = self.social_menu_keys(card, cx);
        deferred(
            anchored()
                .position(menu.at)
                .snap_to_window_with_margin(px(8.))
                .child(card),
        )
        .with_priority(2)
        .into_any_element()
    }
}

/// The Friends or Personal notes row: a 32 px icon circle and the label.
fn nav_row(
    id: &'static str,
    icon: SocialIcon,
    label: String,
    selected: bool,
    badge: usize,
    on_click: impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
) -> AnyElement {
    div()
        .id(id)
        .debug_selector(move || id.into())
        .flex_none()
        .h(px(DM_ROW_HEIGHT))
        .ml_2()
        .px_2()
        .rounded(px(6.))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(12.))
        .cursor_pointer()
        .text_size(px(16.))
        .text_color(if selected {
            theme::text_strong()
        } else {
            theme::text_channel()
        })
        .font_weight(FontWeight::MEDIUM)
        .when(selected, |d| d.bg(theme::selected_bg()))
        .hover(|s| s.bg(theme::hover_bg()).text_color(theme::text()))
        .child(
            div()
                .size(px(DM_AVATAR))
                .flex_none()
                .rounded_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(if selected {
                    theme::brand()
                } else {
                    theme::guild_item()
                })
                .child(icon.el(
                    20.,
                    if selected {
                        theme::on_brand()
                    } else {
                        theme::text_strong()
                    },
                )),
        )
        .child(div().flex_1().min_w_0().child(label))
        .when(badge > 0, |d| d.child(mention_badge(badge as u32)))
        .on_click(on_click)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use inlet_protocol::Ready;
    use serde_json::json;

    #[test]
    fn dm_rows_show_the_person_their_status_and_tag_in_fluxers_order() {
        let at = |ms: u64| (ms << 22).to_string();
        let ready: Ready = serde_json::from_value(json!({
            "session_id": "s", "user": {"id": "1", "username": "me"}, "guilds": [],
            "users": [],
            "private_channels": [
                {"id": at(10), "type": 1, "last_message_id": at(50),
                 "recipients": [{"id": "2", "username": "bot", "bot": true, "avatar": "abc"}]},
                {"id": at(20), "type": 1, "last_message_id": at(90),
                 "recipients": [{"id": "3", "username": "ada", "global_name": "Ada"}]},
                {"id": at(30), "type": 3, "name": null, "icon": "ff",
                 "recipients": [{"id": "2", "username": "bot"}, {"id": "3", "username": "ada", "global_name": "Ada"}]},
                {"id": "1", "type": 999}
            ],
            "presences": [{"user": {"id": "3"}, "status": "online"}],
            "pinned_dms": [at(10)],
            "read_states": [], "user_guild_settings": []
        }))
        .unwrap();
        let s = AccountState::from_ready_at(&ready, 1);
        let rows = dm_rows_of(&s, &[]);
        let order: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(order, ["bot", "Ada", "bot, Ada"], "pinned, then newest");
        assert!(rows[0].pinned && rows[0].user.as_ref().is_some_and(|u| u.bot));
        assert_eq!(rows[0].status, Some("offline"));
        assert_eq!(rows[1].status, Some("online"));
        assert!(rows[2].group && rows[2].status.is_none());
        assert_eq!(rows[2].icon.as_deref(), Some("ff"));
        // A DM only the REST list knows comes last; personal notes never show here.
        let extra = Channel {
            id: "77".into(),
            name: "late".into(),
            kind: ChannelKind::Dm,
            parent_id: None,
            position: 0,
            topic: None,
            slowmode_secs: 0,
        };
        let notes = Channel {
            id: "1".into(),
            kind: ChannelKind::Text,
            ..extra.clone()
        };
        let rows = dm_rows_of(&s, &[extra, notes]);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[3].name, "late");
    }
}
