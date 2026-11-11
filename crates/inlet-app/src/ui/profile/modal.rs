// SPDX-License-Identifier: AGPL-3.0-or-later
//! The full profile (Fluxer's `UserProfileModal`): a 600 px card in the middle of the
//! window with a large banner and avatar, and tabs: about (bio, member since, roles,
//! note), mutual guilds and mutual friends. A click on the backdrop closes it.

use gpui::{
    div, prelude::*, px, rgba, AnyElement, Context, FontWeight, MouseDownEvent, SharedString,
};

use super::{Full, Key, Tab};
use crate::app::Inlet;
use crate::desktop::Layer;
use crate::theme;

/// Width of the card.
pub const WIDTH: f32 = 600.;
/// Banner height with an image, and without one.
const BANNER_HEIGHT: f32 = 212.;
const BANNER_PLAIN_HEIGHT: f32 = 106.;
/// Avatar size.
const AVATAR: f32 = 120.;

impl Inlet {
    pub(super) fn render_full_profile(&mut self, f: &Full, cx: &mut Context<Self>) -> AnyElement {
        let key: Key = (f.user, f.guild);
        let (profile, loading, failed) = self.profile_view(&key);
        let surface = theme::bg_app();
        let head = self.profile_head(
            &key,
            profile.as_deref(),
            WIDTH,
            (BANNER_HEIGHT, BANNER_PLAIN_HEIGHT),
            AVATAR,
            24.,
            surface,
            cx,
        );
        let mine = self.account.as_ref().map(|a| a.read(cx).me_id()) == Some(f.user);
        let mut tabs = vec![(Tab::About, t!("profile-tab-about"))];
        if !mine {
            let (g, fr) = profile
                .as_ref()
                .map_or((0, 0), |p| (p.mutual_guilds.len(), p.mutual_friends.len()));
            tabs.push((Tab::Guilds, t!("profile-tab-guilds", count = g)));
            tabs.push((Tab::Friends, t!("profile-tab-friends", count = fr)));
        }
        let tab_row = div()
            .px(px(16.))
            .mt(px(12.))
            .flex()
            .flex_row()
            .gap(px(20.))
            .border_b_1()
            .border_color(theme::divider())
            .children(tabs.into_iter().map(|(tab, label)| {
                let on = tab == f.tab;
                div()
                    .id(SharedString::from(format!("profile-tab-{tab:?}")))
                    .pb(px(8.))
                    .border_b_2()
                    .border_color(if on {
                        theme::text_strong()
                    } else {
                        gpui::transparent_black()
                    })
                    .cursor_pointer()
                    .text_size(px(14.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(if on {
                        theme::text_strong()
                    } else {
                        theme::text_muted()
                    })
                    .hover(|s| s.text_color(theme::text_strong()))
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(full) = this.profile.full.as_mut() {
                            full.tab = tab;
                        }
                        cx.notify();
                    }))
            }));
        let mut content = div()
            .id("profile-full-content")
            .px(px(16.))
            .py(px(16.))
            .h(px(320.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(16.));
        content = content.children(Self::profile_status_line(
            loading && profile.is_none(),
            failed,
            profile.as_ref().is_some_and(|p| p.limited),
        ));
        if let Some(prof) = &profile {
            match f.tab {
                Tab::About => {
                    if !prof.bio.is_empty() {
                        let bio = self.profile_bio(&format!("full-bio-{}", f.user), prof, cx);
                        content = content.child(Self::profile_section(t!("profile-about"), bio));
                    }
                    content = content.child(Self::profile_section(
                        t!("profile-member-since"),
                        self.member_since_line(prof),
                    ));
                    if let Some(g) = f.guild {
                        let roles = self.role_chips(g, &prof.roles, cx);
                        content = content.child(Self::profile_section(t!("profile-roles"), roles));
                    }
                    content = content.children(self.profile_note(f.user));
                }
                Tab::Guilds => content = content.child(self.mutual_guilds(prof, cx)),
                Tab::Friends => content = content.child(self.mutual_friends(prof, cx)),
            }
        }
        let card = div()
            .id(SharedString::from(format!("profile-full-{}", f.user)))
            .occlude()
            .w(px(WIDTH))
            .rounded(px(8.))
            .overflow_hidden()
            .bg(surface)
            .child(head)
            .child(tab_row)
            .child(content)
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation());
        let card = if self.is_top_layer(Layer::ProfileModal) {
            self.layer_keys(card, cx).into_any_element()
        } else {
            card.into_any_element()
        };
        div()
            .id("profile-full-backdrop")
            .occlude()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x000000a0))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _, cx| {
                    if this.is_top_layer(Layer::ProfileModal) {
                        this.profile.full = None;
                        cx.notify();
                    }
                }),
            )
            .child(card)
            .into_any_element()
    }
}
