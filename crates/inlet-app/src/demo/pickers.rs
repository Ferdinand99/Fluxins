// SPDX-License-Identifier: AGPL-3.0-or-later
//! The demo's answers for the pickers (scenes `picker-emoji` and `picker-gif`): an
//! instance with a GIF service, a few featured search terms and GIFs on the demo media
//! proxy, and synced preferences with favourite and frequently used emoji. Edits are
//! not kept.
//!
//! When animated emoji are held still (`INLET_DEMO_ANIMATE_EMOJI=0`, every scene file of
//! the visual checks), the demo GIFs are stills too, so a screenshot does not depend on
//! when it was taken.

use inlet_protocol as p;
use serde_json::{json, Value};

use super::media::external;
use crate::backend::composer::gif::{Cmd, Ev, Setup};
use crate::backend::composer::Ev as ComposerEv;
use crate::backend::Ui;

/// Featured search terms and the demo picture of each.
const CATEGORIES: [(&str, &str); 6] = [
    ("happy", "picture"),
    ("thumbs up", "thumb"),
    ("wow", "sky"),
    ("party", "article"),
    ("dance", "video"),
    ("thank you", "author"),
];
/// Favourite emoji of the demo account.
const FAVOURITES: [&str; 3] = ["unicode:heart", "unicode:tada", "unicode:rocket"];
/// Frequently used emoji, most first.
const USED: [(&str, u32); 5] = [
    ("unicode:joy", 9),
    ("unicode:fire", 7),
    ("unicode:eyes", 5),
    ("unicode:wave", 3),
    ("unicode:smile", 2),
];

fn moving() -> bool {
    std::env::var("INLET_DEMO_ANIMATE_EMOJI").as_deref() != Ok("0")
}

/// A demo GIF on the demo media proxy.
fn gif(n: usize, stem: &str) -> p::Gif {
    let src = external(stem);
    serde_json::from_value(json!({
        "id": format!("demo-{n}"),
        "slug": format!("demo-gif-{n}"),
        "provider": "demo",
        "title": format!("Demo {n}"),
        "url": format!("https://gifs.example/demo-gif-{n}"),
        "proxy_src": src,
        "width": 240, "height": 240,
        "media": {"tinygif": {"proxy_src": src, "width": 240, "height": 240}}
    }))
    .expect("a demo GIF")
}

fn gifs() -> Vec<p::Gif> {
    let first = if moving() { "gif" } else { "thumb" };
    [
        first, "picture", "sky", "article", "author", "video", "footer", "thumb",
    ]
    .iter()
    .enumerate()
    .map(|(n, stem)| gif(n, stem))
    .collect()
}

fn categories() -> Vec<p::api::gifs::GifCategory> {
    CATEGORIES
        .iter()
        .map(|(name, stem)| {
            serde_json::from_value(json!({"name": name, "proxy_src": external(stem)}))
                .expect("a demo category")
        })
        .collect()
}

// A few protobuf fields by hand (the app has no protobuf encoder): enough for the
// emoji picker state of the demo account.
fn varint(mut v: u64, out: &mut Vec<u8>) {
    while v >= 0x80 {
        out.push((v as u8 & 0x7f) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn field_bytes(field: u64, data: &[u8], out: &mut Vec<u8>) {
    varint(field << 3 | 2, out);
    varint(data.len() as u64, out);
    out.extend_from_slice(data);
}

fn field_varint(field: u64, v: u64, out: &mut Vec<u8>) {
    varint(field << 3, out);
    varint(v, out);
}

/// The demo account's synced preferences.
pub fn preferences() -> p::SyncedPreferences {
    let now = chrono::Utc::now().timestamp_millis().max(0) as u64;
    let mut picker = Vec::new();
    for (key, count) in USED {
        let mut stat = Vec::new();
        field_varint(1, u64::from(count), &mut stat);
        field_varint(2, now - 60_000, &mut stat);
        let mut entry = Vec::new();
        field_bytes(1, key.as_bytes(), &mut entry);
        field_bytes(2, &stat, &mut entry);
        field_bytes(1, &entry, &mut picker);
    }
    for key in FAVOURITES {
        field_bytes(2, key.as_bytes(), &mut picker);
    }
    let mut prefs = Vec::new();
    field_bytes(20, &picker, &mut prefs);
    p::SyncedPreferences::decode(&prefs).expect("the demo preferences decode")
}

/// The guild emoji and stickers of the demo's first guild, for READY.
pub fn guild_expressions() -> (Value, Value) {
    super::emoji::guild_expressions()
}

/// Answers a picker command of the demo account.
pub fn answer(events: &Ui, cmd: Cmd) {
    let ev = match cmd {
        Cmd::Setup { .. } => Ev::Setup(Setup {
            gif_enabled: true,
            gif_attribution: None,
            global_expressions: false,
        }),
        Cmd::SearchGifs { nonce, .. } | Cmd::TrendingGifs { nonce, .. } => Ev::Gifs {
            nonce,
            result: Ok(gifs()),
        },
        Cmd::FeaturedGifs { nonce, .. } => Ev::Categories {
            nonce,
            result: Ok(categories()),
        },
        Cmd::LoadPreferences => Ev::Preferences(Box::new(preferences())),
        Cmd::EditPreferences { base, .. } => Ev::Edited(base),
        Cmd::LoadMemes => Ev::Memes(Vec::new()),
        Cmd::SharedGif { .. } | Cmd::SendSticker { .. } | Cmd::SendMeme { .. } => return,
    };
    let _ = events.unbounded_send(ComposerEv::Pickers(ev));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_demo_preferences_hold_favourites_and_usage() {
        let prefs = preferences();
        let picker = prefs.get().emoji_picker.clone().unwrap();
        assert_eq!(picker.favorite_emoji_ids, FAVOURITES);
        assert_eq!(picker.usage["unicode:joy"].count, 9);
        for key in FAVOURITES.iter().chain(USED.iter().map(|(k, _)| k)) {
            let name = key.trim_start_matches("unicode:");
            assert!(inlet_emoji::by_shortcode(name).is_some(), "{name}");
        }
        let media = p::Media::new(super::super::media::MEDIA_BASE).unwrap();
        for g in gifs() {
            assert!(g.preview(&media, Default::default()).is_some());
        }
    }
}
