// SPDX-License-Identifier: AGPL-3.0-or-later
//! Test helpers for several accounts: a mock instance (REST, with one token per user)
//! and a mock gateway (WebSocket, Fluxer's JSON with zstd-stream), for the backend
//! tests and the switch test that drives the real UI.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use inlet_protocol as p;
use inlet_protocol::gateway::codec::ZstdStream;
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::handshake::server::{Request as WsRequest, Response};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::WebSocketStream;
use wiremock::matchers::{header, method, path, path_regex};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

pub(crate) fn discovery(base: &str, gateway: &str) -> Value {
    json!({
        "api_code_version": 1,
        "endpoints": {
            "api": format!("{base}/api"), "api_client": format!("{base}/api"),
            "api_public": format!("{base}/api"), "gateway": gateway,
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

/// A mock instance with discovery, `me` for each `(token, user ID)`, an empty guild
/// list, empty DM and message lists, empty guild channel lists and the logout route.
pub(crate) async fn mock_instance(
    gateway: &str,
    users: &[(&str, &str)],
) -> (MockServer, p::Instance) {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/fluxer"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("ETag", "\"v1\"")
                .set_body_json(discovery(&server.uri(), gateway)),
        )
        .mount(&server)
        .await;
    for (token, id) in users {
        Mock::given(method("GET"))
            .and(path("/api/v1/users/@me"))
            .and(header("authorization", *token))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": id, "username": format!("user{id}"), "global_name": format!("User {id}"),
                "discriminator": "0001", "avatar": null, "flags": 0
            })))
            .mount(&server)
            .await;
    }
    for route in ["/api/v1/users/@me/guilds", "/api/v1/users/@me/channels"] {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&server)
            .await;
    }
    Mock::given(method("GET"))
        .and(path_regex(
            r"^/api/v1/(channels/\d+/messages|guilds/\d+/channels)$",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/auth/logout"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    let info = p::ClientInfo {
        installation_id: "00000000-0000-4000-a000-000000000000".into(),
        ..p::ClientInfo::default()
    };
    let instance = p::Instance::discover_with(&server.uri(), info)
        .await
        .expect("discovery");
    (server, instance)
}

pub(crate) async fn requests(server: &MockServer) -> Vec<Request> {
    server.received_requests().await.unwrap_or_default()
}

/// The token a request was sent with.
pub(crate) fn token_of(r: &Request) -> String {
    r.headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

/// One connection of the mock gateway.
pub(crate) struct Conn {
    ws: WebSocketStream<TcpStream>,
    codec: Option<ZstdStream>,
}

impl Conn {
    #[allow(clippy::result_large_err)]
    pub(crate) async fn accept(listener: &TcpListener) -> Conn {
        let (stream, _) = timeout(Duration::from_secs(10), listener.accept())
            .await
            .expect("the gateway is dialled")
            .unwrap();
        let uri = Arc::new(StdMutex::new(String::new()));
        let seen = uri.clone();
        let callback = move |req: &WsRequest, resp: Response| {
            *seen.lock().unwrap() = req.uri().to_string();
            Ok(resp)
        };
        let ws = tokio_tungstenite::accept_hdr_async(stream, callback)
            .await
            .unwrap();
        let compress = uri.lock().unwrap().contains("compress=zstd-stream");
        Conn {
            ws,
            codec: compress.then(|| ZstdStream::new().unwrap()),
        }
    }

    pub(crate) async fn send(&mut self, v: Value) {
        let text = v.to_string();
        let msg = match &mut self.codec {
            Some(c) => WsMessage::binary(c.compress(text.as_bytes()).unwrap()),
            None => WsMessage::text(text),
        };
        let _ = self.ws.send(msg).await;
    }

    /// The next payload from the client, or `None` after `wait` (or when it closed).
    /// Heartbeats are answered and skipped.
    pub(crate) async fn recv(&mut self, wait: Duration) -> Option<Value> {
        loop {
            let msg = timeout(wait, self.ws.next()).await.ok()??.ok()?;
            let bytes = match msg {
                WsMessage::Text(t) => t.as_str().as_bytes().to_vec(),
                WsMessage::Binary(b) => self.codec.as_mut()?.decompress(&b).ok()?,
                WsMessage::Close(_) => return None,
                _ => continue,
            };
            let v: Value = serde_json::from_slice(&bytes).ok()?;
            if v["op"] == 1 {
                self.send(json!({"op": 11})).await;
                continue;
            }
            return Some(v);
        }
    }

    /// Hello, the client's Identify (returned), then `ready` as READY.
    pub(crate) async fn identify_with(&mut self, ready: impl FnOnce(&str) -> Value) -> Value {
        self.send(json!({"op": 10, "d": {"heartbeat_interval": 45000}}))
            .await;
        let identify = self.recv(Duration::from_secs(10)).await.expect("identify");
        assert_eq!(identify["op"], 2);
        let token = identify["d"]["token"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        self.send(json!({"op": 0, "s": 1, "t": "READY", "d": ready(&token)}))
            .await;
        identify
    }

    /// Hello, Identify, and a READY with guild 100 and channel 200 for `user`.
    pub(crate) async fn identify(&mut self, user: &str) -> Value {
        let user = user.to_string();
        self.identify_with(move |_| ready_json(&user, "100", "200"))
            .await
    }
}

/// A READY for `user` with one guild and one text channel in it.
pub(crate) fn ready_json(user: &str, guild: &str, channel: &str) -> Value {
    json!({
        "session_id": format!("s{user}"), "version": 1,
        "user": {"id": user, "username": format!("user{user}"), "discriminator": "0001",
                 "avatar": null, "flags": 0},
        "guilds": [{"id": guild,
                    "properties": {"id": guild, "name": format!("Guild {guild}"), "owner_id": "9",
                                   "features": []},
                    "channels": [{"id": channel, "type": 0, "guild_id": guild,
                                  "name": format!("general-{channel}")}],
                    "roles": [{"id": guild, "name": "@everyone",
                               "permissions": "2251799813685247", "position": 0}],
                    "members": [{"user": {"id": user, "username": format!("user{user}")},
                                 "roles": [], "joined_at": "2026-01-01T00:00:00+00:00"}],
                    "member_count": 3, "joined_at": "2026-01-01T00:00:00Z"}],
        "private_channels": [], "read_states": [], "user_guild_settings": []
    })
}

/// Serves the gateway on `listener` for as long as the runtime runs: every connection
/// gets Hello, and after Identify the READY its token maps to (`readies`); then it
/// answers heartbeats and ignores the rest.
pub(crate) fn serve_gateway(listener: TcpListener, readies: HashMap<String, Value>) {
    let readies = Arc::new(readies);
    tokio::spawn(async move {
        loop {
            let mut conn = Conn::accept_forever(&listener).await;
            let readies = readies.clone();
            tokio::spawn(async move {
                let readies = readies.clone();
                conn.identify_with(move |token| readies.get(token).cloned().unwrap_or(Value::Null))
                    .await;
                while conn.recv(Duration::from_secs(3600)).await.is_some() {}
            });
        }
    });
}

impl Conn {
    #[allow(clippy::result_large_err)]
    async fn accept_forever(listener: &TcpListener) -> Conn {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let uri = Arc::new(StdMutex::new(String::new()));
            let seen = uri.clone();
            let callback = move |req: &WsRequest, resp: Response| {
                *seen.lock().unwrap() = req.uri().to_string();
                Ok(resp)
            };
            let Ok(ws) = tokio_tungstenite::accept_hdr_async(stream, callback).await else {
                continue;
            };
            let compress = uri.lock().unwrap().contains("compress=zstd-stream");
            return Conn {
                ws,
                codec: compress.then(|| ZstdStream::new().unwrap()),
            };
        }
    }
}

/// Requests seen per token, as `METHOD path` lines.
pub(crate) fn by_token(reqs: &[Request]) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for r in reqs {
        out.entry(token_of(r))
            .or_default()
            .push(format!("{} {}", r.method, r.url.path()));
    }
    out
}
