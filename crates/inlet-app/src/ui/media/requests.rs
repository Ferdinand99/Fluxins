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
//! `animated` off). Animated requests ask for the logical size only, never times the
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

/// The same for embed media, which may be external media or an instance attachment.
fn embed_url(proxy: &p::Media, proxy_url: Option<&str>, t: Transform) -> Option<String> {
    let u = proxy_url?;
    proxy.external(u, t).or_else(|| proxy.attachment(u, t))
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

/// A person's profile banner for a box of `w` x `h` logical pixels (drawn covering the
/// box): the one of their profile in `guild_id`, or their account's.
pub fn profile_banner(
    proxy: &p::Media,
    guild_id: Option<&str>,
    user_id: &str,
    hash: &str,
    (w, h): (f32, f32),
    scale: f32,
) -> Option<MediaRequest> {
    let px = snap_proxy_size(pixels(w, scale).max(pixels(h, scale)));
    let opts = ImageOptions::size(px);
    let url = match guild_id {
        Some(g) => proxy.member_banner(g, user_id, Some(hash), opts),
        None => proxy.banner(user_id, Some(hash), opts),
    };
    request(
        url,
        MediaKind::Avatar,
        Fit::Contain {
            max_width: px,
            max_height: px,
        },
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
        // An image format makes the proxy return a video's first frame.
        format: video.then_some(ImageFormat::Webp),
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
        format: None,
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
    if animate && !m.animated {
        return None;
    }
    let (w, h) = fit_inside(m.width, m.height, max);
    let scale = if animate { 1. } else { scale };
    let (pw, ph) = (pixels(w, scale), pixels(h, scale));
    let t = Transform {
        width: Some(pw),
        height: Some(ph),
        format: None,
        animated: animate,
    };
    request(
        embed_url(proxy, m.proxy_url.as_deref(), t),
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
        format: Some(ImageFormat::Webp),
        animated: false,
    };
    request(
        embed_url(proxy, m.proxy_url.as_deref(), t),
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
    if animate && !m.animated {
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
        format: None,
        animated: animate,
    };
    request(
        embed_url(proxy, m.proxy_url.as_deref(), t),
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
    Url::parse(&embed_url(
        proxy,
        m.proxy_url.as_deref(),
        Transform::default(),
    )?)
    .ok()
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
