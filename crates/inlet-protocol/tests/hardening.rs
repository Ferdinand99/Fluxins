//! Review fixes: hostile server values, limits, redirects, pinning, tolerant lists.

mod common;

use std::time::Duration;

use inlet_protocol::{
    altcha, sign_in_password, Client, ClientInfo, Discovery, Error, Handoff, Instance, Session,
    SignInResult, Token,
};
use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn rate_limited(retry_after: serde_json::Value) -> ResponseTemplate {
    ResponseTemplate::new(429).set_body_json(json!({
        "code": "RATE_LIMITED", "message": "slow", "global": false, "retry_after": retry_after
    }))
}

#[tokio::test]
async fn huge_or_infinite_server_waits_do_not_panic() {
    let (server, instance) = common::instance().await;
    // 1e20 seconds: clamped to 300 s and returned at once (too long to wait out).
    Mock::given(path("/api/v1/guilds/1/channels"))
        .respond_with(rate_limited(json!(1e20)).insert_header("Retry-After", "inf"))
        .mount(&server)
        .await;
    // A successful response with absurd bucket headers.
    Mock::given(path("/api/v1/users/@me"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("X-RateLimit-Remaining", "1e300")
                .insert_header("X-RateLimit-Reset-After", "inf")
                .set_body_json(common::user_json("10", "ana")),
        )
        .mount(&server)
        .await;
    let client = Client::new(instance, Token::new("tok-hostile-1"));
    match client.channels("1").await.unwrap_err() {
        Error::RateLimited { retry_after } => assert_eq!(retry_after, Duration::from_secs(300)),
        other => panic!("{other:?}"),
    }
    assert_eq!(client.me().await.unwrap().id, "10");
}

#[tokio::test]
async fn negative_retry_after_is_retried_quickly() {
    let (server, instance) = common::instance().await;
    Mock::given(path("/api/v1/users/@me"))
        .respond_with(rate_limited(json!(-5)).insert_header("Retry-After", "-1e20"))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(path("/api/v1/users/@me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(common::user_json("10", "ana")))
        .with_priority(2)
        .mount(&server)
        .await;
    let client = Client::new(instance, Token::new("tok-hostile-2"));
    let start = std::time::Instant::now();
    assert_eq!(client.me().await.unwrap().id, "10");
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn oversized_bodies_are_refused() {
    let (server, instance) = common::instance().await;
    let big = "x".repeat(9 * 1024 * 1024);
    Mock::given(path("/api/v1/users/@me"))
        .respond_with(ResponseTemplate::new(200).set_body_string(big))
        .mount(&server)
        .await;
    let client = Client::new(instance, Token::new("tok-big"));
    assert!(matches!(
        client.me().await.unwrap_err(),
        Error::TooLarge { .. }
    ));
}

#[tokio::test]
async fn redirects_are_not_followed_with_secrets() {
    let (server, instance) = common::instance().await;
    let elsewhere = MockServer::start().await;
    Mock::given(path("/api/v1/auth/login"))
        .respond_with(
            ResponseTemplate::new(307)
                .insert_header("Location", format!("{}/steal", elsewhere.uri()).as_str()),
        )
        .mount(&server)
        .await;
    Mock::given(path("/steal"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&elsewhere)
        .await;
    let err = sign_in_password(&instance, "ana@example.org", "correct horse")
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Decode(_)), "{err:?}");
}

#[tokio::test]
async fn lists_skip_broken_elements() {
    let (server, instance) = common::instance().await;
    Mock::given(path("/api/v1/channels/200/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            common::message_json("999", "200", "fine"),
            {"id": "998", "content": "no author or channel"},
            common::message_json("997", "200", "also fine"),
        ])))
        .mount(&server)
        .await;
    Mock::given(path("/api/v1/users/@me/channels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 300, "type": 1, "recipients": null, "nsfw": null},
            {"type": 1},
        ])))
        .mount(&server)
        .await;
    let client = Client::new(instance, Token::new("tok-lenient"));
    let messages = client.messages("200", None, 50).await.unwrap();
    assert_eq!(messages.len(), 2);
    let dms = client.dm_channels().await.unwrap();
    assert_eq!(dms.len(), 1);
    assert_eq!(dms[0].id, "300");
}

#[tokio::test]
async fn guild_paging_stops_when_the_cursor_does_not_move() {
    let (server, instance) = common::instance().await;
    let page: Vec<_> = (1..=200)
        .map(|i| json!({"id": format!("{i}"), "name": format!("g{i}")}))
        .collect();
    // The server ignores `after` and returns the same full page every time.
    Mock::given(method("GET"))
        .and(path("/api/v1/users/@me/guilds"))
        .and(query_param("limit", "200"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!(page)))
        .expect(2)
        .mount(&server)
        .await;
    let client = Client::new(instance, Token::new("tok-paging"));
    let guilds = client.guilds().await.unwrap();
    assert_eq!(guilds.len(), 400);
}

#[tokio::test]
async fn two_tokens_of_one_user_share_a_limiter() {
    let (server, instance) = common::instance().await;
    Mock::given(path("/api/v1/users/@me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(common::user_json("4242", "ana")))
        .mount(&server)
        .await;
    let a = Client::new(instance.clone(), Token::new("tok-user-a"));
    let b = Client::new(instance, Token::new("tok-user-b"));
    assert!(!std::sync::Arc::ptr_eq(
        &a.rate_limiter(),
        &b.rate_limiter()
    ));
    a.me().await.unwrap();
    b.me().await.unwrap();
    assert!(std::sync::Arc::ptr_eq(&a.rate_limiter(), &b.rate_limiter()));
}

#[tokio::test]
async fn global_pause_survives_dropping_the_client() {
    let (server, instance) = common::instance().await;
    Mock::given(path("/api/v1/users/@me"))
        .respond_with(ResponseTemplate::new(429).set_body_json(
            json!({"code": "RATE_LIMITED", "message": "global", "global": true, "retry_after": 1.0}),
        ))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(path("/api/v1/users/@me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(common::user_json("10", "ana")))
        .with_priority(2)
        .mount(&server)
        .await;
    let first = Client::new(instance.clone(), Token::new("tok-pause"));
    assert!(matches!(
        first.me().await.unwrap_err(),
        Error::GlobalRateLimited
    ));
    drop(first);
    let again = Client::new(instance, Token::new("tok-pause"));
    let start = std::time::Instant::now();
    again.me().await.unwrap();
    assert!(
        start.elapsed() >= Duration::from_millis(900),
        "{:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn pinned_session_refuses_moved_endpoints() {
    let (server, instance) = common::instance().await;
    let session = Session::from_token(instance.clone(), Token::new("tok-pin"));
    let pins = session.pins();
    assert!(Session::from_token_pinned(instance.clone(), Token::new("tok-pin"), &pins).is_ok());

    let moved: Discovery = serde_json::from_value(common::discovery_json(
        "http://localhost:1",
        "ws://127.0.0.1:1/gateway",
    ))
    .unwrap();
    let elsewhere =
        Instance::from_cached(&server.uri(), moved, None, ClientInfo::default()).unwrap();
    assert!(matches!(
        Session::from_token_pinned(elsewhere.clone(), Token::new("tok-pin"), &pins),
        Err(Error::EndpointsChanged { .. })
    ));
    assert!(matches!(
        session.check_endpoints(&elsewhere),
        Err(Error::EndpointsChanged { .. })
    ));
}

#[tokio::test]
async fn insecure_endpoints_from_discovery_are_refused() {
    let server = MockServer::start().await;
    let mut doc = common::discovery_json(&server.uri(), "ws://127.0.0.1:1/gateway");
    doc["endpoints"]["api_public"] = json!("http://fluxer.example.org/api");
    Mock::given(path("/.well-known/fluxer"))
        .respond_with(ResponseTemplate::new(200).set_body_json(doc))
        .mount(&server)
        .await;
    let err = Instance::discover(&server.uri()).await.unwrap_err();
    assert!(matches!(err, Error::Url(_)), "{err:?}");

    let mut doc = common::discovery_json(&server.uri(), "ws://fluxer.example.org/gateway");
    doc["endpoints"]["api_public"] = json!(format!("{}/api", server.uri()));
    server.reset().await;
    Mock::given(path("/.well-known/fluxer"))
        .respond_with(ResponseTemplate::new(200).set_body_json(doc))
        .mount(&server)
        .await;
    assert!(matches!(
        Instance::discover(&server.uri()).await,
        Err(Error::Url(_))
    ));
    assert!(Instance::discover("http://fluxer.opland.net")
        .await
        .is_err());
}

#[tokio::test]
async fn handoff_requires_poll_secret_and_token() {
    let (server, instance) = common::instance().await;
    Mock::given(path("/api/v1/auth/handoff/initiate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": "ABCDEFGHJKLM", "expires_at": "2026-11-10T09:05:00Z"
        })))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    assert!(Handoff::start(&instance).await.is_err());

    Mock::given(path("/api/v1/auth/handoff/initiate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": "ABCDEFGHJKLM", "expires_at": "2026-11-10T09:05:00Z", "poll_secret": "p"
        })))
        .with_priority(2)
        .mount(&server)
        .await;
    // One server error is retried, then "completed" without a token is an error.
    Mock::given(path("/api/v1/auth/handoff/ABCDEFGHJKLM/status"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(path("/api/v1/auth/handoff/ABCDEFGHJKLM/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "completed"})))
        .with_priority(2)
        .mount(&server)
        .await;
    let handoff = Handoff::start(&instance).await.unwrap();
    let err = handoff.wait(Duration::from_millis(1)).await.unwrap_err();
    assert!(matches!(err, Error::Decode(_)), "{err:?}");
}

#[tokio::test]
async fn unsolvable_captcha_is_a_captcha_result() {
    let (server, instance) = common::instance().await;
    let mut challenge = altcha::make_test_challenge(1, 1);
    challenge["parameters"]["algorithm"] = json!("ARGON2ID");
    Mock::given(path("/api/v1/auth/login"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "code": "CAPTCHA_REQUIRED", "message": "Captcha required",
            "captcha_provider": "altcha", "altcha_challenge": challenge
        })))
        .expect(1)
        .mount(&server)
        .await;
    let result = sign_in_password(&instance, "ana@example.org", "correct horse")
        .await
        .unwrap();
    assert!(matches!(result, SignInResult::Captcha), "{result:?}");
}

#[test]
fn errors_do_not_show_urls() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        // Port 9 on loopback refuses the connection.
        let info = ClientInfo::default();
        let doc: Discovery = serde_json::from_value(common::discovery_json(
            "http://127.0.0.1:9",
            "ws://127.0.0.1:9/gateway",
        ))
        .unwrap();
        let instance = Instance::from_cached("http://127.0.0.1:9", doc, None, info).unwrap();
        let ticket_poll =
            inlet_protocol::poll_ip_authorization(&instance, &sign_in_ticket_for_test())
                .await
                .unwrap_err();
        let text = format!("{ticket_poll} {ticket_poll:?}");
        assert!(!text.contains("secret-ticket-value"), "{text}");
    });
}

fn sign_in_ticket_for_test() -> inlet_protocol::Ticket {
    inlet_protocol::Ticket::for_tests("secret-ticket-value")
}
