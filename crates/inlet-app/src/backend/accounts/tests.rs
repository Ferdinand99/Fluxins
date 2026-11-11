// SPDX-License-Identifier: AGPL-3.0-or-later
//! Tests of several accounts: the keychain entries and their migration, separate
//! limiters, what a background account may send, sign-out of a pasted token, the
//! start-up spacing, commands for the wrong account, the composer state per account.

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use serde_json::Value;
use tokio::net::TcpListener;
use wiremock::Request;

use super::testkit::*;
use super::*;
use crate::backend::{Event, Ui};
use crate::token_store::{self, MemoryStore, SecretStore};

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
            .migrate(LEGACY_ORIGIN, info.clone(), session.clone(), true)
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
    assert!(
        !accounts
            .migrate(LEGACY_ORIGIN, info.clone(), session, true)
            .await
    );
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
    // Its gateway starts again with the spacing of "Reconnect" (a timer): wait for it.
    drive(
        &mut b,
        &mut irx,
        |b| matches!(&b.active, Some(Active::Real(r)) if !r.ignores_typing),
    )
    .await;
    // The UI is told which account is active, then gets that account's own image client
    // (its media endpoint), tagged with it, so the previous account's is not kept.
    let mut signed_in = false;
    let mut media_for_it = false;
    while let Ok(event) = rx.try_recv() {
        match event {
            Event::Accounts(Ev::SignedIn { key: k, .. }) => signed_in = k == key,
            Event::Tagged { account, event } => {
                if let Event::Media(crate::backend::media::Ev::Ready(setup)) = *event {
                    assert!(signed_in, "the image client comes after the switch");
                    assert_eq!(account, key);
                    assert_eq!(setup.proxy.base(), instance.media().unwrap().base());
                    media_for_it = true;
                }
            }
            _ => {}
        }
    }
    assert!(
        media_for_it,
        "a switch sets up media for the new active account"
    );
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

/// A backend over `store`, with its own internal channel; the UI side is `rx`.
fn backend_with(
    store: Arc<MemoryStore>,
) -> (
    Backend,
    futures::channel::mpsc::UnboundedReceiver<Event>,
    tokio::sync::mpsc::UnboundedReceiver<Internal>,
) {
    let (tx, rx) = futures::channel::mpsc::unbounded();
    let ui = Ui::new(tx);
    let accounts = Accounts::new(store, None, None);
    let mut b = Backend::with_store(ui, accounts);
    let (itx, irx) = tokio::sync::mpsc::unbounded_channel();
    b.internal = itx;
    (b, rx, irx)
}

/// Waits for the store tasks the backend started so far.
async fn store_done(b: &mut Backend) {
    if let Some(tail) = b.store_tail.take() {
        let _ = tail.await;
    }
}

/// Two accounts on one mock instance (users 11 and 12, tokens `tok-a` and `tok-b`),
/// both running in the background. Their gateways never connect.
async fn two_accounts() -> (
    wiremock::MockServer,
    Backend,
    futures::channel::mpsc::UnboundedReceiver<Event>,
    tokio::sync::mpsc::UnboundedReceiver<Internal>,
    AccountKey,
    AccountKey,
) {
    let (server, instance) = mock_instance(
        "ws://127.0.0.1:1/gateway",
        &[("tok-a", "11"), ("tok-b", "12")],
    )
    .await;
    let (mut b, rx, mut irx) = backend();
    let ka = AccountKey::new(instance.origin(), "11").unwrap();
    let kb = AccountKey::new(instance.origin(), "12").unwrap();
    for (k, token) in [(&ka, "tok-a"), (&kb, "tok-b")] {
        b.start_background(
            Restored {
                key: k.clone(),
                token: p::Token::new(token),
                pins: instance.session_pins(),
                pasted: false,
            },
            Duration::ZERO,
            0,
        );
    }
    drive(&mut b, &mut irx, |b| b.background.len() == 2).await;
    (server, b, rx, irx, ka, kb)
}

async fn wait_for_request(server: &wiremock::MockServer, path: &str) -> Request {
    for _ in 0..100 {
        if let Some(r) = requests(server)
            .await
            .into_iter()
            .find(|r| r.url.path() == path)
        {
            return r;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no request to {path}");
}

#[tokio::test]
async fn a_command_for_another_account_is_dropped() {
    let (server, mut b, _rx, _irx, ka, kb) = two_accounts().await;
    handle(&mut b, Cmd::SwitchAccount(ka.clone()));
    let open = |g: &str| {
        crate::backend::Command::Messages(crate::backend::messages::Cmd::OpenGuild {
            guild_id: Some(g.to_string()),
        })
    };
    // Sent for B before a switch to B reached the backend: never run with A's session.
    b.command(Some(kb.clone()), open("700"));
    // Without an account: not run either while one is active.
    b.command(None, open("701"));
    // For A, while A is active: runs with A's token.
    b.command(Some(ka.clone()), open("600"));
    let r = wait_for_request(&server, "/api/v1/guilds/600/channels").await;
    assert_eq!(token_of(&r), "tok-a");
    let paths: Vec<String> = requests(&server)
        .await
        .iter()
        .map(|r| r.url.path().to_string())
        .collect();
    assert!(!paths
        .iter()
        .any(|p| p.contains("/700/") || p.contains("/701/")));
    let Some(Active::Real(active)) = &b.active else {
        panic!("A is active")
    };
    assert_eq!(active.active_guild.as_deref(), Some("600"));
    // The accounts domain needs no account (switching, signing in).
    b.command(
        None,
        crate::backend::Command::Accounts(Cmd::SwitchAccount(kb.clone())),
    );
    assert!(matches!(&b.active, Some(Active::Real(r)) if r.key == kb));
}

#[tokio::test]
async fn switching_back_keeps_the_composer_state_of_the_account() {
    let (_server, mut b, mut rx, _irx, ka, kb) = two_accounts().await;
    handle(&mut b, Cmd::SwitchAccount(ka.clone()));
    let outgoing = crate::backend::composer::Outgoing {
        channel_id: "200".into(),
        nonce: "nonce-a".into(),
        content: "hello <@5>".into(),
        mentions: Default::default(),
        reply_to: Some("199".into()),
        silent: true,
    };
    {
        let Some(Active::Real(a)) = &b.active else {
            panic!("A is active")
        };
        a.composer.remember_send(a.gen, &outgoing);
        a.composer.mark_failed(a.gen, "nonce-a", true);
        a.composer.set_session(a.gen, "11".into(), 4000);
    }
    crate::backend::composer::handle(
        &mut b,
        crate::backend::composer::Cmd::SearchMembers {
            guild_id: "600".into(),
            query: "ad".into(),
            nonce: "search-a".into(),
        },
    );
    // A → B → A.
    handle(&mut b, Cmd::SwitchAccount(kb.clone()));
    handle(&mut b, Cmd::SwitchAccount(ka.clone()));
    let Some(Active::Real(a)) = &b.active else {
        panic!("A is active again")
    };
    let plan = a
        .composer
        .retry_of(a.gen, "nonce-a", std::time::Instant::now())
        .expect("the retry still finds the message, with its reply and mentions");
    assert_eq!(plan.message.reply_to.as_deref(), Some("199"));
    assert!(plan.message.silent);
    assert_eq!(a.composer.session(a.gen), Some(("11".into(), 4000)));
    // The member search made before the switches is still answered.
    let chunk: p::models::GuildMembersChunk = serde_json::from_value(serde_json::json!({
        "guild_id": "600", "nonce": "search-a", "chunk_index": 0, "chunk_count": 1,
        "members": [{"user": {"id": "5", "username": "ada"}, "roles": []}]
    }))
    .unwrap();
    crate::backend::composer::members_chunk(&a.composer, &a.events, &chunk);
    let mut answered = false;
    while let Ok(event) = rx.try_recv() {
        if let Event::Tagged { account, event } = event {
            if let Event::Composer(crate::backend::composer::Ev::Members { guild_id, members }) =
                *event
            {
                assert_eq!(account, ka);
                assert_eq!(guild_id, "600");
                answered = !members.is_empty();
            }
        }
    }
    assert!(answered);
    // B has a state of its own: nothing of A's there.
    let b_engine = b.background.iter().find(|r| r.key == kb).unwrap();
    assert!(b_engine
        .composer
        .retry_of(b_engine.gen, "nonce-a", std::time::Instant::now())
        .is_none());
}

#[tokio::test]
async fn an_unreadable_keychain_at_sign_out_never_revokes() {
    let (server, instance) =
        mock_instance("ws://127.0.0.1:1/gateway", &[("tok-locked", "21")]).await;
    let store = Arc::new(MemoryStore::default());
    let mut accounts = Accounts::new(store.clone(), None, None);
    // A token Inlet was given (not pasted), with its live session.
    let session = p::Session::from_token_unpinned(instance, p::Token::new("tok-locked"));
    let k = accounts.add(signed_in(session, "21"), false).await.unwrap();
    // The keychain is locked at sign-out: it cannot say whether the token was pasted.
    MemoryStore::refuse(&store.refuse_reads, true);
    accounts.remove(&k, true).await;
    let logouts = requests(&server)
        .await
        .iter()
        .filter(|r| r.url.path() == "/api/v1/auth/logout")
        .count();
    assert_eq!(logouts, 0, "never revoked without knowing");
    assert!(accounts.list().is_empty(), "forgotten here");
    MemoryStore::refuse(&store.refuse_reads, false);
    assert!(store.get(&k.entry()).unwrap().is_none());

    // An entry that is not there (not moved yet) and nothing known about it: not
    // revoked either.
    let mut accounts = Accounts::new(store.clone(), None, None);
    let unknown = AccountKey::new(LEGACY_ORIGIN, "22").unwrap();
    accounts.remove(&unknown, true).await;
    assert_eq!(
        requests(&server)
            .await
            .iter()
            .filter(|r| r.url.path() == "/api/v1/auth/logout")
            .count(),
        0
    );
}

#[tokio::test]
async fn a_failed_delete_keeps_the_account_listed_and_the_next_start_cleans_up() {
    let store = Arc::new(MemoryStore::default());
    let token = p::Token::new("legacy-del");
    token_store::save_session(&*store, LEGACY_ORIGIN, &token, &pins(), false).unwrap();
    let list = temp_list("delete-fails");
    let mut accounts =
        Accounts::new(store.clone(), Some(list.clone()), None).with_last_instance(legacy_origin);
    accounts.restore_all().await;
    let (_, legacy) = accounts.take_restored();
    assert!(legacy.is_some());
    let key = AccountKey::new(LEGACY_ORIGIN, "42").unwrap();
    let info = AccountInfo {
        key: key.clone(),
        name: "Ada".into(),
        handle: "ada".into(),
    };
    let session = legacy_session(&token);
    // The old entry cannot be deleted: the account is listed all the same.
    MemoryStore::refuse(&store.refuse_deletes, true);
    assert!(
        accounts
            .migrate(LEGACY_ORIGIN, info.clone(), session, false)
            .await
    );
    assert_eq!(accounts.list(), vec![info.clone()]);
    assert!(
        store.get(LEGACY_ORIGIN).unwrap().is_some(),
        "the copy is still there"
    );
    accounts.save().await;
    MemoryStore::refuse(&store.refuse_deletes, false);
    // Next start: the account comes from the list, and the copy is deleted.
    let mut next =
        Accounts::new(store.clone(), Some(list.clone()), None).with_last_instance(legacy_origin);
    next.restore_all().await;
    let (restored, legacy) = next.take_restored();
    assert!(legacy.is_none());
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].key, key);
    assert!(store.get(LEGACY_ORIGIN).unwrap().is_none());
    let _ = std::fs::remove_dir_all(list.parent().unwrap());
}

#[tokio::test]
async fn an_entry_not_moved_is_found_on_every_start_even_with_other_accounts_listed() {
    let store = Arc::new(MemoryStore::default());
    let token = p::Token::new("legacy-keep");
    token_store::save_session(&*store, LEGACY_ORIGIN, &token, &pins(), false).unwrap();
    let list = temp_list("not-moved");
    let mut accounts =
        Accounts::new(store.clone(), Some(list.clone()), None).with_last_instance(legacy_origin);
    accounts.restore_all().await;
    let _ = accounts.take_restored();
    let key = AccountKey::new(LEGACY_ORIGIN, "42").unwrap();
    let info = AccountInfo {
        key: key.clone(),
        name: "Ada".into(),
        handle: "ada".into(),
    };
    // The copy fails (the keychain refuses writes): nothing moves, nothing is lost.
    MemoryStore::refuse(&store.refuse_writes, true);
    assert!(
        !accounts
            .migrate(LEGACY_ORIGIN, info.clone(), legacy_session(&token), false)
            .await
    );
    MemoryStore::refuse(&store.refuse_writes, false);
    // Another account is added meanwhile: the list is no longer empty.
    let (_server, instance) =
        mock_instance("ws://127.0.0.1:1/gateway", &[("tok-other", "43")]).await;
    let other = p::Session::from_token_unpinned(instance, p::Token::new("tok-other"));
    let kb = accounts.add(signed_in(other, "43"), true).await.unwrap();
    // Next start: the old entry is still found, and moves then.
    let mut next =
        Accounts::new(store.clone(), Some(list.clone()), None).with_last_instance(legacy_origin);
    next.restore_all().await;
    let (restored, legacy) = next.take_restored();
    assert_eq!(
        restored.iter().map(|r| &r.key).collect::<Vec<_>>(),
        vec![&kb]
    );
    let legacy = legacy.expect("the entry not moved is picked up again");
    assert_eq!(legacy.token.expose_secret(), "legacy-keep");
    assert!(
        next.migrate(LEGACY_ORIGIN, info, legacy_session(&token), false)
            .await
    );
    assert_eq!(next.list().len(), 2);
    assert!(store.get(LEGACY_ORIGIN).unwrap().is_none());
    assert!(store.get(&key.entry()).unwrap().is_some());
    let _ = std::fs::remove_dir_all(list.parent().unwrap());
}

fn legacy_session(token: &p::Token) -> p::Session {
    let doc: p::Discovery =
        serde_json::from_value(discovery(LEGACY_ORIGIN, "wss://fluxer.example/gateway")).unwrap();
    let instance =
        p::Instance::from_cached(LEGACY_ORIGIN, doc, None, p::ClientInfo::default()).unwrap();
    p::Session::from_token_unpinned(instance, token.clone())
}

#[tokio::test]
async fn signing_out_an_account_not_moved_yet_removes_the_old_entry() {
    let (server, instance) = mock_instance("ws://127.0.0.1:1/gateway", &[("tok-old", "31")]).await;
    let origin = instance.origin().to_string();
    let store = Arc::new(MemoryStore::default());
    let token = p::Token::new("tok-old");
    token_store::save_session(&*store, &origin, &token, &instance.session_pins(), false).unwrap();
    let (mut b, mut rx, _irx) = backend_with(store.clone());
    let credential = Credential::Stored {
        token: token.clone(),
        pins: instance.session_pins(),
        pasted: false,
    };
    let connected = session::connect(instance.clone(), credential.clone())
        .await
        .expect("connects");
    // The move fails (the keychain refuses writes): the account runs from the old entry.
    MemoryStore::refuse(&store.refuse_writes, true);
    b.restored_connected(connected, credential, Some(Restore::Legacy(origin.clone())));
    store_done(&mut b).await;
    MemoryStore::refuse(&store.refuse_writes, false);
    assert!(store.get(&origin).unwrap().is_some());
    let key = AccountKey::new(&origin, "31").unwrap();
    assert!(store.get(&key.entry()).unwrap().is_none());

    b.sign_out();
    store_done(&mut b).await;
    assert!(
        store.get(&origin).unwrap().is_none(),
        "the old entry goes too"
    );
    let logouts = requests(&server)
        .await
        .iter()
        .filter(|r| r.url.path() == "/api/v1/auth/logout")
        .count();
    assert_eq!(logouts, 1, "an issued token is ended with the live session");
    // The account leaves the switcher and the tray at once.
    let mut removed = false;
    while let Ok(event) = rx.try_recv() {
        if let Event::Accounts(Ev::AccountRemoved { key: k, .. }) = event {
            removed |= k == key;
        }
    }
    assert!(removed);
}
