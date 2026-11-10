//! Against the private test instance `fluxer.test` (LAN only, private test CA).
//! Needs the feature `test-extra-ca`, `INLET_TEST_EXTRA_CA_FILE` pointing at
//! the instance's `ca.crt`, and a name mapping (`--add-host
//! fluxer.test:10.0.3.196` in Docker). Ignored by default; run with
//! `cargo test -p inlet-protocol --features test-extra-ca --test test_instance -- --ignored --nocapture`.
//!
//! Two requests: discovery, and one gateway connection that reads Hello. No
//! sign-in.
#![cfg(feature = "test-extra-ca")]

use std::time::Duration;

use futures_util::StreamExt;
use inlet_protocol::gateway::{codec::ZstdStream, gateway_url, tls_connector};
use inlet_protocol::Instance;
use serde_json::Value;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as WsMessage;

fn origin() -> String {
    std::env::var("INLET_TEST_ORIGIN").unwrap_or_else(|_| "fluxer.test".into())
}

#[tokio::test]
#[ignore = "talks to the private test instance"]
async fn test_instance_discovery_and_gateway_hello() {
    assert!(
        std::env::var_os(inlet_protocol::test_ca::ENV).is_some(),
        "set {} to the test CA",
        inlet_protocol::test_ca::ENV
    );
    let instance = Instance::discover(&origin()).await.expect("discovery");
    println!("api base     {}", instance.api_base());
    println!("gateway      {}", instance.gateway_url());
    assert!(instance.api_base().starts_with("https://"));
    assert!(instance.gateway_url().starts_with("wss://"));
    println!(
        "media        {:?}",
        instance.media().map(|m| m.base().to_owned())
    );

    let url = gateway_url(instance.gateway_url(), true).unwrap();
    let req = url.as_str().into_client_request().unwrap();
    let (mut ws, _) =
        tokio_tungstenite::connect_async_tls_with_config(req, None, true, tls_connector())
            .await
            .expect("gateway connect with the extra CA");
    let mut codec = ZstdStream::new().unwrap();
    let hello: Value = loop {
        let msg = tokio::time::timeout(Duration::from_secs(15), ws.next())
            .await
            .expect("Hello in time")
            .expect("socket open")
            .expect("frame");
        match msg {
            WsMessage::Binary(b) => {
                break serde_json::from_slice(&codec.decompress(&b).unwrap()).unwrap()
            }
            WsMessage::Text(t) => break serde_json::from_str(t.as_str()).unwrap(),
            WsMessage::Close(f) => panic!("closed: {f:?}"),
            _ => continue,
        }
    };
    println!("gateway      Hello: {hello}");
    assert_eq!(hello["op"], 10);
    let _ = ws.close(None).await;
}
