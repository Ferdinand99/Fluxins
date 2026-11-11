// SPDX-License-Identifier: AGPL-3.0-or-later
//! Demo media: pictures drawn by the demo itself (no files, no network), served by a
//! fetcher that stands in for the media proxy at `https://media.demo.invalid`. The images
//! still go through `inlet-media` with all its checks, like real ones.
//!
//! Also the messages of `#attachments` (every kind of attachment, a spoiler, explicit
//! media, an expired link that is refreshed, an image too large to decode) and `#embeds`
//! (every embed type).

use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use futures::future::BoxFuture;
use inlet_media::{FetchError, FetchResponse, Fetcher};
use inlet_protocol as p;
use serde_json::{json, Value};
use url::Url;

use super::{message_at, snowflake};
use crate::backend::media::png;
use crate::backend::media::{to_attachment, Ev};

/// The demo's stand-in for the media proxy. `.invalid` never resolves.
pub const MEDIA_BASE: &str = "https://media.demo.invalid";

/// The media URL builder and the fetcher of the demo.
pub fn proxy_and_fetcher() -> (p::Media, Arc<dyn Fetcher>) {
    let proxy = p::Media::new(MEDIA_BASE).expect("the demo media base is valid");
    (proxy, Arc::new(DemoFetcher))
}

/// The avatar hash of a demo person (`a_` = animated), if they have one.
pub fn avatar_hash(name: &str) -> Option<&'static str> {
    match name {
        "ada" => Some("ada1"),
        "linus" => Some("a_linus2"),
        "grace" => Some("grace3"),
        "Sylo" => Some("sylo4"),
        super::ME => Some("demo5"),
        _ => None,
    }
}

/// The icon and banner hashes of a demo guild, by name.
pub fn guild_art(name: &str) -> (Option<&'static str>, Option<&'static str>) {
    match name {
        "Inlet Dev" => (Some("inletdev"), Some("inletbanner")),
        "Rust Norway" => (Some("rustno"), None),
        _ => (None, None),
    }
}

// ---- Pictures ---------------------------------------------------------------------------

fn mix(a: [u8; 3], b: [u8; 3], t: f32) -> [u8; 3] {
    let t = t.clamp(0., 1.);
    let m = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round() as u8;
    [m(a[0], b[0]), m(a[1], b[1]), m(a[2], b[2])]
}

fn paint(w: u32, h: u32, f: impl Fn(f32, f32) -> [u8; 3]) -> Vec<u8> {
    let mut out = Vec::with_capacity(w as usize * h as usize * 4);
    for y in 0..h {
        for x in 0..w {
            let c = f(x as f32 / w as f32, y as f32 / h as f32);
            out.extend([c[0], c[1], c[2], 255]);
        }
    }
    out
}

/// A landscape: sky, sun, two rows of hills and a lake.
fn landscape(w: u32, h: u32, hue: f32) -> Vec<u8> {
    let aspect = w as f32 / h as f32;
    paint(w, h, |x, y| {
        let sky = mix([255, 170, 110], [70, 90, 190], 1. - y * 1.6 + hue * 0.2);
        let (sx, sy) = (0.7, 0.32);
        let d = (((x - sx) * aspect).powi(2) + (y - sy).powi(2)).sqrt();
        let mut c = if d < 0.09 {
            [255, 236, 170]
        } else {
            mix(sky, [255, 220, 160], (0.25 - d).max(0.) * 2.)
        };
        let far = 0.55 + 0.06 * (x * 9. + hue).sin() + 0.03 * (x * 23.).sin();
        if y > far {
            c = mix([96, 80, 150], [60, 50, 110], (y - far) * 4.);
        }
        let near = 0.68 + 0.08 * (x * 5. + 1. + hue).sin();
        if y > near {
            c = mix([40, 110, 80], [20, 60, 50], (y - near) * 3.);
        }
        if y > 0.86 {
            c = mix([60, 120, 190], [30, 60, 120], (y - 0.86) * 6.);
        }
        c
    })
}

/// Soft diagonal bands in two colours.
fn bands(w: u32, h: u32, a: [u8; 3], b: [u8; 3]) -> Vec<u8> {
    paint(w, h, |x, y| {
        let t = ((x + y) * 6.).sin() * 0.5 + 0.5;
        mix(a, b, t * 0.8 + y * 0.2)
    })
}

/// A person: a head and shoulders on a coloured background.
fn person(size: u32, bg: [u8; 3], fg: [u8; 3], phase: f32) -> Vec<u8> {
    paint(size, size, |x, y| {
        let head = ((x - 0.5).powi(2) + (y - 0.4).powi(2)).sqrt();
        let body = ((x - 0.5).powi(2) + ((y - 1.05) * 0.8).powi(2)).sqrt();
        let glow = (phase + x * 4.).sin() * 0.15;
        if head < 0.18 || body < 0.38 {
            mix(fg, [255, 255, 255], 0.15 + glow)
        } else {
            mix(bg, [0, 0, 0], y * 0.3 - glow)
        }
    })
}

/// A guild icon: a ring and a dot.
fn emblem(size: u32, bg: [u8; 3], fg: [u8; 3]) -> Vec<u8> {
    paint(size, size, |x, y| {
        let d = ((x - 0.5).powi(2) + (y - 0.5).powi(2)).sqrt();
        if (0.22..0.3).contains(&d) || d < 0.1 {
            fg
        } else {
            mix(bg, [0, 0, 0], y * 0.25)
        }
    })
}

/// A cat-ish loop for the GIF.
fn bouncing(size: u32, frames: usize) -> Vec<Vec<u8>> {
    (0..frames)
        .map(|i| {
            let t = i as f32 / frames as f32 * std::f32::consts::TAU;
            let cy = 0.55 - t.sin().abs() * 0.25;
            paint(size, size, |x, y| {
                let d = ((x - 0.5).powi(2) + (y - cy).powi(2)).sqrt();
                if d < 0.16 {
                    [250, 180, 60]
                } else if y > 0.78 {
                    [90, 70, 60]
                } else {
                    mix([120, 200, 230], [200, 230, 250], y)
                }
            })
        })
        .collect()
}

fn png_of(w: u32, h: u32, rgba: Vec<u8>) -> Vec<u8> {
    png::encode(w, h, &rgba)
}

/// The bytes the demo proxy serves for a path (after the base), or `None` (404).
fn serve(path: &str, query: &str) -> Option<Vec<u8>> {
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let animated = query.contains("animated=true");
    let file = segments.last().copied().unwrap_or_default();
    let stem = file.split('.').next().unwrap_or_default();
    Some(match segments.first().copied()? {
        "avatars" => {
            let (bg, fg) = match stem.trim_start_matches("a_") {
                "ada1" => ([150, 60, 120], [250, 210, 230]),
                "linus2" => ([40, 110, 170], [200, 230, 250]),
                "grace3" => ([30, 130, 100], [210, 250, 230]),
                "sylo4" => ([230, 120, 30], [255, 240, 220]),
                _ => ([80, 70, 150], [230, 225, 255]),
            };
            if animated && stem.starts_with("a_") {
                let frames: Vec<Vec<u8>> = (0..8)
                    .map(|i| person(128, bg, fg, i as f32 * 0.8))
                    .collect();
                png::encode_apng(128, 128, &frames, 120)
            } else {
                png_of(128, 128, person(128, bg, fg, 0.))
            }
        }
        "icons" => {
            let (bg, fg) = match stem {
                "inletdev" => ([60, 50, 140], [255, 160, 70]),
                _ => ([170, 40, 40], [250, 240, 240]),
            };
            png_of(128, 128, emblem(128, bg, fg))
        }
        "banners" => png_of(640, 256, landscape(640, 256, 1.3)),
        "attachments" => {
            if query.contains("hm=expired") {
                return None;
            }
            match stem {
                "weekend" => png_of(1280, 720, landscape(1280, 720, 0.)),
                "lighthouse" => png_of(600, 900, bands(600, 900, [40, 60, 120], [250, 200, 120])),
                // The proxy gives the first frame unless asked for the animation.
                "cat" if animated => png::encode_apng(160, 160, &bouncing(160, 12), 80),
                "cat" => png_of(160, 160, bouncing(160, 1).remove(0)),
                "clip" => png_of(960, 540, landscape(960, 540, 2.4)),
                "finale" => png_of(800, 450, bands(800, 450, [120, 30, 60], [250, 120, 80])),
                "beach" => png_of(800, 450, landscape(800, 450, 4.)),
                "fresh" | "expired" => {
                    png_of(480, 270, bands(480, 270, [20, 90, 90], [140, 230, 200]))
                }
                "huge" => png::claiming(100_000, 50_000),
                "screenshot" => png_of(640, 360, bands(640, 360, [30, 30, 40], [90, 80, 160])),
                _ => return None,
            }
        }
        "external" => match stem {
            "author" => png_of(64, 64, emblem(64, [230, 120, 30], [255, 255, 255])),
            "footer" => png_of(64, 64, emblem(64, [60, 50, 140], [255, 255, 255])),
            "thumb" => png_of(160, 160, bands(160, 160, [241, 196, 15], [180, 90, 20])),
            "article" => png_of(800, 420, landscape(800, 420, 3.)),
            "picture" => png_of(900, 600, landscape(900, 600, 5.)),
            "gif" if animated => png::encode_apng(240, 240, &bouncing(240, 12), 80),
            "gif" => png_of(240, 240, bouncing(240, 1).remove(0)),
            "video" => png_of(1280, 720, landscape(1280, 720, 0.7)),
            "sky" => png_of(800, 450, bands(800, 450, [60, 120, 230], [220, 240, 255])),
            _ => return None,
        },
        "emojis" => super::emoji::emoji_picture(stem, animated)?,
        "stickers" => super::emoji::sticker_picture(stem, animated)?,
        _ => return None,
    })
}

/// Serves the demo pictures. Never touches the network.
struct DemoFetcher;

impl Fetcher for DemoFetcher {
    fn fetch<'a>(&'a self, url: &'a Url) -> BoxFuture<'a, Result<FetchResponse, FetchError>> {
        let found = url
            .as_str()
            .strip_prefix(MEDIA_BASE)
            .and_then(|_| serve(url.path(), url.query().unwrap_or_default()));
        Box::pin(async move {
            match found {
                Some(bytes) => Ok(FetchResponse::from_bytes(Bytes::from(bytes))),
                None => Err(FetchError::Status(404)),
            }
        })
    }
}

// ---- Messages -------------------------------------------------------------------------

/// An attachment as the API sends it, on the demo media proxy.
pub fn attachment(channel: &str, id: u64, filename: &str, kind: &str, extra: Value) -> Value {
    let ch = snowflake(channel);
    let url = format!("{MEDIA_BASE}/attachments/{ch}/{id}/{filename}?ex=7a00&is=6a00&hm=demo");
    let mut v = json!({
        "id": id.to_string(),
        "filename": filename,
        "content_type": kind,
        "size": 0,
        "url": url,
        "proxy_url": url,
    });
    if let (Some(obj), Some(more)) = (v.as_object_mut(), extra.as_object()) {
        for (k, val) in more {
            obj.insert(k.clone(), val.clone());
        }
    }
    v
}

fn external(name: &str) -> String {
    format!("{MEDIA_BASE}/external/demo-signature/https/media.example/{name}.png")
}

fn media_json(name: &str, w: u32, h: u32) -> Value {
    json!({
        "url": format!("https://media.example/{name}.png"),
        "proxy_url": external(name),
        "width": w,
        "height": h,
    })
}

fn at(time: &str) -> i64 {
    super::at(time)
}

fn attachments_channel() -> Vec<p::Message> {
    let ch = "c-attachments";
    let with = |seq, author: &str, time: &str, text: &str, atts: Vec<Value>| {
        message_at(
            seq,
            ch,
            author,
            at(time),
            text,
            json!({ "attachments": atts }),
        )
    };
    vec![
        with(
            40,
            "linus",
            "09:58",
            "And one that says it is far too large to decode: the header is enough to refuse it.",
            vec![attachment(
                ch,
                8001,
                "huge.png",
                "image/png",
                json!({"width": 100_000, "height": 50_000, "size": 4096}),
            )],
        ),
        with(
            45,
            "ada",
            "10:00",
            "Photos from the weekend!",
            vec![
                attachment(
                    ch,
                    8009,
                    "weekend.png",
                    "image/png",
                    json!({"width": 1280, "height": 720, "size": 1_048_576, "description": "Sunset over the hills by the lake"}),
                ),
                attachment(
                    ch,
                    8010,
                    "lighthouse.png",
                    "image/png",
                    json!({"width": 600, "height": 900, "size": 700_000}),
                ),
            ],
        ),
        with(
            41,
            "Sylo",
            "10:02",
            "This link has expired; Inlet asks for a fresh one, once.",
            vec![{
                let mut a = attachment(
                    ch,
                    8002,
                    "expired.png",
                    "image/png",
                    json!({"width": 480, "height": 270, "size": 20_000, "expires_at": "2026-01-01T00:00:00+00:00"}),
                );
                for key in ["url", "proxy_url"] {
                    let u = a[key]
                        .as_str()
                        .unwrap_or_default()
                        .replace("hm=demo", "hm=expired");
                    a[key] = json!(u);
                }
                a
            }],
        ),
        with(
            43,
            "ada",
            "10:08",
            "Spoiler for the finale, one marked sensitive, and the script.",
            vec![
                attachment(
                    ch,
                    8005,
                    "finale.png",
                    "image/png",
                    json!({"width": 330, "height": 186, "size": 300_000, "flags": 8}),
                ),
                attachment(
                    ch,
                    8006,
                    "beach.png",
                    "image/png",
                    json!({"width": 330, "height": 186, "size": 300_000, "flags": 16}),
                ),
                attachment(
                    ch,
                    8011,
                    "finale-script.pdf",
                    "application/pdf",
                    json!({"size": 120_000, "flags": 8}),
                ),
            ],
        ),
        // A link in a spoiler: its embed is covered with the text.
        message_at(
            46,
            ch,
            "grace",
            at("10:10"),
            "How it ends: ||https://blog.example/finale||",
            json!({ "embeds": [{
                "type": "link",
                "url": "https://blog.example/finale",
                "provider": {"name": "Example Blog"},
                "title": "The finale, explained",
                "description": "Everything that happens in the last episode.",
                "thumbnail": media_json("thumb", 160, 160)
            }] }),
        ),
        with(
            44,
            "linus",
            "10:04",
            "The cat, and a clip of it.",
            vec![
                attachment(
                    ch,
                    8007,
                    "cat.gif",
                    "image/gif",
                    json!({"width": 160, "height": 160, "size": 90_000, "flags": 32}),
                ),
                attachment(
                    ch,
                    8008,
                    "clip.mp4",
                    "video/mp4",
                    json!({"width": 480, "height": 270, "size": 6_291_456, "duration": 42}),
                ),
            ],
        ),
        with(
            42,
            "grace",
            "10:06",
            "Release notes and the recording of the talk.",
            vec![
                attachment(
                    ch,
                    8003,
                    "release-notes.pdf",
                    "application/pdf",
                    json!({"size": 253_952}),
                ),
                attachment(
                    ch,
                    8004,
                    "talk.mp3",
                    "audio/mpeg",
                    json!({"size": 4_404_019, "duration": 187}),
                ),
            ],
        ),
    ]
}

fn embeds_channel() -> Vec<p::Message> {
    let ch = "c-embeds";
    let with = |seq, author: &str, time: &str, text: &str, embed: Value| {
        message_at(
            seq,
            ch,
            author,
            at(time),
            text,
            json!({ "embeds": [embed] }),
        )
    };
    vec![
        with(
            50,
            "ada",
            "11:06",
            "",
            json!({
                "type": "bluesky",
                "url": "https://bsky.example/profile/ada/post/1",
                "color": 0x1185FE,
                "author": {"name": "Ada (@ada.bsky.example)", "url": "https://bsky.example/profile/ada", "proxy_icon_url": external("author")},
                "description": "Native clients are back. Smooth scrolling, low memory, and it works with Fluxer.",
                "image": media_json("sky", 800, 450),
                "footer": {"text": "Bluesky", "proxy_icon_url": external("footer")},
                "timestamp": "2026-11-10T10:30:00+00:00"
            }),
        ),
        with(
            51,
            "linus",
            "11:10",
            "",
            json!({"type": "gifv", "url": "https://gifs.example/cat", "provider": {"name": "Gifs"},
                   "thumbnail": {"url": "https://gifs.example/cat.gif", "proxy_url": external("gif"), "width": 240, "height": 240, "flags": 32},
                   "video": {"url": "https://gifs.example/cat.mp4", "proxy_url": external("gif"), "width": 240, "height": 240}}),
        ),
        with(
            52,
            "grace",
            "11:09",
            "",
            json!({"type": "image", "url": "https://images.example/hills.png",
                   "thumbnail": media_json("picture", 900, 600)}),
        ),
        with(
            53,
            "ada",
            "11:05",
            "",
            json!({
                "type": "article",
                "url": "https://news.example/native-clients",
                "provider": {"name": "News Example"},
                "title": "Why native chat clients are coming back",
                "description": "Memory use, battery life and startup time all point the same way.",
                "thumbnail": media_json("article", 800, 420)
            }),
        ),
        with(
            54,
            "grace",
            "11:08",
            "",
            json!({
                "type": "video",
                "url": "https://video.example/watch?v=inlet",
                "color": 0xFF0000,
                "provider": {"name": "VideoSite"},
                "author": {"name": "Inlet Dev"},
                "title": "Inlet in five minutes",
                "thumbnail": media_json("video", 1280, 720),
                "video": {"url": "https://video.example/embed/inlet", "width": 1280, "height": 720}
            }),
        ),
        with(
            55,
            "linus",
            "11:02",
            "https://blog.example/rust-gpui",
            json!({
                "type": "link",
                "url": "https://blog.example/rust-gpui",
                "provider": {"name": "Example Blog"},
                "title": "Building a desktop app with gpui",
                "description": "Layout, text, images and a list that stays smooth with thousands of rows.",
                "thumbnail": media_json("thumb", 160, 160)
            }),
        ),
        with(
            56,
            "Inlet",
            "11:01",
            "",
            json!({"title": "No colour", "description": "An embed without a colour gets the brand bar."}),
        ),
        with(
            57,
            "Sylo",
            "11:00",
            "",
            json!({
                "type": "rich",
                "color": 0xF1C40F,
                "author": {"name": "Sylo", "url": "https://sylo.example", "proxy_icon_url": external("author")},
                "title": "Server stats",
                "url": "https://sylo.example/stats",
                "description": "Weekly summary for **Inlet Dev**.",
                "fields": [
                    {"name": "Members", "value": "1 204", "inline": true},
                    {"name": "Messages", "value": "18 377", "inline": true},
                    {"name": "Voice hours", "value": "212", "inline": true},
                    {"name": "Top channel", "value": "#general, with 6 112 messages", "inline": false}
                ],
                "thumbnail": media_json("thumb", 160, 160),
                "footer": {"text": "Sylo", "proxy_icon_url": external("footer")},
                "timestamp": "2026-11-10T11:00:00+00:00"
            }),
        ),
        with(
            58,
            "linus",
            "11:11",
            "",
            json!({
                "type": "video",
                "url": "https://video.example/watch?v=sensitive",
                "provider": {"name": "VideoSite"},
                "title": "Marked sensitive",
                "nsfw": true,
                "thumbnail": media_json("video", 1280, 720),
                "video": {"url": "https://video.example/embed/sensitive", "width": 1280, "height": 720}
            }),
        ),
    ]
}

/// Adds `#attachments` and `#embeds`.
pub fn messages(messages: &mut HashMap<String, Vec<p::Message>>) {
    messages.insert("c-attachments".into(), attachments_channel());
    messages.insert("c-embeds".into(), embeds_channel());
}

/// The demo's answer to a refresh: the message's attachments with working URLs.
pub fn refreshed(channel_id: &str, message_id: &str) -> Ev {
    let found = (channel_id == snowflake("c-attachments"))
        .then(attachments_channel)
        .unwrap_or_default()
        .into_iter()
        .find(|m| m.id == message_id);
    match found {
        Some(m) => Ev::Refreshed {
            message_id: message_id.to_string(),
            attachments: m
                .attachments
                .iter()
                .map(|a| {
                    let mut a = to_attachment(a);
                    a.url = a.url.map(|u| u.replace("hm=expired", "hm=fresh"));
                    a.proxy_url = a.proxy_url.map(|u| u.replace("hm=expired", "hm=fresh"));
                    a.expires_at_ms = None;
                    a
                })
                .collect(),
        },
        None => Ev::RefreshFailed {
            message_id: message_id.to_string(),
        },
    }
}

/// The attachment of `#general`'s screenshot message.
pub fn screenshot() -> Value {
    attachment(
        "c-general",
        7001,
        "screenshot.png",
        "image/png",
        json!({"width": 640, "height": 360, "size": 48_213}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_pictures_are_served_and_decode() {
        let limits = inlet_media::KindLimits::for_kind(inlet_media::MediaKind::Attachment);
        let avatar = inlet_media::KindLimits::for_kind(inlet_media::MediaKind::Avatar);
        for (path, query, kind) in [
            ("/avatars/2001/ada1.webp", "size=48", &avatar),
            (
                "/avatars/2002/a_linus2.webp",
                "size=48&animated=true",
                &avatar,
            ),
            ("/icons/1/inletdev.webp", "size=48", &avatar),
            ("/banners/1/inletbanner.webp", "size=640", &avatar),
            ("/attachments/1/2/weekend.png", "hm=demo", &limits),
            ("/attachments/1/2/cat.gif", "hm=demo", &limits),
            ("/attachments/1/2/expired.png", "hm=fresh", &limits),
            ("/external/s/https/media.example/article.png", "", &limits),
        ] {
            let bytes = serve(path, query).unwrap_or_else(|| panic!("{path}"));
            inlet_media::inspect(Bytes::from(bytes), kind)
                .unwrap_or_else(|e| panic!("{path}: {e}"));
        }
        assert!(serve("/attachments/1/2/expired.png", "hm=expired").is_none());
        let huge = Bytes::from(serve("/attachments/1/2/huge.png", "").unwrap());
        assert!(matches!(
            inlet_media::inspect(huge, &limits),
            Err(inlet_media::MediaError::BadDimensions { .. })
        ));
    }
}
