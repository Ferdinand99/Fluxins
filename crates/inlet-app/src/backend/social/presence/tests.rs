// SPDX-License-Identifier: AGPL-3.0-or-later
//! The presence budget, the background account's status and the typing budget, against
//! a mock instance and a mock gateway.

use std::sync::Arc;

use serde_json::Value;
use tokio::net::TcpListener;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::backend::accounts::testkit::{mock_instance, requests, Conn};
use crate::backend::accounts::{self, AccountKey, Accounts, Restored};
use crate::backend::{composer, social, Command, Event};
use crate::token_store::MemoryStore;

fn status(s: &str) -> Save {
    Save {
        status: Some(s.into()),
        ..Save::default()
    }
}

fn custom(text: &str) -> CustomStatus {
    CustomStatus {
        text: Some(text.into()),
        expires_at: None,
        emoji_id: None,
        emoji_name: None,
        emoji_animated: false,
    }
}

#[test]
fn status_updates_are_coalesced_five_per_twenty_seconds() {
    let t0 = Instant::now();
    let mut c = Coalescer::default();
    let mut wish = Save::default();
    let mut writes: Vec<(Instant, p::UserSettingsUpdate)> = Vec::new();
    // 40 picks, one every 500 ms (20 s); a status and a custom status take turns.
    for i in 0..40u64 {
        let now = t0 + Duration::from_millis(500 * i);
        wish.merge(if i % 2 == 0 {
            status(["online", "idle", "dnd", "invisible"][(i / 2 % 4) as usize])
        } else {
            Save {
                custom: Some(Some(custom(&format!("pick {i}")))),
                ..Save::default()
            }
        });
        c.want(wish.clone());
        while let Next::Write(u) = c.next(now) {
            writes.push((now, u));
        }
    }
    // What still waits goes out as the budget frees up.
    let mut now = t0 + Duration::from_secs(20);
    loop {
        match c.next(now) {
            Next::Write(u) => writes.push((now, u)),
            Next::Wait(w) => now += w,
            Next::Idle => break,
        }
    }
    // At most 5 writes in any 20 s.
    for (i, (start, _)) in writes.iter().enumerate() {
        let in_window = writes[i..]
            .iter()
            .take_while(|(t, _)| t.saturating_duration_since(*start) < SAVE_WINDOW)
            .count();
        assert!(in_window <= SAVES_PER_WINDOW, "{in_window} writes in 20 s");
    }
    assert_eq!(
        writes.len(),
        SAVES_PER_WINDOW + 1,
        "40 picks: 5, then the newest"
    );
    // The last write carries the newest wish.
    let (_, last) = writes.last().unwrap();
    assert_eq!(last.status, wish.status);
    assert_eq!(last.custom_status, wish.custom);
    // The same wish again (no new pick) writes nothing.
    c.want(wish.clone());
    assert_eq!(c.next(now + SAVE_WINDOW), Next::Idle);
    // A new wish goes out at once while the window has a free slot.
    wish.merge(status("online"));
    c.want(wish.clone());
    let at = now + Duration::from_secs(1);
    assert!(matches!(c.next(at), Next::Write(_)));
    // That write failed: the same wish again writes it again.
    c.failed();
    c.want(wish);
    match c.next(at + Duration::from_secs(1)) {
        Next::Write(u) => assert_eq!(u.status.as_deref(), Some("online")),
        other => panic!("the failed wish is written again: {other:?}"),
    }
}

#[test]
fn a_pick_equal_to_an_earlier_write_is_written_again() {
    let t0 = Instant::now();
    let mut c = Coalescer::default();
    let mut wish = Save::default();
    wish.merge(status("dnd"));
    c.want(wish.clone());
    assert!(matches!(c.next(t0), Next::Write(u) if u.status.as_deref() == Some("dnd")));
    assert!(!c.has_unwritten());
    // Another client sets "online" (Inlet only sees the echo); the person picks "dnd"
    // again here: that is a new wish, and it is written.
    wish.merge(status("dnd"));
    c.want(wish.clone());
    assert!(c.has_unwritten());
    match c.next(t0 + Duration::from_secs(1)) {
        Next::Write(u) => {
            assert_eq!(u.status.as_deref(), Some("dnd"));
            assert_eq!(u.custom_status, None, "only the field picked");
        }
        other => panic!("the same pick again is written: {other:?}"),
    }
    // The custom status the same way.
    let lunch = Save {
        custom: Some(Some(custom("Lunch"))),
        ..Save::default()
    };
    wish.merge(lunch.clone());
    c.want(wish.clone());
    assert!(matches!(
        c.next(t0 + Duration::from_secs(2)),
        Next::Write(_)
    ));
    wish.merge(lunch);
    c.want(wish);
    match c.next(t0 + Duration::from_secs(3)) {
        Next::Write(u) => assert_eq!(u.custom_status, Some(Some(custom("Lunch")))),
        other => panic!("the same custom status again is written: {other:?}"),
    }
}

#[test]
fn a_custom_status_is_trimmed_and_capped() {
    let c = clean_custom(CustomStatus {
        emoji_name: Some(" ".into()),
        ..custom(&format!("  {}  ", "x".repeat(300)))
    });
    assert_eq!(c.text.as_deref().map(|t| t.chars().count()), Some(128));
    // Fluxer counts UTF-16 units: emoji outside the BMP count twice.
    let wide = clean_custom(custom(&"\u{1F600}".repeat(100)));
    assert_eq!(wide.text.as_deref().map(|t| t.chars().count()), Some(64));
    assert_eq!(c.emoji_name, None);
    let only_emoji = clean_custom(CustomStatus {
        emoji_name: Some("\u{1F600}".into()),
        ..custom("   ")
    });
    assert_eq!(only_emoji.text, None);
    assert_eq!(only_emoji.emoji_name.as_deref(), Some("\u{1F600}"));
}

/// A backend with a test store; the UI side is dropped.
fn backend() -> (Backend, tokio::sync::mpsc::UnboundedReceiver<Internal>) {
    let (tx, rx) = futures::channel::mpsc::unbounded::<Event>();
    // Events are not read here; keep the receiver alive so sends succeed.
    std::mem::forget(rx);
    let store = Accounts::new(Arc::new(MemoryStore::default()), None, None);
    let mut b = Backend::with_store(Ui::new(tx), store);
    let (itx, irx) = tokio::sync::mpsc::unbounded_channel();
    b.internal = itx;
    (b, irx)
}

/// Feeds the backend's internal messages back into it until `done` holds.
async fn drive(
    b: &mut Backend,
    irx: &mut tokio::sync::mpsc::UnboundedReceiver<Internal>,
    mut done: impl FnMut(&Backend) -> bool,
) {
    let until = Instant::now() + Duration::from_secs(10);
    while !done(b) {
        let msg = tokio::time::timeout_at(until, irx.recv())
            .await
            .expect("the backend got there in time")
            .expect("internal channel open");
        b.internal(msg);
    }
}

fn presence_cmd(status: &str) -> Command {
    social::Cmd::Presence(Cmd::Presence {
        status: status.into(),
        afk: false,
    })
    .into()
}

/// The Presence Updates (op 3) that arrive within `wait`, by status.
async fn op3s(conn: &mut Conn, wait: Duration) -> Vec<String> {
    let mut out = Vec::new();
    while let Some(v) = conn.recv(wait).await {
        if v["op"] == 3 {
            out.push(v["d"]["status"].as_str().unwrap_or_default().to_string());
        }
    }
    out
}

/// An account started in the background (identified on `listener`), then made the
/// active one; returns the backend, its internal receiver, the key and the active
/// account's gateway connection.
async fn active_account(
    listener: &TcpListener,
    instance: &p::Instance,
) -> (
    Backend,
    tokio::sync::mpsc::UnboundedReceiver<Internal>,
    AccountKey,
    Conn,
    Value,
) {
    let (mut b, mut irx) = backend();
    let key = AccountKey::new(instance.origin(), "9").unwrap();
    b.start_background(
        Restored {
            key: key.clone(),
            token: p::Token::new("tok-presence"),
            pins: instance.session_pins(),
            pasted: false,
        },
        Duration::ZERO,
        0,
    );
    drive(&mut b, &mut irx, |b| !b.background.is_empty()).await;
    let mut conn = Conn::accept(listener).await;
    let identify = conn.identify("9").await;
    // Nothing reaches a background account, so nothing more is sent on its gateway.
    b.command(Some(key.clone()), presence_cmd("online"));
    assert_eq!(
        op3s(&mut conn, Duration::from_millis(1500)).await,
        Vec::<String>::new()
    );
    accounts::handle(&mut b, accounts::Cmd::SwitchAccount(key.clone()));
    drive(
        &mut b,
        &mut irx,
        |b| matches!(&b.active, Some(Active::Real(r)) if !r.ignores_typing),
    )
    .await;
    let mut conn = Conn::accept(listener).await;
    conn.identify("9").await;
    (b, irx, key, conn, identify)
}

#[tokio::test]
async fn a_background_account_is_invisible_and_gets_no_more_presence() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (_server, instance) = mock_instance(&format!("ws://127.0.0.1:{port}/gateway"), &[]).await;
    let (mut b, _irx, key, mut conn, background_identify) =
        active_account(&listener, &instance).await;
    // Started in the background, it identified as invisible.
    assert_eq!(
        background_identify["d"]["presence"]["status"], "invisible",
        "{background_identify}"
    );
    // Away, so the phone still gets its mentions.
    assert_eq!(background_identify["d"]["presence"]["afk"], true);

    // Active: the UI's presence goes out.
    b.command(Some(key.clone()), presence_cmd("dnd"));
    assert_eq!(op3s(&mut conn, Duration::from_millis(800)).await, ["dnd"]);

    // To the background: invisible once, then nothing for it any more.
    b.demote_active();
    assert_eq!(
        op3s(&mut conn, Duration::from_millis(800)).await,
        ["invisible"]
    );
    b.command(Some(key.clone()), presence_cmd("online"));
    assert_eq!(
        op3s(&mut conn, Duration::from_millis(1500)).await,
        Vec::<String>::new()
    );

    // Active again (same gateway): it leaves `invisible` for the status asked for last,
    // and a burst of picks sends at most 5 per 20 s in all on this connection (two went
    // out already).
    accounts::handle(&mut b, accounts::Cmd::SwitchAccount(key.clone()));
    for i in 0..12 {
        b.command(
            Some(key.clone()),
            presence_cmd(["online", "idle", "dnd"][i % 3]),
        );
    }
    let burst = op3s(&mut conn, Duration::from_secs(2)).await;
    assert_eq!(burst.len(), 3, "5 per 20 s on the connection: {burst:?}");
    assert_eq!(burst[0], "dnd", "back in front with its status: {burst:?}");
}

#[tokio::test]
async fn typing_is_sent_at_most_every_eight_seconds() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (server, instance) = mock_instance(&format!("ws://127.0.0.1:{port}/gateway"), &[]).await;
    Mock::given(method("POST"))
        .and(path("/api/v1/channels/200/typing"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    let (mut b, _irx, _key, _conn, _) = active_account(&listener, &instance).await;

    async fn typing_posts(server: &MockServer) -> usize {
        requests(server)
            .await
            .iter()
            .filter(|r| {
                r.method.as_str() == "POST" && r.url.path() == "/api/v1/channels/200/typing"
            })
            .count()
    }
    // The composer asks on every key press it lets through (it lets one through per
    // second; this is four per second), for 9 s.
    let start = Instant::now();
    let press = |b: &mut Backend| {
        composer::handle(
            b,
            composer::Cmd::Typing {
                channel_id: "200".into(),
            },
        )
    };
    while start.elapsed() < Duration::from_millis(7_500) {
        press(&mut b);
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert_eq!(
        typing_posts(&server).await,
        1,
        "one request in the first 8 s"
    );
    while start.elapsed() < Duration::from_millis(9_000) {
        press(&mut b);
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(typing_posts(&server).await, 2, "the next one after 8 s");
}
