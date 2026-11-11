// SPDX-License-Identifier: AGPL-3.0-or-later
//! The pickers' requests against a mock Fluxer instance.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use super::*;
use crate::ui::composer::pickers::prefs::PickerPrefs;
use crate::ui::composer::pickers::SearchDebounce;

fn discovery(base: &str) -> Value {
    json!({
        "api_code_version": 1,
        "endpoints": {
            "api": format!("{base}/api"), "api_client": format!("{base}/api"),
            "api_public": format!("{base}/api"), "gateway": "ws://127.0.0.1:1/gateway",
            "media": format!("{base}/media"), "webapp": base,
            "invite": format!("{base}/invite")
        },
        "captcha": {"provider": "altcha"},
        "features": {"account_identity": "email", "gif_enabled": true},
        "limits": {"version": 2, "rules": []},
        "app_public": {"branding": {"product_name": "Mock"}},
        "sso": {"enabled": false},
        "domain_migration": {"enabled": false}
    })
}

/// A mock instance and a client of its own token on it.
async fn fixture(token: &str) -> (MockServer, p::Client) {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/fluxer"))
        .respond_with(ResponseTemplate::new(200).set_body_json(discovery(&server.uri())))
        .mount(&server)
        .await;
    let info = p::ClientInfo {
        installation_id: "00000000-0000-4000-a000-000000000000".into(),
        ..p::ClientInfo::default()
    };
    let instance = p::Instance::discover_with(&server.uri(), info)
        .await
        .expect("discovery");
    (server, p::Client::new(instance, p::Token::new(token)))
}

async fn requests(server: &MockServer, verb: &str, route: &str) -> Vec<Request> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.method.as_str() == verb && r.url.path() == route)
        .collect()
}

#[tokio::test]
async fn gif_search_is_debounced_and_budgeted() {
    // The UI: typing "cats" a key every 100 ms asks for nothing until the typing has
    // stopped for 350 ms, then for the newest text only, once.
    let t0 = Instant::now();
    let ms = |n: u64| t0 + Duration::from_millis(n);
    let mut debounce = SearchDebounce::default();
    for (n, text) in ["c", "ca", "cat", "cats"].iter().enumerate() {
        debounce.typed(text, ms(n as u64 * 100));
        assert_eq!(debounce.due(ms(n as u64 * 100 + 99)), None, "while typing");
    }
    assert_eq!(debounce.due(ms(500)), None, "200 ms after the last key");
    assert_eq!(debounce.due(ms(650)).as_deref(), Some("cats"));
    assert_eq!(debounce.due(ms(2_000)), None, "asked once");
    debounce.typed("cats ", ms(2_100));
    assert_eq!(
        debounce.due(ms(3_000)),
        None,
        "the same text is not asked again"
    );
    debounce.typed("dogs", ms(3_100));
    assert_eq!(debounce.due(ms(3_500)).as_deref(), Some("dogs"));
    debounce.typed("", ms(3_600));
    assert_eq!(
        debounce.due(ms(9_000)),
        None,
        "an empty field asks for nothing"
    );
    assert_eq!(debounce.immediate("party").as_deref(), Some("party"));
    assert_eq!(debounce.immediate("party"), None);

    // The backend: ten searches at once (more than the UI ever sends) start at least
    // 500 ms apart, only on the instance's own `/gifs/search`, and what would wait more
    // than 3 s is refused without a request.
    let (server, client) = fixture("tok-pickers-search").await;
    let times: Arc<Mutex<Vec<Instant>>> = Arc::default();
    let seen = times.clone();
    Mock::given(method("GET"))
        .and(path("/api/v1/gifs/search"))
        .respond_with(move |_: &Request| {
            seen.lock().unwrap().push(Instant::now());
            ResponseTemplate::new(200).set_body_json(json!([
                {"id": "1", "url": "https://klipy.com/gifs/cat",
                 "proxy_src": "https://media.example/external/x/cat.gif"}
            ]))
        })
        .mount(&server)
        .await;
    let queries: Vec<String> = (0..10).map(|i| format!("cat {i}")).collect();
    let results =
        futures::future::join_all(queries.iter().map(|q| search_gifs(&client, q, "nb"))).await;
    let ok = results.iter().filter(|r| r.is_ok()).count();
    let refused = results
        .iter()
        .filter(|r| matches!(r, Err(p::Error::RateLimited { .. })))
        .count();
    assert_eq!(ok + refused, 10);
    assert!((1..=7).contains(&ok), "{ok} searches went out");
    let mut times = times.lock().unwrap().clone();
    times.sort();
    assert_eq!(times.len(), ok);
    for pair in times.windows(2) {
        assert!(
            pair[1] - pair[0] >= Duration::from_millis(450),
            "searches {:?} apart",
            pair[1] - pair[0]
        );
    }
    let all = server.received_requests().await.unwrap_or_default();
    assert!(all
        .iter()
        .filter(|r| r.url.path() != "/.well-known/fluxer")
        .all(|r| r.url.path() == "/api/v1/gifs/search"));
    assert!(all
        .iter()
        .filter_map(|r| r.url.query())
        .any(|q| q.contains("locale=no")));

    // The GIF home and trending: at most one request of each per account per interval.
    let state = State::default();
    let now = Instant::now();
    assert!(state.gif_home_slot(1, 1, now));
    for n in 1..5u64 {
        assert!(!state.gif_home_slot(1, 1, now + Duration::from_secs(n)));
    }
    assert!(
        state.gif_home_slot(1, 0, now),
        "trending has a slot of its own"
    );
    assert!(state.gif_home_slot(1, 1, now + GIF_HOME_INTERVAL));
    assert!(!state.gif_home_slot(1, 7, now), "no other kinds");
}

/// Standard base64, for the mock server's settings.
fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// A snapshot from a newer Fluxer client: a top-level field Inlet does not know
/// (950, "set by a newer client") and `emoji_picker.favorite_emoji_ids = [unicode:wave]`.
fn varint(mut v: u64, out: &mut Vec<u8>) {
    while v >= 0x80 {
        out.push((v as u8 & 0x7f) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn bytes_field(field: u64, data: &[u8], out: &mut Vec<u8>) {
    varint(field << 3 | 2, out);
    varint(data.len() as u64, out);
    out.extend_from_slice(data);
}

fn varint_field(field: u64, v: u64, out: &mut Vec<u8>) {
    varint(field << 3, out);
    varint(v, out);
}

/// The favourite GIF as Fluxer's client stores it (`slimFavoriteGifEntry`): the page
/// URL, the video preview of the best format in `proxy_url`, its size and content type,
/// and no formats.
fn fluxer_favourite() -> FavoriteGifEntry {
    FavoriteGifEntry {
        url: "https://klipy.com/gifs/dancing-cat".into(),
        proxy_url: "https://media.example/external/x/https/static.klipy.com/dancing.mp4".into(),
        width: 498,
        height: 280,
        media: Default::default(),
        content_type: "video/mp4".into(),
        placeholder: String::new(),
    }
}

/// A snapshot from a newer Fluxer client: a top-level field Inlet does not know
/// (950, "set by a newer client"), `emoji_picker.favorite_emoji_ids = [unicode:wave]`
/// and one favourite GIF in the shape Fluxer stores it.
fn snapshot() -> (Vec<u8>, Vec<u8>) {
    let unknown: Vec<u8> = [&[0xB2, 0x3B, 21][..], b"set by a newer client"].concat();
    let mut picker = Vec::new();
    bytes_field(2, b"unicode:wave", &mut picker);
    let fav = fluxer_favourite();
    let mut entry = Vec::new();
    bytes_field(1, fav.url.as_bytes(), &mut entry);
    bytes_field(2, fav.proxy_url.as_bytes(), &mut entry);
    varint_field(3, u64::from(fav.width), &mut entry);
    varint_field(4, u64::from(fav.height), &mut entry);
    bytes_field(6, fav.content_type.as_bytes(), &mut entry);
    let mut gifs = Vec::new();
    bytes_field(1, &entry, &mut gifs);
    let mut bytes = unknown.clone();
    bytes_field(20, &picker, &mut bytes);
    bytes_field(25, &gifs, &mut bytes);
    (bytes, unknown)
}

/// The snapshot a PATCH sent.
fn sent(r: &Request) -> p::SyncedPreferences {
    let body: Value = serde_json::from_slice(&r.body).unwrap();
    p::SyncedPreferences::from_base64(body["synced_preferences"].as_str().unwrap()).unwrap()
}

#[tokio::test]
async fn favourites_round_trip_through_synced_preferences() {
    let (server, client) = fixture("tok-pickers-prefs").await;
    let (original, unknown) = snapshot();
    Mock::given(method("GET"))
        .and(path("/api/v1/users/@me/settings"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"synced_preferences": base64(&original)})),
        )
        .mount(&server)
        .await;
    // The server stores what it is sent.
    Mock::given(method("PATCH"))
        .and(path("/api/v1/users/@me/settings"))
        .respond_with(|r: &Request| {
            let body: Value = serde_json::from_slice(&r.body).unwrap();
            ResponseTemplate::new(200)
                .set_body_json(json!({"synced_preferences": body["synced_preferences"]}))
        })
        .mount(&server)
        .await;

    // Read: the UI sees the favourite another client set.
    let mut prefs = PickerPrefs::default();
    let stored = client.synced_preferences().await.unwrap();
    assert_eq!(stored.bytes(), original.as_slice());
    let first_base = stored.clone();
    prefs.set_stored(stored);
    assert_eq!(prefs.favourites(), ["unicode:wave"]);
    // ...and the favourite GIF Fluxer stored, as it is.
    assert_eq!(prefs.favourite_gifs(), [fluxer_favourite()]);

    // A right click makes a custom emoji a favourite: shown at once, then stored where
    // Fluxer keeps it, with the field Inlet does not know kept.
    let Some(Cmd::EditPreferences { base, edit }) = prefs.edit(PrefEdit::FavouriteEmoji {
        key: "custom:1:2".into(),
        on: true,
    }) else {
        panic!("an edit command");
    };
    assert_eq!(prefs.favourites(), ["unicode:wave", "custom:1:2"]);
    let stored = edit_preferences(&client, &base, edit).await.unwrap();
    prefs.edited(Some(stored));
    assert_eq!(prefs.favourites(), ["unicode:wave", "custom:1:2"]);
    let patches = requests(&server, "PATCH", "/api/v1/users/@me/settings").await;
    assert_eq!(patches.len(), 1);
    let first = sent(&patches[0]);
    assert_eq!(
        first
            .get()
            .emoji_picker
            .as_ref()
            .unwrap()
            .favorite_emoji_ids,
        ["unicode:wave", "custom:1:2"]
    );
    assert!(
        first
            .bytes()
            .windows(unknown.len())
            .any(|w| w == unknown.as_slice()),
        "the newer client's field is kept"
    );
    let body: Value = serde_json::from_slice(&patches[0].body).unwrap();
    assert_eq!(
        body["synced_preferences_base_hash"],
        json!(first_base.base_hash())
    );

    // Four edits at once from the snapshot the UI first had (its own write replaced
    // it): one batch, one PATCH, no conflict and no new read; nothing is lost.
    // A GIF from a search becomes a favourite in Fluxer's shape.
    let found: p::Gif = serde_json::from_value(json!({
        "id": "1", "url": "https://klipy.com/gifs/cat",
        "proxy_src": "https://media.example/external/x/https/static.klipy.com/cat.mp4",
        "width": 498, "height": 280,
        "media": {
            "tinymp4": {"proxy_src": "https://media.example/external/x/tiny.mp4", "width": 220, "height": 124},
            "tinygif": {"proxy_src": "https://media.example/external/x/tiny.gif", "width": 220, "height": 124}
        }
    }))
    .unwrap();
    let gif = favourite_entry(&found);
    let now = 1_800_000_000_000;
    let edits = [
        PrefEdit::UsedEmoji {
            key: "custom:1:2".into(),
            at_ms: now,
        },
        PrefEdit::SkinTone("\u{1F3FD}".into()),
        PrefEdit::FavouriteGif {
            entry: Box::new(gif),
            on: true,
        },
        PrefEdit::FavouriteEmoji {
            key: "unicode:wave".into(),
            on: false,
        },
    ];
    let results = futures::future::join_all(
        edits
            .into_iter()
            .map(|e| edit_preferences(&client, &first_base, e)),
    )
    .await;
    let stored = results.into_iter().last().unwrap().unwrap();
    assert_eq!(
        requests(&server, "PATCH", "/api/v1/users/@me/settings")
            .await
            .len(),
        2
    );
    assert_eq!(
        requests(&server, "GET", "/api/v1/users/@me/settings")
            .await
            .len(),
        1,
        "no conflict, no new read"
    );
    let mut prefs = PickerPrefs::default();
    prefs.set_stored(stored);
    assert_eq!(prefs.favourites(), ["custom:1:2"]);
    assert_eq!(prefs.recent(now, 10), ["custom:1:2"]);
    assert_eq!(prefs.skin_tone(), inlet_emoji::SkinTone::Medium);
    assert!(prefs.is_favourite_gif("https://klipy.com/gifs/cat"));
    let last = requests(&server, "PATCH", "/api/v1/users/@me/settings").await;
    let stored = sent(&last[1]);
    assert!(stored
        .bytes()
        .windows(unknown.len())
        .any(|w| w == unknown.as_slice()));
    // Fluxer's favourite is kept as it was; Inlet's is in the same shape: one image
    // preview, its size and content type, no formats (Fluxer reads it unchanged).
    let entries = &stored.get().favorite_gifs.as_ref().unwrap().entries;
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0], fluxer_favourite());
    assert_eq!(
        entries[1],
        FavoriteGifEntry {
            url: "https://klipy.com/gifs/cat".into(),
            proxy_url: "https://media.example/external/x/tiny.gif".into(),
            width: 220,
            height: 124,
            media: Default::default(),
            content_type: "image/gif".into(),
            placeholder: String::new(),
        }
    );
}

#[test]
fn edits_set_values_and_refuse_bad_input_without_panicking() {
    let mut prefs = Preferences::default();
    let used = PrefEdit::UsedEmoji {
        key: "unicode:smile".into(),
        at_ms: 1_000,
    };
    used.apply(&mut prefs);
    used.apply(&mut prefs);
    let usage = &prefs.emoji_picker.as_ref().unwrap().usage;
    assert_eq!(usage["unicode:smile"].count, 2);
    assert_eq!(usage["unicode:smile"].last_used_ms, 1_000);
    // Favourites: on twice is one entry; off removes it.
    let fav = |on| PrefEdit::FavouriteEmoji {
        key: "custom:1:2".into(),
        on,
    };
    fav(true).apply(&mut prefs);
    fav(true).apply(&mut prefs);
    assert_eq!(
        prefs.emoji_picker.as_ref().unwrap().favorite_emoji_ids,
        ["custom:1:2"]
    );
    fav(false).apply(&mut prefs);
    assert!(prefs
        .emoji_picker
        .as_ref()
        .unwrap()
        .favorite_emoji_ids
        .is_empty());
    // Bad keys, tones and GIFs change nothing.
    let before = prefs.clone();
    for edit in [
        PrefEdit::UsedEmoji {
            key: "smile".into(),
            at_ms: i64::MIN,
        },
        PrefEdit::UsedEmoji {
            key: format!("unicode:{}", "x".repeat(300)),
            at_ms: 0,
        },
        PrefEdit::FavouriteEmoji {
            key: String::new(),
            on: true,
        },
        PrefEdit::FavouriteEmoji {
            key: "unicode:".into(),
            on: true,
        },
        PrefEdit::FavouriteEmoji {
            key: "custom:".into(),
            on: true,
        },
        PrefEdit::UsedEmoji {
            key: "custom:1:not-an-id".into(),
            at_ms: 0,
        },
        PrefEdit::UsedEmoji {
            key: "custom:x:2".into(),
            at_ms: 0,
        },
        PrefEdit::SkinTone("blue".into()),
        PrefEdit::SkinTone("\u{1F3FB}\u{1F3FC}".into()),
        PrefEdit::FavouriteGif {
            entry: Box::default(),
            on: true,
        },
    ] {
        edit.apply(&mut prefs);
    }
    assert_eq!(prefs, before);
    // Counts and times at the extremes do not overflow.
    prefs.emoji_picker.as_mut().unwrap().usage.insert(
        "unicode:x".into(),
        UsageStat {
            count: u32::MAX,
            last_used_ms: i64::MAX,
        },
    );
    PrefEdit::UsedEmoji {
        key: "unicode:x".into(),
        at_ms: i64::MIN,
    }
    .apply(&mut prefs);
    assert_eq!(
        prefs.emoji_picker.as_ref().unwrap().usage["unicode:x"].count,
        MAX_USAGE_COUNT
    );
    assert!(ranked(&prefs.emoji_picker.as_ref().unwrap().usage, i64::MIN).len() == 2);
    PrefEdit::SkinTone("\u{1F3FD}".into()).apply(&mut prefs);
    assert_eq!(prefs.emoji.as_ref().unwrap().skin_tone, "\u{1F3FD}");
    PrefEdit::SkinTone(String::new()).apply(&mut prefs);
    assert_eq!(prefs.emoji.as_ref().unwrap().skin_tone, "");
}

#[test]
fn usage_is_capped_by_frecency_like_fluxer() {
    let mut prefs = Preferences::default();
    let now = 10 * 24 * 3600 * 1000;
    for i in 0..(MAX_USAGE_KEYS + 20) {
        PrefEdit::UsedEmoji {
            key: format!("unicode:e{i}"),
            at_ms: i as i64,
        }
        .apply(&mut prefs);
    }
    // A recent one survives the cap; the oldest go.
    PrefEdit::UsedEmoji {
        key: "unicode:fresh".into(),
        at_ms: now,
    }
    .apply(&mut prefs);
    let usage = &prefs.emoji_picker.as_ref().unwrap().usage;
    assert_eq!(usage.len(), MAX_USAGE_KEYS);
    assert!(usage.contains_key("unicode:fresh"));
    assert!(!usage.contains_key("unicode:e0"));
    // Ranking: score first, then the newer use.
    let map: std::collections::BTreeMap<String, UsageStat> = [
        (
            "b".to_string(),
            UsageStat {
                count: 1,
                last_used_ms: now,
            },
        ),
        (
            "a".to_string(),
            UsageStat {
                count: 3,
                last_used_ms: now,
            },
        ),
    ]
    .into();
    assert_eq!(ranked(&map, now), ["a", "b"]);
}

#[test]
fn a_favourite_gif_is_stored_in_fluxers_shape() {
    let fmt = |key: &str, w: u32| {
        (
            key.to_string(),
            json!({"src": format!("https://static.klipy.com/{key}"), "proxy_src": format!("https://media.example.org/external/s/{key}"), "width": w, "height": w / 2}),
        )
    };
    let media: serde_json::Map<String, Value> = [
        fmt("tinywebm", 220),
        fmt("tinymp4", 220),
        fmt("tinygif", 220),
        fmt("tinywebp", 220),
        fmt("mediumgif", 498),
        fmt("webp", 498),
        fmt("mp4", 640),
    ]
    .into_iter()
    .collect();
    let gif: p::Gif = serde_json::from_value(json!({
        "id": "1", "url": "https://klipy.com/gifs/cat",
        "proxy_src": "https://media.example.org/external/s/default.mp4",
        "width": 640, "height": 320, "media": media
    }))
    .unwrap();
    let entry = favourite_entry(&gif);
    // Fluxer's rule for images: the narrowest at least 400 px wide, then its order
    // (`webp` before `mediumgif`). No formats, no provider URL.
    assert_eq!(entry.url, "https://klipy.com/gifs/cat");
    assert_eq!(entry.proxy_url, "https://media.example.org/external/s/webp");
    assert_eq!((entry.width, entry.height), (498, 249));
    assert_eq!(entry.content_type, "image/webp");
    assert!(entry.media.is_empty());
    // Only small images: the widest of them.
    let small: p::Gif = serde_json::from_value(json!({
        "id": "2", "url": "https://klipy.com/gifs/dog",
        "media": {"tinygif": {"proxy_src": "https://media.example.org/external/s/a", "width": 220, "height": 100},
                  "nanogif": {"proxy_src": "https://media.example.org/external/s/b", "width": 90, "height": 40}}
    }))
    .unwrap();
    assert_eq!(
        favourite_entry(&small).proxy_url,
        "https://media.example.org/external/s/a"
    );
    // No image at all: the default format, no content type.
    let video: p::Gif = serde_json::from_value(json!({
        "id": "3", "url": "https://klipy.com/gifs/x",
        "proxy_src": "https://media.example.org/external/s/x.mp4", "width": 10, "height": 10,
        "media": {"tinymp4": {"proxy_src": "https://media.example.org/external/s/t.mp4", "width": 220, "height": 100}}
    }))
    .unwrap();
    let v = favourite_entry(&video);
    assert_eq!(v.proxy_url, "https://media.example.org/external/s/x.mp4");
    assert_eq!(v.content_type, "");
    // Fluxer's content types.
    assert_eq!(format_content_type("tinymp4"), "video/mp4");
    assert_eq!(format_content_type("nanowebm"), "video/webm");
    assert_eq!(format_content_type("mediumgif"), "image/gif");
    assert_eq!(format_content_type("other"), "");
}

#[test]
fn a_network_error_counts_as_offline() {
    let f = Failure::from(p::Error::Network("connection reset".into()));
    assert_eq!(f.kind, FailureKind::Offline);
    assert_eq!(f.message, t!("error-unreachable"));
}

#[test]
fn global_expressions_are_off_unless_the_instance_grants_them() {
    let discovery = |limits: Value, features: Value| -> p::Discovery {
        serde_json::from_value(json!({
            "api_code_version": 1,
            "endpoints": {"api_public": "https://a.example/api", "gateway": "wss://g.example"},
            "features": features,
            "limits": limits,
            "gif": {"provider": "klipy", "display_name": "KLIPY", "attribution_required": true}
        }))
        .unwrap()
    };
    let none = discovery(json!({"version": 2, "rules": []}), json!({}));
    let s = Setup::of(&none, &[]);
    assert!(!s.gif_enabled && !s.global_expressions);
    assert_eq!(s.gif_attribution.as_deref(), Some("KLIPY"));
    let premium = discovery(
        json!({"version": 2, "rules": [
            {"id": "p", "filters": {"traits": ["premium"]}, "overrides": {"feature_global_expressions": 1}}
        ]}),
        json!({"gif_enabled": true}),
    );
    assert!(!Setup::of(&premium, &[]).global_expressions);
    let s = Setup::of(&premium, &["premium".to_string()]);
    assert!(s.gif_enabled && s.global_expressions);
}

#[test]
fn only_fluxers_emoji_keys_are_accepted() {
    for good in ["unicode:smile", "custom:1:2", "custom::2"] {
        assert!(is_emoji_key(good), "{good}");
    }
    for bad in [
        "",
        "unicode:",
        "unicode:a b",
        "custom:",
        "custom:1",
        "custom:1:",
        "custom:1:x",
        "custom:g:2",
        "smile",
    ] {
        assert!(!is_emoji_key(bad), "{bad}");
    }
}
