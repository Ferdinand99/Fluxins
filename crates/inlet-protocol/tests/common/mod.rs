//! Shared mock-server helpers. The shapes follow Fluxer's OpenAPI document and docs.
#![allow(dead_code)]

use inlet_protocol::{ClientInfo, Instance};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A discovery document like fluxer.opland.net's, pointing at `base`.
pub fn discovery_json(base: &str, gateway: &str) -> Value {
    json!({
        "api_code_version": 1,
        "endpoints": {
            "api": format!("{base}/api"),
            "api_client": format!("{base}/api"),
            "api_public": format!("{base}/api"),
            "gateway": gateway,
            "media": format!("{base}/media"),
            "webapp": base,
            "invite": format!("{base}/invite")
        },
        "captcha": {"provider": "altcha"},
        "features": {"account_identity": "email", "voice_enabled": true},
        "limits": {"version": 2, "rules": [{"id": "default", "overrides": {"max_message_length": 4000}}]},
        "app_public": {"branding": {"product_name": "Mock Social"}},
        "sso": {"enabled": false},
        "domain_migration": {"enabled": false}
    })
}

/// Starts a mock server with discovery mounted and returns it with a discovered instance.
pub async fn instance() -> (MockServer, Instance) {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/fluxer"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("ETag", "\"v1\"")
                .set_body_json(discovery_json(&server.uri(), "ws://127.0.0.1:1/gateway")),
        )
        .mount(&server)
        .await;
    let info = ClientInfo {
        installation_id: "00000000-0000-4000-a000-000000000000".into(),
        ..ClientInfo::default()
    };
    let instance = Instance::discover_with(&server.uri(), info)
        .await
        .expect("discovery");
    (server, instance)
}

pub fn user_json(id: &str, name: &str) -> Value {
    json!({
        "id": id, "username": name, "discriminator": "0001", "global_name": null,
        "avatar": null, "avatar_color": null, "flags": 0
    })
}

pub fn message_json(id: &str, channel: &str, content: &str) -> Value {
    json!({
        "id": id, "channel_id": channel, "author": user_json("10", "ana"), "type": 0, "flags": 0,
        "content": content, "timestamp": "2026-11-10T08:00:00.000Z", "edited_timestamp": null,
        "pinned": false, "mention_everyone": false, "tts": false, "mentions": [], "mention_roles": [],
        "attachments": [{"id": "77", "filename": "cat.png", "size": 1234, "flags": 0,
                          "content_type": "image/png", "width": 64, "height": 64,
                          "url": "https://media.example/attachments/1/77/cat.png"}],
        "embeds": [], "nonce": "999"
    })
}
