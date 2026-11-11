// SPDX-License-Identifier: AGPL-3.0-or-later
//! Stickers under a message, 160 x 160 each in a row, as in Fluxer's message
//! attachments. The proxy serves every sticker as WebP whatever was uploaded (PNG,
//! APNG, GIF or WebP); `inlet-media` reads all four, so any of them shows. An animated
//! sticker moves under the one animation budget, and with animated stickers switched
//! off only its still is fetched.
//!
//! Lottie stickers are not drawn: Fluxer does not have them.

use gpui::{div, prelude::*, px, AnyElement, Context, ObjectFit, SharedString};
use inlet_media::{snap_proxy_size, Fit, MediaKind, MediaRequest};
use inlet_protocol as p;
use url::Url;

use super::animation_budget::{Motion, MotionPrefs};
use super::image::{picture_of, Shown};
use crate::app::Inlet;
use crate::model::Sticker;

/// The box of a sticker, in logical pixels (`.stickerWrapper`: 10rem).
pub const STICKER_SIZE: f32 = 160.;

/// The requests for a sticker: the animation (only for an animated sticker while
/// animated stickers are on; at its logical size, as every animation) and the still (at
/// the device pixels shown).
pub fn sticker_requests(
    proxy: &p::Media,
    sticker: &Sticker,
    prefs: &MotionPrefs,
    scale: f32,
) -> (Option<MediaRequest>, Option<MediaRequest>) {
    let request = |animate: bool, scale: f32| {
        let px = snap_proxy_size((STICKER_SIZE * scale.max(1.)).ceil() as u32);
        let url = Url::parse(&proxy.sticker(&sticker.id, animate, px)?).ok()?;
        Some(
            MediaRequest::new(
                url,
                MediaKind::Emoji,
                Fit::Contain {
                    max_width: px,
                    max_height: px,
                },
            )
            .with_proxy_size(px),
        )
    };
    let moving = (sticker.animated && prefs.allows(Motion::Sticker))
        .then(|| request(true, 1.))
        .flatten();
    (moving, request(false, scale))
}

impl Inlet {
    /// The stickers of a message, in a row; `None` when it has none.
    pub(crate) fn render_stickers(
        &self,
        message_id: &str,
        stickers: &[Sticker],
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if stickers.is_empty() {
            return None;
        }
        let prefs = self.media.images.animations().borrow().prefs;
        let anim = self.media.images.animator(Motion::Sticker);
        let size = (STICKER_SIZE, STICKER_SIZE);
        let items = stickers.iter().enumerate().map(|(i, s)| {
            let shown = match &self.media.setup {
                Some(setup) => {
                    let (moving, still) =
                        sticker_requests(&setup.proxy, s, &prefs, self.media.scale);
                    self.moving(moving.as_ref(), still.as_ref(), cx)
                }
                None => Shown::Unavailable,
            };
            div()
                .id(SharedString::from(format!("sticker-{message_id}-{i}")))
                .flex_none()
                .size(px(STICKER_SIZE))
                .child(picture_of(shown, size, 0., ObjectFit::Contain, &anim))
        });
        Some(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap_2()
                .mt_1()
                .children(items)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use bytes::Bytes;
    use futures::future::BoxFuture;
    use inlet_media::{FetchError, FetchResponse, Fetcher};

    use super::*;
    use crate::backend::media::png;
    use crate::ui::media::animation_budget::AnimationBudget;

    /// A stand-in for the media proxy that serves each sticker in its own format.
    struct Stickers {
        seen: Mutex<Vec<String>>,
    }

    /// Three frames of a moving dot, `n` x `n`, RGBA.
    fn frames(n: u32) -> Vec<Vec<u8>> {
        (0..3)
            .map(|f| {
                (0..n * n)
                    .flat_map(|i| {
                        let (x, y) = (i % n, i / n);
                        if (x / 8 + f) % 3 == 0 && y > n / 4 && y < 3 * n / 4 {
                            [240, 160, 40, 255]
                        } else {
                            [40, 90, 200, 255]
                        }
                    })
                    .collect()
            })
            .collect()
    }

    impl Fetcher for Stickers {
        fn fetch<'a>(&'a self, url: &'a Url) -> BoxFuture<'a, Result<FetchResponse, FetchError>> {
            self.seen.lock().unwrap().push(url.to_string());
            let animated = url.query().is_some_and(|q| q.contains("animated=true"));
            let id = url
                .path_segments()
                .and_then(|mut s| s.next_back())
                .unwrap_or_default()
                .trim_end_matches(".webp")
                .to_string();
            let n = 64;
            let f = frames(n);
            // What the uploader sent, whatever the `.webp` in the URL says.
            let body = match (id.as_str(), animated) {
                ("1001", _) => png::encode(n, n, &f[0]),
                ("1002", true) => png::encode_apng(n, n, &f, 100),
                ("1002", false) => png::encode(n, n, &f[0]),
                ("1003", true) => png::encode_gif(n as u16, n as u16, &f, 10),
                ("1003", false) => png::encode_gif(n as u16, n as u16, &f[..1], 10),
                ("1004", _) => png::encode_webp(n, n, &f[0]),
                ("1005", true) => png::encode_animated_webp(n, n, &f, 100),
                ("1005", false) => png::encode_webp(n, n, &f[0]),
                _ => Vec::new(),
            };
            Box::pin(async move { Ok(FetchResponse::from_bytes(Bytes::from(body))) })
        }
    }

    fn sticker(id: &str, animated: bool) -> Sticker {
        Sticker {
            id: id.into(),
            name: format!("s{id}"),
            animated,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sticker_formats_png_apng_gif_webp_render() {
        let proxy = p::Media::new("https://media.example.org/media").unwrap();
        let fetcher = Arc::new(Stickers {
            seen: Mutex::default(),
        });
        // The app's own image client: proxy-only hosts, its limits, gpui images.
        let setup = crate::backend::media::setup(proxy.clone(), fetcher.clone());
        let prefs = MotionPrefs::default();
        let mut budget = AnimationBudget::default();
        let t0 = Instant::now();
        budget.next_frame(true, t0);
        let cases = [
            ("1001", false, "png", 1),
            ("1002", true, "apng", 3),
            ("1003", true, "gif", 3),
            ("1004", false, "webp", 1),
            ("1005", true, "animated webp", 3),
        ];
        for (id, animated, format, frames) in cases {
            let s = sticker(id, animated);
            let (moving, still) = sticker_requests(&proxy, &s, &prefs, 2.);
            let still = still.expect("a still");
            assert!(still.url.path().ends_with(&format!("/stickers/{id}.webp")));
            assert_eq!(still.proxy_size, Some(320));
            let image = setup.client.load(still).await.unwrap_or_else(|e| {
                panic!("{format}: the still does not decode: {e}");
            });
            assert_eq!(image.frame_count(), 1, "{format}");
            assert!(image.size(0).width.0 > 0 && image.size(0).height.0 > 0);
            assert_eq!(moving.is_some(), animated, "{format}");
            if let Some(moving) = moving {
                // The animation at its logical size, every frame decoded, and it moves.
                assert_eq!(moving.proxy_size, Some(160));
                let image = setup.client.load(moving).await.unwrap_or_else(|e| {
                    panic!("{format}: the animation does not decode: {e}");
                });
                assert_eq!(image.frame_count(), frames, "{format}");
                let delays: Vec<Duration> = (0..image.frame_count())
                    .map(|i| Duration::from(image.delay(i)))
                    .collect();
                assert_eq!(
                    budget.frame_to_draw(image.id, Motion::Sticker, || delays.clone(), t0),
                    0
                );
                assert_eq!(
                    budget.frame_to_draw(
                        image.id,
                        Motion::Sticker,
                        || delays.clone(),
                        t0 + Duration::from_millis(150)
                    ),
                    1,
                    "{format} moves"
                );
            }
        }
        // Only the proxy was asked, and only for stickers.
        let seen = fetcher.seen.lock().unwrap();
        assert!(seen
            .iter()
            .all(|u| u.starts_with("https://media.example.org/media/stickers/")));

        // Animated stickers off: only the still is asked for.
        let off = MotionPrefs {
            animate_stickers: false,
            ..MotionPrefs::default()
        };
        let (moving, still) = sticker_requests(&proxy, &sticker("1002", true), &off, 1.);
        assert!(moving.is_none());
        assert!(still.is_some());
        // A sticker ID that is not a snowflake asks for nothing.
        assert_eq!(
            sticker_requests(&proxy, &sticker("../1", true), &prefs, 1.),
            (None, None)
        );
    }
}
