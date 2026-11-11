// SPDX-License-Identifier: AGPL-3.0-or-later
//! Avatars of message authors, guild icons in the rail and the guild banner above the
//! channel list. Without an image (none set, still loading, failed) avatars and icons
//! show initials, as before.

use gpui::{div, img, prelude::*, px, AnyElement, App, Context, FontWeight, Img, ObjectFit};

use super::animation_budget::{animated, Motion};
use super::image::Shown;
use super::requests;
use crate::app::Inlet;
use crate::model::{Guild, UserAvatar};
use crate::theme;

/// Height of the guild banner above the channel list.
// Deviation: Fluxer's banner is 16:9 (at most 30 % of the window) with the name over it
// on a gradient; here it is a fixed 120 px strip with the name below.
pub const BANNER_HEIGHT: f32 = 120.;

impl Inlet {
    /// A user's avatar, `size` logical pixels and round. An animated avatar plays while
    /// `animate` is set (the pointer is over the message); until the animated version
    /// has loaded, the still one shows.
    pub(crate) fn user_avatar(
        &self,
        avatar: Option<&UserAvatar>,
        name: &str,
        size: f32,
        animate: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (Some(setup), Some(a)) = (&self.media.setup, avatar) else {
            return initials(name, size).into_any_element();
        };
        let scale = self.media.scale;
        // Through the animation budget, like every other animation.
        let moving = (animate && a.hash.starts_with("a_"))
            .then(|| requests::avatar(&setup.proxy, a, size, scale, true))
            .flatten();
        let still = requests::avatar(&setup.proxy, a, size, scale, false);
        let shown = self.moving(moving.as_ref(), still.as_ref(), cx);
        match shown {
            Shown::Ready(image) if image.frame_count() > 1 => animated(
                image,
                (size, size),
                size / 2.,
                ObjectFit::Cover,
                Motion::Avatar,
                self.media.images.animations().clone(),
            ),
            Shown::Ready(image) => img(image)
                .size(px(size))
                .flex_none()
                .rounded_full()
                .object_fit(ObjectFit::Cover)
                .into_any_element(),
            _ => initials(name, size).into_any_element(),
        }
    }

    /// The icon of a guild as an image, `size` logical pixels square, when it has one
    /// and it has loaded. The caller rounds it like the rail entry.
    pub(crate) fn guild_icon_image(
        &self,
        guild_id: &str,
        size: f32,
        cx: &mut Context<Self>,
    ) -> Option<Img> {
        let setup = self.media.setup.as_ref()?;
        let hash = self.guild_field(guild_id, cx, |g| g.icon.clone())?;
        let req = requests::guild_icon(&setup.proxy, guild_id, &hash, size, self.media.scale)?;
        match self.image(&req, cx) {
            Shown::Ready(image) => Some(
                img(image)
                    .size(px(size))
                    .flex_none()
                    .object_fit(ObjectFit::Cover),
            ),
            _ => None,
        }
    }

    /// The banner of the open guild, `width` logical pixels wide, when it has one.
    pub(crate) fn guild_banner(&self, width: f32, cx: &mut Context<Self>) -> Option<AnyElement> {
        let setup = self.media.setup.as_ref()?;
        let guild_id = self.selected_guild.clone()?;
        let hash = self.guild_field(&guild_id, cx, |g| {
            g.extra
                .get("banner")
                .and_then(|b| b.as_str())
                .map(str::to_owned)
        })?;
        let req = requests::guild_banner(
            &setup.proxy,
            &guild_id,
            &hash,
            (width, BANNER_HEIGHT),
            self.media.scale,
        )?;
        let shown = self.image(&req, cx);
        Some(super::image::picture_of(
            shown,
            (width, BANNER_HEIGHT),
            0.,
            ObjectFit::Cover,
            &self.media.images.animator(Motion::Gif),
        ))
    }

    /// The signed-in account's avatar, for its messages on their way.
    pub(crate) fn my_avatar(&self, cx: &App) -> Option<UserAvatar> {
        let state = self.account.as_ref()?.read(cx);
        crate::backend::media::user_avatar(state.me())
    }

    /// A field of a guild's record from the account state.
    fn guild_field<T>(
        &self,
        guild_id: &str,
        cx: &App,
        f: impl FnOnce(&inlet_protocol::Guild) -> Option<T>,
    ) -> Option<T> {
        let id = inlet_state::GuildId::parse(guild_id)?;
        let state = self.account.as_ref()?.read(cx);
        f(state.guild(id)?.properties.as_ref()?)
    }
}

/// The initials circle used when there is no avatar image.
pub(crate) fn initials(name: &str, size: f32) -> gpui::Div {
    div()
        .size(px(size))
        .flex_none()
        .rounded_full()
        .bg(theme::guild_item())
        .flex()
        .items_center()
        .justify_center()
        .text_size(px((size * 0.35).max(10.)))
        .font_weight(FontWeight::MEDIUM)
        .text_color(theme::text_muted())
        .child(
            Guild {
                id: String::new(),
                name: name.to_string(),
            }
            .initials(),
        )
}
