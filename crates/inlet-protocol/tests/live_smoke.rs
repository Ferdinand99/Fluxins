//! Unauthenticated smoke test against a real instance. Ignored by default; run with
//! `INLET_SMOKE_ORIGIN=fluxer.opland.net cargo test --test live_smoke -- --ignored --nocapture`.
//!
//! Four requests: discovery, discovery again with If-None-Match, the OpenAPI
//! document, and one gateway connection with zstd-stream that reads Hello,
//! sends one Heartbeat (no Identify, no token) and waits for the ACK.

use std::time::Duration;

use inlet_protocol::gateway::{codec::ZstdStream, gateway_url};
use inlet_protocol::Instance;
use futures_util::{SinkExt, StreamExt};
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
            WsMessage::Binary(b) => return serde_json::from_slice(&codec.decompress(&b).unwrap()).unwrap(),
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
    let http = reqwest::Client::builder().user_agent(ua.clone()).build().unwrap();
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
        doc["components"]["schemas"].as_object().map_or(0, |s| s.len()),
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

    let url = gateway_url(&d.endpoints.gateway, true).unwrap();
    println!("gateway url  {url}");
    let mut req = url.as_str().into_client_request().unwrap();
    req.headers_mut().insert("User-Agent", ua.parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.expect("gateway connect");
    let mut codec = ZstdStream::new().unwrap();
    let hello = read_zstd(&mut ws, &mut codec).await;
    println!("gateway      Hello: {hello}");
    assert_eq!(hello["op"], 10);
    assert!(hello["d"]["heartbeat_interval"].as_u64().is_some());

    let beat = codec.compress(json!({"op": 1, "d": null}).to_string().as_bytes()).unwrap();
    ws.send(WsMessage::binary(beat)).await.unwrap();
    let ack = read_zstd(&mut ws, &mut codec).await;
    println!("gateway      after Heartbeat: {ack}");
    assert_eq!(ack["op"], 11);
    let _ = ws.close(None).await;
}
