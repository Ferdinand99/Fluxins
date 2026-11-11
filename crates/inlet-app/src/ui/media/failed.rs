// SPDX-License-Identifier: AGPL-3.0-or-later
//! What an image that cannot be shown turns into: a tile of the image's size (so the row
//! keeps its height) that says why, names the file or the site it came from, and offers
//! "Open in browser" (through the link check) and, where asking again can help, "Retry".
//!
//! Every way an attachment or embed image can end goes through here or shows a picture:
//! no URL on the media proxy, a refused host, a failed or timed out fetch, an expired
//! link, data that does not decode. A load never waits forever (see `image.rs`).

use gpui::{div, prelude::*, px, AnyElement, Context, FontWeight, ObjectFit, SharedString};
use inlet_media::{FetchError, MediaError, MediaRequest};

use super::animation_budget::Motion;
use super::image::{picture_of, Shown};
use super::MediaIcon;
use crate::app::Inlet;
use crate::theme;

/// Why an image is not shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Trouble {
    /// Nothing to load from the media proxy: no proxy URL, a host other than the
    /// instance's media endpoint, or no media endpoint at all.
    Unavailable,
    /// The link has expired or the file is gone (401, 403, 404, 410).
    Expired,
    /// No answer in time.
    TimedOut,
    /// The network or the proxy failed (connection, 5xx, 429).
    Unreachable,
    /// The data cannot be shown: too large, not an image, damaged.
    Unreadable,
}

impl Trouble {
    /// The trouble of a failure; `None` is an image with no request at all.
    pub(crate) fn of(err: Option<&MediaError>) -> Trouble {
        let Some(err) = err else {
            return Trouble::Unavailable;
        };
        match err {
            MediaError::InvalidUrl(_) | MediaError::HostNotAllowed(_) => Trouble::Unavailable,
            MediaError::Fetch(FetchError::Status(401 | 403 | 404 | 410)) => Trouble::Expired,
            MediaError::FetchTimeout | MediaError::DecodeTimeout => Trouble::TimedOut,
            MediaError::Fetch(FetchError::Status(code)) if (400..500).contains(code) => {
                if matches!(code, 408 | 429) {
                    Trouble::Unreachable
                } else {
                    Trouble::Unreadable
                }
            }
            MediaError::Fetch(_) | MediaError::Cancelled | MediaError::TaskFailed => {
                Trouble::Unreachable
            }
            MediaError::TooLargeEncoded { .. }
            | MediaError::Unsupported
            | MediaError::BadDimensions { .. }
            | MediaError::TooManyFrames { .. }
            | MediaError::AnimationTooLarge
            | MediaError::Corrupt(_)
            | MediaError::DecoderPanic => Trouble::Unreadable,
        }
    }

    /// Whether asking again can help.
    pub(crate) fn can_retry(self) -> bool {
        matches!(self, Trouble::TimedOut | Trouble::Unreachable)
    }

    fn message(self) -> String {
        match self {
            Trouble::Unavailable => t!("media-error-unavailable"),
            Trouble::Expired => t!("media-error-expired"),
            Trouble::TimedOut => t!("media-error-timeout"),
            Trouble::Unreachable => t!("media-error-unreachable"),
            Trouble::Unreadable => t!("media-error-unreadable"),
        }
    }
}

/// Where an image came from, for its tile.
#[derive(Clone, Debug, Default)]
pub(crate) struct Origin {
    /// The file name, or the site an embed is from.
    pub name: String,
    /// What "Open in browser" opens, and whether the link check asks first (links from
    /// embeds point anywhere).
    pub link: Option<(String, bool)>,
}

/// How much a tile of `(w, h)` holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TileLayout {
    /// Icon, reason, name and buttons.
    Full,
    /// Icon and reason; a click on the tile retries, or opens the link.
    Compact,
    /// The icon alone (a thumbnail); a click retries or opens.
    Icon,
}

impl TileLayout {
    pub(crate) fn of((w, h): (f32, f32)) -> TileLayout {
        if w >= 180. && h >= 112. {
            TileLayout::Full
        } else if w >= 110. && h >= 56. {
            TileLayout::Compact
        } else {
            TileLayout::Icon
        }
    }
}

/// A small text button on a tile.
fn tile_button(id: String, label: String) -> gpui::Stateful<gpui::Div> {
    div()
        .id(SharedString::from(id))
        .px(px(10.))
        .py(px(4.))
        .rounded(px(4.))
        .bg(theme::media_overlay())
        .hover(|s| s.bg(theme::media_overlay_hover()))
        .cursor_pointer()
        .text_size(px(12.))
        .line_height(px(16.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme::text_strong())
        .child(label)
}

impl Inlet {
    /// The tile of an image that is not shown. `retry` are the requests to load again
    /// (empty when asking again cannot help).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn failed_tile(
        &self,
        id: &str,
        (w, h): (f32, f32),
        radius: f32,
        trouble: Trouble,
        origin: &Origin,
        retry: Vec<MediaRequest>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let retry = (trouble.can_retry() && !retry.is_empty()).then_some(retry);
        let link = origin.link.clone();
        let layout = TileLayout::of((w, h));
        let icon_size: f32 = if layout == TileLayout::Full { 24. } else { 20. };
        let mut tile = div()
            .id(SharedString::from(format!("media-failed-{id}")))
            .w(px(w))
            .h(px(h))
            .flex_none()
            .overflow_hidden()
            .rounded(px(radius))
            .border_1()
            .border_color(theme::accent_border())
            .bg(theme::media_placeholder())
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(6.))
            .p_2();
        if w >= 20. && h >= 20. {
            let icon = px(icon_size.min(w.min(h) - 4.));
            tile = tile.child(MediaIcon::Broken.el(icon, theme::text_muted()));
        }
        if layout != TileLayout::Icon {
            tile = tile.child(
                div()
                    .max_w_full()
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_size(px(13.))
                    .line_height(px(16.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme::text_muted())
                    .child(trouble.message()),
            );
        }
        if layout == TileLayout::Full {
            if !origin.name.is_empty() {
                tile = tile.child(
                    div()
                        .max_w_full()
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_ellipsis()
                        .text_size(px(12.))
                        .line_height(px(16.))
                        .text_color(theme::text_tertiary())
                        .child(origin.name.clone()),
                );
            }
            let mut buttons = div().flex().flex_row().gap_2().pt_1();
            if let Some(reqs) = retry.clone() {
                buttons = buttons.child(
                    tile_button(format!("media-retry-{id}"), t!("media-retry")).on_click(
                        cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.retry_media(&reqs, cx);
                        }),
                    ),
                );
            }
            if let Some((url, masked)) = link {
                buttons = buttons.child(
                    tile_button(format!("media-open-{id}"), t!("media-open-in-browser")).on_click(
                        cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.open_link(&url, masked, cx);
                        }),
                    ),
                );
            }
            // A click beside the buttons does nothing (no viewer for a broken image).
            tile = tile
                .child(buttons)
                .on_click(|_, _, cx| cx.stop_propagation());
        } else {
            // Too small for buttons: the tile itself retries, or else opens the link.
            tile = tile
                .when(retry.is_some() || link.is_some(), |d| d.cursor_pointer())
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    if let Some(reqs) = &retry {
                        this.retry_media(reqs, cx);
                    } else if let Some((url, masked)) = &link {
                        this.open_link(url, *masked, cx);
                    }
                }));
        }
        tile.into_any_element()
    }

    /// What a view draws for `shown`: the picture, the loading box, or the tile of a
    /// failure (`retry` are the requests behind it).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn media_or_tile(
        &self,
        id: &str,
        shown: Shown,
        size: (f32, f32),
        radius: f32,
        fit: ObjectFit,
        origin: &Origin,
        retry: Vec<MediaRequest>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match shown {
            Shown::Failed(err) => {
                self.failed_tile(id, size, radius, Trouble::of(Some(&err)), origin, retry, cx)
            }
            Shown::Unavailable => {
                self.failed_tile(id, size, radius, Trouble::Unavailable, origin, retry, cx)
            }
            shown => picture_of(
                shown,
                size,
                radius,
                fit,
                &self.media.images.animator(Motion::Gif),
            ),
        }
    }
}

/// The site a link is on, for naming an embed's media.
pub(crate) fn host_of(url: Option<&str>) -> Option<String> {
    let host = url::Url::parse(url?).ok()?.host_str()?.to_string();
    Some(host.trim_start_matches("www.").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_failure_has_a_reason_and_only_passing_ones_retry() {
        use Trouble::*;
        let status = |c| Some(MediaError::Fetch(FetchError::Status(c)));
        for (err, want) in [
            (None, Unavailable),
            (
                Some(MediaError::HostNotAllowed("x.example".into())),
                Unavailable,
            ),
            (Some(MediaError::InvalidUrl("x".into())), Unavailable),
            (status(403), Expired),
            (status(404), Expired),
            (status(410), Expired),
            (Some(MediaError::FetchTimeout), TimedOut),
            (Some(MediaError::DecodeTimeout), TimedOut),
            (status(503), Unreachable),
            (status(429), Unreachable),
            (
                Some(MediaError::Fetch(FetchError::Network("reset".into()))),
                Unreachable,
            ),
            (Some(MediaError::Cancelled), Unreachable),
            (status(400), Unreadable),
            (Some(MediaError::Unsupported), Unreadable),
            (Some(MediaError::Corrupt("eof".into())), Unreadable),
            (Some(MediaError::AnimationTooLarge), Unreadable),
            (
                Some(MediaError::TooLargeEncoded { size: 9, max: 1 }),
                Unreadable,
            ),
        ] {
            let got = Trouble::of(err.as_ref());
            assert_eq!(got, want, "{err:?}");
            assert!(!got.message().is_empty());
            // Retry only where the same request can work the next time.
            assert_eq!(
                got.can_retry(),
                matches!(want, TimedOut | Unreachable),
                "{err:?}"
            );
            // The client agrees: what may pass is not permanent.
            if let Some(e) = &err {
                if got.can_retry() {
                    assert!(!e.is_permanent(), "{e:?}");
                }
            }
        }
    }

    #[test]
    fn tiles_fit_their_box() {
        assert_eq!(TileLayout::of((550., 350.)), TileLayout::Full);
        assert_eq!(TileLayout::of((240., 240.)), TileLayout::Full);
        assert_eq!(TileLayout::of((160., 90.)), TileLayout::Compact);
        assert_eq!(TileLayout::of((80., 80.)), TileLayout::Icon);
        assert_eq!(
            host_of(Some("https://www.tenor.com/view/x")),
            Some("tenor.com".into())
        );
        assert_eq!(host_of(Some("not a url")), None);
    }
}
