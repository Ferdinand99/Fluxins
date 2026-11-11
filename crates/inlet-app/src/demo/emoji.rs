// SPDX-License-Identifier: AGPL-3.0-or-later
//! Demo custom emoji and stickers: their pictures (drawn here, served by the demo media
//! proxy) and the messages of two scenes in `#development`:
//!
//! - `custom-emoji`: still and animated custom emoji inline, in a quote and large
//!   (emoji only), next to Twemoji; stickers in every format the proxy may hold (PNG,
//!   APNG, GIF, WebP and animated WebP).
//! - `emoji-100`: one message with 100 different animated custom emoji, for measuring
//!   what the animations cost (at most 32 move at once).

use inlet_protocol as p;
use serde_json::{json, Value};

use super::markdown::SCENE_CHANNEL;
use super::{msg, msg_with};
use crate::backend::media::png;

/// The still custom emoji `:inlet:`.
const INLET: &str = "1234567890123456789";
/// Animated custom emoji.
const PARTY: &str = "1234567890123456790";
const WAVE: &str = "1234567890123456791";
const SPIN: &str = "1234567890123456792";
/// The 100 animated emoji of `emoji-100` are this plus 0 to 99.
const MANY_BASE: u64 = 1_334_567_890_123_456_000;

/// Stickers, one per format the uploader may have sent.
const STICKER_PNG: &str = "1434567890123456001";
const STICKER_APNG: &str = "1434567890123456002";
const STICKER_GIF: &str = "1434567890123456003";
const STICKER_WEBP: &str = "1434567890123456004";
const STICKER_AWEBP: &str = "1434567890123456005";

fn paint(n: u32, f: impl Fn(f32, f32) -> [u8; 4]) -> Vec<u8> {
    (0..n * n)
        .flat_map(|i| {
            let (x, y) = ((i % n) as f32 / n as f32, (i / n) as f32 / n as f32);
            f(x, y)
        })
        .collect()
}

/// A round face whose colour turns with `phase` (0 to 1) and whose mouth opens and
/// closes; transparent around it.
fn face(n: u32, hue: f32, phase: f32) -> Vec<u8> {
    let turn = (hue + phase) * std::f32::consts::TAU;
    let base = [
        (170. + 80. * turn.cos()) as u8,
        (170. + 80. * (turn + 2.1).cos()) as u8,
        (170. + 80. * (turn + 4.2).cos()) as u8,
    ];
    let mouth = 0.04 + 0.06 * (phase * std::f32::consts::TAU).sin().abs();
    paint(n, |x, y| {
        let d = ((x - 0.5).powi(2) + (y - 0.5).powi(2)).sqrt();
        let eye = ((x - 0.36).powi(2) + (y - 0.4).powi(2)).sqrt() < 0.06
            || ((x - 0.64).powi(2) + (y - 0.4).powi(2)).sqrt() < 0.06;
        let mouth = (y - 0.66).abs() < mouth && (x - 0.5).abs() < 0.18;
        if d > 0.46 {
            [0, 0, 0, 0]
        } else if eye || mouth {
            [40, 30, 40, 255]
        } else {
            [base[0], base[1], base[2], 255]
        }
    })
}

/// A sticker: a rounded card with a ball that bounces with `phase`.
fn card(n: u32, colour: [u8; 3], phase: f32) -> Vec<u8> {
    let cy = 0.6 - (phase * std::f32::consts::PI).sin().abs() * 0.3;
    paint(n, |x, y| {
        let corner = (x - x.clamp(0.15, 0.85)).hypot(y - y.clamp(0.15, 0.85));
        if corner > 0.12 {
            [0, 0, 0, 0]
        } else if (x - 0.5).hypot(y - cy) < 0.14 {
            [255, 250, 240, 255]
        } else {
            [colour[0], colour[1], colour[2], 255]
        }
    })
}

/// The custom emoji picture the demo proxy serves for `id`: an animation of 8 frames
/// when asked for one (and the emoji is animated), else the still.
pub fn emoji_picture(id: &str, animated: bool) -> Option<Vec<u8>> {
    let n: u64 = id.parse().ok()?;
    let hue = ((n % 97) as f32 * 0.29).fract();
    const SIZE: u32 = 48;
    if id == INLET || !animated {
        return Some(png::encode(SIZE, SIZE, &face(SIZE, hue, 0.)));
    }
    let frames: Vec<Vec<u8>> = (0..8).map(|i| face(SIZE, hue, i as f32 / 8.)).collect();
    Some(png::encode_apng(SIZE, SIZE, &frames, 90))
}

/// The sticker picture the demo proxy serves for `id`, in the format of its kind
/// (every one under a `.webp` URL, as the real proxy).
pub fn sticker_picture(id: &str, animated: bool) -> Option<Vec<u8>> {
    const N: u32 = 160;
    // Ten frames; the APNG writer does not compress, so it gets four to stay under the
    // 512 KiB a sticker may have.
    let frames = |colour, count: u32| -> Vec<Vec<u8>> {
        (0..count)
            .map(|i| card(N, colour, i as f32 / count as f32))
            .collect()
    };
    let still = |colour| card(N, colour, 0.);
    Some(match id {
        STICKER_PNG => png::encode(N, N, &still([88, 101, 242])),
        STICKER_APNG if animated => png::encode_apng(N, N, &frames([59, 165, 93], 4), 120),
        STICKER_APNG => png::encode(N, N, &still([59, 165, 93])),
        STICKER_GIF if animated => {
            png::encode_gif(N as u16, N as u16, &frames([237, 66, 69], 10), 8)
        }
        STICKER_GIF => png::encode_gif(N as u16, N as u16, &[still([237, 66, 69])], 8),
        STICKER_WEBP => png::encode_webp(N, N, &still([250, 168, 26])),
        STICKER_AWEBP if animated => {
            png::encode_animated_webp(N, N, &frames([235, 69, 158], 10), 80)
        }
        STICKER_AWEBP => png::encode_webp(N, N, &still([235, 69, 158])),
        _ => return None,
    })
}

fn sticker(id: &str, name: &str, animated: bool) -> Value {
    json!({"id": id, "name": name, "animated": animated})
}

/// `#development` for the `custom-emoji` scene.
pub fn custom() -> Vec<p::Message> {
    let ch = SCENE_CHANNEL;
    vec![
        msg(
            41,
            ch,
            "ada",
            "12:00",
            &format!(
                "Custom emoji in text: <:inlet:{INLET}> still, <a:party:{PARTY}> <a:wave:{WAVE}> animated, next to Twemoji \u{1F389}"
            ),
        ),
        msg(
            42,
            ch,
            "linus",
            "12:01",
            &format!("> quoted <a:spin:{SPIN}> and **bold <:inlet:{INLET}>**"),
        ),
        msg(
            43,
            ch,
            "grace",
            "12:02",
            &format!("<a:party:{PARTY}> <:inlet:{INLET}> <a:spin:{SPIN}>"),
        ),
        msg_with(
            44,
            ch,
            "ada",
            "12:03",
            "Stickers: PNG, APNG, GIF",
            json!({"stickers": [
                sticker(STICKER_PNG, "png", false),
                sticker(STICKER_APNG, "apng", true),
                sticker(STICKER_GIF, "gif", true),
            ]}),
        ),
        msg_with(
            45,
            ch,
            "linus",
            "12:04",
            "",
            json!({"stickers": [
                sticker(STICKER_WEBP, "webp", false),
                sticker(STICKER_AWEBP, "animated webp", true),
            ]}),
        ),
    ]
}

/// `#development` for the `emoji-100` scene: 100 different animated emoji at once.
pub fn hundred() -> Vec<p::Message> {
    let text: Vec<String> = (0..100)
        .map(|i| format!("<a:e{i}:{}>", MANY_BASE + i))
        .collect();
    vec![msg(51, SCENE_CHANNEL, "grace", "12:10", &text.join(" "))]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_emoji_and_stickers_are_served_in_every_format() {
        let emoji = inlet_media::KindLimits::for_kind(inlet_media::MediaKind::Emoji);
        let control = inlet_media::DecodeControl::with_budget(std::time::Duration::from_secs(5));
        let decode = |bytes: Vec<u8>| {
            inlet_media::decode(&bytes, inlet_media::Fit::square(320), &emoji, &control)
                .map(|d| d.frames.len())
        };
        assert_eq!(decode(emoji_picture(INLET, true).unwrap()).unwrap(), 1);
        assert_eq!(decode(emoji_picture(PARTY, true).unwrap()).unwrap(), 8);
        assert_eq!(decode(emoji_picture(PARTY, false).unwrap()).unwrap(), 1);
        assert!(emoji_picture("../x", true).is_none());
        for (id, frames) in [
            (STICKER_PNG, 1),
            (STICKER_APNG, 4),
            (STICKER_GIF, 10),
            (STICKER_WEBP, 1),
            (STICKER_AWEBP, 10),
        ] {
            assert_eq!(
                decode(sticker_picture(id, true).unwrap()).unwrap(),
                frames,
                "{id}"
            );
            assert_eq!(
                decode(sticker_picture(id, false).unwrap()).unwrap(),
                1,
                "{id}"
            );
        }
        // The scenes' messages carry what they promise.
        let hundred = hundred();
        let blocks = crate::ui::markdown::parse(&hundred[0].content);
        let custom = crate::ui::markdown::custom_emoji(&blocks);
        assert_eq!(custom.len(), 100);
        assert!(custom.iter().all(|(_, animated)| *animated));
        let stickers: usize = custom_stickers().len();
        assert_eq!(stickers, 5);
    }

    fn custom_stickers() -> Vec<p::models::StickerItem> {
        custom().iter().flat_map(|m| m.stickers()).collect()
    }
}
