// SPDX-License-Identifier: AGPL-3.0-or-later
//! The user menu (right click on a member): Profile, Copy Username, Copy User ID. A
//! layer like the popout: Escape or a click outside closes it.

use gpui::{
    anchored, div, point, prelude::*, px, AnyElement, ClipboardItem, Context, FontWeight,
    MouseDownEvent, Pixels, Point, SharedString,
};
use inlet_state::{GuildId, UserId};

use crate::app::Inlet;
use crate::desktop::Layer;
use crate::theme;

/// Width of the menu.
const WIDTH: f32 = 188.;

/// The open user menu.
#[derive(Clone, Debug, PartialEq)]
pub struct UserMenu {
    pub user: UserId,
    pub guild: Option<GuildId>,
    pub at: Point<Pixels>,
}

fn item(id: &'static str, label: String) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(px(32.))
        .px(px(8.))
        .rounded(px(4.))
        .flex()
        .items_center()
        .cursor_pointer()
        .text_size(px(14.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(theme::text_muted())
        .hover(|s| {
            s.bg(theme::floating_hover())
                .text_color(theme::text_strong())
        })
        .child(label)
}

fn separator() -> gpui::Div {
    div()
        .my(px(4.))
        .mx(px(4.))
        .h(px(1.))
        .bg(theme::accent_border())
}

impl Inlet {
    /// Right click on a person: their menu at the pointer.
    pub(crate) fn open_user_menu(
        &mut self,
        user: UserId,
        guild: Option<GuildId>,
        at: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.profile.menu = Some(UserMenu { user, guild, at });
        cx.notify();
    }

    pub(super) fn render_user_menu(&mut self, m: &UserMenu, cx: &mut Context<Self>) -> AnyElement {
        let (user, guild) = (m.user, m.guild);
        let username = {
            let cached = self
                .profile
                .cache
                .any(&(user, guild))
                .map(|p| p.username.clone());
            cached.or_else(|| {
                let b = self.profile_basics(&(user, guild), None, cx);
                Some(b.username).filter(|u| !u.is_empty())
            })
        };
        let mut menu = div().w(px(WIDTH)).flex().flex_col().child(
            item("user-menu-profile", t!("user-menu-profile")).on_click(
                cx.listener(move |this, _, _, cx| this.open_full_profile(user, guild, cx)),
            ),
        );
        menu = menu.child(separator());
        if let Some(name) = username {
            menu = menu.child(
                item("user-menu-copy-name", t!("user-menu-copy-username")).on_click(cx.listener(
                    move |this, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(name.clone()));
                        this.profile.menu = None;
                        cx.notify();
                    },
                )),
            );
        }
        menu = menu.child(item("user-menu-copy-id", t!("user-menu-copy-id")).on_click(
            cx.listener(move |this, _, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(user.to_string()));
                this.profile.menu = None;
                cx.notify();
            }),
        ));
        let card = div()
            .id(SharedString::from(format!("user-menu-{user}")))
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
            .child(menu)
            .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, _, cx| {
                this.profile.menu = None;
                cx.notify();
            }));
        let card = if self.is_top_layer(Layer::UserMenu) {
            self.layer_keys(card, cx).into_any_element()
        } else {
            card.into_any_element()
        };
        // Not deferred: the overlays are drawn in layer order (`render_overlays`), so a
        // layer over this one (the link confirmation, the link prompt, the status
        // picker) covers it and takes the clicks.
        anchored()
            .position(m.at)
            .snap_to_window_with_margin(px(8.))
            .child(card)
            .into_any_element()
    }
}
