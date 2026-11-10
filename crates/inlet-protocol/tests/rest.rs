//! REST client and discovery against a mock instance.

mod common;

use std::time::{Duration, Instant};

use inlet_protocol::{Client, Error, Instance, RateLimitConfig, Token};
use serde_json::json;
use wiremock::matchers::{body_json, header, header_exists, method, path, query_param};
use wiremock::{Mock, ResponseTemplate};

#[tokio::test]
async fn discovery_reads_endpoints_and_uses_etag() {
    let (server, instance) = common::instance().await;
    assert_eq!(instance.api_base(), format!("{}/api/v1", server.uri()));
    assert_eq!(instance.etag(), Some("\"v1\""));
    assert_eq!(instance.discovery().product_name(), Some("Mock Social"));
    assert!(instance.discovery().captcha_required());
    assert_eq!(instance.discovery().max_message_length(), Some(4000));
    assert_eq!(
        instance.avatar_url("5", Some("abc"), 64).as_deref(),
        Some(format!("{}/media/avatars/5/abc.webp?size=64", server.uri()).as_str())
    );

    // A refresh with the same ETag gets 304 and keeps the document.
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/fluxer"))
        .and(header("If-None-Match", "\"v1\""))
        .respond_with(ResponseTemplate::new(304))
        .expect(1)
        .mount(&server)
        .await;
    let again = instance.refresh().await.unwrap();
    assert_eq!(again.etag(), Some("\"v1\""));
    assert_eq!(again.api_base(), instance.api_base());
}

#[tokio::test]
async fn rejects_newer_api_code_version() {
    let server = wiremock::MockServer::start().await;
    let mut doc = common::discovery_json(&server.uri(), "ws://127.0.0.1:1/");
    doc["api_code_version"] = json!(2);
    Mock::given(path("/.well-known/fluxer"))
        .respond_with(ResponseTemplate::new(200).set_body_json(doc))
        .mount(&server)
        .await;
    let err = Instance::discover(&server.uri()).await.unwrap_err();
    assert!(matches!(err, Error::UnsupportedApiVersion(2)), "{err:?}");
}

#[tokio::test]
async fn endpoints_send_token_and_parse_models() {
    let (server, instance) = common::instance().await;
    let auth = || header("Authorization", "tok-rest-1");
    Mock::given(method("GET"))
        .and(path("/api/v1/users/@me"))
        .and(auth())
        .and(|req: &wiremock::Request| {
            req.headers.get("x-fluxer-features").and_then(|v| v.to_str().ok())
                == Some("view_channel_members_permission,channel_threads")
        })
        .and(header_exists("X-Fluxer-Client-Installation-ID"))
        .respond_with(ResponseTemplate::new(200).set_body_json({
            let mut u = common::user_json("10", "ana");
            u["email"] = json!("ana@example.org");
            u["mfa_enabled"] = json!(true);
            u
        }))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/users/@me/guilds"))
        .and(query_param("limit", "200"))
        .and(auth())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": "100", "name": "Hedgerow", "icon": null, "owner_id": "10", "features": [],
             "splash_card_alignment": 0, "system_channel_flags": 0, "afk_timeout": 300}
        ])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/guilds/100/channels"))
        .and(auth())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": "200", "type": 0, "guild_id": "100", "name": "general", "position": 0, "parent_id": "199"},
            {"id": "199", "type": 4, "guild_id": "100", "name": "Text", "position": 0},
            {"id": "201", "type": 15, "guild_id": "100", "name": "forum"}
        ])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/users/@me/channels"))
        .and(auth())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": "300", "type": 1, "recipients": [common::user_json("11", "bo")], "last_message_id": "5"}
        ])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/channels/200/messages"))
        .and(query_param("limit", "50"))
        .and(query_param("before", "1000"))
        .and(auth())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            common::message_json("999", "200", "second"),
            common::message_json("998", "200", "first")
        ])))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/channels/200/messages"))
        .and(auth())
        .and(body_json(json!({"content": "hello", "nonce": "12345"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(common::message_json("1001", "200", "hello")))
        .expect(1)
        .mount(&server)
        .await;

    let client = Client::new(instance, Token::new("tok-rest-1"));
    let me = client.me().await.unwrap();
    assert_eq!(me.username, "ana");
    assert_eq!(me.email(), Some("ana@example.org"));
    assert!(me.mfa_enabled());

    let guilds = client.guilds().await.unwrap();
    assert_eq!(guilds.len(), 1);
    assert_eq!(guilds[0].name, "Hedgerow");

    let channels = client.channels("100").await.unwrap();
    assert_eq!(channels.len(), 3);
    assert!(channels[0].is_text());
    assert_eq!(channels[0].parent_id.as_deref(), Some("199"));
    assert!(!channels[1].is_text());

    let dms = client.dm_channels().await.unwrap();
    assert_eq!(dms[0].recipients[0].username, "bo");

    let messages = client.messages("200", Some("1000"), 50).await.unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].content, "second");
    assert_eq!(messages[0].attachments[0].content_type.as_deref(), Some("image/png"));

    let sent = client.send_message("200", "hello", Some("12345")).await.unwrap();
    assert_eq!(sent.id, "1001");
}

#[tokio::test]
async fn route_429_is_retried_after_retry_after() {
    let (server, instance) = common::instance().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/users/@me"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "1")
                .set_body_json(json!({"code": "RATE_LIMITED", "message": "slow down", "global": false, "retry_after": 0.3})),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/users/@me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(common::user_json("10", "ana")))
        .with_priority(2)
        .mount(&server)
        .await;
    let client = Client::new(instance, Token::new("tok-429-route"));
    let start = Instant::now();
    let me = client.me().await.unwrap();
    assert_eq!(me.id, "10");
    // Waited the body's retry_after (0.3 s), not the rounded header (1 s).
    let waited = start.elapsed();
    assert!(waited >= Duration::from_millis(300) && waited < Duration::from_millis(1000), "{waited:?}");
}

#[tokio::test]
async fn global_429_stops_and_reports_revocation() {
    let (server, instance) = common::instance().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/users/@me"))
        .respond_with(ResponseTemplate::new(429).set_body_json(
            json!({"code": "RATE_LIMITED", "message": "global", "global": true, "retry_after": 0.5}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    let client = Client::new(instance, Token::new("tok-429-global"));
    let err = client.me().await.unwrap_err();
    assert!(matches!(err, Error::GlobalRateLimited), "{err:?}");
}

#[tokio::test]
async fn unauthorized_and_api_errors() {
    let (server, instance) = common::instance().await;
    Mock::given(path("/api/v1/users/@me"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({"code": "UNAUTHORIZED", "message": "no"})))
        .mount(&server)
        .await;
    Mock::given(path("/api/v1/guilds/1/channels"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"code": "UNKNOWN_GUILD", "message": "Unknown guild"})))
        .mount(&server)
        .await;
    let client = Client::new(instance, Token::new("tok-401"));
    assert!(matches!(client.me().await.unwrap_err(), Error::Unauthorized));
    match client.channels("1").await.unwrap_err() {
        Error::Api(e) => {
            assert_eq!(e.status, 404);
            assert_eq!(e.code, "UNKNOWN_GUILD");
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn concurrent_requests_respect_the_shared_cap() {
    let (server, instance) = common::instance().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/users/@me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(common::user_json("10", "ana")))
        .mount(&server)
        .await;
    let config = RateLimitConfig {
        rate_per_sec: 20.0,
        burst: 5,
    };
    // Two clients for the same token share one limiter.
    let a = Client::with_rate_limit(instance.clone(), Token::new("tok-cap"), config);
    let b = Client::with_rate_limit(instance, Token::new("tok-cap"), config);
    assert!(std::sync::Arc::ptr_eq(a.rate_limiter(), b.rate_limiter()));

    let start = Instant::now();
    let mut tasks = Vec::new();
    for i in 0..45 {
        let c = if i % 2 == 0 { a.clone() } else { b.clone() };
        tasks.push(tokio::spawn(async move { c.me().await.map(|_| Instant::now()) }));
    }
    let mut times = Vec::new();
    for t in tasks {
        times.push(t.await.unwrap().unwrap());
    }
    times.sort();
    // 40 requests after the burst of 5 at 20/s need at least about 2 s.
    let total = start.elapsed();
    assert!(total >= Duration::from_millis(1_900), "{total:?}");
    // No one-second window holds more than burst + rate (+1 for timer slack).
    let mut start_idx = 0;
    for end in 0..times.len() {
        while times[end] - times[start_idx] >= Duration::from_secs(1) {
            start_idx += 1;
        }
        assert!(end - start_idx < 26, "window of {} requests", end - start_idx + 1);
    }
}
