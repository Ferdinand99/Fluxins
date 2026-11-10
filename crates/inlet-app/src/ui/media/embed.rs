// SPDX-License-Identifier: AGPL-3.0-or-later
//! The media parts of embeds, for `ui/chat/embed.rs`: the large image, the thumbnail,
//! and the author and footer icons. All of them load through the media proxy only; an
//! embed whose media is not on the proxy shows a placeholder. A click on an image opens
//! the viewer.

use gpui::{div, prelude::*, AnyElement, Context, ObjectFit, SharedString};

use super::image::{picture_of, Shown};
use super::{requests, Viewer};
use crate::app::Inlet;
use crate::model::EmbedMedia;

impl Inlet {
    /// An embed image inside `max` logical pixels (the box of its kind), covered when
    /// explicit, opening the viewer on click. `chrome` is the width the card takes
    /// around it, so a narrow window shrinks the image instead of clipping it.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn embed_image(
        &self,
        key: &str,
        m: &EmbedMedia,
        max: (f32, f32),
        chrome: f32,
        nsfw: bool,
        name: String,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let size = self.fit_column(requests::fit_inside(m.width, m.height, max), chrome);
        if let Some(cover) = self.cover(key, false, nsfw || m.explicit, size, cx) {
            return cover;
        }
        let shown = match &self.media.setup {
            Some(setup) => match requests::embed_image(&setup.proxy, m, max, self.media.scale) {
                Some(req) => self.image(&req, cx),
                None => Shown::Unavailable,
            },
            None => Shown::Unavailable,
        };
        let viewer = Viewer::for_embed(self, m, name);
        div()
            .id(SharedString::from(format!("embed-img-{key}")))
            .flex_none()
            .cursor_pointer()
            .child(picture_of(shown, size, 4., ObjectFit::Contain))
            .on_click(cx.listener(move |this, _, _, cx| {
                if let Some(v) = viewer.clone() {
                    this.media.viewer = Some(v);
                    cx.notify();
                }
            }))
            .into_any_element()
    }

    /// An author or footer icon, `size` logical pixels and round, when it is on the
    /// media proxy and has loaded.
    pub(crate) fn embed_icon(
        &self,
        proxy_url: Option<&str>,
        size: f32,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let setup = self.media.setup.as_ref()?;
        let req = requests::embed_icon(&setup.proxy, proxy_url?, size, self.media.scale)?;
        match self.image(&req, cx) {
            Shown::Ready(image) => Some(
                gpui::img(image)
                    .size(gpui::px(size))
                    .flex_none()
                    .rounded_full()
                    .object_fit(ObjectFit::Cover)
                    .into_any_element(),
            ),
            // Keep the text where it will be once the icon is there.
            Shown::Loading => Some(super::image::placeholder((size, size), size / 2., false)),
            Shown::Failed(_) | Shown::Unavailable => None,
        }
    }
}
