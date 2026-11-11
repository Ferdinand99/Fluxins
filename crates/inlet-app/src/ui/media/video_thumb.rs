// SPDX-License-Identifier: AGPL-3.0-or-later
//! Videos as a still: the first frame from the media proxy with a play button. Inlet has
//! no video player; the play button opens the video in the browser, through the link
//! check. Video sites in embeds (YouTube and the like) always open in the browser.
//!
//! Deviation: the plan opens attachment videos in the system's player. Until that is
//! decided they open in the browser, which needs no new code path for local files.

use gpui::{div, prelude::*, px, AnyElement, Context, FontWeight, ObjectFit, SharedString};

use super::failed::Origin;
use super::image::Shown;
use super::{duration, requests, MediaIcon};
use crate::app::Inlet;
use crate::model::{Attachment, EmbedMedia};
use crate::theme;

/// Whether an embed's video is covered as explicit: the embed is marked, or its video
/// or its still is.
pub(crate) fn video_explicit(nsfw: bool, video: &EmbedMedia, still: Option<&EmbedMedia>) -> bool {
    nsfw || video.explicit || still.is_some_and(|s| s.explicit)
}

/// The round play button in the middle of a video still.
fn play_button() -> gpui::Div {
    div()
        .size(px(48.))
        .rounded_full()
        .bg(theme::media_overlay())
        .flex()
        .items_center()
        .justify_center()
        .child(MediaIcon::Play.el(px(22.), theme::text_strong()))
}

/// A still with the play button over it and the length in a corner.
fn with_play(id: String, still: AnyElement, length: Option<u64>) -> gpui::Stateful<gpui::Div> {
    div()
        .id(SharedString::from(id))
        .relative()
        .flex_none()
        .cursor_pointer()
        .child(still)
        .child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(play_button()),
        )
        .children(length.map(|secs| {
            div()
                .absolute()
                .bottom(px(6.))
                .left(px(6.))
                .px(px(6.))
                .rounded(px(4.))
                .bg(theme::media_overlay())
                .text_size(px(12.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme::text_strong())
                .child(duration(secs))
        }))
}

impl Inlet {
    /// A video attachment: its first frame, the play button (opens the video in the
    /// browser) and the save button.
    pub(super) fn video_attachment(
        &self,
        channel_id: &str,
        message_id: &str,
        key: &str,
        a: &Attachment,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let size = self.fit_column(
            requests::fit_inside(a.width, a.height, requests::PREVIEW_MAX),
            0.,
        );
        if let Some(cover) = self.cover(key, a.spoiler, a.explicit, size, cx) {
            return cover;
        }
        let (still, failed) = self.attachment_frame(channel_id, message_id, key, a, size, cx);
        if failed {
            // The tile says why and opens the video itself; no play button over it.
            return still;
        }
        let group = SharedString::from(format!("vid-{key}"));
        let save = self.save_button(&group, key, a, cx);
        let url = self
            .media
            .setup
            .as_ref()
            .and_then(|s| requests::attachment_original(&s.proxy, a))
            .map(String::from);
        with_play(format!("vid-{key}"), still, a.duration_secs)
            .group(group)
            .children(save)
            .on_click(cx.listener(move |this, _, _, cx| {
                if let Some(url) = &url {
                    this.open_link(url, false, cx);
                }
            }))
            .into_any_element()
    }

    /// The video of an embed: the thumbnail (or the video's first frame) with the play
    /// button. A click opens `link` (the page of the video) in the browser. A still
    /// that cannot be shown is a tile named after `origin`, without the play button.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn embed_video(
        &self,
        key: &str,
        video: &EmbedMedia,
        thumbnail: Option<&EmbedMedia>,
        link: Option<String>,
        nsfw: bool,
        origin: &Origin,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let shape = thumbnail.unwrap_or(video);
        let size = requests::embed_media_box(&EmbedMedia {
            width: video.width.or(shape.width),
            height: video.height.or(shape.height),
            ..Default::default()
        });
        let shown_size = self.fit_column(size, super::EMBED_CHROME);
        // Covered before anything loads.
        let explicit = video_explicit(nsfw, video, thumbnail);
        if let Some(cover) = self.cover(&format!("evid-{key}"), false, explicit, shown_size, cx) {
            return cover;
        }
        let (shown, req) = match &self.media.setup {
            Some(setup) => {
                let req = match thumbnail {
                    Some(t) => {
                        requests::embed_image(&setup.proxy, t, size, self.media.scale, false)
                    }
                    None => {
                        requests::embed_video_frame(&setup.proxy, video, size, self.media.scale)
                    }
                };
                match req {
                    Some(req) => (self.image(&req, cx), Some(req)),
                    None => (Shown::Unavailable, None),
                }
            }
            None => (Shown::Unavailable, None),
        };
        let failed = matches!(shown, Shown::Failed(_) | Shown::Unavailable);
        let link = link.or_else(|| (!video.url.is_empty()).then(|| video.url.clone()));
        let origin = Origin {
            name: origin.name.clone(),
            link: origin
                .link
                .clone()
                .or_else(|| link.clone().map(|l| (l, true))),
        };
        let still = self.media_or_tile(
            &format!("evid-{key}"),
            shown,
            shown_size,
            4.,
            ObjectFit::Cover,
            &origin,
            req.into_iter().collect(),
            cx,
        );
        if failed {
            return still;
        }
        with_play(format!("evid-{key}"), still, None)
            .on_click(cx.listener(move |this, _, _, cx| {
                if let Some(url) = &link {
                    this.open_link(url, true, cx);
                }
            }))
            .into_any_element()
    }
}
