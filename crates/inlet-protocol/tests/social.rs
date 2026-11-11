//! Social, profiles and settings against a mock instance: group DMs with the
//! CAPTCHA, synced preferences, relationship events, GIF search and invites.

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use inlet_protocol::api::invites::parse_invite;
use inlet_protocol::api::preferences::Preferences;
use inlet_protocol::proto::preferences::{AccessibilitySettings, EmojiState, UsageStat};
use inlet_protocol::{
    altcha, Client, Dispatch, Error, GroupDmOutcome, Instance, InviteLink, Ready,
    SyncedPreferences, Token, Transform,
};
use prost::Message;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use wiremock::matchers::{body_json, method, path, query_param};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

fn client(instance: &Instance, token: &str) -> Client {
    Client::new(instance.clone(), Token::new(token))
}

/// Matches a request whose `X-Captcha-Token` solves its challenge.
struct SolvedCaptcha;

impl wiremock::Match for SolvedCaptcha {
    fn matches(&self, req: &Request) -> bool {
        req.headers
            .get("x-captcha-token")
            .and_then(|v| v.to_str().ok())
            .is_some_and(altcha::verify_test_token)
    }
}

fn captcha_required(challenge: Value) -> ResponseTemplate {
    ResponseTemplate::new(400).set_body_json(json!({
        "code": "CAPTCHA_REQUIRED", "message": "Captcha required",
        "captcha_provider": "altcha", "altcha_challenge": challenge
    }))
}

fn group_dm_json() -> Value {
    json!({"id": "500", "type": 3, "owner_id": "10", "name": null,
           "recipients": [common::user_json("11", "bo"), common::user_json("12", "cy")]})
}

async fn requests_to(server: &MockServer, p: &str) -> Vec<Request> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path() == p)
        .collect()
}

#[tokio::test]
async fn group_dm_create_solves_captcha_or_returns_captcha_result() {
    // 1. A solvable challenge: solved off the async threads, then created.
    let (server, instance) = common::instance().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/users/@me/channels"))
        .and(SolvedCaptcha)
        .and(body_json(json!({"recipients": ["11", "12"]})))
        .respond_with(ResponseTemplate::new(200).set_body_json(group_dm_json()))
        .with_priority(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/users/@me/channels"))
        .respond_with(captcha_required(altcha::make_test_challenge(4_000, 20)))
        .expect(1)
        .mount(&server)
        .await;
    let c = client(&instance, "tok-gdm-ok");
    // The executor keeps running while the challenge is solved. This test
    // runs on a single-threaded runtime, and a task that only yields measures
    // the longest time it was not polled: a solve on the runtime thread would
    // stall it for about the whole solve. (Timer-free, so it holds on every OS.)
    let longest_stall_us = Arc::new(AtomicU64::new(0));
    let watcher = {
        let longest = longest_stall_us.clone();
        tokio::spawn(async move {
            let mut last = Instant::now();
            loop {
                tokio::task::yield_now().await;
                let now = Instant::now();
                let gap = now.duration_since(last).as_micros() as u64;
                longest.fetch_max(gap, Ordering::Relaxed);
                last = now;
            }
        })
    };
    let started = Instant::now();
    let out = c
        .create_group_dm(&["11".into(), "12".into(), "11".into()])
        .await
        .unwrap();
    let elapsed = started.elapsed();
    watcher.abort();
    match out {
        GroupDmOutcome::Done(ch) => assert_eq!((ch.id.as_str(), ch.kind), ("500", 3)),
        other => panic!("{other:?}"),
    }
    let longest = Duration::from_micros(longest_stall_us.load(Ordering::Relaxed));
    // Only meaningful when the call took a while (the solve dominates it).
    if elapsed >= Duration::from_millis(100) {
        assert!(
            longest * 2 < elapsed,
            "solving blocked the executor: stalled {longest:?} of {elapsed:?}"
        );
    }

    // 2. A challenge that cannot be solved: the CAPTCHA result, one request.
    let (server, instance) = common::instance().await;
    let mut unsolvable = altcha::make_test_challenge(1, 1);
    unsolvable["parameters"]["algorithm"] = json!("ARGON2ID");
    Mock::given(method("POST"))
        .and(path("/api/v1/users/@me/channels"))
        .respond_with(captcha_required(unsolvable))
        .expect(1)
        .mount(&server)
        .await;
    let c = client(&instance, "tok-gdm-unsolvable");
    let out = c.create_group_dm(&["11".into()]).await.unwrap();
    assert_eq!(out, GroupDmOutcome::Captcha);

    // 3. A server that refuses every answer: two solves, three requests, then
    //    the CAPTCHA result. Never a loop.
    let (server, instance) = common::instance().await;
    Mock::given(method("PUT"))
        .and(path("/api/v1/channels/500/recipients/13"))
        .respond_with(move |_: &Request| {
            ResponseTemplate::new(400).set_body_json(json!({
                "code": "INVALID_CAPTCHA", "message": "Invalid captcha",
                "captcha_provider": "altcha",
                "altcha_challenge": altcha::make_test_challenge(3, 5)
            }))
        })
        .expect(3)
        .mount(&server)
        .await;
    let c = client(&instance, "tok-gdm-refused");
    let out = c.add_group_dm_recipient("500", "13").await.unwrap();
    assert_eq!(out, GroupDmOutcome::Captcha);

    // 4. Adding a participant with a solvable challenge.
    let (server, instance) = common::instance().await;
    Mock::given(method("PUT"))
        .and(path("/api/v1/channels/500/recipients/13"))
        .and(SolvedCaptcha)
        .respond_with(ResponseTemplate::new(204))
        .with_priority(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/api/v1/channels/500/recipients/13"))
        .respond_with(captcha_required(altcha::make_test_challenge(10, 5)))
        .expect(1)
        .mount(&server)
        .await;
    let c = client(&instance, "tok-gdm-add");
    assert_eq!(
        c.add_group_dm_recipient("500", "13").await.unwrap(),
        GroupDmOutcome::Done(())
    );

    // 5. Recipients that cannot be added come back as data, not as a CAPTCHA.
    let (server, instance) = common::instance().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/users/@me/channels"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "code": "GROUP_DM_RECIPIENTS_NOT_ADDABLE", "message": "no",
            "unaddable_recipients": [{"user_id": "12", "reason": "not_friends"}],
            "addable_recipients": ["11"]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let c = client(&instance, "tok-gdm-unaddable");
    match c
        .create_group_dm(&["11".into(), "12".into()])
        .await
        .unwrap()
    {
        GroupDmOutcome::NotAddable { unaddable, addable } => {
            assert_eq!(unaddable[0].user_id, "12");
            assert_eq!(unaddable[0].reason, "not_friends");
            assert_eq!(addable, ["11"]);
        }
        other => panic!("{other:?}"),
    }

    // 6. Bad input never reaches the server.
    let too_many: Vec<String> = (100..150).map(|i| i.to_string()).collect();
    for bad in [
        vec![],
        vec!["@me".to_owned()],
        vec!["../x".to_owned()],
        too_many,
    ] {
        assert!(matches!(
            c.create_group_dm(&bad).await,
            Err(Error::InvalidInput(_))
        ));
    }
    assert_eq!(
        requests_to(&server, "/api/v1/users/@me/channels")
            .await
            .len(),
        1
    );
}

// ----- Synced preferences -----

fn key(field: u32, wire: u64) -> Vec<u8> {
    let mut v = Vec::new();
    prost::encoding::encode_varint((u64::from(field) << 3) | wire, &mut v);
    v
}

fn len_field(field: u32, payload: &[u8]) -> Vec<u8> {
    let mut v = key(field, 2);
    prost::encoding::encode_varint(payload.len() as u64, &mut v);
    v.extend_from_slice(payload);
    v
}

fn varint_field(field: u32, value: u64) -> Vec<u8> {
    let mut v = key(field, 0);
    prost::encoding::encode_varint(value, &mut v);
    v
}

/// A snapshot as a newer Fluxer client wrote it: known fields, an unknown
/// field inside `accessibility` (300) and an unknown top-level field (950).
fn snapshot_from_a_newer_client(top_unknown: &[u8]) -> Vec<u8> {
    let mut acc = AccessibilitySettings {
        font_size: Some(16.0),
        show_gif_button: Some(true),
        ..Default::default()
    }
    .encode_to_vec();
    acc.extend(varint_field(300, 1));
    let rest = Preferences {
        emoji: Some(EmojiState {
            skin_tone: "2".into(),
        }),
        ..Default::default()
    }
    .encode_to_vec();
    // emoji_picker as Fluxer's client stores it: usage in insertion order
    // (wave, smile, grin; not sorted), and an unknown field 9 inside wave.
    let mut picker = Vec::new();
    for e in [
        wave_entry(),
        usage_entry("smile", 3),
        usage_entry("grin", 1),
    ] {
        picker.extend(len_field(1, &e));
    }
    picker.extend(len_field(2, b"smile"));
    let mut bytes = len_field(1, &acc);
    bytes.extend(len_field(20, &picker));
    bytes.extend(rest);
    bytes.extend(len_field(950, top_unknown));
    bytes
}

/// A `usage` map entry: key, then `UsageStat { count, last_used_ms }`.
fn usage_entry(key_text: &str, count: u64) -> Vec<u8> {
    let mut stat = varint_field(1, count);
    stat.extend(varint_field(2, 1_700_000_000_000));
    let mut e = len_field(1, key_text.as_bytes());
    e.extend(len_field(2, &stat));
    e
}

/// The `wave` entry, with an unknown field 9 inside its value.
fn wave_entry() -> Vec<u8> {
    let mut stat = varint_field(1, 4);
    stat.extend(varint_field(9, 1));
    let mut e = len_field(1, b"wave");
    e.extend(len_field(2, &stat));
    e
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn settings_json(prefs: &[u8]) -> Value {
    json!({"status": "online", "theme": "dark", "locale": "en-US", "guild_folders": [],
           "synced_preferences": b64(prefs)})
}

/// Answers a settings PATCH like Fluxer: stores the snapshot and echoes it.
struct EchoSettings;

impl Respond for EchoSettings {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&req.body).unwrap();
        ResponseTemplate::new(200).set_body_json(json!({
            "status": "online", "theme": "dark", "locale": "en-US",
            "synced_preferences": body["synced_preferences"]
        }))
    }
}

fn patched_snapshot(req: &Request) -> (Vec<u8>, String) {
    let body: Value = serde_json::from_slice(&req.body).unwrap();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(body["synced_preferences"].as_str().unwrap())
        .unwrap();
    let hash = body["synced_preferences_base_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    (bytes, hash)
}

/// The value of the unknown varint field 300 inside `accessibility` (tag 1).
fn nested_unknown(snapshot: &[u8]) -> Option<u64> {
    let mut buf = snapshot;
    while !buf.is_empty() {
        let k = prost::encoding::decode_varint(&mut buf).ok()?;
        let (field, wire) = ((k >> 3) as u32, k & 7);
        match wire {
            0 => {
                prost::encoding::decode_varint(&mut buf).ok()?;
            }
            1 => buf = &buf[8..],
            5 => buf = &buf[4..],
            2 => {
                let len = prost::encoding::decode_varint(&mut buf).ok()? as usize;
                let (payload, rest) = buf.split_at(len);
                if field == 1 {
                    let needle = varint_field(300, 1);
                    let mut inner = payload;
                    while !inner.is_empty() {
                        if inner.starts_with(&needle) {
                            return Some(1);
                        }
                        let k = prost::encoding::decode_varint(&mut inner).ok()?;
                        match k & 7 {
                            0 => {
                                prost::encoding::decode_varint(&mut inner).ok()?;
                            }
                            1 => inner = &inner[8..],
                            5 => inner = &inner[4..],
                            2 => {
                                let l = prost::encoding::decode_varint(&mut inner).ok()? as usize;
                                inner = &inner[l..];
                            }
                            _ => return None,
                        }
                    }
                }
                buf = rest;
            }
            _ => return None,
        }
    }
    None
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[tokio::test]
async fn synced_preferences_round_trip_unknown_fields_kept() {
    let (server, instance) = common::instance().await;
    let original = snapshot_from_a_newer_client(b"set by a newer client");
    Mock::given(method("GET"))
        .and(path("/api/v1/users/@me/settings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(settings_json(&original)))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/api/v1/users/@me/settings"))
        .respond_with(EchoSettings)
        .expect(1)
        .mount(&server)
        .await;
    let c = client(&instance, "tok-prefs");

    // Read: the known fields are typed, and the stored bytes are kept.
    let base = c.synced_preferences().await.unwrap();
    assert_eq!(base.bytes(), original.as_slice());
    let acc = base.get().accessibility.clone().unwrap();
    assert_eq!(acc.font_size, Some(16.0));
    assert_eq!(base.get().emoji.as_ref().unwrap().skin_tone, "2");

    // Write: change a nested field, a top-level field, a list and the map
    // (bump smile, drop grin, add party; wave is untouched).
    let saved = c
        .update_synced_preferences(&base, |p| {
            p.accessibility
                .get_or_insert_with(Default::default)
                .font_size = Some(18.0);
            let picker = p.emoji_picker.get_or_insert_with(Default::default);
            picker.favorite_emoji_ids.push("wave".into());
            picker.usage.get_mut("smile").unwrap().count = 4;
            picker.usage.remove("grin");
            picker.usage.insert(
                "party".into(),
                UsageStat {
                    count: 1,
                    last_used_ms: 1_800_000_000_000,
                },
            );
            p.sanitize_urls = true;
        })
        .await
        .unwrap();

    let patch = requests_to(&server, "/api/v1/users/@me/settings")
        .await
        .into_iter()
        .find(|r| r.method.as_str() == "PATCH")
        .unwrap();
    let (sent, hash) = patched_snapshot(&patch);
    // The base hash is SHA-256 of the bytes the server had.
    assert_eq!(hash, hex::encode(Sha256::digest(&original)));
    // Fields Inlet does not know survive: at the top level and inside the
    // message it changed.
    assert!(contains(&sent, &len_field(950, b"set by a newer client")));
    assert_eq!(nested_unknown(&sent), Some(1));
    // The changes are there, and untouched fields are as they were.
    let p = Preferences::decode(sent.as_slice()).unwrap();
    let acc = p.accessibility.clone().unwrap();
    assert_eq!(acc.font_size, Some(18.0));
    assert_eq!(acc.show_gif_button, Some(true));
    assert!(p.sanitize_urls);
    assert_eq!(p.emoji.unwrap().skin_tone, "2");
    let picker = p.emoji_picker.unwrap();
    assert_eq!(picker.favorite_emoji_ids, ["smile", "wave"]);
    // The map: matched by key, whatever the stored order.
    assert_eq!(picker.usage["smile"].count, 4);
    assert_eq!(picker.usage["wave"].count, 4);
    assert_eq!(picker.usage["party"].count, 1);
    assert!(!picker.usage.contains_key("grin"));
    assert_eq!(picker.usage.len(), 3);
    // The untouched entry is sent as it was stored, unknown field included.
    assert!(contains(&sent, &len_field(1, &wave_entry())));
    // The returned snapshot is what the server stored.
    assert_eq!(saved.bytes(), sent.as_slice());
    assert_eq!(saved.base_hash(), hex::encode(Sha256::digest(&sent)));

    // A second round trip from the stored snapshot keeps the unknowns again.
    let again = saved.encode_change(&saved.get().clone()).unwrap();
    assert_eq!(again, sent);
}

#[tokio::test]
async fn synced_preferences_conflict_is_retried_on_the_new_snapshot() {
    let (server, instance) = common::instance().await;
    let first = snapshot_from_a_newer_client(b"v1");
    let newer = snapshot_from_a_newer_client(b"v2 from the phone");
    Mock::given(method("PATCH"))
        .and(path("/api/v1/users/@me/settings"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "code": "SYNCED_PREFERENCES_CONFLICT", "message": "conflict"
        })))
        .up_to_n_times(1)
        .with_priority(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/api/v1/users/@me/settings"))
        .respond_with(EchoSettings)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/users/@me/settings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(settings_json(&newer)))
        .expect(1)
        .mount(&server)
        .await;
    let c = client(&instance, "tok-prefs-conflict");
    let base = SyncedPreferences::decode(&first).unwrap();
    let saved = c
        .update_synced_preferences(&base, |p| p.sanitize_urls = true)
        .await
        .unwrap();
    assert!(saved.get().sanitize_urls);
    let patches: Vec<Request> = requests_to(&server, "/api/v1/users/@me/settings")
        .await
        .into_iter()
        .filter(|r| r.method.as_str() == "PATCH")
        .collect();
    assert_eq!(patches.len(), 2);
    let (_, h1) = patched_snapshot(&patches[0]);
    let (sent, h2) = patched_snapshot(&patches[1]);
    assert_eq!(h1, hex::encode(Sha256::digest(&first)));
    assert_eq!(h2, hex::encode(Sha256::digest(&newer)));
    // The change was applied to the newer snapshot, whose unknown field stays.
    assert!(contains(&sent, &len_field(950, b"v2 from the phone")));
    assert!(!contains(&sent, &len_field(950, b"v1")));

    // An edit that changes nothing sends nothing.
    let before = server.received_requests().await.unwrap().len();
    let same = c.update_synced_preferences(&saved, |_| {}).await.unwrap();
    assert_eq!(same, saved);
    assert_eq!(server.received_requests().await.unwrap().len(), before);
}

fn add_favorite(id: String) -> impl Fn(&mut Preferences) + Send + Sync + 'static {
    move |p: &mut Preferences| {
        let favs = &mut p
            .emoji_picker
            .get_or_insert_with(Default::default)
            .favorite_emoji_ids;
        if !favs.contains(&id) {
            favs.push(id.clone());
        }
    }
}

fn patches(reqs: &[Request]) -> usize {
    reqs.iter()
        .filter(|r| r.method.as_str() == "PATCH" && r.url.path() == "/api/v1/users/@me/settings")
        .count()
}

fn gets(reqs: &[Request]) -> usize {
    reqs.iter()
        .filter(|r| r.method.as_str() == "GET" && r.url.path() == "/api/v1/users/@me/settings")
        .count()
}

#[tokio::test]
async fn synced_preference_writes_are_batched_and_paced() {
    // Worst user behaviour: ten edits at once (emoji sent in a burst), then
    // edits one after another, all built on the same stale snapshot.
    let (server, instance) = common::instance().await;
    Mock::given(method("PATCH"))
        .and(path("/api/v1/users/@me/settings"))
        .respond_with(EchoSettings)
        .mount(&server)
        .await;
    let c = client(&instance, "tok-prefs-budget");
    let stale = SyncedPreferences::decode(&snapshot_from_a_newer_client(b"x")).unwrap();

    let mut tasks = Vec::new();
    for i in 0..10 {
        let (c, stale) = (c.clone(), stale.clone());
        tasks.push(tokio::spawn(async move {
            c.update_synced_preferences(&stale, add_favorite(format!("e{i}")))
                .await
        }));
    }
    let mut last = None;
    for t in tasks {
        last = Some(t.await.unwrap().unwrap());
    }
    // One PATCH carried all ten.
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(patches(&reqs), 1);
    let favs = last
        .unwrap()
        .get()
        .emoji_picker
        .clone()
        .unwrap()
        .favorite_emoji_ids;
    for i in 0..10 {
        assert!(favs.contains(&format!("e{i}")), "e{i}");
    }

    // Five more, one after another, still from the stale snapshot: the crate
    // builds on what it stored (no self-conflicts, no GET), at least 1 s apart.
    let started = Instant::now();
    for i in 10..15 {
        let saved = c
            .update_synced_preferences(&stale, add_favorite(format!("e{i}")))
            .await
            .unwrap();
        let favs = &saved
            .get()
            .emoji_picker
            .as_ref()
            .unwrap()
            .favorite_emoji_ids;
        assert!(favs.contains(&"e0".to_owned()) && favs.contains(&format!("e{i}")));
    }
    assert!(started.elapsed() >= Duration::from_millis(4_900));
    let reqs = server.received_requests().await.unwrap();
    assert_eq!((patches(&reqs), gets(&reqs)), (6, 0));
    // Under Fluxer's 20 per 10 s for the settings bucket with room to spare:
    // 6 PATCH in the 5+ s above, at most 10 in any 10 s.

    // A server that always reports a conflict: one batch, bounded retries.
    let (server, instance) = common::instance().await;
    Mock::given(method("PATCH"))
        .and(path("/api/v1/users/@me/settings"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "code": "SYNCED_PREFERENCES_CONFLICT", "message": "conflict"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/users/@me/settings"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(settings_json(&snapshot_from_a_newer_client(b"other"))),
        )
        .mount(&server)
        .await;
    let c = client(&instance, "tok-prefs-storm");
    let mut tasks = Vec::new();
    for i in 0..10 {
        let (c, stale) = (c.clone(), stale.clone());
        tasks.push(tokio::spawn(async move {
            c.update_synced_preferences(&stale, add_favorite(format!("s{i}")))
                .await
        }));
    }
    for t in tasks {
        match t.await.unwrap() {
            Err(Error::Api(e)) => assert_eq!(e.code, "SYNCED_PREFERENCES_CONFLICT"),
            other => panic!("{other:?}"),
        }
    }
    let reqs = server.received_requests().await.unwrap();
    assert_eq!((patches(&reqs), gets(&reqs)), (3, 2));
}

#[tokio::test]
async fn every_caller_of_a_failed_batch_gets_a_network_error_not_a_decode_error() {
    // The API host does not answer (nothing listens on its port): the PATCH
    // fails on the network. The writer and the callers that only waited must
    // all see that kind, so the UI can say "offline" and not "the server sent
    // something odd".
    let closed_port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let discovery = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/fluxer"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(common::discovery_json(
                &format!("http://127.0.0.1:{closed_port}"),
                "ws://127.0.0.1:1/gateway",
            )),
        )
        .mount(&discovery)
        .await;
    let instance = Instance::discover_with(
        &discovery.uri(),
        inlet_protocol::ClientInfo {
            installation_id: "00000000-0000-4000-a000-000000000000".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let c = client(&instance, "tok-prefs-offline");
    let base = SyncedPreferences::decode(&snapshot_from_a_newer_client(b"x")).unwrap();
    let mut tasks = Vec::new();
    for i in 0..3 {
        let (c, base) = (c.clone(), base.clone());
        tasks.push(tokio::spawn(async move {
            c.update_synced_preferences(&base, add_favorite(format!("n{i}")))
                .await
        }));
    }
    for t in tasks {
        match t.await.unwrap() {
            Err(Error::Network(text)) => assert!(!text.is_empty()),
            other => panic!("{other:?}"),
        }
    }
}

/// Answers a settings PATCH like [`EchoSettings`], after a delay.
struct SlowEcho(Duration);

impl Respond for SlowEcho {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        EchoSettings.respond(req).set_delay(self.0)
    }
}

#[tokio::test]
async fn dropping_the_caller_that_holds_the_writer_does_not_cancel_the_batch() {
    let (server, instance) = common::instance().await;
    Mock::given(method("PATCH"))
        .and(path("/api/v1/users/@me/settings"))
        .respond_with(SlowEcho(Duration::from_millis(1_500)))
        .mount(&server)
        .await;
    let c = client(&instance, "tok-prefs-drop");
    let stale = SyncedPreferences::decode(&snapshot_from_a_newer_client(b"x")).unwrap();

    let first = {
        let (c, stale) = (c.clone(), stale.clone());
        tokio::spawn(async move {
            c.update_synced_preferences(&stale, add_favorite("first".into()))
                .await
        })
    };
    let second = {
        let (c, stale) = (c.clone(), stale.clone());
        tokio::spawn(async move {
            c.update_synced_preferences(&stale, add_favorite("second".into()))
                .await
        })
    };
    // Wait until the PATCH of the batch is in flight, then drop the first caller.
    for _ in 0..200 {
        if patches(&server.received_requests().await.unwrap()) == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(patches(&server.received_requests().await.unwrap()), 1);
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());

    // The other caller gets the real result, with both edits in it.
    let saved = second.await.unwrap().unwrap();
    let favs = &saved
        .get()
        .emoji_picker
        .as_ref()
        .unwrap()
        .favorite_emoji_ids;
    assert!(favs.contains(&"first".to_owned()) && favs.contains(&"second".to_owned()));

    // The write was stored, so the next one, from the same stale snapshot,
    // does not conflict with it: no GET and no second PATCH of the old base.
    let third = c
        .update_synced_preferences(&stale, add_favorite("third".into()))
        .await
        .unwrap();
    assert!(third
        .get()
        .emoji_picker
        .as_ref()
        .unwrap()
        .favorite_emoji_ids
        .contains(&"first".to_owned()));
    let reqs = server.received_requests().await.unwrap();
    assert_eq!((patches(&reqs), gets(&reqs)), (2, 0));
}

#[test]
fn synced_preferences_decoding_is_lenient_about_base64_and_strict_about_size() {
    let bytes = snapshot_from_a_newer_client(b"x");
    let std = b64(&bytes);
    let url_safe = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&bytes);
    assert_eq!(
        SyncedPreferences::from_base64(&std).unwrap(),
        SyncedPreferences::from_base64(&url_safe).unwrap()
    );
    assert_eq!(
        SyncedPreferences::from_base64("").unwrap(),
        SyncedPreferences::empty()
    );
    assert!(SyncedPreferences::from_base64("not base64!").is_err());
    assert!(SyncedPreferences::from_base64(&"A".repeat(700_000)).is_err());
    // An empty snapshot hashes as zero bytes.
    assert_eq!(
        SyncedPreferences::empty().base_hash(),
        hex::encode(Sha256::digest(b""))
    );
}

// ----- Relationship and account events -----

#[test]
fn relationship_events_update_types() {
    let add = Dispatch::parse(
        "RELATIONSHIP_ADD",
        json!({"id": "11", "type": 3, "user": common::user_json("11", "bo"),
               "since": "2026-11-10T08:00:00.000Z", "nickname": null,
               "share_voice_activity": true, "friend_shares_voice_activity": true}),
    );
    let mut stored = match add {
        Dispatch::RelationshipAdd(r) => *r,
        other => panic!("{other:?}"),
    };
    assert!(stored.is_incoming_request());

    // Accepted: the same account becomes a friend. The update has no user here;
    // the stored one stays.
    let update = Dispatch::parse(
        "RELATIONSHIP_UPDATE",
        json!({"id": "11", "type": 1, "nickname": "Bo from climbing", "since": null}),
    );
    match update {
        Dispatch::RelationshipUpdate(r) => stored.apply_update(*r),
        other => panic!("{other:?}"),
    }
    assert!(stored.is_friend() && !stored.is_incoming_request());
    assert_eq!(stored.user.as_ref().unwrap().username, "bo");
    assert_eq!(stored.nickname.as_deref(), Some("Bo from climbing"));

    // Blocked, then removed.
    match Dispatch::parse("RELATIONSHIP_UPDATE", json!({"id": "11", "type": 2})) {
        Dispatch::RelationshipUpdate(r) => stored.apply_update(*r),
        other => panic!("{other:?}"),
    }
    assert!(stored.is_blocked());
    assert!(matches!(
        Dispatch::parse("RELATIONSHIP_REMOVE", json!({"id": 11})),
        Dispatch::RelationshipRemove(r) if r.id == "11"
    ));

    // READY: relationships without `user` get it from `users`; notes and pins.
    let ready: Ready = serde_json::from_value(json!({
        "session_id": "s", "user": common::user_json("10", "ana"),
        "guilds": [], "private_channels": [], "read_states": [],
        "users": [common::user_json("11", "bo")],
        "relationships": [{"id": "11", "type": 1}, {"id": "12", "type": 4}, {"type": 1}],
        "notes": {"11": "met at the meetup", "bad": "x"},
        "pinned_dms": ["500", 501],
        "favorite_memes": [{"id": "70", "name": "parrot", "url": "x", "tags": ["party"]}]
    }))
    .unwrap();
    let rels = ready.relationships();
    assert_eq!(rels.len(), 2);
    assert_eq!(rels[0].user.as_ref().unwrap().username, "bo");
    assert!(rels[1].user.is_none() && rels[1].is_outgoing_request());
    assert_eq!(ready.notes().len(), 1);
    assert_eq!(ready.pinned_dms(), ["500", "501"]);
    assert_eq!(ready.favorite_memes()[0].tags, ["party"]);

    // The other account-level events.
    assert!(matches!(
        Dispatch::parse("USER_NOTE_UPDATE", json!({"id": "11", "note": ""})),
        Dispatch::UserNoteUpdate(n) if n.id == "11" && n.note.is_empty()
    ));
    assert!(matches!(
        Dispatch::parse("USER_PINNED_DMS_UPDATE", json!(["501", "500"])),
        Dispatch::UserPinnedDmsUpdate(ids) if ids == ["501", "500"]
    ));
    assert!(matches!(
        Dispatch::parse("CHANNEL_RECIPIENT_ADD",
            json!({"channel_id": "500", "user": common::user_json("13", "di")})),
        Dispatch::ChannelRecipientAdd(e) if e.channel_id == "500" && e.user.id == "13"
    ));
    assert!(matches!(
        Dispatch::parse(
            "CHANNEL_RECIPIENT_REMOVE",
            json!({"channel_id": "500", "user": common::user_json("13", "di")})
        ),
        Dispatch::ChannelRecipientRemove(_)
    ));
    assert!(matches!(
        Dispatch::parse("FAVORITE_MEME_CREATE", json!({"id": "70", "name": "parrot"})),
        Dispatch::FavoriteMemeCreate(m) if m.id == "70"
    ));
    assert!(matches!(
        Dispatch::parse("FAVORITE_MEME_DELETE", json!({"meme_id": "70"})),
        Dispatch::FavoriteMemeDelete(m) if m.meme_id == "70"
    ));
    // Payloads that do not parse stay raw.
    for (name, data) in [
        ("RELATIONSHIP_ADD", json!({"type": 1})),
        ("RELATIONSHIP_REMOVE", json!({})),
        ("USER_PINNED_DMS_UPDATE", json!({"x": 1})),
        ("CHANNEL_RECIPIENT_ADD", json!({"channel_id": "1"})),
    ] {
        assert!(
            matches!(Dispatch::parse(name, data), Dispatch::Other { .. }),
            "{name}"
        );
    }
}

// ----- GIFs -----

fn gif_json(base: &str, slug: &str, proxied: bool) -> Value {
    let proxy = |file: &str| {
        if proxied {
            format!("{base}/media/external/abc/{file}")
        } else {
            // A "proxy" URL that is not the instance's media endpoint.
            format!("https://cdn.elsewhere.example/{file}")
        }
    };
    json!({
        "id": slug, "slug": slug, "provider": "klipy", "title": "Goat Plays Banjo",
        "url": format!("https://klipy.example/gifs/{slug}"),
        "src": "https://static.klipy.example/goat.webm",
        "proxy_src": proxy("goat.webm"), "width": 220, "height": 229,
        "media": {
            "webm": {"src": "https://static.klipy.example/goat.webm",
                     "proxy_src": proxy("goat.webm"), "width": 220, "height": 229},
            "tinygif": {"src": "https://static.klipy.example/goat-tiny.gif",
                        "proxy_src": proxy("goat-tiny.gif"), "width": 137, "height": 90}
        }
    })
}

#[tokio::test]
async fn gif_search_uses_the_instance_endpoint_only() {
    let (server, instance) = common::instance().await;
    let base = server.uri();
    Mock::given(method("GET"))
        .and(path("/api/v1/gifs/search"))
        .and(query_param("q", "goat & banjo #1"))
        .and(query_param("locale", "en-US"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            gif_json(&base, "goat-1", true),
            gif_json(&base, "goat-2", false),
            {"broken": true}
        ])))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/gifs/featured"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "gifs": [gif_json(&base, "goat-3", true)],
            "categories": [{"name": "dance", "src": "https://static.klipy.example/d.gif",
                            "proxy_src": format!("{base}/media/external/d/d.gif"), "gif": null}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/gifs/trending"))
        .and(query_param("locale", "no"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/gifs/suggest"))
        .and(query_param("q", "goa"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!(["goat", "goal"])))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/gifs/register-share"))
        .and(body_json(
            json!({"id": "goat-1", "q": "goat", "locale": "en-US"}),
        ))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/users/@me/memes"))
        .and(body_json(
            json!({"url": "https://klipy.example/gifs/goat-1",
                              "gif_slug": "goat-1", "gif_provider": "klipy",
                              "name": "Goat Plays Banjo"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "70", "name": "Goat Plays Banjo", "tags": [],
            "url": format!("{base}/media/attachments/1/2/goat.gif?ex=1&is=2&hm=3"),
            "content_type": "image/gif", "size": 10, "is_gifv": true
        })))
        .expect(1)
        .mount(&server)
        .await;
    let c = client(&instance, "tok-gif");
    let media = instance.media().unwrap();

    let gifs = c.gif_search("goat & banjo #1", "en-US").await.unwrap();
    assert_eq!(gifs.len(), 2);
    // Previews only through the instance's media proxy...
    let preview = gifs[0].preview(&media, Transform::default()).unwrap();
    assert!(
        preview.starts_with(&format!("{base}/media/external/")),
        "{preview}"
    );
    assert!(preview.ends_with("goat-tiny.gif"));
    // ...and none at all when the proxy URL points somewhere else.
    assert_eq!(gifs[1].preview(&media, Transform::default()), None);
    // The provider's direct URLs are not even kept.
    let kept = serde_json::to_string(&gifs).unwrap();
    assert!(!kept.contains("static.klipy.example"), "{kept}");

    let featured = c.gif_featured("en-US").await.unwrap();
    assert_eq!(featured.categories[0].name, "dance");
    assert!(featured.categories[0]
        .preview(&media, Transform::default())
        .is_some_and(|u| u.starts_with(&format!("{base}/media/external/"))));
    assert!(c.gif_trending("nb").await.unwrap().is_empty());
    assert_eq!(
        c.gif_suggestions("goa", "en-US").await.unwrap(),
        ["goat", "goal"]
    );
    c.register_gif_share(&gifs[0], Some("goat"), "en-US")
        .await
        .unwrap();
    // Saving a GIF as a meme: the instance fetches it, not this client.
    let meme = c.save_gif_as_meme(&gifs[0]).await.unwrap();
    assert!(meme.proxied(&media, Transform::default()).is_some());

    // Bad queries never leave the client; locales are mapped onto Fluxer's
    // list, so `nb` went out as `no` above and nothing else can be injected.
    for q in ["", "   ", "a\nb"] {
        assert!(matches!(
            c.gif_search(q, "en-US").await,
            Err(Error::InvalidInput(_))
        ));
    }
    assert_eq!(
        inlet_protocol::api::gifs::fluxer_locale("en-US&q=y"),
        "en-US"
    );
    // Every request went to the instance's API, under /gifs (or the meme save).
    for r in server.received_requests().await.unwrap() {
        let p = r.url.path();
        assert!(
            p == "/.well-known/fluxer"
                || p.starts_with("/api/v1/gifs/")
                || p == "/api/v1/users/@me/memes",
            "{p}"
        );
    }

    // The instance says whether it has GIFs at all.
    assert!(!instance.discovery().gif_enabled());
    assert_eq!(instance.discovery().gif_provider(), None);
}

#[tokio::test]
async fn gif_searches_are_paced() {
    let (server, instance) = common::instance().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/gifs/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    let c = client(&instance, "tok-gif-budget");
    // Ten searches at once (fast typing without a debounce): slots every
    // 500 ms up to 3 s away get through, the rest are refused at once.
    let started = Instant::now();
    let mut tasks = Vec::new();
    for i in 0..10 {
        let c = c.clone();
        tasks.push(tokio::spawn(async move {
            c.gif_search(&format!("q{i}"), "en-US").await
        }));
    }
    let (mut ok, mut refused) = (0, 0);
    for t in tasks {
        match t.await.unwrap() {
            Ok(_) => ok += 1,
            Err(Error::RateLimited { .. }) => refused += 1,
            Err(e) => panic!("{e}"),
        }
    }
    assert_eq!((ok, refused), (7, 3));
    assert!(started.elapsed() >= Duration::from_millis(2_900));
    assert_eq!(requests_to(&server, "/api/v1/gifs/search").await.len(), 7);
}

// ----- Invites -----

#[tokio::test]
async fn invite_code_is_validated_before_use_in_a_path() {
    let (server, instance) = common::instance().await;
    let invite = json!({
        "code": "aB3dEf7h", "type": 0,
        "guild": {"id": "1489002177550843904", "name": "Example guild", "icon": null},
        "channel": {"id": "1489002177550843905", "name": "general", "type": 0},
        "inviter": common::user_json("10", "ana"),
        "member_count": 412, "presence_count": 87, "expires_at": null, "temporary": false
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/invites/aB3dEf7h"))
        .respond_with(ResponseTemplate::new(200).set_body_json(invite.clone()))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/invites/aB3dEf7h"))
        .respond_with(ResponseTemplate::new(200).set_body_json(invite))
        .expect(1)
        .mount(&server)
        .await;
    let c = client(&instance, "tok-invite");

    let long = "a".repeat(257);
    for bad in [
        "",
        "../users/@me",
        "..",
        "aB3/dEf7h",
        "aB3dEf7h?x=1",
        "aB3dEf7h#x",
        "a b",
        "%2e%2e",
        "a\\b",
        "a\u{0}b",
        "ä",
        long.as_str(),
    ] {
        assert!(
            matches!(c.invite(bad).await, Err(Error::InvalidInput(_))),
            "{bad:?}"
        );
        assert!(matches!(
            c.accept_invite(bad).await,
            Err(Error::InvalidInput(_))
        ));
    }
    // None of those reached the server.
    let invites = |reqs: Vec<Request>| {
        reqs.iter()
            .filter(|r| r.url.path().starts_with("/api/v1/invites"))
            .count()
    };
    assert_eq!(invites(server.received_requests().await.unwrap()), 0);

    let found = c.invite("aB3dEf7h").await.unwrap();
    assert_eq!(
        found.guild.as_ref().unwrap().name.as_deref(),
        Some("Example guild")
    );
    assert_eq!(found.member_count, Some(412));
    let joined = c.accept_invite("aB3dEf7h").await.unwrap();
    assert_eq!(joined.code, "aB3dEf7h");
    assert_eq!(invites(server.received_requests().await.unwrap()), 2);

    // Links: only this instance's invite links give a code; invite-shaped
    // links of other origins come back with their origin, so the UI can route
    // them.
    let base = server.uri();
    let parse = |s: &str| parse_invite(s, &instance);
    let code = |c: &str| InviteLink::Code(c.into());
    assert_eq!(parse("aB3dEf7h"), code("aB3dEf7h"));
    assert_eq!(parse(&format!("{base}/invite/aB3dEf7h")), code("aB3dEf7h"));
    assert_eq!(
        parse(&format!("{base}/invite/a%2F..")),
        InviteLink::NotAnInvite
    );
    assert_eq!(parse(&format!("{base}/invite/a?x=1")), code("a"));
    assert_eq!(
        parse(&format!("{base}/channels/aB3dEf7h")),
        InviteLink::NotAnInvite
    );
    assert_eq!(parse("not a link"), InviteLink::NotAnInvite);
    let other = |origin: &str, c: &str| InviteLink::OtherInstance {
        origin: origin.into(),
        code: c.into(),
    };
    assert_eq!(
        parse("https://Other.Example/invite/aB3dEf7h"),
        other("https://other.example", "aB3dEf7h")
    );
    // The port is part of the origin; a default port is not.
    assert_eq!(
        parse("https://chat.example:8443/invite/aB3dEf7h"),
        other("https://chat.example:8443", "aB3dEf7h")
    );
    assert_eq!(
        parse("https://chat.example:443/invite/aB3dEf7h"),
        other("https://chat.example", "aB3dEf7h")
    );
    assert_eq!(
        parse("https://fluxer.gg/xyz"),
        other("https://fluxer.gg", "xyz")
    );
    // Not invite paths: any other https link, even with a code-like segment.
    for link in [
        "https://github.com/foo",
        "https://github.com/foo/bar",
        "https://github.com/",
        "https://chat.example:8443/channels/1/2",
        "https://other.example/invite/a%2F..",
        "https://other.example/invite/",
        "https://user:pw@other.example/invite/aB3dEf7h",
    ] {
        assert_eq!(parse(link), InviteLink::NotAnInvite, "{link}");
    }
    // Plain http elsewhere is not followed at all.
    assert_eq!(
        parse("http://evil.example/invite/aB3dEf7h"),
        InviteLink::NotAnInvite
    );

    // An instance whose `endpoints.invite` is a bare host: `{invite}/{code}`.
    let short = MockServer::start().await;
    let mut doc = common::discovery_json(&short.uri(), "ws://127.0.0.1:1/gateway");
    doc["endpoints"]["invite"] = json!(short.uri());
    Mock::given(method("GET"))
        .and(path("/.well-known/fluxer"))
        .respond_with(ResponseTemplate::new(200).set_body_json(doc))
        .mount(&short)
        .await;
    let short_instance = Instance::discover_with(
        &short.uri(),
        inlet_protocol::ClientInfo {
            installation_id: "00000000-0000-4000-a000-000000000000".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        parse_invite(&format!("{}/aB3dEf7h", short.uri()), &short_instance),
        code("aB3dEf7h")
    );
    assert_eq!(
        parse_invite(&format!("{}/invite/aB3dEf7h", short.uri()), &short_instance),
        code("aB3dEf7h")
    );
}
