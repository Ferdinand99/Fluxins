//! Typing, presences and the account's own status.

mod common;

use common::*;
use inlet_protocol::models::parse_timestamp_ms;
use inlet_state::limits::{PASSIVE_MAX_PRESENCES, TYPING_EXPIRY_MS};
use inlet_state::AccountState;
use serde_json::json;

#[test]
fn typing_expires_after_ten_seconds() {
    assert_eq!(TYPING_EXPIRY_MS, 10_000);
    let mut s = state();
    let start = s.now_ms();
    s.apply(&d(
        "TYPING_START",
        json!({"channel_id": GENERAL, "user_id": OTHER}),
    ));
    s.tick(start + TYPING_EXPIRY_MS - 1);
    assert_eq!(
        s.typing(ch(GENERAL)),
        vec![uid(OTHER)],
        "still typing at 9.999 s"
    );
    let c = s.tick(start + TYPING_EXPIRY_MS);
    assert!(c.typing.contains(&ch(GENERAL)));
    assert!(s.typing(ch(GENERAL)).is_empty(), "gone at 10 s");

    // A new TYPING_START (the client sends one every 8 s) keeps it for 10 s more.
    let now = start + TYPING_EXPIRY_MS;
    s.apply(&d(
        "TYPING_START",
        json!({"channel_id": GENERAL, "user_id": OTHER}),
    ));
    s.tick(now + 8_000);
    s.apply(&d(
        "TYPING_START",
        json!({"channel_id": GENERAL, "user_id": OTHER}),
    ));
    s.tick(now + 8_000 + TYPING_EXPIRY_MS - 1);
    assert_eq!(s.typing(ch(GENERAL)), vec![uid(OTHER)]);
    s.tick(now + 8_000 + TYPING_EXPIRY_MS);
    assert!(s.typing(ch(GENERAL)).is_empty());
}

fn ready_with_settings(settings: serde_json::Value) -> AccountState {
    let mut v = ready_json(
        guild_ready(default_roles(), default_channels(), &[MOD_ROLE]),
        json!([]),
        json!([]),
    );
    v["user_settings"] = settings;
    AccountState::from_ready_at(&ready_from(v), JOINED_MS as i64)
}

#[test]
fn own_status_comes_from_the_settings_and_the_custom_status_expires() {
    let expires = "2026-01-01T01:00:00Z";
    let expires_ms = parse_timestamp_ms(expires).unwrap();
    let mut s = ready_with_settings(json!({
        "status": "dnd",
        "custom_status": {"text": "Focus time", "emoji_name": "\u{1F3A7}", "expires_at": expires}
    }));
    let own = s.presences().own();
    assert_eq!(own.status(), "dnd");
    assert_eq!(own.custom_expires_ms(), Some(expires_ms));
    let shown = own
        .custom_at(expires_ms - 1)
        .expect("shown before it expires");
    assert_eq!(shown.text.as_deref(), Some("Focus time"));
    assert!(own.custom_at(expires_ms).is_none(), "hidden once expired");

    // USER_SETTINGS_UPDATE changes both; the same settings again change nothing.
    let update = json!({"status": "invisible", "custom_status": null});
    let c = s.apply(&d("USER_SETTINGS_UPDATE", update.clone()));
    assert!(c.presence);
    assert_eq!(s.presences().own().status(), "invisible");
    assert!(s.presences().own().custom_at(0).is_none());
    assert!(!s.apply(&d("USER_SETTINGS_UPDATE", update)).presence);

    // Unknown statuses read as online; an unreadable expiry hides the custom status.
    let c = s.apply(&d(
        "USER_SETTINGS_UPDATE",
        json!({"status": "busy", "custom_status": {"text": "x", "expires_at": "soon"}}),
    ));
    assert!(c.presence);
    assert_eq!(s.presences().own().status(), "online");
    assert!(s.presences().own().custom_at(0).is_none());
}

#[test]
fn a_passive_account_keeps_almost_no_presences() {
    let mut s = state();
    for i in 0..50u64 {
        s.apply(&d(
            "PRESENCE_UPDATE",
            json!({"user": {"id": (1000 + i).to_string()}, "status": "online"}),
        ));
    }
    assert!(s.presences().len() > 40);
    s.set_passive(true);
    assert!(s.presences().len() <= PASSIVE_MAX_PRESENCES);
    for i in 0..50u64 {
        s.apply(&d(
            "PRESENCE_UPDATE",
            json!({"user": {"id": (2000 + i).to_string()}, "status": "idle"}),
        ));
    }
    assert!(s.presences().len() <= PASSIVE_MAX_PRESENCES);
    // The own status is not a cached presence: it stays.
    assert_eq!(s.presences().own().status(), "online");
}

#[test]
fn a_picked_status_shows_at_once_until_the_server_answers() {
    let mut s = state();
    let custom: inlet_protocol::models::CustomStatus =
        serde_json::from_value(json!({"text": "Lunch", "emoji_name": "\u{1F355}"})).unwrap();
    let c = s.set_own_status(Some("dnd"), Some(Some(custom)));
    assert!(c.presence);
    assert_eq!(s.presences().own().status(), "dnd");
    assert_eq!(
        s.presences()
            .own()
            .custom_at(0)
            .and_then(|c| c.text.as_deref()),
        Some("Lunch")
    );
    // Only the status: the custom status stays. Nothing new: no change.
    assert!(s.set_own_status(Some("idle"), None).presence);
    assert!(s.presences().own().custom_at(0).is_some());
    assert!(!s.set_own_status(Some("idle"), None).presence);
    // The server's USER_SETTINGS_UPDATE has the last word.
    s.apply(&d(
        "USER_SETTINGS_UPDATE",
        json!({"status": "online", "custom_status": null}),
    ));
    assert_eq!(s.presences().own().status(), "online");
    assert!(s.presences().own().custom_at(0).is_none());
}
