// SPDX-License-Identifier: AGPL-3.0-or-later
//! Group DMs: the window to start a DM or a group DM with friends (the + in the DM
//! list, as Fluxer's `CreateDMModal`), to add a friend to a group, and the group's
//! settings (name, icon, people, leave). A layer, like the other social windows.
//!
//! Creating a group and adding someone may need a CAPTCHA (solved by the protocol when
//! it can). When the instance wants one Inlet cannot pass, or the hourly limit is
//! reached, the window says so and stays as it is: nothing is sent again on its own,
//! and a long wait shows as minutes or hours.

use gpui::{
    div, prelude::*, px, AnyElement, Context, Entity, Focusable, FontWeight, PathPromptOptions,
    SharedString, Subscription, Window,
};
use inlet_protocol::api::dms::MAX_GROUP_DM_RECIPIENTS;
use inlet_state::relationships::Kind;
use inlet_state::{AccountState, ChannelId, UserId};

use super::{person_name, person_user, SocialIcon, SocialPart};
use crate::app::Inlet;
use crate::backend::social::relationships::{Cmd, Icon};
use crate::text_input::TextInput;
use crate::theme;
use crate::ui::overlays::modal::modal;
use crate::ui::widgets::button;

/// What the window does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// A new DM (one friend picked) or group DM.
    Create,
    /// Adds one friend to a group.
    Add { channel: ChannelId },
    /// The group's settings.
    Settings { channel: ChannelId },
}

/// The group DM window.
pub struct GroupDmWindow {
    pub mode: Mode,
    pub search: Entity<TextInput>,
    /// The group's name field (settings only).
    pub name: Option<Entity<TextInput>>,
    pub picked: Vec<UserId>,
    /// A lasting line: the CAPTCHA, the hourly limit or another failure.
    pub message: Option<String>,
    _observe: Subscription,
}

impl GroupDmWindow {
    /// The key its calls go by (see `relationships::Cmd::key`).
    pub fn key(&self) -> String {
        match self.mode {
            Mode::Create => "group-dm".into(),
            Mode::Add { channel } | Mode::Settings { channel } => format!("group:{channel}"),
        }
    }
}

/// The friends the window offers: friends matching `query`, not already in `group`,
/// by name.
pub fn friend_choices(
    state: &AccountState,
    query: &str,
    group: Option<ChannelId>,
) -> Vec<(UserId, String)> {
    let query = query.trim().to_lowercase();
    let members: Vec<UserId> = group
        .and_then(|g| state.channel(g))
        .map(|c| {
            c.recipients
                .iter()
                .filter_map(|u| UserId::parse(&u.id))
                .collect()
        })
        .unwrap_or_default();
    let mut out: Vec<(UserId, String)> = state
        .relationships()
        .of_kind(Kind::Friend)
        .filter(|p| !members.contains(&p.id))
        .map(|p| (p.id, person_name(state, p.id)))
        .filter(|(_, name)| query.is_empty() || name.to_lowercase().contains(&query))
        .collect();
    out.sort_by(|a, b| {
        a.1.to_lowercase()
            .cmp(&b.1.to_lowercase())
            .then(a.0.cmp(&b.0))
    });
    out.truncate(super::friends::MAX_ROWS);
    out
}

/// The name a group shows: its own, or its people's.
pub fn group_name(state: &AccountState, channel: ChannelId) -> String {
    let Some(c) = state.channel(channel) else {
        return String::new();
    };
    match c.name.as_deref().filter(|n| !n.trim().is_empty()) {
        Some(n) => crate::model::clean_name(n),
        None => c
            .recipients
            .iter()
            .map(|u| crate::model::clean_name(u.display_name()))
            .collect::<Vec<_>>()
            .join(", "),
    }
}

impl Inlet {
    /// Opens the window.
    pub(crate) fn open_group_dm(
        &mut self,
        mode: Mode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let search = cx.new(|cx| TextInput::new(t!("social-group-dm-search"), cx).max_chars(100));
        let observe = cx.observe(&search, |_, _, cx| cx.notify());
        let name = match mode {
            Mode::Settings { channel } => {
                let current = self
                    .account
                    .as_ref()
                    .and_then(|a| a.read(cx).channel(channel))
                    .and_then(|c| c.name.clone())
                    .unwrap_or_default();
                Some(cx.new(|cx| {
                    TextInput::new(t!("social-group-name-placeholder"), cx)
                        .max_chars(100)
                        .with_text(current)
                }))
            }
            _ => None,
        };
        let focus = name
            .clone()
            .map_or_else(|| search.focus_handle(cx), |n| n.focus_handle(cx));
        focus.focus(window, cx);
        self.social.dm_menu = None;
        self.social.group_dm = Some(GroupDmWindow {
            mode,
            search,
            name,
            picked: Vec::new(),
            message: None,
            _observe: observe,
        });
        cx.notify();
    }

    /// The field the window's keyboard goes to.
    pub(crate) fn group_dm_field(&self, cx: &gpui::App) -> Option<gpui::FocusHandle> {
        let w = self.social.group_dm.as_ref()?;
        Some(match &w.name {
            Some(n) => n.focus_handle(cx),
            None => w.search.focus_handle(cx),
        })
    }

    /// The person was added: back to nothing (the gateway updates the group).
    pub(super) fn group_dm_added(&mut self, cx: &mut Context<Self>) {
        self.social.group_dm = None;
        cx.notify();
    }

    fn toggle_pick(&mut self, user: UserId, cx: &mut Context<Self>) {
        if let Some(w) = self.social.group_dm.as_mut() {
            if let Some(i) = w.picked.iter().position(|u| *u == user) {
                w.picked.remove(i);
            } else if matches!(w.mode, Mode::Add { .. }) {
                // One at a time: each addition is its own CAPTCHA-gated call.
                w.picked = vec![user];
            } else if w.picked.len() < MAX_GROUP_DM_RECIPIENTS {
                w.picked.push(user);
            }
        }
        cx.notify();
    }

    /// Create (or open the DM, with one friend picked), or add the picked friend.
    fn group_dm_go(&mut self, cx: &mut Context<Self>) {
        let Some(w) = self.social.group_dm.as_mut() else {
            return;
        };
        let picked: Vec<String> = w.picked.iter().map(|u| u.to_string()).collect();
        let cmd = match (w.mode, picked.as_slice()) {
            (_, []) => return,
            (Mode::Create, [one]) => Cmd::OpenDm {
                user_id: one.clone(),
            },
            (Mode::Create, _) => Cmd::CreateGroupDm { recipients: picked },
            (Mode::Add { channel }, [one, ..]) => Cmd::AddRecipient {
                channel_id: channel.to_string(),
                user_id: one.clone(),
            },
            (Mode::Settings { .. }, _) => return,
        };
        // A new try clears the last answer; it is never sent by itself.
        w.message = None;
        if let Cmd::OpenDm { .. } = cmd {
            self.social.group_dm = None;
        }
        self.social_action(cmd, cx);
    }

    /// Saves the name in the settings.
    fn group_dm_save_name(&mut self, channel: ChannelId, cx: &mut Context<Self>) {
        let Some(name) = self.social.group_dm.as_ref().and_then(|w| w.name.clone()) else {
            return;
        };
        let text = name.read(cx).text().trim().to_owned();
        self.social_action(
            Cmd::EditGroupDm {
                channel_id: channel.to_string(),
                name: Some((!text.is_empty()).then_some(text)),
                icon: None,
            },
            cx,
        );
    }

    /// Asks for a picture and sets it as the group's icon.
    fn group_dm_pick_icon(&mut self, channel: ChannelId, cx: &mut Context<Self>) {
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(SharedString::from(t!("social-group-icon-prompt"))),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = chosen.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                this.social_action(
                    Cmd::EditGroupDm {
                        channel_id: channel.to_string(),
                        name: None,
                        icon: Some(Icon::File(path)),
                    },
                    cx,
                )
            });
        })
        .detach();
    }

    pub(super) fn render_group_dm(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(w) = self.social.group_dm.as_ref() else {
            return div().into_any_element();
        };
        let (mode, search, name, picked, message) = (
            w.mode,
            w.search.clone(),
            w.name.clone(),
            w.picked.clone(),
            w.message.clone(),
        );
        let busy = self.social_busy(&w.key());
        let query = search.read(cx).text().to_owned();
        let (choices, title, members) = match self.account.as_ref().map(|a| a.read(cx)) {
            Some(state) => {
                let group = match mode {
                    Mode::Add { channel } | Mode::Settings { channel } => Some(channel),
                    Mode::Create => None,
                };
                let title = match mode {
                    Mode::Create => t!("social-new-dm-title"),
                    Mode::Add { channel } => {
                        t!("social-group-add-title", name = group_name(state, channel))
                    }
                    Mode::Settings { channel } => group_name(state, channel),
                };
                let members: Vec<(UserId, String, bool)> = group
                    .and_then(|g| state.channel(g))
                    .map(|c| {
                        let owner = c
                            .extra
                            .get("owner_id")
                            .and_then(|v| v.as_str())
                            .and_then(UserId::parse);
                        c.recipients
                            .iter()
                            .filter_map(|u| UserId::parse(&u.id))
                            .map(|u| (u, person_name(state, u), owner == Some(u)))
                            .collect()
                    })
                    .unwrap_or_default();
                (friend_choices(state, &query, group), title, members)
            }
            None => (Vec::new(), String::new(), Vec::new()),
        };
        let me_owner = match mode {
            Mode::Settings { channel } => self.account.as_ref().is_some_and(|a| {
                let s = a.read(cx);
                s.channel(channel).and_then(|c| {
                    c.extra
                        .get("owner_id")
                        .and_then(|v| v.as_str())
                        .and_then(UserId::parse)
                }) == Some(s.me_id())
            }),
            _ => false,
        };
        let mut parts: Vec<AnyElement> = vec![div()
            .text_size(px(20.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(theme::text_strong())
            .overflow_hidden()
            .text_ellipsis()
            .whitespace_nowrap()
            .child(title)
            .into_any_element()];
        if let Mode::Settings { channel } = mode {
            parts.extend(self.group_settings_parts(channel, name, members, me_owner, busy, cx));
        } else {
            let left = MAX_GROUP_DM_RECIPIENTS.saturating_sub(picked.len());
            parts.push(
                div()
                    .text_size(px(14.))
                    .text_color(theme::text_muted())
                    .child(match mode {
                        Mode::Create => t!("social-new-dm-body", count = left),
                        _ => t!("social-group-add-body"),
                    })
                    .into_any_element(),
            );
            parts.push(search.into_any_element());
            let rows: Vec<AnyElement> = choices
                .iter()
                .map(|(user, name)| self.render_choice(*user, name, picked.contains(user), cx))
                .collect();
            let empty = rows.is_empty().then(|| {
                div()
                    .py_4()
                    .flex()
                    .justify_center()
                    .text_color(theme::text_muted())
                    .child(t!("social-group-no-friends"))
            });
            parts.push(
                div()
                    .id("group-dm-friends")
                    .max_h(px(320.))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .children(rows)
                    .children(empty)
                    .into_any_element(),
            );
        }
        parts.push(
            div()
                .children(message.map(|m| {
                    div()
                        .debug_selector(|| "group-dm-message".into())
                        .px_3()
                        .py_2()
                        .rounded(px(6.))
                        .bg(theme::bg_app())
                        .border_1()
                        .border_color(theme::danger())
                        .text_size(px(14.))
                        .text_color(theme::text_strong())
                        .child(m)
                }))
                .into_any_element(),
        );
        let go_label = match (mode, picked.len()) {
            (Mode::Create, 0 | 1) => t!("social-new-dm-open"),
            (Mode::Create, _) => t!("social-new-group-create"),
            _ => t!("social-group-add"),
        };
        let mut actions = div().flex().flex_row().justify_end().gap_2().child(
            button("group-dm-cancel", t!("social-close"), false).on_click(cx.listener(
                |this, _, _, cx| {
                    this.social.group_dm = None;
                    cx.notify();
                },
            )),
        );
        if !matches!(mode, Mode::Settings { .. }) {
            let ready = !picked.is_empty() && !busy;
            actions = actions.child(
                button(
                    "group-dm-go",
                    if busy { t!("social-sending") } else { go_label },
                    true,
                )
                .when(!ready, |b| b.opacity(0.5))
                .on_click(cx.listener(|this, _, _, cx| this.group_dm_go(cx))),
            );
        }
        parts.push(actions.into_any_element());
        let card = modal("group-dm", 460., parts);
        self.social_keys(SocialPart::GroupDm, card, cx)
            .into_any_element()
    }

    /// A friend to pick, with a box.
    fn render_choice(
        &mut self,
        user: UserId,
        name: &str,
        picked: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let record = self
            .account
            .as_ref()
            .and_then(|a| person_user(a.read(cx), user));
        let avatar = record.as_ref().and_then(crate::backend::media::user_avatar);
        let picture = self.user_avatar(avatar.as_ref(), name, 32., false, cx);
        div()
            .id(SharedString::from(format!("group-pick-{user}")))
            .h(px(44.))
            .px_2()
            .rounded(px(6.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.))
            .cursor_pointer()
            .hover(|s| s.bg(theme::hover_bg()))
            .child(picture)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .text_color(theme::text_strong())
                    .child(name.to_owned()),
            )
            .child(
                div()
                    .size(px(22.))
                    .flex_none()
                    .rounded(px(6.))
                    .border_1()
                    .border_color(if picked {
                        theme::brand()
                    } else {
                        theme::text_muted()
                    })
                    .when(picked, |d| d.bg(theme::brand()))
                    .flex()
                    .items_center()
                    .justify_center()
                    .when(picked, |d| {
                        d.child(SocialIcon::Check.el(14., theme::on_brand()))
                    }),
            )
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_pick(user, cx)))
            .into_any_element()
    }

    /// The settings: name, icon, people, add, leave.
    fn group_settings_parts(
        &mut self,
        channel: ChannelId,
        name: Option<Entity<TextInput>>,
        members: Vec<(UserId, String, bool)>,
        me_owner: bool,
        busy: bool,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let me = self.account.as_ref().map(|a| a.read(cx).me_id());
        let label = |text: String| {
            div()
                .text_size(px(12.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme::text_muted())
                .child(text.to_uppercase())
        };
        let mut out: Vec<AnyElement> = Vec::new();
        out.push(label(t!("social-group-name")).into_any_element());
        out.push(
            div()
                .flex()
                .flex_row()
                .gap_2()
                .items_center()
                .children(name.map(|n| div().flex_1().child(n)))
                .child(
                    button("group-save-name", t!("social-save"), true)
                        .when(busy, |b| b.opacity(0.5))
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.group_dm_save_name(channel, cx)),
                        ),
                )
                .into_any_element(),
        );
        out.push(
            div()
                .flex()
                .flex_row()
                .gap_2()
                .child(
                    button("group-icon-change", t!("social-group-icon-change"), false).on_click(
                        cx.listener(move |this, _, _, cx| this.group_dm_pick_icon(channel, cx)),
                    ),
                )
                .child(
                    button("group-icon-remove", t!("social-group-icon-remove"), false).on_click(
                        cx.listener(move |this, _, _, cx| {
                            this.social_action(
                                Cmd::EditGroupDm {
                                    channel_id: channel.to_string(),
                                    name: None,
                                    icon: Some(Icon::Remove),
                                },
                                cx,
                            )
                        }),
                    ),
                )
                .into_any_element(),
        );
        out.push(label(t!("social-group-members", count = members.len() + 1)).into_any_element());
        let rows: Vec<AnyElement> = members
            .into_iter()
            .map(|(user, name, owner)| {
                let removable = me_owner && Some(user) != me;
                div()
                    .h(px(36.))
                    .px_2()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(name),
                    )
                    .when(owner, |d| {
                        d.child(
                            div()
                                .text_size(px(12.))
                                .text_color(theme::text_muted())
                                .child(t!("social-group-owner")),
                        )
                    })
                    .when(removable, |d| {
                        d.child(
                            super::round_action(
                                format!("group-remove-{user}"),
                                SocialIcon::Close,
                                true,
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.social_action(
                                        Cmd::RemoveRecipient {
                                            channel_id: channel.to_string(),
                                            user_id: user.to_string(),
                                        },
                                        cx,
                                    )
                                },
                            )),
                        )
                    })
                    .into_any_element()
            })
            .collect();
        out.push(
            div()
                .id("group-members")
                .max_h(px(220.))
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .children(rows)
                .into_any_element(),
        );
        out.push(
            div()
                .flex()
                .flex_row()
                .gap_2()
                .child(
                    button("group-add-friends", t!("social-group-add"), false).on_click(
                        cx.listener(move |this, _, window, cx| {
                            this.open_group_dm(Mode::Add { channel }, window, cx)
                        }),
                    ),
                )
                .child(
                    button("group-leave", t!("social-leave-group"), false)
                        .bg(theme::danger())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let name = this
                                .account
                                .as_ref()
                                .map(|a| group_name(a.read(cx), channel))
                                .unwrap_or_default();
                            this.social.confirm =
                                Some(super::Confirm::LeaveGroup { channel, name });
                            cx.notify();
                        })),
                )
                .into_any_element(),
        );
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use inlet_protocol::Ready;
    use serde_json::json;

    #[test]
    fn the_window_offers_friends_not_in_the_group() {
        let ready: Ready = serde_json::from_value(json!({
            "session_id": "s", "user": {"id": "1", "username": "me"}, "guilds": [],
            "users": [{"id": "2", "username": "bo"}, {"id": "3", "username": "ada"},
                      {"id": "4", "username": "cy"}],
            "private_channels": [{"id": "100", "type": 3, "name": "  ",
                "recipients": [{"id": "2", "username": "bo"}]}],
            "relationships": [{"id": "2", "type": 1}, {"id": "3", "type": 1}, {"id": "4", "type": 3}],
            "read_states": [], "user_guild_settings": []
        }))
        .unwrap();
        let s = AccountState::from_ready_at(&ready, 1);
        let all: Vec<String> = friend_choices(&s, "", None)
            .into_iter()
            .map(|c| c.1)
            .collect();
        assert_eq!(all, ["ada", "bo"], "friends only, by name");
        let add: Vec<String> = friend_choices(&s, "", Some(ChannelId(100)))
            .into_iter()
            .map(|c| c.1)
            .collect();
        assert_eq!(add, ["ada"], "not the people already in the group");
        assert_eq!(friend_choices(&s, "B", None).len(), 1);
        assert_eq!(
            group_name(&s, ChannelId(100)),
            "bo",
            "a blank name shows the people"
        );
    }
}
