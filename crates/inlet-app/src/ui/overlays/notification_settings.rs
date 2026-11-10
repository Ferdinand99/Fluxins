// SPDX-License-Identifier: AGPL-3.0-or-later
//! The notification settings window: mute and notification level for the open channel and
//! for its server (or for all direct messages), what `@everyone` and role mentions count
//! as, and "mark as read". It is the bell in the channel header, as in Fluxer.

use gpui::{div, prelude::*, px, AnyElement, Context, Div, FontWeight, SharedString, Stateful};

use super::modal::modal;
use crate::app::Inlet;
use crate::assets::Icon;
use crate::backend::notifications::{Change, Target};
use crate::theme;
use crate::ui::sidebar::channel_list::channel_label;
use crate::ui::widgets::button;

// Fluxer's notification levels.
const ALL: i64 = 0;
const ONLY_MENTIONS: i64 = 1;
const NOTHING: i64 = 2;
const INHERIT: i64 = 3;

/// The mute lengths Fluxer offers, in milliseconds.
const DURATIONS: [i64; 8] = [
    15 * 60_000,
    30 * 60_000,
    60 * 60_000,
    3 * 60 * 60_000,
    4 * 60 * 60_000,
    8 * 60 * 60_000,
    24 * 60 * 60_000,
    3 * 24 * 60 * 60_000,
];

fn duration_label(ms: i64) -> String {
    match ms {
        900_000 => t!("notifications-for-15m"),
        1_800_000 => t!("notifications-for-30m"),
        3_600_000 => t!("notifications-for-1h"),
        10_800_000 => t!("notifications-for-3h"),
        14_400_000 => t!("notifications-for-4h"),
        28_800_000 => t!("notifications-for-8h"),
        86_400_000 => t!("notifications-for-24h"),
        _ => t!("notifications-for-3d"),
    }
}

fn level_label(level: i64) -> String {
    match level {
        ALL => t!("notifications-level-all"),
        ONLY_MENTIONS => t!("notifications-level-mentions"),
        NOTHING => t!("notifications-level-nothing"),
        _ => String::new(),
    }
}

/// "Muted until 10/11/2026, 18:30", or "Muted" for a mute without an end.
fn muted_text(until_ms: Option<i64>) -> String {
    let until = until_ms
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format(&t!("timestamp-format"))
                .to_string()
        });
    match until {
        Some(time) => t!("notifications-muted-until", time = time),
        None => t!("notifications-muted"),
    }
}

fn heading(text: String) -> Div {
    div()
        .text_size(px(12.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme::text_muted())
        .child(text.to_uppercase())
}

fn chip(id: SharedString, label: String) -> Stateful<Div> {
    div()
        .id(id)
        .px(px(10.))
        .py(px(4.))
        .rounded(px(6.))
        .cursor_pointer()
        .text_size(px(13.))
        .text_color(theme::text())
        .bg(theme::bg_textarea())
        .hover(|s| s.bg(theme::selected_bg()))
        .child(label)
}

/// A radio row: a circle, the label and an optional line under it.
fn radio(id: SharedString, label: String, sub: Option<String>, selected: bool) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.))
        .px(px(6.))
        .py(px(4.))
        .rounded(px(6.))
        .cursor_pointer()
        .hover(|s| s.bg(theme::hover_bg()))
        .child(
            div()
                .flex_none()
                .size(px(16.))
                .rounded_full()
                .border_2()
                .border_color(if selected {
                    theme::brand()
                } else {
                    theme::text_channel()
                })
                .flex()
                .items_center()
                .justify_center()
                .when(selected, |d| {
                    d.child(div().size(px(8.)).rounded_full().bg(theme::brand()))
                }),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .child(
                    div()
                        .text_size(px(14.))
                        .text_color(theme::text())
                        .child(label),
                )
                .children(sub.map(|s| {
                    div()
                        .text_size(px(12.))
                        .text_color(theme::text_tertiary())
                        .child(s)
                })),
        )
}

fn checkbox(id: SharedString, label: String, checked: bool) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.))
        .px(px(6.))
        .py(px(4.))
        .rounded(px(6.))
        .cursor_pointer()
        .hover(|s| s.bg(theme::hover_bg()))
        .child(if checked {
            Icon::CheckSquare
                .el(px(16.), theme::brand())
                .into_any_element()
        } else {
            div()
                .flex_none()
                .size(px(16.))
                .rounded(px(3.))
                .border_2()
                .border_color(theme::text_channel())
                .into_any_element()
        })
        .child(
            div()
                .text_size(px(14.))
                .text_color(theme::text())
                .child(label),
        )
}

/// What one column of the window shows.
struct Section {
    /// Element IDs start with this.
    key: &'static str,
    title: String,
    target: Target,
    muted: bool,
    until_ms: Option<i64>,
    /// The level as stored (0 to 3).
    level: i64,
    /// The level the choice "default" stands for, and its name (channels in a server).
    default_row: Option<(String, i64)>,
    /// The level shown as chosen when none is stored.
    level_if_default: i64,
    /// `@everyone` and role suppression, for a server.
    suppress: Option<(bool, bool)>,
}

impl Inlet {
    pub(super) fn render_notification_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(window) = &self.notifications.settings else {
            return div().into_any_element();
        };
        let (guild_id, channel_id) = (window.guild_id.clone(), window.channel_id.clone());
        let view = window.view.clone();
        let Some(view) = view else {
            // The backend has not answered yet: an empty frame, replaced a moment later.
            return self.settings_frame(Vec::new(), cx);
        };

        let channel = channel_id
            .as_ref()
            .and_then(|id| self.channels.iter().find(|c| &c.id == id));
        let has_category = channel.is_some_and(|c| c.parent_id.is_some());
        let mut columns: Vec<AnyElement> = Vec::new();
        if let (Some(c), Some(id)) = (channel, &channel_id) {
            let default_row = guild_id.is_some().then(|| {
                (
                    if has_category {
                        t!("notifications-level-category-default")
                    } else {
                        t!("notifications-level-server-default")
                    },
                    view.channel_inherited_level,
                )
            });
            columns.push(self.settings_section(
                Section {
                    key: "channel",
                    title: channel_label(c),
                    target: Target::Channel {
                        guild_id: guild_id.clone(),
                        channel_id: id.clone(),
                    },
                    muted: view.channel_muted,
                    until_ms: view.channel_mute_until_ms,
                    level: view.channel_level,
                    default_row,
                    level_if_default: view.channel_inherited_level,
                    suppress: None,
                },
                cx,
            ));
        }
        let guild_title = match &guild_id {
            Some(id) => self
                .guilds
                .iter()
                .find(|g| &g.id == id)
                .map(|g| g.name.clone())
                .unwrap_or_default(),
            None => t!("notifications-direct-messages"),
        };
        columns.push(
            self.settings_section(
                Section {
                    key: "guild",
                    title: guild_title,
                    target: Target::Guild(guild_id.clone()),
                    muted: view.guild_muted,
                    until_ms: view.guild_mute_until_ms,
                    level: view.guild_level,
                    default_row: None,
                    level_if_default: view.guild_effective_level,
                    suppress: guild_id
                        .is_some()
                        .then_some((view.suppress_everyone, view.suppress_roles)),
                },
                cx,
            ),
        );
        self.settings_frame(columns, cx)
    }

    /// The window around the columns: title, the columns, "mark as read" and "close".
    fn settings_frame(&self, columns: Vec<AnyElement>, cx: &mut Context<Self>) -> AnyElement {
        let in_guild = self.selected_guild.is_some();
        let mark_server = if in_guild {
            t!("notifications-mark-server-read")
        } else {
            t!("notifications-mark-dms-read")
        };
        let has_channel = self.selected_channel.is_some();
        modal(
            "notification-settings-window",
            if columns.len() > 1 { 720. } else { 400. },
            [
                div()
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme::text_strong())
                    .child(t!("notifications-title"))
                    .into_any_element(),
                div()
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap_6()
                    .children(
                        columns
                            .into_iter()
                            .map(|c| div().flex_1().min_w_0().child(c)),
                    )
                    .into_any_element(),
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap_2()
                    .when(has_channel, |d| {
                        d.child(
                            button(
                                "mark-channel-read",
                                t!("notifications-mark-channel-read"),
                                false,
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.mark_channel_read();
                                cx.notify();
                            })),
                        )
                    })
                    .child(
                        button("mark-server-read", mark_server, false).on_click(cx.listener(
                            |this, _, _, cx| {
                                this.mark_guild_read();
                                cx.notify();
                            },
                        )),
                    )
                    .child(
                        button("mark-all-read", t!("notifications-mark-all-read"), false).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.mark_all_read();
                                cx.notify();
                            }),
                        ),
                    )
                    .into_any_element(),
                div()
                    .flex()
                    .flex_row()
                    .child(
                        button(
                            "notification-settings-close",
                            t!("notifications-close"),
                            true,
                        )
                        .on_click(
                            cx.listener(|this, _, _, cx| this.close_notification_settings(cx)),
                        ),
                    )
                    .into_any_element(),
            ],
        )
        .into_any_element()
    }

    fn settings_section(&self, s: Section, cx: &mut Context<Self>) -> AnyElement {
        let Section {
            key,
            title,
            target,
            muted,
            until_ms,
            level,
            default_row,
            level_if_default,
            suppress,
        } = s;
        let id = |what: &str| SharedString::from(format!("ns-{key}-{what}"));
        let change = |target: Target, change: Change| {
            move |this: &mut Inlet,
                  _: &gpui::ClickEvent,
                  _: &mut gpui::Window,
                  cx: &mut Context<Inlet>| {
                this.notification_change(target.clone(), change);
                cx.notify();
            }
        };

        // Mute: a button to end a mute, or the lengths to pick from.
        let mute = if muted {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(heading(t!("notifications-muted-heading")))
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(theme::text_tertiary())
                        .child(muted_text(until_ms)),
                )
                .child(
                    div().flex().flex_row().child(
                        chip(id("unmute"), t!("notifications-unmute"))
                            .on_click(cx.listener(change(target.clone(), Change::Unmute))),
                    ),
                )
        } else {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(heading(t!("notifications-mute-heading")))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap_1()
                        .children(DURATIONS.iter().map(|ms| {
                            chip(id(&format!("mute-{ms}")), duration_label(*ms)).on_click(
                                cx.listener(change(
                                    target.clone(),
                                    Change::Mute { for_ms: Some(*ms) },
                                )),
                            )
                        }))
                        .child(
                            chip(id("mute-forever"), t!("notifications-until-off")).on_click(
                                cx.listener(change(target.clone(), Change::Mute { for_ms: None })),
                            ),
                        ),
                )
        };

        // Level: the default of the parent (channels in a server) and the three levels.
        let chosen = if level == INHERIT || level < ALL {
            if default_row.is_some() {
                INHERIT
            } else {
                level_if_default
            }
        } else {
            level
        };
        let mut levels = div()
            .flex()
            .flex_col()
            .child(heading(t!("notifications-level-heading")));
        if let Some((label, inherited)) = default_row {
            levels = levels.child(
                radio(
                    id("level-default"),
                    label,
                    Some(level_label(inherited)),
                    chosen == INHERIT,
                )
                .on_click(cx.listener(change(target.clone(), Change::Level(INHERIT)))),
            );
        }
        for l in [ALL, ONLY_MENTIONS, NOTHING] {
            levels = levels.child(
                radio(id(&format!("level-{l}")), level_label(l), None, chosen == l)
                    .on_click(cx.listener(change(target.clone(), Change::Level(l)))),
            );
        }

        let suppress = suppress.map(|(everyone, roles)| {
            div()
                .flex()
                .flex_col()
                .child(
                    checkbox(
                        id("suppress-everyone"),
                        t!("notifications-suppress-everyone"),
                        everyone,
                    )
                    .on_click(
                        cx.listener(change(target.clone(), Change::SuppressEveryone(!everyone))),
                    ),
                )
                .child(
                    checkbox(
                        id("suppress-roles"),
                        t!("notifications-suppress-roles"),
                        roles,
                    )
                    .on_click(cx.listener(change(target.clone(), Change::SuppressRoles(!roles)))),
                )
        });

        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .text_color(theme::text_strong())
                    .font_weight(FontWeight::SEMIBOLD)
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(title),
            )
            .child(mute)
            .child(levels)
            .children(suppress)
            .into_any_element()
    }
}
