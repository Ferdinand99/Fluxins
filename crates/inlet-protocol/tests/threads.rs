//! Threads and forums against a mock instance, and thread gateway payloads.

mod common;

use inlet_protocol::gateway::events::ThreadMembersUpdate;
use inlet_protocol::gateway::FLAG_CHANNEL_THREADS;
use inlet_protocol::models::channel_type;
use inlet_protocol::rest::FLUXER_FEATURES;
use inlet_protocol::{
    Client, Dispatch, Error, GatewayOptions, Instance, NewForumPost, NewThread, SearchOutcome,
    ThreadEdit, ThreadSearch, Token,
};
use serde_json::{json, Value};
use wiremock::matchers::{body_json, header_exists, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(instance: &Instance, token: &str) -> Client {
    Client::new(instance.clone(), Token::new(token))
}

fn thread_json(id: &str, kind: i64, parent: &str) -> Value {
    json!({
        "id": id, "type": kind, "guild_id": "100", "parent_id": parent,
        "owner_id": "10", "name": "Release checklist", "last_message_id": null,
        "rate_limit_per_user": 0, "flags": 0,
        "thread_metadata": {"archived": false, "auto_archive_duration": 4320,
                            "archive_timestamp": "2026-05-05T20:30:05.018Z", "locked": false,
                            "create_timestamp": "2026-05-05T20:30:05.018Z"},
        "message_count": 12, "total_message_sent": 14, "member_count": 3
    })
}

async fn mount(server: &MockServer, m: &str, p: &str, status: u16, body: Option<Value>) {
    let mut resp = ResponseTemplate::new(status);
    if let Some(b) = body {
        resp = resp.set_body_json(b);
    }
    Mock::given(method(m))
        .and(path(p))
        .and(header_exists("X-Fluxer-Features"))
        .respond_with(resp)
        .expect(1)
        .mount(server)
        .await;
}

#[tokio::test]
async fn thread_routes_send_the_feature_header() {
    // The gateway asks for thread events with the Identify flag by default.
    assert_ne!(GatewayOptions::default().flags & FLAG_CHANNEL_THREADS, 0);
    assert!(FLUXER_FEATURES.split(',').any(|f| f == "channel_threads"));

    let (server, instance) = common::instance().await;
    let c = client(&instance, "tok-threads");
    mount(
        &server,
        "POST",
        "/api/v1/channels/200/messages/500/threads",
        201,
        Some(thread_json("500", channel_type::PUBLIC_THREAD, "200")),
    )
    .await;
    mount(
        &server,
        "POST",
        "/api/v1/channels/200/threads",
        201,
        Some(thread_json("501", channel_type::PRIVATE_THREAD, "200")),
    )
    .await;
    mount(
        &server,
        "PUT",
        "/api/v1/channels/501/thread-members/@me",
        204,
        None,
    )
    .await;
    mount(
        &server,
        "DELETE",
        "/api/v1/channels/501/thread-members/@me",
        204,
        None,
    )
    .await;
    mount(
        &server,
        "PATCH",
        "/api/v1/channels/501",
        200,
        Some({
            let mut t = thread_json("501", channel_type::PRIVATE_THREAD, "200");
            t["thread_metadata"]["archived"] = json!(true);
            t
        }),
    )
    .await;
    mount(
        &server,
        "GET",
        "/api/v1/channels/200/threads/archived/public",
        200,
        Some(json!({
            "threads": [thread_json("502", channel_type::PUBLIC_THREAD, "200")],
            "members": [{"id": "502", "user_id": "10", "join_timestamp": "2026-01-01T00:00:00Z", "flags": 1}],
            "has_more": true
        })),
    )
    .await;
    mount(
        &server,
        "GET",
        "/api/v1/channels/200/users/@me/threads/archived/private",
        200,
        Some(json!({"threads": [], "members": [], "has_more": false})),
    )
    .await;
    mount(
        &server,
        "GET",
        "/api/v1/channels/300/threads/search",
        202,
        Some(
            json!({"code": "SEARCH_INDEX_NOT_READY", "message": "building",
                    "documents_indexed": 0, "retry_after": 2}),
        ),
    )
    .await;

    let t = c
        .start_thread_from_message("200", "500", &NewThread::named("From a message"))
        .await
        .unwrap();
    assert!(t.is_thread());
    let private = NewThread {
        kind: channel_type::PRIVATE_THREAD,
        invitable: Some(false),
        ..NewThread::named("Private")
    };
    let p = c.start_thread("200", &private).await.unwrap();
    assert_eq!(p.kind, channel_type::PRIVATE_THREAD);
    c.join_thread("501").await.unwrap();
    c.leave_thread("501").await.unwrap();
    let archived = c.set_thread_archived("501", true).await.unwrap();
    assert!(archived.thread_metadata().unwrap().archived);
    let page = c
        .public_archived_threads("200", Some("2026-05-05T20:30:05.018Z"), 50)
        .await
        .unwrap();
    assert!(page.has_more);
    // The membership is attached to its thread.
    assert!(page.threads[0].is_thread_member());
    c.joined_private_archived_threads("200", None, 50)
        .await
        .unwrap();
    match c
        .search_threads("300", &ThreadSearch::default())
        .await
        .unwrap()
    {
        SearchOutcome::Indexing { retry_after } => {
            assert_eq!(retry_after, Some(std::time::Duration::from_secs(2)))
        }
        other => panic!("{other:?}"),
    }

    // Every request carried the capability header.
    let requests = server.received_requests().await.unwrap();
    let api: Vec<_> = requests
        .iter()
        .filter(|r| r.url.path().starts_with("/api/v1/"))
        .collect();
    assert_eq!(api.len(), 8);
    for r in api {
        let features = r.headers.get("X-Fluxer-Features").unwrap();
        assert!(features.to_str().unwrap().contains("channel_threads"));
    }

    // Bad input never reaches the server.
    assert!(matches!(
        c.start_thread("200", &NewThread::named("")).await,
        Err(Error::InvalidInput(_))
    ));
    assert!(matches!(
        c.join_thread("../1").await,
        Err(Error::InvalidInput(_))
    ));
    assert!(matches!(
        c.edit_thread("501", &ThreadEdit::default()).await,
        Err(Error::InvalidInput(_))
    ));
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        requests.len()
    );
}

#[tokio::test]
async fn forum_post_with_tags_round_trips() {
    let (server, instance) = common::instance().await;
    let c = client(&instance, "tok-forum");
    let mut post = thread_json("600", channel_type::PUBLIC_THREAD, "300");
    post["applied_tags"] = json!(["31", "32"]);
    post["member"] = json!({"join_timestamp": "2026-01-01T00:00:00Z", "flags": 1});
    post["message"] = common::message_json("600", "600", "How do I pin a post?");
    post["future_field"] = json!({"nested": [1, 2, 3]});
    Mock::given(method("POST"))
        .and(path("/api/v1/channels/300/threads"))
        .and(body_json(json!({
            "name": "Pinning posts",
            "type": 11,
            "message": {"content": "How do I pin a post?", "flags": 4096},
            "applied_tags": ["31", "32"],
            "auto_archive_duration": 1440
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(post))
        .expect(1)
        .mount(&server)
        .await;

    let mut new =
        NewForumPost::new("Pinning posts", "How do I pin a post?").with_tags(["31", "32"]);
    new.message = new.message.silent().with_nonce("ignored");
    new.auto_archive_duration = Some(1440);
    let created = c.create_forum_post("300", &new).await.unwrap();
    assert_eq!(created.thread.applied_tags(), vec!["31", "32"]);
    assert_eq!(created.thread.message_count(), Some(12));
    assert!(created.thread.is_thread_member());
    assert!(!created.thread.extra.contains_key("message"));
    assert_eq!(created.message.unwrap().content, "How do I pin a post?");

    // More than five tags, or a bad archive duration, is refused locally.
    let too_many = NewForumPost::new("x", "y").with_tags(["1", "2", "3", "4", "5", "6"]);
    assert!(matches!(
        c.create_forum_post("300", &too_many).await,
        Err(Error::InvalidInput(_))
    ));
    let mut odd = NewForumPost::new("x", "y");
    odd.auto_archive_duration = Some(5);
    assert!(matches!(
        c.create_forum_post("300", &odd).await,
        Err(Error::InvalidInput(_))
    ));

    // The forum's tags and post data.
    let forum: inlet_protocol::Channel = serde_json::from_value(json!({
        "id": "300", "type": 15, "guild_id": "100", "flags": 16,
        "available_tags": [{"id": "31", "name": "Solved", "moderated": false, "emoji_id": null, "emoji_name": "✅"},
                           {"name": "no id"}]
    }))
    .unwrap();
    assert!(forum.is_forum());
    assert_eq!(forum.available_tags().len(), 1);
    assert_eq!(forum.available_tags()[0].emoji_name.as_deref(), Some("✅"));
    Mock::given(method("POST"))
        .and(path("/api/v1/channels/300/post-data"))
        .and(body_json(json!({"thread_ids": ["600", "601"]})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"threads": {
            "600": {"owner": {"user": common::user_json("10", "ana"), "roles": []},
                    "first_message": common::message_json("600", "600", "first")},
            "601": {"owner": null, "first_message": null}
        }})))
        .expect(1)
        .mount(&server)
        .await;
    let data = c.forum_post_data("300", &["600", "601"]).await.unwrap();
    assert_eq!(data["600"].first_message.as_ref().unwrap().content, "first");
    assert!(data["601"].owner.is_none());
}

#[tokio::test]
async fn unknown_thread_fields_are_ignored() {
    let mut create = thread_json("700", channel_type::PRIVATE_THREAD, "200");
    create["newly_created"] = json!(true);
    create["member"] = json!({"id": "700", "user_id": "10", "join_timestamp": null,
                              "flags": "3", "muted": null, "mute_config": null, "brand_new": 1});
    create["thread_metadata"]["invitable"] = json!(null);
    create["thread_metadata"]["some_new_flag"] = json!(true);
    create["last_message_id"] = json!(null);
    create["applied_tags"] = json!(null);
    create["unknown"] = json!({"x": [1]});
    let thread = match Dispatch::parse("THREAD_CREATE", create) {
        Dispatch::ThreadCreate(t) => t,
        other => panic!("{other:?}"),
    };
    assert!(thread.newly_created());
    assert!(thread.applied_tags().is_empty());
    let member = thread.thread_member().unwrap();
    assert_eq!(member.flags, 3);
    assert!(!member.muted);
    let meta = thread.thread_metadata().unwrap();
    assert_eq!(meta.invitable, None);
    assert!(meta.extra.contains_key("some_new_flag"));

    let update = match Dispatch::parse(
        "THREAD_UPDATE",
        json!({"id": "700", "type": 12, "guild_id": "100", "parent_id": "200",
               "name": null, "thread_metadata": null, "member_ids_preview": [1, 2]}),
    ) {
        Dispatch::ThreadUpdate(t) => t,
        other => panic!("{other:?}"),
    };
    assert!(update.thread_metadata().is_none());
    let mut stored = (*thread).clone();
    stored.merge_thread_update(*update);
    assert!(stored.is_thread_member());

    let sync = match Dispatch::parse(
        "THREAD_LIST_SYNC",
        json!({"guild_id": "100", "channel_ids": null,
               "threads": [thread_json("701", 11, "200"), {"no": "id"}, null],
               "members": [{"id": "701", "user_id": "10", "flags": 0}, 5],
               "extra_thing": true}),
    ) {
        Dispatch::ThreadListSync(s) => s,
        other => panic!("{other:?}"),
    };
    assert_eq!(sync.channel_ids, None);
    assert_eq!(sync.threads.len(), 1);
    assert!(sync.threads_with_members()[0].is_thread_member());

    let me = match Dispatch::parse(
        "THREAD_MEMBER_UPDATE",
        json!({"id": "701", "user_id": "10", "guild_id": "100", "flags": 4, "muted": true,
               "mute_config": {"end_time": null}, "whatever": "x"}),
    ) {
        Dispatch::ThreadMemberUpdate(m) => m,
        other => panic!("{other:?}"),
    };
    assert_eq!(me.guild_id.as_deref(), Some("100"));
    assert!(me.muted);

    let members: ThreadMembersUpdate = serde_json::from_value(json!({
        "id": "701", "guild_id": "100", "member_count": null,
        "added_members": null, "removed_member_ids": ["10"]
    }))
    .unwrap();
    assert_eq!(members.member_count, None);
    assert_eq!(members.membership_change("10"), Some(None));

    let list = match Dispatch::parse(
        "THREAD_MEMBER_LIST_UPDATE",
        json!({"guild_id": "100", "thread_id": "701", "members": [
            {"user_id": "10", "join_timestamp": "2026-01-01T00:00:00Z", "flags": 0,
             "member": "not a member", "presence": {"status": "offline", "mobile": false, "afk": false}},
            {"join_timestamp": "no user id"}
        ]}),
    ) {
        Dispatch::ThreadMemberListUpdate(l) => l,
        other => panic!("{other:?}"),
    };
    assert_eq!(list.members.len(), 1);
    assert!(list.members[0].member.is_none());
    assert_eq!(list.members[0].presence.as_ref().unwrap().status, "offline");

    // A thread search answer with extra and broken parts.
    let (server, instance) = common::instance().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/channels/300/threads/search"))
        .and(query_param("name", "pin"))
        .and(query_param("archived", "false"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "threads": [thread_json("702", 11, "300"), "junk"],
            "members": null, "has_more": null, "total_results": "1",
            "first_messages": [common::message_json("702", "702", "first")],
            "brand_new_field": [1]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let found = client(&instance, "tok-thread-search")
        .search_threads(
            "300",
            &ThreadSearch {
                name: Some("pin".into()),
                archived: Some(false),
                ..ThreadSearch::default()
            },
        )
        .await
        .unwrap()
        .ready()
        .unwrap();
    assert_eq!(found.threads.len(), 1);
    assert!(!found.has_more);
    assert_eq!(found.total_results, 1);
    assert_eq!(found.first_messages[0].content, "first");
}
