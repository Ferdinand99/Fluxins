// SPDX-License-Identifier: AGPL-3.0-or-later
//! A member row (Fluxer's `MemberListItem`): 32 px avatar with the status dot, 10 px
//! gap, the name (16 px / 500, the colour of the highest coloured role), a crown for
//! the guild's owner, a BOT tag, and the custom status under it (11 px / 500). Rows of
//! offline members are drawn at 30 % until the pointer is over them. A click opens the
//! profile popout beside the list, a right click the user menu.

use gpui::{
    div, prelude::*, px, AnyElement, App, ClickEvent, Context, FontWeight, Hsla, MouseButton,
    MouseDownEvent, SharedString,
};
use inlet_state::{GuildId, RoleId, UserId};

use super::list::{normalize_status, MemberRow};
use super::{MemberIcon, ROW_HEIGHT};
use crate::app::Inlet;
use crate::theme;
use crate::ui::profile::Opener;
use crate::ui::social::presence::avatar_status;

/// Size of the avatar in a row.
pub const AVATAR: f32 = 32.;

/// A row for a member that is not loaded yet.
pub fn placeholder(ix: usize) -> AnyElement {
    // Bars of a few lengths, so a page of placeholders does not look like a grid.
    let width = [96., 128., 80., 112., 72.][ix % 5];
    div()
        .h(px(ROW_HEIGHT))
        .pb(px(2.))
        .px(px(8.))
        .flex()
        .items_center()
        .gap(px(10.))
        .child(
            div()
                .size(px(AVATAR))
                .rounded_full()
                .bg(theme::guild_item()),
        )
        .child(
            div()
                .h(px(12.))
                .w(px(width))
                .rounded_full()
                .bg(theme::guild_item()),
        )
        .into_any_element()
}

/// The small BOT tag after a name.
fn bot_tag() -> gpui::Div {
    div()
        .flex_none()
        .ml(px(4.))
        .px(px(5.6))
        .min_h(px(15.))
        .flex()
        .items_center()
        .rounded(px(4.))
        .bg(theme::brand())
        .text_size(px(10.))
        .line_height(px(15.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme::on_brand())
        .child(t!("message-bot-tag"))
}

impl Inlet {
    /// A member's status: the live presence when the account state has it, else the
    /// one the list item carried.
    pub(crate) fn member_status(
        &self,
        user: UserId,
        fallback: &'static str,
        cx: &App,
    ) -> &'static str {
        // The account itself: the status this session shows.
        if self.account.as_ref().map(|a| a.read(cx).me_id()) == Some(user) {
            return self.shown_status(cx);
        }
        self.account
            .as_ref()
            .and_then(|a| {
                a.read(cx)
                    .presences()
                    .get(user)
                    .map(|p| normalize_status(&p.status))
            })
            .unwrap_or(fallback)
    }

    /// The colour of a member's highest role that has one.
    pub(crate) fn role_color(&self, guild: GuildId, roles: &[RoleId], cx: &App) -> Option<Hsla> {
        let state = self.account.as_ref()?.read(cx);
        let gs = state.guild(guild)?;
        roles
            .iter()
            .filter_map(|r| gs.roles.get(*r))
            .filter(|r| r.color & 0xFF_FF_FF != 0)
            .max_by_key(|r| (r.position, std::cmp::Reverse(RoleId::parse(&r.id))))
            .map(|r| gpui::rgb((r.color & 0xFF_FF_FF) as u32).into())
    }

    /// Whether `user` owns `guild`.
    pub(crate) fn is_guild_owner(&self, guild: GuildId, user: UserId, cx: &App) -> bool {
        self.account.as_ref().is_some_and(|a| {
            a.read(cx)
                .guild(guild)
                .and_then(|g| g.properties.as_ref())
                .and_then(|p| p.owner_id.as_deref())
                .and_then(UserId::parse)
                == Some(user)
        })
    }

    /// One member row; `key` makes its element IDs unique.
    pub(crate) fn render_member_row(
        &mut self,
        key: &str,
        guild: GuildId,
        m: &MemberRow,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let status = self.member_status(m.user_id, m.status, cx);
        let offline = status == "offline";
        let open = self.profile.shows(m.user_id);
        let name_color = self
            .role_color(guild, &m.roles, cx)
            .unwrap_or(theme::text());
        let owner = self.is_guild_owner(guild, m.user_id, cx);
        let avatar = self.user_avatar(m.avatar.as_ref(), &m.name, AVATAR, false, cx);
        let user = m.user_id;
        let mut name_line = div().flex().flex_row().items_center().min_w_0().child(
            div()
                .min_w_0()
                .truncate()
                .text_size(px(16.))
                .line_height(px(20.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(name_color)
                .child(m.name.clone()),
        );
        if owner {
            name_line = name_line.child(
                div()
                    .id(SharedString::from(format!("{key}-crown")))
                    .flex_none()
                    .ml(px(4.))
                    .child(MemberIcon::Crown.el(px(14.), theme::crown())),
            );
        }
        if m.bot {
            name_line = name_line.child(bot_tag());
        }
        let text = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .child(name_line)
            .children(m.custom_status.clone().map(|s| {
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(px(11.))
                    .line_height(px(14.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme::text_muted().opacity(0.85))
                    .child(s)
            }));
        div()
            .h(px(ROW_HEIGHT))
            .pb(px(2.))
            .child(
                div()
                    .id(SharedString::from(format!("member-{key}")))
                    .h_full()
                    .px(px(8.))
                    .rounded(px(6.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.))
                    .cursor_pointer()
                    .when(offline && !open, |d| d.opacity(0.3))
                    .when(open, |d| d.bg(theme::selected_bg()))
                    .hover(|s| s.bg(theme::hover_bg()).opacity(1.))
                    .child(
                        div()
                            .relative()
                            .flex_none()
                            .size(px(AVATAR))
                            .child(avatar)
                            .child(avatar_status(status, theme::bg_chat())),
                    )
                    .child(text)
                    .on_click(cx.listener(move |this, e: &ClickEvent, _, cx| {
                        this.open_profile_popout(
                            user,
                            Some(guild),
                            e.position(),
                            Opener::MemberList,
                            cx,
                        );
                    }))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, e: &MouseDownEvent, _, cx| {
                            this.open_user_menu(user, Some(guild), e.position, cx);
                            cx.stop_propagation();
                        }),
                    ),
            )
            .into_any_element()
    }
}
