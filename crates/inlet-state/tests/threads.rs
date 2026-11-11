//! Threads and forum posts: membership from the THREAD_* events (private
//! threads fail closed), the per-guild cap, and op 28 for an open forum.

mod common;

use common::*;
use inlet_protocol::Channel;
use inlet_state::limits::MAX_THREADS_PER_GUILD;
use inlet_state::threads::MAX_FORUM_UNREAD_POSTS;
use inlet_state::{AccountState, AckKind, ForumUnreadRequests};
use serde_json::{json, Value};

const FORUM: &str = "400";
const PRIVATE: &str = "501";

fn member() -> Value {
    json!({"join_timestamp": "2026-01-01T00:00:00Z", "flags": 1})
}

fn thread_json(id: &str, kind: i64, parent: &str, extra: Value) -> Value {
    let mut v = json!({
        "id": id, "type": kind, "guild_id": GUILD, "parent_id": parent, "name": format!("t{id}"),
        "thread_metadata": {"archived": false, "locked": false},
    });
    if let (Value::Object(o), Value::Object(e)) = (&mut v, extra) {
        o.extend(e);
    }
    v
}

fn visible(s: &AccountState, id: &str) -> bool {
    s.permissions(g(), ch(id)).can_view()
}

fn listed(s: &AccountState, parent: &str) -> Vec<String> {
    s.threads_of(ch(parent))
        .iter()
        .map(|t| t.id.clone())
        .collect()
}

#[test]
fn private_threads_fail_closed_without_membership() {
    let mut s = state();
    // Added to a private thread: THREAD_CREATE carries the membership.
    s.apply(&d(
        "THREAD_CREATE",
        thread_json(PRIVATE, 12, GENERAL, json!({"member": member()})),
    ));
    assert!(visible(&s, PRIVATE));
    assert_eq!(listed(&s, GENERAL), [PRIVATE]);

    // THREAD_UPDATE never carries it: the stored one stays.
    s.apply(&d(
        "THREAD_UPDATE",
        thread_json(PRIVATE, 12, GENERAL, json!({"name": "renamed"})),
    ));
    assert!(visible(&s, PRIVATE), "a rename keeps the membership");
    assert_eq!(
        s.channel(ch(PRIVATE)).and_then(|t| t.name.clone()).as_deref(),
        Some("renamed")
    );

    // Removed: hidden at once.
    let c = s.apply(&d(
        "THREAD_MEMBERS_UPDATE",
        json!({"id": PRIVATE, "guild_id": GUILD, "member_count": 1, "removed_member_ids": [ME]}),
    ));
    assert!(c.permissions && c.channels.contains(&ch(PRIVATE)));
    assert!(!visible(&s, PRIVATE));
    assert!(listed(&s, GENERAL).is_empty());

    // Added again through THREAD_MEMBER_UPDATE (the account's own membership).
    s.apply(&d(
        "THREAD_MEMBER_UPDATE",
        json!({"id": PRIVATE, "user_id": ME, "guild_id": GUILD, "flags": 1}),
    ));
    assert!(visible(&s, PRIVATE));
    // Someone else's membership changes nothing for the account.
    s.apply(&d(
        "THREAD_MEMBERS_UPDATE",
        json!({"id": PRIVATE, "removed_member_ids": [OTHER]}),
    ));
    assert!(visible(&s, PRIVATE));

    // THREAD_LIST_SYNC is authoritative: no membership listed means none.
    s.apply(&d(
        "THREAD_LIST_SYNC",
        json!({"guild_id": GUILD, "threads": [thread_json(PRIVATE, 12, GENERAL, json!({}))],
               "members": []}),
    ));
    assert!(!visible(&s, PRIVATE));
    // ... and a membership it lists is taken.
    s.apply(&d(
        "THREAD_LIST_SYNC",
        json!({"guild_id": GUILD, "channel_ids": [GENERAL],
               "threads": [thread_json(PRIVATE, 12, GENERAL, json!({}))],
               "members": [{"id": PRIVATE, "user_id": ME, "flags": 1}]}),
    ));
    assert!(visible(&s, PRIVATE));

    // A new READY rebuilds threads from the guild's list: a membership whose
    // removal was missed does not stay.
    let mut v = ready_json(
        guild_ready(default_roles(), default_channels(), &[MOD_ROLE]),
        json!([]),
        json!([]),
    );
    v["guilds"][0]["threads"] = json!([thread_json(PRIVATE, 12, GENERAL, json!({}))]);
    s.apply(&inlet_protocol::Dispatch::Ready(Box::new(ready_from(v))));
    assert!(s.channel(ch(PRIVATE)).is_some(), "READY's thread is stored");
    assert!(!visible(&s, PRIVATE));

    // A public thread needs no membership.
    s.apply(&d(
        "THREAD_CREATE",
        thread_json("502", 11, GENERAL, json!({})),
    ));
    assert!(visible(&s, "502"));
    // REST threads are authoritative too.
    let rest: Channel = serde_json::from_value(thread_json(
        PRIVATE,
        12,
        GENERAL,
        json!({"member": member()}),
    ))
    .unwrap();
    s.upsert_thread(&rest);
    assert!(visible(&s, PRIVATE));

    // THREAD_DELETE removes it with its permissions.
    s.apply(&d(
        "THREAD_DELETE",
        json!({"id": PRIVATE, "guild_id": GUILD, "parent_id": GENERAL, "type": 12}),
    ));
    assert!(s.channel(ch(PRIVATE)).is_none());
    assert!(!visible(&s, PRIVATE));
}

#[test]
fn threads_need_a_known_parent_in_their_guild() {
    let mut s = state();
    // An unknown parent, a parent in another guild (a DM) and a thread under a
    // thread are dropped; so is a channel that is no thread.
    s.apply(&d("THREAD_CREATE", thread_json("601", 11, "999", json!({}))));
    s.apply(&d("THREAD_CREATE", thread_json("602", 11, DM, json!({}))));
    s.apply(&d("THREAD_CREATE", thread_json("603", 11, GENERAL, json!({}))));
    s.apply(&d("THREAD_CREATE", thread_json("604", 11, "603", json!({}))));
    s.apply(&d("THREAD_CREATE", thread_json("605", 0, GENERAL, json!({}))));
    // A thread never takes the place of a DM.
    s.apply(&d("THREAD_CREATE", thread_json(DM, 11, GENERAL, json!({}))));
    assert!(s.channel(ch("601")).is_none());
    assert!(s.channel(ch("602")).is_none());
    assert!(s.channel(ch("603")).is_some());
    assert!(s.channel(ch("604")).is_none());
    assert!(s.channel(ch("605")).is_none());
    assert_eq!(s.channel(ch(DM)).map(|c| c.kind), Some(1));
}

#[test]
fn threads_are_capped_per_guild() {
    let mut s = state();
    for i in 0..(MAX_THREADS_PER_GUILD + 50) {
        let id = (10_000 + i).to_string();
        s.apply(&d("THREAD_CREATE", thread_json(&id, 11, GENERAL, json!({}))));
    }
    assert_eq!(listed(&s, GENERAL).len(), MAX_THREADS_PER_GUILD);
    // A known thread still takes its updates.
    s.apply(&d(
        "THREAD_UPDATE",
        thread_json("10000", 11, GENERAL, json!({"name": "still here"})),
    ));
    assert_eq!(
        s.channel(ch("10000")).and_then(|t| t.name.clone()).as_deref(),
        Some("still here")
    );
}

/// A guild with a forum and posts in READY: 401 read before and not joined,
/// 402 joined, 403 never read, and 50 more read and not joined.
fn forum_state() -> AccountState {
    let mut channels = default_channels();
    channels
        .as_array_mut()
        .unwrap()
        .push(channel(FORUM, 15, None, 5, json!([])));
    let mut threads = vec![
        thread_json("401", 11, FORUM, json!({"last_message_id": mid(9, 0).to_string()})),
        thread_json("402", 11, FORUM, json!({"member": member()})),
        thread_json("403", 11, FORUM, json!({})),
    ];
    let mut read_states = vec![
        json!({"id": "401", "last_message_id": mid(1, 0).to_string()}),
        json!({"id": "402", "last_message_id": mid(1, 0).to_string()}),
    ];
    for i in 0..50 {
        let id = (700 + i).to_string();
        threads.push(thread_json(&id, 11, FORUM, json!({})));
        read_states.push(json!({"id": id, "last_message_id": mid(1, 0).to_string()}));
    }
    let mut guild = guild_ready(default_roles(), channels, &[MOD_ROLE]);
    guild["threads"] = json!(threads);
    let v = ready_json(guild, json!(read_states), json!([]));
    AccountState::from_ready_at(&ready_from(v), JOINED_MS as i64 + 3_600_000)
}

#[test]
fn forum_unreads_use_op28_once_per_open() {
    let mut s = forum_state();
    let forum = ch(FORUM);
    let posts: Vec<Channel> = s.threads_of(forum).into_iter().cloned().collect();
    let first: Vec<&Channel> = posts
        .iter()
        .filter(|p| ["401", "402", "403"].contains(&p.id.as_str()))
        .collect();
    let all: Vec<&Channel> = posts.iter().collect();

    let mut op28 = ForumUnreadRequests::default();
    assert!(
        op28.take(&s, forum, &first).is_none(),
        "nothing while the forum is not open"
    );
    op28.open(forum);
    let sent = op28.take(&s, forum, &first).expect("one request");
    // Only the post the account read before and did not join; with its ack.
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].thread_id, "401");
    assert_eq!(
        sent[0].ack_message_id.as_deref(),
        Some(mid(1, 0).to_string().as_str())
    );
    // Once per opening: more posts showing up ask nothing more.
    assert!(op28.take(&s, forum, &all).is_none());
    assert!(op28.take(&s, forum, &first).is_none());
    // Another forum is not the open one.
    assert!(op28.take(&s, ch(GENERAL), &first).is_none());

    // Opened again: one more request, capped at 40 posts.
    op28.close();
    assert!(op28.take(&s, forum, &all).is_none());
    op28.open(forum);
    let sent = op28.take(&s, forum, &all).expect("one request per opening");
    assert_eq!(sent.len(), MAX_FORUM_UNREAD_POSTS);
    assert!(sent.iter().all(|r| r.thread_id != "402" && r.thread_id != "403"));
    assert!(op28.take(&s, forum, &all).is_none());

    // The answer: counts for the posts, not for joined ones.
    s.apply(&d(
        "FORUM_UNREADS",
        json!({"guild_id": GUILD, "channel_id": FORUM, "threads": [
            {"thread_id": "401", "count": 3},
            {"thread_id": "402", "count": 5},
            {"thread_id": "700", "missing": true},
        ]}),
    ));
    let post = |s: &AccountState, id: &str| s.channel(ch(id)).cloned().unwrap();
    assert_eq!(s.forum_post_unread(&post(&s, "401")), 3);
    assert_eq!(s.forum_post_unread(&post(&s, "402")), 0, "joined: its read state counts");
    assert_eq!(s.forum_post_unread(&post(&s, "700")), 0);
    // Reading the post moves its ack: the old count no longer applies.
    s.mark_read(ch("401"), None, AckKind::Explicit);
    assert_eq!(s.forum_post_unread(&post(&s, "401")), 0);
}
