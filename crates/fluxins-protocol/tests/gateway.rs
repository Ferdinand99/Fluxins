//! Gateway against a mock WebSocket server that speaks Fluxer's protocol
//! (JSON, optional zstd-stream, Hello/Identify/Resume/Heartbeat).

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use fluxins_protocol::gateway::codec::ZstdStream;
use fluxins_protocol::{
    ClientInfo, CloseReason, ConnectionState, Discovery, Dispatch, Gateway, GatewayEvent, GatewayOptions, Instance,
    Token,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::Receiver;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::WebSocketStream;

struct Conn {
    ws: WebSocketStream<TcpStream>,
    codec: Option<ZstdStream>,
    query: String,
}

impl Conn {
    async fn accept(listener: &TcpListener) -> Conn {
        let (stream, _) = timeout(Duration::from_secs(10), listener.accept())
            .await
            .expect("client connects")
            .unwrap();
        let uri = Arc::new(Mutex::new(String::new()));
        let seen = uri.clone();
        let ws = tokio_tungstenite::accept_hdr_async(stream, move |req: &Request, resp: Response| {
            *seen.lock().unwrap() = req.uri().to_string();
            Ok(resp)
        })
        .await
        .unwrap();
        let query = uri.lock().unwrap().clone();
        let codec = query.contains("compress=zstd-stream").then(|| ZstdStream::new().unwrap());
        Conn { ws, codec, query }
    }

    async fn send(&mut self, v: Value) {
        let text = v.to_string();
        let msg = match &mut self.codec {
            Some(c) => WsMessage::binary(c.compress(text.as_bytes()).unwrap()),
            None => WsMessage::text(text),
        };
        self.ws.send(msg).await.unwrap();
    }

    /// Next client payload, skipping heartbeats unless `heartbeats` is set.
    async fn recv(&mut self, heartbeats: bool) -> Value {
        loop {
            let msg = timeout(Duration::from_secs(10), self.ws.next())
                .await
                .expect("client payload in time")
                .expect("stream open")
                .unwrap();
            let bytes = match msg {
                WsMessage::Text(t) => t.as_str().as_bytes().to_vec(),
                WsMessage::Binary(b) => self.codec.as_mut().expect("binary only with zstd").decompress(&b).unwrap(),
                WsMessage::Close(_) => panic!("client closed"),
                _ => continue,
            };
            let v: Value = serde_json::from_slice(&bytes).unwrap();
            if v["op"] == 1 && !heartbeats {
                continue;
            }
            return v;
        }
    }

    async fn hello(&mut self, interval_ms: u64) {
        self.send(json!({"op": 10, "d": {"heartbeat_interval": interval_ms}})).await;
    }

    async fn dispatch(&mut self, seq: u64, name: &str, d: Value) {
        self.send(json!({"op": 0, "s": seq, "t": name, "d": d})).await;
    }
}

fn instance_for(port: u16) -> Instance {
    let base = "http://127.0.0.1:9";
    let doc: Discovery =
        serde_json::from_value(common::discovery_json(base, &format!("ws://127.0.0.1:{port}/gateway"))).unwrap();
    Instance::from_cached(base, doc, None, ClientInfo::default()).unwrap()
}

fn fast_options(compress: bool) -> GatewayOptions {
    GatewayOptions {
        compress,
        backoff_base: Duration::from_millis(20),
        backoff_max: Duration::from_millis(100),
        ..GatewayOptions::default()
    }
}

fn ready(session: &str) -> Value {
    json!({
        "session_id": session, "version": 1,
        "user": common::user_json("10", "ana"),
        "guilds": [
            {"id": "100", "properties": {"id": "100", "name": "Hedgerow", "owner_id": "10", "features": []},
             "channels": [{"id": "200", "type": 0, "guild_id": "100", "name": "general"}],
             "roles": [], "emojis": [], "stickers": [], "members": [], "member_count": 3, "online_count": 1,
             "presences": [], "voice_states": [], "joined_at": "2026-01-01T00:00:00Z"},
            {"id": "101", "unavailable": true}
        ],
        "private_channels": [], "relationships": [], "presences": [], "users": [], "sessions": [],
        "read_states": [], "user_settings": null, "user_guild_settings": [], "notes": {}, "pinned_dms": [],
        "favorite_memes": [], "webauthn_credentials": [], "rtc_regions": [], "country_code": "NO"
    })
}

async fn next_dispatch(events: &mut Receiver<GatewayEvent>, states: &mut Vec<ConnectionState>) -> (u64, Dispatch) {
    loop {
        let ev = timeout(Duration::from_secs(10), events.recv())
            .await
            .expect("event in time")
            .expect("gateway alive");
        match ev {
            GatewayEvent::State(s) => states.push(s),
            GatewayEvent::Dispatch { seq, event } => return (seq, event),
        }
    }
}

async fn next_state(events: &mut Receiver<GatewayEvent>) -> ConnectionState {
    loop {
        let ev = timeout(Duration::from_secs(10), events.recv()).await.expect("event in time").expect("alive");
        if let GatewayEvent::State(s) = ev {
            return s;
        }
    }
}

#[tokio::test]
async fn identify_dispatch_and_resume_with_zstd() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (handle, mut events) = Gateway::connect(instance_for(port), Token::new("gw-token"), fast_options(true));
    let mut states = Vec::new();

    // First connection: Hello, Identify, READY, a message, then the socket dies.
    let mut c1 = Conn::accept(&listener).await;
    assert!(c1.query.contains("v=1"), "{}", c1.query);
    assert!(c1.query.contains("encoding=json"));
    assert!(c1.query.contains("compress=zstd-stream"));
    assert!(c1.query.contains("stream=1"));
    c1.hello(45_000).await;
    let identify = c1.recv(false).await;
    assert_eq!(identify["op"], 2);
    assert_eq!(identify["d"]["token"], "gw-token");
    assert_eq!(identify["d"]["flags"], 14);
    assert_eq!(identify["d"]["properties"]["browser"], "Fluxins");
    assert!(identify["d"]["properties"]["os"].is_string());

    c1.dispatch(1, "READY", ready("sess-1")).await;
    c1.dispatch(2, "MESSAGE_CREATE", {
        let mut m = common::message_json("500", "200", "hello there");
        m["guild_id"] = json!("100");
        m
    })
    .await;
    c1.dispatch(3, "PRESENCE_UPDATE", json!({"user": {"id": "11"}, "status": "online"})).await;

    let (seq, ev) = next_dispatch(&mut events, &mut states).await;
    assert_eq!(seq, 1);
    match ev {
        Dispatch::Ready(r) => {
            assert_eq!(r.session_id, "sess-1");
            assert_eq!(r.user.username, "ana");
            assert_eq!(r.guilds[0].name(), Some("Hedgerow"));
            assert_eq!(r.guilds[0].channels[0].name.as_deref(), Some("general"));
            assert!(r.guilds[1].unavailable);
        }
        other => panic!("{other:?}"),
    }
    let (seq, ev) = next_dispatch(&mut events, &mut states).await;
    assert_eq!(seq, 2);
    match ev {
        Dispatch::MessageCreate(m) => {
            assert_eq!(m.content, "hello there");
            assert_eq!(m.guild_id.as_deref(), Some("100"));
        }
        other => panic!("{other:?}"),
    }
    let (_, ev) = next_dispatch(&mut events, &mut states).await;
    assert!(matches!(ev, Dispatch::Other { ref name, .. } if name == "PRESENCE_UPDATE"), "{ev:?}");
    assert!(states.contains(&ConnectionState::Identifying));
    assert!(states.contains(&ConnectionState::Connected));
    drop(c1);

    // Second connection: the client resumes with the session and last sequence.
    let mut c2 = Conn::accept(&listener).await;
    c2.hello(45_000).await;
    let resume = c2.recv(false).await;
    assert_eq!(resume["op"], 6, "{resume}");
    assert_eq!(resume["d"]["session_id"], "sess-1");
    assert_eq!(resume["d"]["seq"], 3);
    assert_eq!(resume["d"]["token"], "gw-token");
    // Replay one missed event, then RESUMED.
    c2.dispatch(4, "MESSAGE_DELETE", json!({"id": "500", "channel_id": "200", "guild_id": "100"})).await;
    c2.dispatch(4, "RESUMED", json!({})).await;
    c2.dispatch(5, "TYPING_START", json!({"channel_id": "200", "user_id": "11", "timestamp": 1_800_000_000})).await;

    states.clear();
    let (_, ev) = next_dispatch(&mut events, &mut states).await;
    assert!(matches!(ev, Dispatch::MessageDelete(ref d) if d.id == "500"), "{ev:?}");
    assert!(states.iter().any(|s| matches!(s, ConnectionState::Reconnecting { .. })), "{states:?}");
    assert!(states.contains(&ConnectionState::Resuming), "{states:?}");
    let (_, ev) = next_dispatch(&mut events, &mut states).await;
    assert_eq!(ev, Dispatch::Resumed);
    let (seq, ev) = next_dispatch(&mut events, &mut states).await;
    assert_eq!(seq, 5);
    assert!(matches!(ev, Dispatch::TypingStart(ref t) if t.user_id == "11"), "{ev:?}");

    // Commands reach the server, compressed.
    handle.set_guild_active("100", true);
    let lazy = c2.recv(false).await;
    assert_eq!(lazy["op"], 14);
    assert_eq!(lazy["d"]["subscriptions"]["100"]["active"], true);

    handle.close();
    loop {
        match next_state(&mut events).await {
            ConnectionState::Closed(reason) => {
                assert_eq!(reason, CloseReason::ClientClosed);
                break;
            }
            _ => continue,
        }
    }
}

#[tokio::test]
async fn heartbeats_and_answers_server_heartbeat() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (_handle, mut events) = Gateway::connect(instance_for(port), Token::new("gw-hb"), fast_options(false));
    let mut c = Conn::accept(&listener).await;
    assert!(!c.query.contains("compress"));
    c.hello(300).await;
    assert_eq!(c.recv(false).await["op"], 2);
    c.dispatch(1, "READY", ready("sess-hb")).await;
    let mut states = Vec::new();
    next_dispatch(&mut events, &mut states).await;

    // A heartbeat comes within the interval (first one is jittered), with the last sequence.
    // (The first one can race READY and carry null.)
    let hb = c.recv(true).await;
    assert_eq!(hb["op"], 1);
    assert!(hb["d"] == 1 || hb["d"].is_null(), "{hb}");
    c.send(json!({"op": 11})).await;
    // And again on schedule.
    let hb2 = c.recv(true).await;
    assert_eq!(hb2["op"], 1);
    assert_eq!(hb2["d"], 1);
    c.send(json!({"op": 11})).await;
    // A server heartbeat request gets an immediate answer.
    c.send(json!({"op": 1, "d": null})).await;
    let answer = timeout(Duration::from_millis(250), c.recv(true)).await.expect("immediate heartbeat");
    assert_eq!(answer["op"], 1);
}

#[tokio::test]
async fn missing_ack_reconnects_and_resumes() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (_handle, mut events) = Gateway::connect(instance_for(port), Token::new("gw-zombie"), fast_options(false));
    let mut c = Conn::accept(&listener).await;
    c.hello(200).await;
    c.recv(false).await;
    c.dispatch(1, "READY", ready("sess-z")).await;
    let mut states = Vec::new();
    next_dispatch(&mut events, &mut states).await;
    // Never acknowledge: the client gives up on this socket and resumes on a new one.
    let mut c2 = Conn::accept(&listener).await;
    c2.hello(45_000).await;
    let resume = c2.recv(false).await;
    assert_eq!(resume["op"], 6);
    assert_eq!(resume["d"]["session_id"], "sess-z");
    drop(c);
}

#[tokio::test]
async fn invalid_session_identifies_again() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (_handle, mut events) = Gateway::connect(instance_for(port), Token::new("gw-inv"), fast_options(true));
    let mut c = Conn::accept(&listener).await;
    c.hello(45_000).await;
    assert_eq!(c.recv(false).await["op"], 2);
    c.send(json!({"op": 9, "d": false})).await;
    // Same socket, a new Identify after 1-5 s.
    let again = c.recv(false).await;
    assert_eq!(again["op"], 2);
    c.dispatch(1, "READY", ready("sess-2")).await;
    let mut states = Vec::new();
    let (_, ev) = next_dispatch(&mut events, &mut states).await;
    assert!(matches!(ev, Dispatch::Ready(_)));
}

#[tokio::test]
async fn auth_failure_stops_for_good() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (_handle, mut events) = Gateway::connect(instance_for(port), Token::new("gw-bad"), fast_options(false));
    let mut c = Conn::accept(&listener).await;
    c.hello(45_000).await;
    c.recv(false).await;
    c.ws.close(Some(CloseFrame {
        code: CloseCode::from(4004),
        reason: "Invalid token".into(),
    }))
    .await
    .unwrap();
    loop {
        match next_state(&mut events).await {
            ConnectionState::Closed(reason) => {
                assert_eq!(reason, CloseReason::AuthenticationFailed);
                break;
            }
            ConnectionState::Reconnecting { .. } => panic!("must not reconnect after 4004"),
            _ => continue,
        }
    }
    // No second connection attempt.
    assert!(timeout(Duration::from_millis(300), listener.accept()).await.is_err());
}

#[tokio::test]
async fn server_reconnect_request_resumes() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (_handle, mut events) = Gateway::connect(instance_for(port), Token::new("gw-op7"), fast_options(true));
    let mut c = Conn::accept(&listener).await;
    c.hello(45_000).await;
    c.recv(false).await;
    c.dispatch(1, "READY", ready("sess-7")).await;
    let mut states = Vec::new();
    next_dispatch(&mut events, &mut states).await;
    c.send(json!({"op": 7})).await;
    let mut c2 = Conn::accept(&listener).await;
    c2.hello(45_000).await;
    let resume = c2.recv(false).await;
    assert_eq!(resume["op"], 6);
    assert_eq!(resume["d"]["seq"], 1);
}
