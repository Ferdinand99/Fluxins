// SPDX-License-Identifier: AGPL-3.0-or-later
//! Tests of several accounts: the keychain entries and their migration, separate
//! limiters, what a background account may send, sign-out of a pasted token, the
//! start-up spacing.

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use inlet_protocol::gateway::codec::ZstdStream;
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::handshake::server::{Request as WsRequest, Response};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::WebSocketStream;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use super::*;
use crate::backend::{Event, Ui};
use crate::token_store::{self, MemoryStore, SecretStore};

fn discovery(base: &str, gateway: &str) -> Value {
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
/// list and the logout route.
async fn mock_instance(gateway: &str, users: &[(&str, &str)]) -> (MockServer, p::Instance) {
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
    Mock::given(method("GET"))
        .and(path("/api/v1/users/@me/guilds"))
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

async fn requests(server: &MockServer) -> Vec<Request> {
    server.received_requests().await.unwrap_or_default()
}

fn temp_list(name: &str) -> std::path::PathBuf {
    std::env::temp_dir()
        .join(format!("inlet-accounts-{name}-{}", std::process::id()))
        .join("accounts.json")
}

fn signed_in(session: p::Session, id: &str) -> SignedIn {
    SignedIn {
        session,
        user_id: inlet_state::UserId::parse(id).unwrap(),
        name: format!("User {id}"),
        handle: format!("user{id}"),
    }
}

const LEGACY_ORIGIN: &str = "https://fluxer.example";

fn legacy_origin() -> Option<String> {
    Some(LEGACY_ORIGIN.to_string())
}

fn pins() -> p::SessionPins {
    p::SessionPins {
        api: "https://fluxer.example".into(),
        gateway: "wss://fluxer.example".into(),
        media: None,
    }
}

#[tokio::test]
async fn old_keyring_entry_is_migrated_once_and_deleted() {
    let store = Arc::new(MemoryStore::default());
    let token = p::Token::new("legacy-secret");
    token_store::save_session(&*store, LEGACY_ORIGIN, &token, &pins(), true).unwrap();
    let raw = store.get(LEGACY_ORIGIN).unwrap().unwrap().to_string();
    let list = temp_list("migrate");
    let mut accounts =
        Accounts::new(store.clone(), Some(list.clone()), None).with_last_instance(legacy_origin);

    // First start: no list yet, so the old entry is picked up as it is.
    accounts.restore_all().await;
    let (restored, legacy) = accounts.take_restored();
    assert!(restored.is_empty());
    let legacy = legacy.expect("the old entry is found");
    assert_eq!(legacy.origin, LEGACY_ORIGIN);
    assert_eq!(legacy.token.expose_secret(), "legacy-secret");
    assert!(legacy.pasted);

    // Once the account is known (after `me`), it moves to `<origin>#<user_id>`.
    let key = AccountKey::new(LEGACY_ORIGIN, "42").unwrap();
    let info = AccountInfo {
        key: key.clone(),
        name: "Ada".into(),
        handle: "ada".into(),
    };
    let doc: p::Discovery =
        serde_json::from_value(discovery(LEGACY_ORIGIN, "wss://fluxer.example/gateway")).unwrap();
    let instance =
        p::Instance::from_cached(LEGACY_ORIGIN, doc, None, p::ClientInfo::default()).unwrap();
    let session = p::Session::from_token_unpinned(instance, token.clone());
    assert!(
        accounts
            .migrate(LEGACY_ORIGIN, info.clone(), session.clone())
            .await
    );
    assert!(
        store.get(LEGACY_ORIGIN).unwrap().is_none(),
        "old entry deleted"
    );
    let moved = store.get(&key.entry()).unwrap().expect("new entry");
    assert_eq!(
        *moved, raw,
        "the token, pins and pasted flag moved byte for byte"
    );
    assert_eq!(accounts.list(), vec![info.clone()]);
    assert_eq!(accounts.active(), Some(&key));

    // Only once: nothing left to move, and the new entry stays.
    assert!(!accounts.migrate(LEGACY_ORIGIN, info.clone(), session).await);
    assert_eq!(*store.get(&key.entry()).unwrap().unwrap(), raw);

    // The next start reads the list and the new entry; no old entry any more.
    let mut again =
        Accounts::new(store.clone(), Some(list.clone()), None).with_last_instance(legacy_origin);
    again.restore_all().await;
    let (restored, legacy) = again.take_restored();
    assert!(legacy.is_none());
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].key, key);
    assert_eq!(restored[0].token.expose_secret(), "legacy-secret");
    assert!(restored[0].pasted);
    let _ = std::fs::remove_dir_all(list.parent().unwrap());
}

#[tokio::test]
async fn two_accounts_on_one_instance_get_separate_limiters() {
    let (server, instance) = mock_instance(
        "ws://127.0.0.1:1/gateway",
        &[("tok-limit-a", "1"), ("tok-limit-b", "2")],
    )
    .await;
    let a = session::connect(
        instance.clone(),
        Credential::Pasted(p::Token::new("tok-limit-a")),
    )
    .await
    .expect("account a");
    let b = session::connect(
        instance.clone(),
        Credential::Pasted(p::Token::new("tok-limit-b")),
    )
    .await
    .expect("account b");
    assert_eq!(a.user_id, "1");
    assert_eq!(b.user_id, "2");
    assert!(
        !Arc::ptr_eq(
            &a.session.client().rate_limiter(),
            &b.session.client().rate_limiter()
        ),
        "each account has its own limiter"
    );

    // Each is stored under its own entry with its own token and pins.
    let store = Arc::new(MemoryStore::default());
    let list = temp_list("limiters");
    let mut accounts = Accounts::new(store.clone(), Some(list.clone()), None);
    let ka = accounts
        .add(signed_in(a.session.clone(), "1"), true)
        .await
        .unwrap();
    let kb = accounts
        .add(signed_in(b.session.clone(), "2"), true)
        .await
        .unwrap();
    assert_ne!(ka, kb);
    assert_eq!(ka.origin, kb.origin);
    let sa = token_store::load_session(&*store, &ka.entry())
        .unwrap()
        .unwrap();
    let sb = token_store::load_session(&*store, &kb.entry())
        .unwrap()
        .unwrap();
    assert_eq!(sa.token.expose_secret(), "tok-limit-a");
    assert_eq!(sb.token.expose_secret(), "tok-limit-b");
    assert_eq!(sa.pins, Some(instance.session_pins()));
    assert_eq!(accounts.list().len(), 2);
    assert_eq!(accounts.active(), Some(&kb));
    // The list on disk has no secret.
    let text = std::fs::read_to_string(&list).unwrap();
    assert!(!text.contains("tok-limit"));
    drop(server);
    let _ = std::fs::remove_dir_all(list.parent().unwrap());
}

#[tokio::test]
async fn pasted_token_is_forgotten_not_revoked() {
    let (server, instance) = mock_instance(
        "ws://127.0.0.1:1/gateway",
        &[("tok-pasted", "7"), ("tok-issued", "8")],
    )
    .await;
    let store = Arc::new(MemoryStore::default());
    let mut accounts = Accounts::new(store.clone(), None, None);
    let logouts = |reqs: &[Request]| {
        reqs.iter()
            .filter(|r| r.url.path() == "/api/v1/auth/logout")
            .count()
    };

    // A pasted token: signing out only forgets it.
    let pasted = p::Session::from_token_unpinned(instance.clone(), p::Token::new("tok-pasted"));
    let k = accounts.add(signed_in(pasted, "7"), true).await.unwrap();
    accounts.remove(&k, true).await;
    assert_eq!(logouts(&requests(&server).await), 0, "never revoked");
    assert!(store.get(&k.entry()).unwrap().is_none(), "forgotten here");
    assert!(accounts.list().is_empty());

    // A token Inlet was given: signing out ends it on the server.
    let issued = p::Session::from_token_unpinned(instance.clone(), p::Token::new("tok-issued"));
    let k = accounts.add(signed_in(issued, "8"), false).await.unwrap();
    accounts.remove(&k, true).await;
    assert_eq!(logouts(&requests(&server).await), 1);
    assert!(store.get(&k.entry()).unwrap().is_none());

    // Signing in again over a pasted token does not revoke the old one either.
    let first = p::Session::from_token_unpinned(instance.clone(), p::Token::new("tok-pasted"));
    let k = accounts.add(signed_in(first, "7"), true).await.unwrap();
    let again = p::Session::from_token_unpinned(instance, p::Token::new("tok-issued"));
    accounts.add(signed_in(again, "7"), false).await.unwrap();
    assert_eq!(logouts(&requests(&server).await), 1);
    let now = token_store::load_session(&*store, &k.entry())
        .unwrap()
        .unwrap();
    assert_eq!(now.token.expose_secret(), "tok-issued");
}

fn restored(origin: &str, id: &str) -> Restored {
    Restored {
        key: AccountKey::new(origin, id).unwrap(),
        token: p::Token::new(format!("tok-{id}")),
        pins: pins(),
        pasted: false,
    }
}

/// A backend with a test store and its own internal channel; the UI side is `rx`.
fn backend() -> (
    Backend,
    futures::channel::mpsc::UnboundedReceiver<Event>,
    tokio::sync::mpsc::UnboundedReceiver<Internal>,
) {
    let (tx, rx) = futures::channel::mpsc::unbounded();
    let ui = Ui::new(tx);
    let store = Accounts::new(Arc::new(MemoryStore::default()), None, None);
    let mut b = Backend::with_store(ui, store);
    let (itx, irx) = tokio::sync::mpsc::unbounded_channel();
    b.internal = itx;
    (b, rx, irx)
}

#[tokio::test(start_paused = true)]
async fn startup_staggers_accounts_by_two_seconds() {
    let all = vec![
        restored("https://one.example", "1"),
        restored("https://one.example", "2"),
        restored("https://two.example", "1"),
    ];
    let (first, later) = startup_plan(all);
    assert_eq!(
        first.unwrap().key,
        AccountKey::new("https://one.example", "1").unwrap()
    );
    let delays: Vec<Duration> = later.iter().map(|(_, d)| *d).collect();
    assert_eq!(delays, vec![Duration::from_secs(2), Duration::from_secs(4)]);

    // The backend connects the active account at once and schedules the others.
    let (mut b, _rx, _irx) = backend();
    let list = vec![
        restored("https://one.example", "1"),
        restored("https://one.example", "2"),
        restored("https://two.example", "1"),
    ];
    let start = tokio::time::Instant::now();
    b.on_restored(list, None, Vec::new(), None);
    assert!(matches!(b.connecting, Some((_, _, Mode::Restore))));
    let due = |origin: &str, id: &str| {
        b.starts
            .get(&AccountKey::new(origin, id).unwrap())
            .map(|s| s.due - start)
    };
    assert_eq!(
        due("https://one.example", "1"),
        None,
        "the active one is not waiting"
    );
    assert_eq!(
        due("https://one.example", "2"),
        Some(Duration::from_secs(2))
    );
    assert_eq!(
        due("https://two.example", "1"),
        Some(Duration::from_secs(4))
    );
}

/// One connection of the mock gateway.
struct Conn {
    ws: WebSocketStream<TcpStream>,
    codec: Option<ZstdStream>,
}

impl Conn {
    #[allow(clippy::result_large_err)]
    async fn accept(listener: &TcpListener) -> Conn {
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

    async fn send(&mut self, v: Value) {
        let text = v.to_string();
        let msg = match &mut self.codec {
            Some(c) => WsMessage::binary(c.compress(text.as_bytes()).unwrap()),
            None => WsMessage::text(text),
        };
        self.ws.send(msg).await.unwrap();
    }

    /// The next payload from the client (heartbeats skipped), or `None` after `wait`.
    async fn recv(&mut self, wait: Duration) -> Option<Value> {
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
                continue;
            }
            return Some(v);
        }
    }

    /// Hello, the client's Identify (returned), then READY.
    async fn identify(&mut self, user: &str) -> Value {
        self.send(json!({"op": 10, "d": {"heartbeat_interval": 45000}}))
            .await;
        let identify = self.recv(Duration::from_secs(10)).await.expect("identify");
        assert_eq!(identify["op"], 2);
        self.send(json!({"op": 0, "s": 1, "t": "READY", "d": {
            "session_id": "s1", "version": 1,
            "user": {"id": user, "username": "bg", "discriminator": "0001", "avatar": null, "flags": 0},
            "guilds": [{"id": "100", "properties": {"id": "100", "name": "Guild", "owner_id": "9", "features": []},
                        "channels": [{"id": "200", "type": 0, "guild_id": "100", "name": "general"}],
                        "roles": [], "members": [], "member_count": 3, "joined_at": "2026-01-01T00:00:00Z"}],
            "private_channels": [], "read_states": [], "user_guild_settings": []
        }}))
        .await;
        identify
    }
}

fn ignores_typing(identify: &Value) -> bool {
    identify["d"]["ignored_events"]
        .as_array()
        .is_some_and(|l| l.iter().any(|e| e == "TYPING_START"))
}

/// Feeds the backend's internal messages back into it until `done` holds.
async fn drive(
    b: &mut Backend,
    irx: &mut tokio::sync::mpsc::UnboundedReceiver<Internal>,
    mut done: impl FnMut(&Backend) -> bool,
) {
    let until = tokio::time::Instant::now() + Duration::from_secs(10);
    while !done(b) {
        let msg = tokio::time::timeout_at(until, irx.recv())
            .await
            .expect("the backend got there in time")
            .expect("internal channel open");
        b.internal(msg);
    }
}

#[tokio::test]
async fn background_account_sends_no_op14_and_no_rest() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (server, instance) = mock_instance(&format!("ws://127.0.0.1:{port}/gateway"), &[]).await;
    let (mut b, mut rx, mut irx) = backend();
    let key = AccountKey::new(instance.origin(), "9").unwrap();
    b.start_background(
        Restored {
            key: key.clone(),
            token: p::Token::new("tok-background"),
            pins: instance.session_pins(),
            pasted: false,
        },
        Duration::ZERO,
        0,
    );
    // Discovery, then the session is built and the gateway starts (in the background).
    drive(&mut b, &mut irx, |b| !b.background.is_empty()).await;
    let mut conn = Conn::accept(&listener).await;
    let identify = conn.identify("9").await;
    assert!(
        ignores_typing(&identify),
        "a background gateway asks for no typing"
    );

    // The person works with the (absent) active account: opening a guild, reading,
    // writing settings. None of it reaches the background account.
    crate::backend::messages::handle(
        &mut b,
        crate::backend::messages::Cmd::OpenGuild {
            guild_id: Some("100".into()),
        },
    );
    crate::backend::notifications::handle(
        &mut b,
        crate::backend::notifications::Cmd::Ack {
            items: vec![("200".into(), "300".into())],
        },
    );
    // No Lazy Request (op 14) and no other command from the background gateway.
    assert_eq!(conn.recv(Duration::from_millis(1500)).await, None);
    // REST: discovery only, no request with the token.
    let api: Vec<String> = requests(&server)
        .await
        .iter()
        .map(|r| r.url.path().to_string())
        .filter(|p| p.starts_with("/api/"))
        .collect();
    assert!(api.is_empty(), "no REST from a background account: {api:?}");
    // Its events reach the UI tagged with it; no lists for the sidebar.
    let mut ready_seen = false;
    while let Ok(event) = rx.try_recv() {
        match event {
            Event::Tagged { account, event } => {
                assert_eq!(account, key);
                match *event {
                    Event::Messages(messages::Ev::Dispatch { dispatch, rendered }) => {
                        ready_seen |= matches!(*dispatch, p::Dispatch::Ready(_));
                        assert!(rendered.is_empty(), "nothing rendered in the background");
                    }
                    Event::Messages(messages::Ev::Guilds(_))
                    | Event::Messages(messages::Ev::Channels { .. }) => {
                        panic!("a background account sent a sidebar list")
                    }
                    _ => {}
                }
            }
            Event::Accounts(_) => {}
            _ => panic!("an untagged event from a background account"),
        }
    }
    assert!(ready_seen);

    // Made active, it starts again with typing events, and opening a guild sends the
    // Lazy Request (the check above could see one).
    switch_and_open(&mut b, &key);
    let mut conn = Conn::accept(&listener).await;
    let identify = conn.identify("9").await;
    assert!(!ignores_typing(&identify));
    let mut lazy = None;
    while let Some(v) = conn.recv(Duration::from_secs(3)).await {
        if v["op"] == 14 {
            lazy = Some(v);
            break;
        }
    }
    let lazy = lazy.expect("the active account sends op 14 for the open guild");
    assert_eq!(lazy["d"]["subscriptions"]["100"]["active"], true);
}

fn switch_and_open(b: &mut Backend, key: &AccountKey) {
    handle(b, Cmd::SwitchAccount(key.clone()));
    assert!(matches!(&b.active, Some(Active::Real(r)) if &r.key == key));
    crate::backend::messages::handle(
        b,
        crate::backend::messages::Cmd::OpenGuild {
            guild_id: Some("100".into()),
        },
    );
}

#[test]
fn restored_entries_without_a_media_pin_are_upgraded_once() {
    let old = p::SessionPins {
        api: "https://fluxer.example".into(),
        gateway: "wss://fluxer.example".into(),
        media: None,
    };
    let now = p::SessionPins {
        media: Some("https://media.fluxer.example".into()),
        ..old.clone()
    };
    // Old entry, instance has a media host: store the full pins.
    assert_eq!(upgraded_pins(&old, &now), Some(now.clone()));
    // Already pinned: nothing to write.
    assert_eq!(upgraded_pins(&now, &now), None);
    // The instance has no media host: nothing to pin.
    assert_eq!(upgraded_pins(&old, &old), None);
    // Other API or gateway host (cannot happen after a pinned connect): never.
    let moved = p::SessionPins {
        api: "https://elsewhere.example".into(),
        ..now.clone()
    };
    assert_eq!(upgraded_pins(&old, &moved), None);
}

#[test]
fn pasted_tokens_are_never_revoked() {
    assert!(!should_revoke(true));
    assert!(should_revoke(false));
    let stored_pasted = Credential::Stored {
        token: p::Token::new("t"),
        pins: pins(),
        pasted: true,
    };
    let stored_issued = Credential::Stored {
        token: p::Token::new("t"),
        pins: pins(),
        pasted: false,
    };
    assert!(stored_pasted.is_pasted());
    assert!(!stored_issued.is_pasted());
    assert!(Credential::Pasted(p::Token::new("t")).is_pasted());
}

#[test]
fn account_keys_name_their_keychain_entry() {
    let k = AccountKey::new("https://fluxer.example", "42").unwrap();
    assert_eq!(k.entry(), "https://fluxer.example#42");
    assert_eq!(k.origin.host(), "fluxer.example");
    assert!(AccountKey::new("https://fluxer.example", "not-an-id").is_none());
    // The same user on two instances is two accounts.
    assert_ne!(k, AccountKey::new("https://other.example", "42").unwrap());
}

#[tokio::test]
async fn store_tasks_run_in_the_order_they_were_asked_for() {
    let (mut b, _rx, _irx) = backend();
    let order = Arc::new(StdMutex::new(Vec::new()));
    let (first, second) = (order.clone(), order.clone());
    // A slow restore-time write, then a sign-out: the sign-out must come second.
    b.store_job(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        first.lock().unwrap().push("store session");
    });
    b.store_job(async move {
        second.lock().unwrap().push("sign out");
    });
    let last = b.store_tail.take().unwrap();
    last.await.unwrap();
    assert_eq!(*order.lock().unwrap(), vec!["store session", "sign out"]);
}
