// SPDX-License-Identifier: AGPL-3.0-or-later
//! The guilds and friends both accounts have, in the full profile. A click on a guild
//! opens it; a click on a friend opens their profile.

use gpui::{div, prelude::*, px, AnyElement, Context, FontWeight, SharedString};
use inlet_state::UserId;

use crate::app::Inlet;
use crate::backend::social::profile::Profile;
use crate::theme;

/// Size of a guild icon or avatar in the lists.
const ICON: f32 = 32.;

fn row(id: SharedString) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(px(44.))
        .px(px(8.))
        .rounded(px(6.))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(12.))
        .cursor_pointer()
        .hover(|s| s.bg(theme::hover_bg()))
}

fn empty(text: String) -> AnyElement {
    div()
        .py(px(24.))
        .flex()
        .justify_center()
        .text_size(px(14.))
        .text_color(theme::text_tertiary())
        .child(text)
        .into_any_element()
}

impl Inlet {
    pub(super) fn mutual_guilds(&mut self, prof: &Profile, cx: &mut Context<Self>) -> AnyElement {
        if prof.mutual_guilds.is_empty() {
            return empty(t!("profile-no-mutual-guilds"));
        }
        let mut list = div().flex().flex_col();
        for (id, nick) in &prof.mutual_guilds {
            // Only guilds the account knows are listed (the name comes from there).
            let Some(guild) = self.guilds.iter().find(|g| &g.id == id).cloned() else {
                continue;
            };
            let icon = match self.guild_icon_image(&guild.id, ICON, cx) {
                Some(img) => img.rounded(px(ICON * 0.3)).into_any_element(),
                // Without an icon: the initials, as for a person without an avatar.
                None => self.user_avatar(None, &guild.name, ICON, false, cx),
            };
            let target = guild.id.clone();
            list = list.child(
                row(SharedString::from(format!("mutual-guild-{id}")))
                    .child(icon)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(16.))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(theme::text_strong())
                                    .child(guild.name.clone()),
                            )
                            .children(nick.clone().map(|n| {
                                div()
                                    .truncate()
                                    .text_size(px(12.))
                                    .text_color(theme::text_tertiary())
                                    .child(n)
                            })),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.profile.full = None;
                        this.select_guild(Some(target.clone()), cx);
                    })),
            );
        }
        list.into_any_element()
    }

    pub(super) fn mutual_friends(&mut self, prof: &Profile, cx: &mut Context<Self>) -> AnyElement {
        if prof.mutual_friends.is_empty() {
            return empty(t!("profile-no-mutual-friends"));
        }
        let mut list = div().flex().flex_col();
        for f in &prof.mutual_friends {
            let Some(user) = UserId::parse(&f.user_id) else {
                continue;
            };
            let avatar = self.user_avatar(f.avatar.as_ref(), &f.name, ICON, false, cx);
            list = list.child(
                row(SharedString::from(format!("mutual-friend-{}", f.user_id)))
                    .child(avatar)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(16.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme::text_strong())
                            .child(f.name.clone()),
                    )
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.open_full_profile(user, None, cx)),
                    ),
            );
        }
        list.into_any_element()
    }
}
