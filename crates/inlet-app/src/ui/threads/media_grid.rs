// SPDX-License-Identifier: AGPL-3.0-or-later
//! A media channel's posts as a grid of tiles: the first image of each post's first
//! message (from the media proxy only, as every image), its title and tags. A spoiler
//! or explicit image is not shown on a tile; the post shows it behind its cover.

use gpui::{div, prelude::*, px, AnyElement, Context, FontWeight, ObjectFit, SharedString};
use inlet_protocol as p;
use inlet_protocol::api::threads::ForumTag;

use super::tags;
use crate::app::Inlet;
use crate::model::{self, AttachmentKind};
use crate::theme;
use crate::ui::media::requests;
use crate::ui::media::{picture_of, placeholder};

/// A tile's side.
const TILE: f32 = 200.;

impl Inlet {
    pub(super) fn media_grid(
        &self,
        posts: &[p::Channel],
        forum_tags: &[ForumTag],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap_2()
            .children(
                posts
                    .iter()
                    .map(|post| self.media_tile(post, forum_tags, cx)),
            )
            .into_any_element()
    }

    fn media_tile(
        &self,
        post: &p::Channel,
        forum_tags: &[ForumTag],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let title = model::clean_name(post.name.as_deref().unwrap_or_default());
        let image = self
            .threads
            .forum
            .as_ref()
            .and_then(|f| f.preview(&post.id))
            .and_then(|p| p.image.clone())
            .filter(|a| !a.spoiler && !a.explicit);
        let picture = match (&image, &self.media.setup) {
            (Some(a), Some(setup)) => {
                let a = self.media.refresh.current(a);
                // The proxy's still: an image, or a video's first frame.
                let still = matches!(a.kind, AttachmentKind::Image | AttachmentKind::Video)
                    .then(|| {
                        requests::attachment_preview(&setup.proxy, &a, self.media.scale, false)
                    })
                    .flatten();
                match still {
                    Some(req) => {
                        picture_of(self.image(&req, cx), (TILE, TILE), 8., ObjectFit::Cover)
                    }
                    None => placeholder((TILE, TILE), 8., false),
                }
            }
            _ => placeholder((TILE, TILE), 8., false),
        };
        let open = self
            .threads
            .panel
            .as_ref()
            .is_some_and(|p| p.thread.to_string() == post.id);
        let target = post.clone();
        div()
            .id(SharedString::from(format!("tile-{}", post.id)))
            .w(px(TILE))
            .flex_none()
            .flex()
            .flex_col()
            .gap_1()
            .cursor_pointer()
            .rounded(px(8.))
            .when(open, |d| d.bg(theme::selected_bg()))
            .hover(|s| s.opacity(0.9))
            .child(picture)
            .child(
                div()
                    .px_1()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(px(14.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme::text_strong())
                    .child(title),
            )
            .children(
                tags::post_tags(&post.applied_tags(), forum_tags).map(|t| div().px_1().child(t)),
            )
            .on_click(
                cx.listener(move |this, _, window, cx| this.open_post(target.clone(), window, cx)),
            )
            .into_any_element()
    }
}
