//! Chat features against a mock instance: messages, replies, reactions, pins,
//! typing, read states, settings, profiles, members, roles and uploads.

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use inlet_protocol::models::ReferencedMessage;
use inlet_protocol::{
    Client, ClientInfo, Emoji, Error, ImageOptions, Instance, OutgoingMessage, Token, Transform,
    UploadFile, UserSettingsUpdate,
};
use serde_json::{json, Value};
use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn client(instance: &Instance, token: &str) -> Client {
    Client::new(instance.clone(), Token::new(token))
}

async fn requests_to(server: &MockServer, m: &str, p: &str) -> Vec<Request> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.method.as_str() == m && r.url.path() == p)
        .collect()
}

#[tokio::test]
async fn get_edit_delete_and_reply() {
    let (server, instance) = common::instance().await;
    let c = client(&instance, "tok-msg");
    Mock::given(method("GET"))
        .and(path("/api/v1/channels/2/messages/5"))
        .and(header("Authorization", "tok-msg"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(common::message_json("5", "2", "hi")),
        )
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/api/v1/channels/2/messages/5"))
        .and(body_json(json!({"content": "edited"})))
        .respond_with(ResponseTemplate::new(200).set_body_json({
            let mut m = common::message_json("5", "2", "edited");
            m["edited_timestamp"] = json!("2026-11-10T08:01:00.000Z");
            m
        }))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/api/v1/channels/2/messages/5"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/channels/2/messages"))
        .and(body_json(json!({
            "content": "agreed", "nonce": "77",
            "message_reference": {"message_id": "5", "type": 0},
            "allowed_mentions": {"parse": ["users", "roles", "everyone"], "replied_user": false}
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json({
            let mut m = common::message_json("6", "2", "agreed");
            m["type"] = json!(19);
            m["message_reference"] = json!({"channel_id": "2", "message_id": "5", "type": 0});
            m["referenced_message"] = common::message_json("5", "2", "hi");
            m
        }))
        .expect(1)
        .mount(&server)
        .await;

    assert_eq!(c.message("2", "5").await.unwrap().content, "hi");
    let edited = c.edit_message("2", "5", "edited").await.unwrap();
    assert!(edited.edited_timestamp.is_some());
    c.delete_message("2", "5").await.unwrap();
    let reply = c
        .reply("2", "5", "agreed", false, Some("77"))
        .await
        .unwrap();
    assert!(reply.is_reply());
    match reply.referenced_message() {
        ReferencedMessage::Message(m) => assert_eq!(m.content, "hi"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn reactions_encode_the_emoji_and_list_users() {
    let (server, instance) = common::instance().await;
    let c = client(&instance, "tok-react");
    Mock::given(method("PUT"))
        .and(path(
            "/api/v1/channels/2/messages/3/reactions/%F0%9F%91%8D/@me",
        ))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path(
            "/api/v1/channels/2/messages/3/reactions/party%3A55/@me",
        ))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(
            "/api/v1/channels/2/messages/3/reactions/party%3A55/users",
        ))
        .and(query_param("limit", "100"))
        .and(query_param("after", "9"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [common::user_json("10", "ana"), {"bad": true}],
            "has_more": true, "next_after": "10"
        })))
        .mount(&server)
        .await;

    c.add_reaction("2", "3", &Emoji::unicode("👍"))
        .await
        .unwrap();
    let party = Emoji::custom("55", "party", true);
    c.remove_reaction("2", "3", &party).await.unwrap();
    let page = c
        .reaction_users("2", "3", &party, Some("9"), 200)
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert!(page.has_more);
    assert_eq!(page.next_after.as_deref(), Some("10"));

    // Hostile emoji never reach the network.
    for bad in [
        Emoji::custom("55/../x", "party", false),
        Emoji::custom("55", "par:ty", false),
        Emoji::unicode(""),
        Emoji::unicode("\u{7}"),
        Emoji::unicode("x".repeat(65)),
    ] {
        let err = c.add_reaction("2", "3", &bad).await.unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)), "{err:?}");
    }
    assert_eq!(server.received_requests().await.unwrap().len(), 4); // discovery + 3
}

#[tokio::test]
async fn pins_page_pin_unpin_and_ack() {
    let (server, instance) = common::instance().await;
    let c = client(&instance, "tok-pins");
    Mock::given(method("GET"))
        .and(path("/api/v1/channels/2/messages/pins"))
        .and(query_param("limit", "50"))
        .and(query_param("before", "2026-11-10T08:00:00.000+01:00"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [{"message": common::message_json("5", "2", "pinned"),
                       "pinned_at": "2026-11-09T08:00:00.000Z"}],
            "has_more": false
        })))
        .mount(&server)
        .await;
    for (m, p) in [
        ("PUT", "/api/v1/channels/2/pins/5"),
        ("DELETE", "/api/v1/channels/2/pins/5"),
        ("POST", "/api/v1/channels/2/pins/ack"),
    ] {
        Mock::given(method(m))
            .and(path(p))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
    }
    let page = c
        .pins("2", Some("2026-11-10T08:00:00.000+01:00"), 99)
        .await
        .unwrap();
    assert_eq!(page.items[0].message.content, "pinned");
    assert_eq!(page.items[0].pinned_at, "2026-11-09T08:00:00.000Z");
    c.pin_message("2", "5").await.unwrap();
    c.unpin_message("2", "5").await.unwrap();
    c.ack_pins("2").await.unwrap();
}

#[tokio::test]
async fn typing_is_sent_once_per_channel_per_interval() {
    let (server, instance) = common::instance().await;
    let c = client(&instance, "tok-typing");
    Mock::given(method("POST"))
        .and(path("/api/v1/channels/2/typing"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/channels/3/typing"))
        .respond_with(
            ResponseTemplate::new(403).set_body_json(json!({"code": "MISSING_PERMISSIONS"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    for _ in 0..25 {
        c.trigger_typing("2").await.unwrap();
    }
    // Clones share the debounce.
    c.clone().trigger_typing("2").await.unwrap();
    // A refused channel is not retried on every key press either.
    assert!(c.trigger_typing("3").await.is_err());
    for _ in 0..10 {
        c.trigger_typing("3").await.unwrap();
    }
}

#[tokio::test]
async fn acks_are_coalesced_into_one_batch() {
    let (server, instance) = common::instance().await;
    let c = client(&instance, "tok-acks");
    Mock::given(method("POST"))
        .and(path("/api/v1/read-states/ack"))
        .respond_with(|req: &Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap();
            let states: Vec<Value> = body["read_states"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| json!({"id": e["channel_id"], "mention_count": 0, "last_message_id": e["message_id"]}))
                .collect();
            ResponseTemplate::new(200).set_body_json(json!({ "read_states": states }))
        })
        .mount(&server)
        .await;

    for i in 100..150u64 {
        c.ack("2", &i.to_string());
    }
    c.ack("3", "7");
    c.ack("2", "120"); // older than what is pending: dropped
    tokio::time::sleep(Duration::from_millis(1_600)).await;
    let sent = requests_to(&server, "POST", "/api/v1/read-states/ack").await;
    assert_eq!(sent.len(), 1, "one request for 51 acks");
    let body: Value = serde_json::from_slice(&sent[0].body).unwrap();
    let entries = body["read_states"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0], json!({"channel_id": "2", "message_id": "149"}));
    assert_eq!(entries[1], json!({"channel_id": "3", "message_id": "7"}));

    // Already acknowledged: nothing more goes out.
    c.ack("2", "149");
    assert!(c.flush_acks().await.unwrap().is_empty());
    assert_eq!(
        requests_to(&server, "POST", "/api/v1/read-states/ack")
            .await
            .len(),
        1
    );

    // Mark unread is sent with manual and the mention count, and returns read states.
    c.mark_unread("2", "130", 2);
    for i in 0..150u64 {
        c.ack(&format!("{}", 1_000 + i), "1");
    }
    let states = c.flush_acks().await.unwrap();
    assert_eq!(states.len(), 151);
    let all = requests_to(&server, "POST", "/api/v1/read-states/ack").await;
    assert_eq!(all.len(), 3, "151 channels go out as 100 + 51");
    let manual: Vec<Value> = all[1..]
        .iter()
        .flat_map(|r| {
            let v: Value = serde_json::from_slice(&r.body).unwrap();
            v["read_states"].as_array().unwrap().clone()
        })
        .filter(|e| e["channel_id"] == "2")
        .collect();
    assert_eq!(
        manual,
        vec![json!({"channel_id": "2", "message_id": "130", "mention_count": 2, "manual": true})]
    );
}

#[tokio::test]
async fn settings_profile_members_and_roles() {
    let (server, instance) = common::instance().await;
    let c = client(&instance, "tok-account");
    Mock::given(method("GET"))
        .and(path("/api/v1/users/@me/settings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "theme": "coal", "locale": "nb-NO", "message_display_compact": false, "status": "online"
        })))
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/api/v1/users/@me/settings"))
        .and(body_json(
            json!({"theme": "light", "message_display_compact": true}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "theme": "light", "locale": "nb-NO", "message_display_compact": true
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/users/10/profile"))
        .and(query_param("guild_id", "100"))
        .and(query_param("with_mutual_guilds", "true"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "user": common::user_json("10", "ana"),
            "user_profile": {"bio": "hi", "pronouns": null, "banner": "b1", "accent_color": 16711680},
            "guild_member": {"user": common::user_json("10", "ana"), "nick": "Ana", "roles": ["7"]},
            "mutual_guilds": [{"id": "100", "nick": "Ana"}],
            "profile_badges": [], "connected_accounts": [], "timezone_offset": null
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/guilds/100/members/10"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "user": common::user_json("10", "ana"), "nick": null, "roles": ["7"],
            "joined_at": "2026-01-01T00:00:00Z", "mute": false, "deaf": false,
            "communication_disabled_until": null
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/guilds/100/members"))
        .and(query_param("limit", "1000"))
        .and(query_param("after", "5"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"user": common::user_json("10", "ana"), "roles": []}, "not a member"
        ])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/guilds/100/roles"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": "7", "name": "Mod", "color": 3447003, "position": 4, "permissions": "8192",
             "hoist": true, "mentionable": false, "icon": null, "unicode_emoji": null, "hoist_position": 1},
            {"id": "100", "name": "@everyone", "color": 0, "position": 0, "permissions": "3072"}
        ])))
        .mount(&server)
        .await;

    let s = c.user_settings().await.unwrap();
    assert_eq!(s.theme, "coal");
    let s = c
        .update_user_settings(&UserSettingsUpdate {
            theme: Some("light".into()),
            message_display_compact: Some(true),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(s.message_display_compact);
    let p = c.user_profile("10", Some("100"), true).await.unwrap();
    assert_eq!(p.user_profile.accent_color, Some(0xff0000));
    assert_eq!(p.guild_member.unwrap().display_name(), Some("Ana"));
    assert_eq!(p.mutual_guilds[0].id, "100");
    let m = c.guild_member("100", "10").await.unwrap();
    assert_eq!(m.display_name(), Some("ana"));
    let members = c.guild_members("100", Some("5"), 5000).await.unwrap();
    assert_eq!(members.len(), 1);
    let roles = c.roles("100").await.unwrap();
    assert_eq!(roles[0].hoist_position, Some(1));
    assert_eq!(roles[1].permissions, 3072);
}

/// An instance whose limit rules allow `max_size` bytes and 2 files.
async fn upload_instance(max_size: u64) -> (MockServer, Instance) {
    let server = MockServer::start().await;
    let mut doc = common::discovery_json(&server.uri(), "ws://127.0.0.1:1/gateway");
    doc["limits"] = json!({"version": 2, "rules": [{"id": "default", "overrides": {
        "max_attachment_file_size": max_size, "max_attachments_per_message": 2}}]});
    Mock::given(path("/.well-known/fluxer"))
        .respond_with(ResponseTemplate::new(200).set_body_json(doc))
        .mount(&server)
        .await;
    let instance = Instance::discover_with(&server.uri(), ClientInfo::default())
        .await
        .unwrap();
    (server, instance)
}

#[tokio::test]
async fn upload_sends_multipart_with_progress_and_checks_limits() {
    let (server, instance) = upload_instance(200_000).await;
    let c = client(&instance, "tok-upload");
    assert_eq!(c.upload_limits().max_file_size, 200_000);
    assert_eq!(c.upload_limits().max_files, 2);
    // First a 429 (the body must be sent again), then success.
    Mock::given(method("POST"))
        .and(path("/api/v1/channels/2/messages"))
        .respond_with(ResponseTemplate::new(429).set_body_json(json!({
            "code": "RATE_LIMITED", "message": "slow", "global": false, "retry_after": 0.05
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/channels/2/messages"))
        .and(|req: &Request| {
            let ct = req
                .headers
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default();
            let body = String::from_utf8_lossy(&req.body);
            ct.starts_with("multipart/form-data; boundary=")
                && body.contains("name=\"payload_json\"")
                && body.contains("\"content\":\"look\"")
                && body.contains("\"attachments\":[{\"id\":0,\"filename\":\"cat.png\"}")
                && body
                    .contains("name=\"files[0]\"; filename=\"cat.png\"\r\nContent-Type: image/png")
                && body.contains("name=\"files[1]\"; filename=\"evil_name.txt\"")
                && req.body.windows(4).any(|w| w == [7, 7, 7, 7])
        })
        .respond_with(
            ResponseTemplate::new(200).set_body_json(common::message_json("9", "2", "look")),
        )
        .expect(1)
        .mount(&server)
        .await;

    let sent = Arc::new(AtomicU64::new(0));
    let total = Arc::new(AtomicU64::new(0));
    let (s2, t2) = (sent.clone(), total.clone());
    let progress: inlet_protocol::ProgressFn = Arc::new(move |done, all| {
        s2.store(done, Ordering::SeqCst);
        t2.store(all, Ordering::SeqCst);
    });
    let files = vec![
        UploadFile::new("cat.png", vec![7u8; 150_000]).content_type("image/png"),
        UploadFile::new("evil\"name.txt", b"x".to_vec()),
    ];
    let m = c
        .send_with_files("2", &OutgoingMessage::text("look"), &files, Some(progress))
        .await
        .unwrap();
    assert_eq!(m.id, "9");
    let total = total.load(Ordering::SeqCst);
    assert!(total > 150_000);
    assert_eq!(sent.load(Ordering::SeqCst), total);

    // Over the limits: refused before anything is sent.
    let before = server.received_requests().await.unwrap().len();
    let big = vec![UploadFile::new("big.bin", vec![0u8; 200_001])];
    let err = c
        .send_with_files("2", &OutgoingMessage::default(), &big, None)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::TooLarge { limit: 200_000 }), "{err:?}");
    let three: Vec<UploadFile> = (0..3)
        .map(|i| UploadFile::new(format!("{i}"), vec![1u8]))
        .collect();
    let err = c
        .send_with_files("2", &OutgoingMessage::default(), &three, None)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidInput(_)), "{err:?}");
    assert_eq!(server.received_requests().await.unwrap().len(), before);
}

#[tokio::test]
async fn new_routes_share_the_hard_cap() {
    let (server, instance) = common::instance().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    let c = client(&instance, "tok-cap");
    let start = tokio::time::Instant::now();
    let mut tasks = Vec::new();
    for i in 0..45u32 {
        let c = c.clone();
        tasks.push(tokio::spawn(async move {
            c.add_reaction("2", &format!("{}", 100 + i), &Emoji::unicode("👍"))
                .await
                .unwrap();
            tokio::time::Instant::now()
        }));
    }
    let mut times = Vec::new();
    for t in tasks {
        times.push(t.await.unwrap());
    }
    times.sort();
    // 45 requests at no more than 20 per rolling second take at least 2 s.
    assert!(
        times[44] - start >= Duration::from_millis(1_900),
        "{:?}",
        times[44] - start
    );
    for w in times.windows(21) {
        assert!(
            w[20] - w[0] >= Duration::from_millis(950),
            "21 within a second"
        );
    }
}

#[tokio::test]
async fn media_urls_come_from_the_instance_only() {
    let (server, instance) = common::instance().await;
    let media = instance.media().expect("loopback media endpoint");
    let base = format!("{}/media", server.uri());
    assert_eq!(media.base(), base);
    assert_eq!(
        media.avatar("10", Some("a_abc"), ImageOptions::size(64).animated()),
        Some(format!(
            "{base}/avatars/10/a_abc.webp?size=64&animated=true"
        ))
    );
    // The mock message's attachment points at another host: refused.
    let m: inlet_protocol::Message =
        serde_json::from_value(common::message_json("1", "2", "x")).unwrap();
    assert_eq!(
        media.attachment_of(&m.attachments[0], Transform::default()),
        None
    );
    let own = format!("{base}/attachments/2/77/cat.png?ex=1&is=2&hm=3");
    assert_eq!(
        media.attachment(
            &own,
            Transform {
                width: Some(320),
                ..Default::default()
            }
        ),
        Some(format!("{own}&width=320"))
    );
}
