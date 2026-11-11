//! The DM list order, pinned DMs, group DM recipients and personal notes.

mod common;

use common::*;
use inlet_protocol::Dispatch;
use inlet_state::relationships::Kind;
use inlet_state::{AccountState, ChannelId};
use serde_json::json;

fn order(s: &AccountState) -> Vec<String> {
    s.dm_list().iter().map(|c| c.id.clone()).collect()
}

fn state_with(private: serde_json::Value, extra: serde_json::Value) -> AccountState {
    let mut v = ready_json(
        guild_ready(default_roles(), default_channels(), &[MOD_ROLE]),
        json!([]),
        json!([]),
    );
    v["private_channels"] = private;
    for (k, val) in extra.as_object().unwrap() {
        v[k] = val.clone();
    }
    AccountState::from_ready_at(&ready_from(v), 1)
}

#[test]
fn the_dm_list_is_pinned_first_then_newest_activity_first() {
    // Snowflakes: 4194304 << n is a later time for a larger n.
    let at = |ms: u64| (ms << 22).to_string();
    let s = state_with(
        json!([
            {"id": at(100), "type": 1, "last_message_id": at(500),
             "recipients": [{"id": "61", "username": "old"}]},
            {"id": at(200), "type": 1, "last_message_id": at(900),
             "recipients": [{"id": "62", "username": "new"}]},
            // No message yet: counts from its creation.
            {"id": at(700), "type": 3, "recipients": [{"id": "63", "username": "g"}]},
            {"id": at(50), "type": 1, "recipients": [{"id": "64", "username": "pinned"}]},
            // Personal notes are not in the list.
            {"id": ME, "type": 999},
        ]),
        json!({"pinned_dms": [at(50)]}),
    );
    assert_eq!(order(&s), [at(50), at(200), at(700), at(100)]);

    // A new message moves a DM up, live.
    let mut s = s;
    s.apply(&create(
        inlet_state::MessageId(1000 << 22),
        &at(100),
        "61",
        json!({}),
    ));
    assert_eq!(order(&s), [at(50), at(100), at(200), at(700)]);
    // Unpinned everywhere: it sorts by activity again.
    s.apply(&Dispatch::UserPinnedDmsUpdate(vec![]));
    assert_eq!(order(&s)[3], at(50));
}

#[test]
fn a_new_friendship_lifts_its_dm_and_a_request_does_not() {
    let at = |ms: u64| (ms << 22).to_string();
    // The friendship began 2026-01-01, long after both DMs' last messages.
    let since = "2026-01-01T00:00:00.000Z";
    let s = state_with(
        json!([
            {"id": at(10), "type": 1, "last_message_id": at(100),
             "recipients": [{"id": "61", "username": "friend"}]},
            {"id": at(20), "type": 1, "last_message_id": at(200),
             "recipients": [{"id": "62", "username": "other"}]},
            {"id": at(30), "type": 1, "last_message_id": at(150),
             "recipients": [{"id": "63", "username": "asked"}]},
        ]),
        json!({"relationships": [
            {"id": "61", "type": 1, "since": since},
            {"id": "63", "type": 3, "since": since},
        ]}),
    );
    assert_eq!(order(&s), [at(10), at(20), at(30)]);
}

#[test]
fn group_dm_recipients_follow_the_events() {
    let mut s = state_with(
        json!([{"id": "300", "type": 3, "recipients": [{"id": "61", "username": "a"}]}]),
        json!({}),
    );
    let add = d(
        "CHANNEL_RECIPIENT_ADD",
        json!({"channel_id": "300", "user": {"id": "62", "username": "b"}}),
    );
    let c = s.apply(&add);
    assert!(c.channels.contains(&ChannelId(300)));
    let names: Vec<_> = s
        .channel(ChannelId(300))
        .unwrap()
        .recipients
        .iter()
        .map(|u| u.username.clone())
        .collect();
    assert_eq!(names, ["a", "b"]);
    s.apply(&d(
        "CHANNEL_RECIPIENT_REMOVE",
        json!({"channel_id": "300", "user": {"id": "61", "username": "a"}}),
    ));
    assert_eq!(s.channel(ChannelId(300)).unwrap().recipients.len(), 1);
    // An unknown channel or a guild channel is left alone.
    let c = s.apply(&d(
        "CHANNEL_RECIPIENT_ADD",
        json!({"channel_id": "777", "user": {"id": "62", "username": "b"}}),
    ));
    assert!(c.channels.is_empty());
}

#[test]
fn relationships_come_from_ready_and_personal_notes_open_like_a_dm() {
    let mut s = state_with(
        json!([]),
        json!({"relationships": [
            {"id": "61", "type": 1}, {"id": "62", "type": 3}, {"id": "63", "type": 2}
        ], "users": [{"id": "61", "username": "fr"}]}),
    );
    let r = s.relationships();
    assert_eq!(r.kind(uid("61")), Some(Kind::Friend));
    assert_eq!(r.incoming_count(), 1);
    assert!(s.is_blocked(uid("63")));
    assert_eq!(
        r.get(uid("61"))
            .and_then(|p| p.user.as_ref())
            .map(|u| u.username.as_str()),
        Some("fr")
    );
    let notes = s.ensure_personal_notes();
    assert_eq!(notes.to_string(), ME);
    assert!(s.channel(notes).is_some());
    assert!(s.channel_permissions(notes).can_send());
    assert!(s.dm_list().is_empty());
}
