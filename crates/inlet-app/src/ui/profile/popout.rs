// SPDX-License-Identifier: AGPL-3.0-or-later
//! The profile popout (Fluxer's `UserProfilePopout`): a 300 px card beside the member
//! list with the banner (or the accent colour), the avatar and status, name, account
//! name and pronouns, the custom status, the bio, roles in the guild, member since,
//! and the note. "View Full Profile" opens the full profile. A click outside closes it.

use std::rc::Rc;

use gpui::{
    anchored, div, point, prelude::*, px, Anchor, AnyElement, App, Context, FontWeight, Hsla,
    MouseDownEvent, SharedString,
};
use inlet_state::UserId;

use super::{Key, Opener, Popout};
use crate::app::Inlet;
use crate::backend::social::profile::Profile;
use crate::desktop::Layer;
use crate::model::{self, UserAvatar};
use crate::theme;
use crate::ui::markdown::{self, RenderCtx};
use crate::ui::members::list::normalize_status;
use crate::ui::members::WIDTH as MEMBER_LIST_WIDTH;

/// Width of the popout.
pub const WIDTH: f32 = 300.;
/// Banner height in the popout with a banner image, and without one.
const BANNER_HEIGHT: f32 = 106.;
const BANNER_PLAIN_HEIGHT: f32 = 60.;
/// Avatar size in the popout.
const AVATAR: f32 = 80.;

/// The status dot of a large avatar (the presence domain's dot, `size` square), in a
/// ring of `ring` on the avatar's bottom right.
fn large_status(status: &str, size: f32, ring: Hsla) -> AnyElement {
    let pad = (size / 4.).round();
    div()
        .absolute()
        .right(px(-pad))
        .bottom(px(-pad))
        .size(px(size + 2. * pad))
        .rounded_full()
        .bg(ring)
        .flex()
        .items_center()
        .justify_center()
        .child(crate::ui::social::presence::status_dot(status, size))
        .into_any_element()
}

/// What is known of a person before (or without) the fetched profile.
pub(super) struct Basics {
    pub name: String,
    pub username: String,
    pub avatar: Option<UserAvatar>,
    pub bot: bool,
}

impl Inlet {
    /// The name and avatar of `key` from the profile, else from the account state.
    pub(super) fn profile_basics(&self, key: &Key, profile: Option<&Profile>, cx: &App) -> Basics {
        if let Some(p) = profile {
            return Basics {
                name: p.name.clone(),
                username: p.username.clone(),
                avatar: p.avatar.clone(),
                bot: p.bot,
            };
        }
        let state = self.account.as_ref().map(|a| a.read(cx));
        let member = state.and_then(|s| key.1.and_then(|g| s.member(g, key.0)));
        let user = member
            .and_then(|m| m.user.as_ref())
            .or_else(|| state.and_then(|s| s.user(key.0)));
        let name = member
            .and_then(|m| m.display_name())
            .or_else(|| user.map(|u| u.display_name()))
            .unwrap_or_default();
        Basics {
            name: model::clean_name(name),
            username: user
                .map(|u| model::clean_name(&u.username))
                .unwrap_or_default(),
            avatar: user.and_then(crate::backend::media::user_avatar),
            bot: user.is_some_and(|u| u.bot),
        }
    }

    /// The person's status and custom status text from the account state.
    pub(super) fn profile_presence(
        &self,
        user: UserId,
        cx: &App,
    ) -> (&'static str, Option<String>) {
        let Some(state) = self.account.as_ref().map(|a| a.read(cx)) else {
            return ("offline", None);
        };
        if state.me_id() == user {
            // The account itself: the status this session shows.
            return (self.shown_status(cx), self.own_custom_text(cx));
        }
        let presence = state.presences().get(user);
        let status = normalize_status(presence.map_or("", |p| p.status.as_str()));
        let custom = presence
            .and_then(|p| p.custom_status.as_ref())
            .and_then(|c| c.text.as_deref())
            .and_then(model::clean_text)
            .map(|t| t.chars().take(128).collect::<String>());
        (status, custom)
    }

    /// Banner, avatar with status, and the names: the top of the popout and of the
    /// full profile.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn profile_head(
        &mut self,
        key: &Key,
        profile: Option<&Profile>,
        width: f32,
        banner_h: (f32, f32),
        avatar: f32,
        name_size: f32,
        surface: Hsla,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let basics = self.profile_basics(key, profile, cx);
        let (status, custom) = self.profile_presence(key.0, cx);
        let accent: Hsla = profile
            .and_then(|p| p.accent)
            .map(|c| gpui::rgb(c).into())
            .unwrap_or_else(theme::brand);
        let banner = profile.and_then(|p| p.banner.clone()).and_then(|b| {
            self.profile_banner(
                b.guild_id.as_deref(),
                &key.0.to_string(),
                &b.hash,
                (width, banner_h.0),
                cx,
            )
        });
        let has_banner = profile.is_some_and(|p| p.banner.is_some());
        let banner_height = if has_banner { banner_h.0 } else { banner_h.1 };
        let avatar_el = self.user_avatar(basics.avatar.as_ref(), &basics.name, avatar, true, cx);
        let ring = (avatar / 13.).round();
        let mut names = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.))
            .min_w_0()
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(px(name_size))
                    .line_height(px(name_size * 1.25))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::text_strong())
                    .child(basics.name.clone()),
            );
        if basics.bot {
            names = names.child(
                div()
                    .flex_none()
                    .px(px(5.6))
                    .rounded(px(4.))
                    .bg(theme::brand())
                    .text_size(px(10.))
                    .line_height(px(15.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::on_brand())
                    .child(t!("message-bot-tag")),
            );
        }
        let pronouns = profile.and_then(|p| p.pronouns.clone());
        let sub = match pronouns {
            Some(p) => t!(
                "profile-username-pronouns",
                username = basics.username,
                pronouns = p
            ),
            None => basics.username.clone(),
        };
        div()
            .flex()
            .flex_col()
            .child(
                div()
                    .relative()
                    .w_full()
                    .h(px(banner_height))
                    .bg(accent)
                    .overflow_hidden()
                    .children(banner),
            )
            .child(
                div().relative().px(px(16.)).h(px(avatar / 2. + 8.)).child(
                    div()
                        .absolute()
                        .left(px(16. - ring))
                        .top(px(-avatar / 2. - ring))
                        .p(px(ring))
                        .rounded_full()
                        .bg(surface)
                        .child(
                            div()
                                .relative()
                                .size(px(avatar))
                                .child(avatar_el)
                                .child(large_status(status, (avatar / 5.).round(), surface)),
                        ),
                ),
            )
            .child(
                div()
                    .px(px(16.))
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(names)
                    .child(
                        div()
                            .text_size(px(14.))
                            .line_height(px(18.))
                            .text_color(theme::text_secondary())
                            .truncate()
                            .child(sub),
                    )
                    .children(custom.map(|c| {
                        div()
                            .pt(px(4.))
                            .text_size(px(14.))
                            .line_height(px(18.))
                            .text_color(theme::text())
                            .child(c)
                    })),
            )
            .into_any_element()
    }

    /// A section of a profile: a small heading and its content.
    pub(super) fn profile_section(title: String, content: AnyElement) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(
                div()
                    .text_size(px(12.))
                    .line_height(px(16.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::text_strong())
                    .child(title),
            )
            .child(content)
            .into_any_element()
    }

    /// The bio, drawn as markdown (links go through the link checks).
    pub(super) fn profile_bio(
        &self,
        id: &str,
        profile: &Rc<Profile>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let weak = cx.entity().downgrade();
        let ctx = RenderCtx {
            revealed: false,
            names: Rc::new(Default::default()),
            jumbo: false,
            emoji: None,
            off_thread: None,
            on_link: Rc::new(move |url, masked, _window, cx| {
                let url = url.to_string();
                let _ = weak.update(cx, |this, cx| this.open_link(&url, masked, cx));
            }),
            on_reveal: Rc::new(|_window, _cx| {}),
        };
        div()
            .text_size(px(14.))
            .line_height(px(20.))
            .text_color(theme::text())
            .child(markdown::render(id, &profile.bio, &ctx))
            .into_any_element()
    }

    /// The note field, or nothing (own profile, not loaded yet).
    pub(super) fn profile_note(&self, user: UserId) -> Option<AnyElement> {
        let n = self.profile.note.as_ref().filter(|n| n.user == user)?;
        Some(Self::profile_section(
            t!("profile-note"),
            div().child(n.input.clone()).into_any_element(),
        ))
    }

    /// "Loading…", why the profile could not be loaded, or that privacy settings
    /// hide parts of it.
    pub(super) fn profile_status_line(
        loading: bool,
        failed: Option<String>,
        limited: bool,
    ) -> Option<AnyElement> {
        let text = match (failed, loading) {
            (Some(reason), _) => t!("profile-error", reason = reason),
            (None, true) => t!("profile-loading"),
            (None, false) if limited => t!("profile-limited"),
            (None, false) => return None,
        };
        Some(
            div()
                .text_size(px(13.))
                .text_color(theme::text_tertiary())
                .child(text)
                .into_any_element(),
        )
    }

    pub(super) fn render_profile_popout(
        &mut self,
        p: &Popout,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key: Key = (p.user, p.guild);
        let (profile, loading, failed) = self.profile_view(&key);
        let surface = theme::bg_app();
        let head = self.profile_head(
            &key,
            profile.as_deref(),
            WIDTH,
            (BANNER_HEIGHT, BANNER_PLAIN_HEIGHT),
            AVATAR,
            20.,
            surface,
            cx,
        );
        let mut body = div()
            .px(px(16.))
            .pt(px(12.))
            .pb(px(16.))
            .flex()
            .flex_col()
            .gap(px(12.));
        body = body.children(Self::profile_status_line(
            loading && profile.is_none(),
            failed,
            profile.as_ref().is_some_and(|p| p.limited),
        ));
        if let Some(prof) = &profile {
            if !prof.bio.is_empty() {
                let bio = self.profile_bio(&format!("popout-bio-{}", p.user), prof, cx);
                body = body.child(Self::profile_section(t!("profile-about"), bio));
            }
            body = body.child(Self::profile_section(
                t!("profile-member-since"),
                self.member_since_line(prof),
            ));
            if let Some(g) = p.guild {
                if !prof.roles.is_empty() {
                    let roles = self.role_chips(g, &prof.roles, cx);
                    body = body.child(Self::profile_section(t!("profile-roles"), roles));
                }
            }
        }
        body = body.children(self.profile_note(p.user));
        let user = p.user;
        let guild = p.guild;
        body = body.child(
            div()
                .id("profile-view-full")
                .mt(px(4.))
                .h(px(32.))
                .rounded(px(6.))
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .bg(theme::guild_item())
                .hover(|s| s.bg(theme::floating_hover()))
                .text_size(px(14.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme::text_strong())
                .child(t!("profile-view-full"))
                .on_click(
                    cx.listener(move |this, _, _, cx| this.open_full_profile(user, guild, cx)),
                ),
        );
        let card = div()
            .id(SharedString::from(format!("profile-popout-{}", p.user)))
            .occlude()
            .w(px(WIDTH))
            .max_h(px(640.))
            .overflow_y_scroll()
            .rounded(px(8.))
            .bg(surface)
            .border_1()
            .border_color(theme::accent_border())
            .shadow(vec![gpui::BoxShadow {
                color: theme::floating_shadow(),
                offset: point(px(0.), px(8.)),
                blur_radius: px(24.),
                spread_radius: px(0.),
                inset: false,
            }])
            .child(head)
            .child(body)
            .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, _, cx| {
                // A click into a layer over the popout (the link confirmation, the
                // user menu) does not close it.
                if this.is_top_layer(Layer::ProfilePopout) {
                    this.profile.popout = None;
                    cx.notify();
                }
            }));
        let card = if self.is_top_layer(Layer::ProfilePopout) {
            self.layer_keys(card, cx).into_any_element()
        } else {
            card.into_any_element()
        };
        let (at, corner) = match p.opener {
            // Left of the member list, at the row's height.
            Opener::MemberList => (
                point(
                    px((self.profile.window_width - MEMBER_LIST_WIDTH - 8.).max(WIDTH)),
                    p.at.y - px(20.),
                ),
                Anchor::TopRight,
            ),
        };
        // Not deferred: the overlays are drawn in layer order (`render_overlays`), so a
        // layer over this one (the link confirmation, the link prompt, the status
        // picker) covers it and takes the clicks.
        anchored()
            .position(at)
            .anchor(corner)
            .snap_to_window_with_margin(px(8.))
            .child(card)
            .into_any_element()
    }

    /// When the account was made, and when it joined the guild ("1 Jan 2026").
    pub(super) fn member_since_line(&self, prof: &Profile) -> AnyElement {
        let mut line = div()
            .flex()
            .flex_col()
            .gap(px(2.))
            .text_size(px(14.))
            .text_color(theme::text());
        if let Some(ms) = prof.created_ms {
            line = line.child(t!("profile-since-instance", date = super::date(ms)));
        }
        if let Some(ms) = prof.joined_ms {
            line = line.child(t!("profile-since-guild", date = super::date(ms)));
        }
        line.into_any_element()
    }
}
