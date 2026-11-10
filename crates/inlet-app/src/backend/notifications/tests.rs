use std::time::{Duration, Instant};

use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use super::*;

fn discovery(base: &str) -> Value {
    json!({
        "api_code_version": 1,
        "endpoints": {
            "api": format!("{base}/api"), "api_client": format!("{base}/api"),
            "api_public": format!("{base}/api"), "gateway": "ws://127.0.0.1:1/gateway",
            "media": format!("{base}/media"), "webapp": base, "invite": format!("{base}/invite")
        },
        "captcha": {"provider": "altcha"},
        "features": {"account_identity": "email", "voice_enabled": true},
        "limits": {"version": 2, "rules": [{"id": "default", "overrides": {"max_message_length": 4000}}]},
        "app_public": {"branding": {"product_name": "Mock Social"}},
        "sso": {"enabled": false},
        "domain_migration": {"enabled": false}
    })
}

/// A client for a mock server that answers acknowledgements like the real one.
async fn fixture() -> (MockServer, p::Client) {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/fluxer"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("ETag", "\"v1\"")
                .set_body_json(discovery(&server.uri())),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/read-states/ack"))
        .respond_with(|req: &Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap();
            let states: Vec<Value> = body["read_states"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| {
                    json!({"id": e["channel_id"], "mention_count": 0,
                                    "last_message_id": e["message_id"]})
                })
                .collect();
            ResponseTemplate::new(200).set_body_json(json!({ "read_states": states }))
        })
        .mount(&server)
        .await;
    let info = p::ClientInfo {
        installation_id: "00000000-0000-4000-a000-000000000000".into(),
        ..p::ClientInfo::default()
    };
    let instance = p::Instance::discover_with(&server.uri(), info)
        .await
        .expect("discovery");
    // One token per fixture: the protocol shares its ack and rate state between clients
    // of one token, and tests run side by side.
    let token = format!(
        "tok-notify-{}",
        NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    );
    (server, p::Client::new(instance, p::Token::new(token)))
}

async fn requests(server: &MockServer, m: &str, p: &str) -> Vec<Request> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.method.as_str() == m && r.url.path() == p)
        .collect()
}

fn entries(r: &Request) -> Vec<(String, String)> {
    let body: Value = serde_json::from_slice(&r.body).unwrap();
    body["read_states"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["channel_id"].as_str().unwrap().to_string(),
                e["message_id"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn items(n: u64) -> Vec<(String, String)> {
    (0..n)
        .map(|i| ((1_000 + i).to_string(), (50_000 + i).to_string()))
        .collect()
}

#[tokio::test]
async fn mark_all_read_is_one_bulk_call() {
    let (server, client) = fixture().await;
    send_acks(&client, &items(3));
    client.flush_acks().await.unwrap();
    let sent = requests(&server, "POST", "/api/v1/read-states/ack").await;
    assert_eq!(sent.len(), 1, "one bulk call");
    let mut got = entries(&sent[0]);
    got.sort();
    assert_eq!(got, items(3));

    // The same again: the protocol drops what it already sent.
    send_acks(&client, &items(3));
    client.flush_acks().await.unwrap();
    assert_eq!(
        requests(&server, "POST", "/api/v1/read-states/ack")
            .await
            .len(),
        1
    );
}

#[tokio::test]
async fn many_channels_go_out_100_per_request_at_least_a_second_apart() {
    let (server, client) = fixture().await;
    let started = Instant::now();
    send_acks(&client, &items(250));
    client.flush_acks().await.unwrap();
    let sent = requests(&server, "POST", "/api/v1/read-states/ack").await;
    let sizes: Vec<usize> = sent.iter().map(|r| entries(r).len()).collect();
    assert_eq!(sizes, vec![100, 100, 50]);
    assert!(
        started.elapsed() >= Duration::from_millis(1_900),
        "three requests need two gaps of a second, took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_settings_write_goes_to_the_guild_route_and_reports_failure() {
    let (server, client) = fixture().await;
    let route = "/api/v1/users/@me/guilds/42/settings";
    Mock::given(method("PATCH"))
        .and(path(route))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"guild_id": "42", "version": 3})),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/api/v1/users/@me/guilds/@me/settings"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({"code": "UNKNOWN"})))
        .mount(&server)
        .await;

    let body = json!({"suppress_roles": true});
    let answer = write_settings(&client, GuildId::parse("42"), &body)
        .await
        .unwrap();
    assert_eq!(answer["version"], json!(3));
    let sent = requests(&server, "PATCH", route).await;
    assert_eq!(sent.len(), 1);
    assert_eq!(
        serde_json::from_slice::<Value>(&sent[0].body).unwrap(),
        body
    );

    // The direct messages have their own route; a failure comes back as text.
    assert!(write_settings(&client, None, &body).await.is_err());
    assert_eq!(settings_route(None), "/users/@me/guilds/@me/settings");
}
