//! Unauthenticated smoke test against a real instance. Ignored by default; run with
//! `INLET_SMOKE_ORIGIN=fluxer.opland.net cargo test --test live_smoke -- --ignored --nocapture`.
//!
//! Four requests: discovery, discovery again with If-None-Match, the OpenAPI
//! document, and one gateway connection with zstd-stream that reads Hello,
//! sends one Heartbeat (no Identify, no token) and waits for the ACK.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use inlet_protocol::gateway::{codec::ZstdStream, gateway_url, tls_connector};
use inlet_protocol::Instance;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

fn origin() -> String {
    std::env::var("INLET_SMOKE_ORIGIN").unwrap_or_else(|_| "fluxer.opland.net".into())
}

async fn read_zstd(ws: &mut Ws, codec: &mut ZstdStream) -> Value {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(15), ws.next())
            .await
            .expect("gateway frame in time")
            .expect("socket open")
            .expect("frame");
        match msg {
            WsMessage::Binary(b) => {
                return serde_json::from_slice(&codec.decompress(&b).unwrap()).unwrap()
            }
            WsMessage::Text(t) => panic!("expected a zstd binary frame, got text {t}"),
            WsMessage::Close(f) => panic!("closed: {f:?}"),
            _ => continue,
        }
    }
}

#[tokio::test]
#[ignore = "talks to a live instance"]
async fn live_discovery_openapi_and_gateway_hello() {
    let instance = Instance::discover(&origin()).await.expect("discovery");
    let d = instance.discovery();
    println!("origin       {}", instance.origin());
    println!("api base     {}", instance.api_base());
    println!("gateway      {}", d.endpoints.gateway);
    println!("api version  {}", d.api_code_version);
    println!("captcha      {:?}", d.captcha);
    println!("product name {:?}", d.product_name());
    println!("etag         {:?}", instance.etag());
    let again = instance.refresh().await.expect("refresh");
    println!("refresh kept etag: {}", again.etag() == instance.etag());

    let ua = instance.client_info().user_agent.clone();
    let http = reqwest::Client::builder()
        .user_agent(ua.clone())
        .build()
        .unwrap();
    let resp = http
        .get(format!("{}/openapi.json", instance.api_base()))
        .send()
        .await
        .expect("openapi");
    let status = resp.status();
    let doc: Value = resp.json().await.expect("openapi json");
    println!(
        "openapi      HTTP {} {} paths, {} schemas",
        status,
        doc["paths"].as_object().map_or(0, |p| p.len()),
        doc["components"]["schemas"]
            .as_object()
            .map_or(0, |s| s.len()),
    );
    for path in [
        "/auth/login",
        "/auth/login/mfa/totp",
        "/auth/handoff/initiate",
        "/users/@me",
        "/users/@me/guilds",
        "/users/@me/channels",
        "/guilds/{guild_id}/channels",
        "/channels/{channel_id}/messages",
    ] {
        assert!(doc["paths"].get(path).is_some(), "openapi lacks {path}");
    }
    // Routes the chat features use. Path parameter names follow the document.
    let mut missing = Vec::new();
    for (method, path) in [
        ("patch", "/channels/{channel_id}/messages/{message_id}"),
        ("delete", "/channels/{channel_id}/messages/{message_id}"),
        (
            "put",
            "/channels/{channel_id}/messages/{message_id}/reactions/{emoji}/@me",
        ),
        (
            "get",
            "/channels/{channel_id}/messages/{message_id}/reactions/{emoji}/users",
        ),
        ("get", "/channels/{channel_id}/messages/pins"),
        ("put", "/channels/{channel_id}/pins/{message_id}"),
        ("post", "/channels/{channel_id}/pins/ack"),
        ("post", "/channels/{channel_id}/typing"),
        ("post", "/read-states/ack"),
        ("get", "/users/@me/settings"),
        ("patch", "/users/@me/settings"),
        ("get", "/users/{target_id}/profile"),
        ("get", "/guilds/{guild_id}/members/{user_id}"),
        ("get", "/guilds/{guild_id}/members"),
        ("get", "/guilds/{guild_id}/roles"),
        ("get", "/guilds/{guild_id}/emojis"),
        ("get", "/guilds/{guild_id}/stickers"),
    ] {
        if doc["paths"].get(path).and_then(|p| p.get(method)).is_none() {
            missing.push(format!("{method} {path}"));
        }
    }
    assert!(missing.is_empty(), "openapi lacks {missing:?}");

    // Limits, brand colour and media endpoint from the same document (no requests).
    let traits: Vec<String> = Vec::new();
    println!(
        "limits       file {} B, {} files, message {:?} chars",
        d.max_attachment_file_size(&traits),
        d.max_attachments_per_message(&traits),
        d.resolve_limit("max_message_length", &traits)
    );
    println!(
        "brand colour {:?}",
        d.theme_color().map(|c| format!("#{c:06x}"))
    );
    println!("presigned    {}", d.presigned_attachment_uploads());
    let media = instance.media().expect("an https media endpoint");
    println!("media        {}", media.base());
    println!("emoji url    {:?}", media.emoji("1", true, 48));

    let url = gateway_url(&d.endpoints.gateway, true).unwrap();
    println!("gateway url  {url}");
    let mut req = url.as_str().into_client_request().unwrap();
    req.headers_mut().insert("User-Agent", ua.parse().unwrap());
    // The same TLS setup as the gateway task.
    let (mut ws, _) =
        tokio_tungstenite::connect_async_tls_with_config(req, None, true, tls_connector())
            .await
            .expect("gateway connect");
    let mut codec = ZstdStream::new().unwrap();
    let hello = read_zstd(&mut ws, &mut codec).await;
    println!("gateway      Hello: {hello}");
    assert_eq!(hello["op"], 10);
    assert!(hello["d"]["heartbeat_interval"].as_u64().is_some());

    let beat = codec
        .compress(json!({"op": 1, "d": null}).to_string().as_bytes())
        .unwrap();
    ws.send(WsMessage::binary(beat)).await.unwrap();
    let ack = read_zstd(&mut ws, &mut codec).await;
    println!("gateway      after Heartbeat: {ack}");
    assert_eq!(ack["op"], 11);
    let _ = ws.close(None).await;
}
