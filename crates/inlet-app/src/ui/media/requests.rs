// SPDX-License-Identifier: AGPL-3.0-or-later
//! The image requests the views make: which media proxy URL, at which size, under which
//! limits. Every URL comes from [`p::Media`], which only builds URLs on the session's
//! checked media endpoint, so a URL from a message (an embed's third-party `url`, a
//! `proxy_url` that points elsewhere) never becomes a request: the functions return
//! `None` and the view draws a placeholder.
//!
//! Sizes follow Fluxer: previews fit 550 x 350, embed media 400 x 300 (400 x 450 when
//! tall), embed thumbnails 80 x 80, avatars 40 px. The proxy is asked for the pixels
//! shown (logical size times the display scale), so it does the scaling and the decode
//! limits shrink to match (`MediaRequest::with_proxy_size`).
//!
//! Animations come in two requests: the animated one and its still (the first frame,
//! `animated` off, as WebP like Fluxer's previews). Every still asks for an image
//! format, so a video (a GIF page's MP4) gives its first frame instead of the file. Animated requests ask for the logical size only, never times the
//! display scale, and the viewer's for at most [`VIEWER_ANIMATED_MAX_PIXELS`]: every
//! frame is kept decoded, so their size is what an animation costs. The views fall
//! back to the still when an animation is too large or the budget is spent (see
//! `image.rs`).

use inlet_media::{snap_proxy_size, Fit, MediaKind, MediaRequest};
use inlet_protocol::{self as p, ImageFormat, ImageOptions, Transform};
use url::Url;

use crate::model::{Attachment, EmbedMedia, UserAvatar};

/// Largest attachment preview, in logical pixels (`MediaDimensionConfig`).
pub const PREVIEW_MAX: (f32, f32) = (550., 350.);
/// Largest embed image or video (`EMBED_MEDIA`).
pub const EMBED_MEDIA_MAX: (f32, f32) = (400., 300.);
/// Largest tall embed image.
pub const EMBED_MEDIA_TALL_MAX: (f32, f32) = (400., 450.);
/// Largest embed thumbnail.
pub const EMBED_THUMBNAIL_MAX: (f32, f32) = (80., 80.);
/// Longest side, in pixels, the image viewer asks for.
pub const VIEWER_MAX_PIXELS: u32 = 2560;
/// Longest side, in pixels, of an animation in the viewer.
pub const VIEWER_ANIMATED_MAX_PIXELS: u32 = 1024;

/// The logical size an image of `w` x `h` shows at inside `max`, never enlarged. An
/// unknown size takes the whole box.
pub fn fit_inside(w: Option<u32>, h: Option<u32>, max: (f32, f32)) -> (f32, f32) {
    match (w, h) {
        (Some(w), Some(h)) if w > 0 && h > 0 => {
            let (w, h) = (w as f32, h as f32);
            let s = (max.0 / w).min(max.1 / h).min(1.0);
            ((w * s).round().max(1.), (h * s).round().max(1.))
        }
        _ => max,
    }
}

/// The box for embed media: tall images get the taller box.
pub fn embed_media_box(m: &EmbedMedia) -> (f32, f32) {
    let tall = matches!((m.width, m.height), (Some(w), Some(h)) if h > w);
    fit_inside(
        m.width,
        m.height,
        if tall {
            EMBED_MEDIA_TALL_MAX
        } else {
            EMBED_MEDIA_MAX
        },
    )
}

fn pixels(logical: f32, scale: f32) -> u32 {
    (logical * scale.max(1.)).ceil().clamp(1., 16_384.) as u32
}

fn request(
    url: Option<String>,
    kind: MediaKind,
    fit: Fit,
    proxy_size: u32,
) -> Option<MediaRequest> {
    let url = Url::parse(&url?).ok()?;
    Some(MediaRequest::new(url, kind, fit).with_proxy_size(proxy_size))
}

/// An attachment URL on the media endpoint, with a resize: the proxy URL first, then
/// the signed URL, both only when they are on the endpoint.
fn attachment_url(proxy: &p::Media, a: &Attachment, t: Transform) -> Option<String> {
    a.proxy_url
        .as_deref()
        .and_then(|u| proxy.attachment(u, t))
        .or_else(|| a.url.as_deref().and_then(|u| proxy.attachment(u, t)))
}

/// The format of every still: WebP, as Fluxer asks for. An image format also turns a
/// video into its first frame (without one the proxy sends the whole video, or refuses
/// an attachment).
const STILL_FORMAT: Option<ImageFormat> = Some(ImageFormat::Webp);

/// The same for embed media: its proxy URL, which may be external media, an instance
/// attachment, or other media of the instance itself (Fluxer leaves a link to its own
/// media host as it is), and only then its URL, when that is on the endpoint too.
fn embed_url(proxy: &p::Media, m: &EmbedMedia, t: Transform) -> Option<String> {
    [m.proxy_url.as_deref(), Some(m.url.as_str())]
        .into_iter()
        .flatten()
        .filter(|u| !u.is_empty())
        .find_map(|u| {
            proxy
                .external(u, t)
                .or_else(|| proxy.attachment(u, t))
                .or_else(|| own_media(proxy, u, t))
        })
}

/// Media of the instance itself (emoji, stickers, avatars) at a URL on the media
/// endpoint, with the transform. `None` for any URL elsewhere.
fn own_media(proxy: &p::Media, u: &str, t: Transform) -> Option<String> {
    let base = Url::parse(proxy.base()).ok()?;
    let mut url = Url::parse(u.trim()).ok()?;
    if !crate::backend::media::on_media_base(&base, &url) {
        return None;
    }
    url.set_fragment(None);
    let keep: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| !matches!(k.as_ref(), "width" | "height" | "format" | "animated"))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    url.set_query(None);
    {
        let mut q = url.query_pairs_mut();
        for (k, v) in &keep {
            q.append_pair(k, v);
        }
        if let Some(w) = t.width {
            q.append_pair("width", &w.clamp(1, 16_384).to_string());
        }
        if let Some(h) = t.height {
            q.append_pair("height", &h.clamp(1, 16_384).to_string());
        }
        if let Some(f) = t.format {
            q.append_pair("format", image_format_name(f));
        }
        if t.animated {
            q.append_pair("animated", "true");
        }
    }
    let mut s: String = url.into();
    if s.ends_with('?') {
        s.pop();
    }
    Some(s)
}

fn image_format_name(f: ImageFormat) -> &'static str {
    match f {
        ImageFormat::Webp => "webp",
        ImageFormat::Png => "png",
        ImageFormat::Gif => "gif",
        ImageFormat::Jpeg => "jpg",
    }
}

/// A user's avatar, `size` logical pixels square. `animated` asks for the animated
/// variant (an `a_` hash), shown while the pointer is over the message.
pub fn avatar(
    proxy: &p::Media,
    a: &UserAvatar,
    size: f32,
    scale: f32,
    animated: bool,
) -> Option<MediaRequest> {
    let px = snap_proxy_size(pixels(size, scale));
    let mut opts = ImageOptions::size(px);
    if animated {
        opts = opts.animated();
    }
    request(
        proxy.avatar(&a.user_id, Some(&a.hash), opts),
        MediaKind::Avatar,
        Fit::square(pixels(size, scale)),
        px,
    )
}

/// A guild icon, `size` logical pixels square.
pub fn guild_icon(
    proxy: &p::Media,
    guild_id: &str,
    hash: &str,
    size: f32,
    scale: f32,
) -> Option<MediaRequest> {
    let px = snap_proxy_size(pixels(size, scale));
    request(
        proxy.guild_icon(guild_id, Some(hash), ImageOptions::size(px)),
        MediaKind::Avatar,
        Fit::square(pixels(size, scale)),
        px,
    )
}

/// A guild banner for a box of `w` x `h` logical pixels (drawn covering the box).
pub fn guild_banner(
    proxy: &p::Media,
    guild_id: &str,
    hash: &str,
    (w, h): (f32, f32),
    scale: f32,
) -> Option<MediaRequest> {
    let (pw, ph) = (pixels(w, scale), pixels(h, scale));
    let px = snap_proxy_size(pw.max(ph));
    request(
        proxy.banner(guild_id, Some(hash), ImageOptions::size(px)),
        MediaKind::Avatar,
        // Covering a wide box: keep enough height for the crop.
        Fit::Contain {
            max_width: px,
            max_height: px,
        },
        px,
    )
}

/// The preview of an image attachment (or a video's first frame), at its size inside
/// 550 x 350. With `animate`, the animated version of an animated image (`None` for
/// anything else); without, always a still.
pub fn attachment_preview(
    proxy: &p::Media,
    a: &Attachment,
    scale: f32,
    animate: bool,
) -> Option<MediaRequest> {
    let video = a.kind == crate::model::AttachmentKind::Video;
    let animated = animate && a.animated && !video;
    if animate && !animated {
        return None;
    }
    let (w, h) = fit_inside(a.width, a.height, PREVIEW_MAX);
    let scale = if animated { 1. } else { scale };
    let (pw, ph) = (pixels(w, scale), pixels(h, scale));
    let t = Transform {
        width: Some(pw),
        height: Some(ph),
        // The still (an image's or a video's first frame) as an image.
        format: if animated { None } else { STILL_FORMAT },
        animated,
    };
    request(
        attachment_url(proxy, a, t),
        MediaKind::Attachment,
        Fit::Contain {
            max_width: pw,
            max_height: ph,
        },
        pw.max(ph),
    )
}

/// The large version of an image attachment for the viewer: the still, or with
/// `animate` the animation of an animated one (smaller, see
/// [`VIEWER_ANIMATED_MAX_PIXELS`]).
pub fn attachment_full(proxy: &p::Media, a: &Attachment, animate: bool) -> Option<MediaRequest> {
    if animate && !a.animated {
        return None;
    }
    let max = if animate {
        VIEWER_ANIMATED_MAX_PIXELS
    } else {
        VIEWER_MAX_PIXELS
    } as f32;
    let (w, h) = fit_inside(a.width, a.height, (max, max));
    let (pw, ph) = (w as u32, h as u32);
    let t = Transform {
        width: Some(pw),
        height: Some(ph),
        format: if animate { None } else { STILL_FORMAT },
        animated: animate,
    };
    request(
        attachment_url(proxy, a, t),
        MediaKind::Attachment,
        Fit::Contain {
            max_width: pw,
            max_height: ph,
        },
        pw.max(ph),
    )
}

/// The attachment's original file on the media endpoint (for saving and opening).
pub fn attachment_original(proxy: &p::Media, a: &Attachment) -> Option<Url> {
    Url::parse(&attachment_url(proxy, a, Transform::default())?).ok()
}

/// An embed image or thumbnail for a box of `max` logical pixels: the still, or with
/// `animate` the animation of an animated one (`None` otherwise).
pub fn embed_image(
    proxy: &p::Media,
    m: &EmbedMedia,
    max: (f32, f32),
    scale: f32,
    animate: bool,
) -> Option<MediaRequest> {
    // A video has no animation the proxy can give, only its first frame.
    if animate && (!m.animated || m.video) {
        return None;
    }
    let (w, h) = fit_inside(m.width, m.height, max);
    let scale = if animate { 1. } else { scale };
    let (pw, ph) = (pixels(w, scale), pixels(h, scale));
    let t = Transform {
        width: Some(pw),
        height: Some(ph),
        format: if animate { None } else { STILL_FORMAT },
        animated: animate,
    };
    request(
        embed_url(proxy, m, t),
        MediaKind::Attachment,
        Fit::Contain {
            max_width: pw,
            max_height: ph,
        },
        pw.max(ph),
    )
}

/// The first frame of an embed video, as an image.
pub fn embed_video_frame(
    proxy: &p::Media,
    m: &EmbedMedia,
    max: (f32, f32),
    scale: f32,
) -> Option<MediaRequest> {
    let (w, h) = fit_inside(m.width, m.height, max);
    let (pw, ph) = (pixels(w, scale), pixels(h, scale));
    let t = Transform {
        width: Some(pw),
        height: Some(ph),
        format: STILL_FORMAT,
        animated: false,
    };
    request(
        embed_url(proxy, m, t),
        MediaKind::Attachment,
        Fit::Contain {
            max_width: pw,
            max_height: ph,
        },
        pw.max(ph),
    )
}

/// The large version of an embed image for the viewer: the still, or with `animate`
/// the animation of an animated one (smaller, see [`VIEWER_ANIMATED_MAX_PIXELS`]).
pub fn embed_full(proxy: &p::Media, m: &EmbedMedia, animate: bool) -> Option<MediaRequest> {
    if animate && (!m.animated || m.video) {
        return None;
    }
    let max = if animate {
        VIEWER_ANIMATED_MAX_PIXELS
    } else {
        VIEWER_MAX_PIXELS
    } as f32;
    let (w, h) = fit_inside(m.width, m.height, (max, max));
    let (pw, ph) = (w as u32, h as u32);
    let t = Transform {
        width: Some(pw),
        height: Some(ph),
        format: if animate { None } else { STILL_FORMAT },
        animated: animate,
    };
    request(
        embed_url(proxy, m, t),
        MediaKind::Attachment,
        Fit::Contain {
            max_width: pw,
            max_height: ph,
        },
        pw.max(ph),
    )
}

/// An embed's media through the proxy, unscaled (for saving).
pub fn embed_original(proxy: &p::Media, m: &EmbedMedia) -> Option<Url> {
    Url::parse(&embed_url(proxy, m, Transform::default())?).ok()
}

/// An embed author or footer icon, `size` logical pixels square.
pub fn embed_icon(
    proxy: &p::Media,
    proxy_url: &str,
    size: f32,
    scale: f32,
) -> Option<MediaRequest> {
    let px = pixels(size, scale);
    let t = Transform {
        width: Some(px),
        height: Some(px),
        ..Default::default()
    };
    request(
        proxy.external(proxy_url, t),
        MediaKind::Avatar,
        Fit::square(px),
        px,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(req: &MediaRequest) -> Vec<(String, String)> {
        req.url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect()
    }

    fn has(req: &MediaRequest, key: &str, value: &str) -> bool {
        query(req).iter().any(|(k, v)| k == key && v == value)
    }

    #[test]
    fn gif_pages_and_videos_ask_for_a_still_frame() {
        let proxy = p::Media::new("https://media.example.org").unwrap();
        // A GIF page (`gifv`) whose only media is its MP4: the proxy is asked for the
        // first frame as WebP, never for the video as an image or as an animation.
        let video = EmbedMedia {
            url: "https://media.tenor.example/loop.mp4".into(),
            proxy_url: Some(
                "https://media.example.org/external/sig/https/media.tenor.example/loop.mp4".into(),
            ),
            width: Some(498),
            height: Some(280),
            video: true,
            ..Default::default()
        };
        let still = embed_image(&proxy, &video, PREVIEW_MAX, 2., false).unwrap();
        assert!(has(&still, "format", "webp"), "{}", still.url);
        assert!(!has(&still, "animated", "true"));
        assert!(embed_image(&proxy, &video, PREVIEW_MAX, 2., true).is_none());
        assert!(embed_full(&proxy, &video, true).is_none());
        assert!(has(
            &embed_full(&proxy, &video, false).unwrap(),
            "format",
            "webp"
        ));

        // A GIF: the animation as it is, the still as WebP.
        let gif = EmbedMedia {
            url: "https://gifs.example/dance.gif".into(),
            proxy_url: Some(
                "https://media.example.org/external/sig/https/gifs.example/dance.gif".into(),
            ),
            width: Some(240),
            height: Some(180),
            animated: true,
            ..Default::default()
        };
        let moving = embed_image(&proxy, &gif, PREVIEW_MAX, 2., true).unwrap();
        assert!(has(&moving, "animated", "true"));
        assert!(!query(&moving).iter().any(|(k, _)| k == "format"));
        let still = embed_image(&proxy, &gif, PREVIEW_MAX, 2., false).unwrap();
        assert!(has(&still, "format", "webp") && has(&still, "width", "480"));

        // The same for a GIF attachment, and the signed query stays.
        let a = Attachment {
            proxy_url: Some(
                "https://media.example.org/attachments/1/2/dance.gif?ex=1&is=2&hm=3".into(),
            ),
            kind: crate::model::AttachmentKind::Image,
            width: Some(240),
            height: Some(180),
            animated: true,
            ..Default::default()
        };
        let still = attachment_preview(&proxy, &a, 2., false).unwrap();
        assert!(has(&still, "format", "webp") && has(&still, "hm", "3"));
        let moving = attachment_preview(&proxy, &a, 2., true).unwrap();
        assert!(has(&moving, "animated", "true") && !has(&moving, "format", "webp"));
    }

    #[test]
    fn embed_media_on_the_instance_itself_loads_and_other_hosts_never_do() {
        let proxy = p::Media::new("https://media.example.org").unwrap();
        // Fluxer leaves a link to its own media host as it is (an emoji GIF, say).
        let own = EmbedMedia {
            url: "https://media.example.org/emojis/123.gif?size=96".into(),
            proxy_url: Some("https://media.example.org/emojis/123.gif?size=96".into()),
            width: Some(96),
            height: Some(96),
            animated: true,
            ..Default::default()
        };
        let req = embed_image(&proxy, &own, PREVIEW_MAX, 1., false).unwrap();
        assert_eq!(req.url.host_str(), Some("media.example.org"));
        assert_eq!(req.url.path(), "/emojis/123.gif");
        assert!(has(&req, "size", "96") && has(&req, "format", "webp"));
        // No proxy URL, but the media's own URL is on the endpoint: used.
        let bare = EmbedMedia {
            proxy_url: None,
            ..own.clone()
        };
        assert!(embed_image(&proxy, &bare, PREVIEW_MAX, 1., false).is_some());
        // A provider's URL as the "proxy" URL, or another port or scheme: refused.
        for bad in [
            "https://media.tenor.example/dance.gif",
            "https://media.example.org:8443/emojis/1.gif",
            "http://media.example.org/emojis/1.gif",
            "https://user@media.example.org/emojis/1.gif",
        ] {
            let m = EmbedMedia {
                url: bad.into(),
                proxy_url: Some(bad.into()),
                ..Default::default()
            };
            assert!(
                embed_image(&proxy, &m, PREVIEW_MAX, 1., false).is_none(),
                "{bad}"
            );
            assert!(embed_original(&proxy, &m).is_none(), "{bad}");
        }
    }

    #[test]
    fn previews_fit_the_box_and_are_never_enlarged() {
        assert_eq!(fit_inside(Some(1100), Some(700), PREVIEW_MAX), (550., 350.));
        assert_eq!(fit_inside(Some(100), Some(50), PREVIEW_MAX), (100., 50.));
        assert_eq!(fit_inside(Some(300), Some(3000), PREVIEW_MAX), (35., 350.));
        assert_eq!(fit_inside(None, Some(10), PREVIEW_MAX), PREVIEW_MAX);
        let tall = EmbedMedia {
            width: Some(400),
            height: Some(900),
            ..Default::default()
        };
        assert_eq!(embed_media_box(&tall), (200., 450.));

        let proxy = p::Media::new("https://media.example.org").unwrap();
        let a = Attachment {
            proxy_url: Some(
                "https://media.example.org/attachments/1/2/v.mp4?ex=1&is=2&hm=3".into(),
            ),
            kind: crate::model::AttachmentKind::Video,
            width: Some(1920),
            height: Some(1080),
            ..Default::default()
        };
        let req = attachment_preview(&proxy, &a, 2.0, false).unwrap();
        assert!(attachment_preview(&proxy, &a, 2.0, true).is_none());
        assert_eq!(
            req.url.as_str(),
            "https://media.example.org/attachments/1/2/v.mp4?ex=1&is=2&hm=3&width=1100&height=618&format=webp"
        );
        assert_eq!(req.proxy_size, Some(1100));
        assert_eq!(
            attachment_original(&proxy, &a).unwrap().as_str(),
            "https://media.example.org/attachments/1/2/v.mp4?ex=1&is=2&hm=3"
        );
    }
}
