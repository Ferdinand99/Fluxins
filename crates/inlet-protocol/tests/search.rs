//! Message search against a mock instance: the request body, the result
//! shapes and the one-search-per-second budget.

mod common;

use std::time::{Duration, Instant};

use inlet_protocol::api::search::{search_has, search_scope};
use inlet_protocol::models::snowflake_at_ms;
use inlet_protocol::{Client, Error, Instance, MessageSearch, SearchOutcome, Token};
use serde_json::json;
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, ResponseTemplate};

fn client(instance: &Instance, token: &str) -> Client {
    Client::new(instance.clone(), Token::new(token))
}

#[tokio::test]
async fn search_query_is_built_from_filters() {
    let (server, instance) = common::instance().await;
    let c = client(&instance, "tok-search");
    let before = snowflake_at_ms(1_800_000_000_000);
    let after = snowflake_at_ms(1_700_000_000_000);
    Mock::given(method("POST"))
        .and(path("/api/v1/search/messages"))
        .and(body_json(json!({
            "context_guild_id": "100",
            "channel_ids": ["200"],
            "author_id": ["10"],
            "mentions": ["11"],
            "content": "release notes",
            "has": ["image", "link"],
            "pinned": true,
            "min_id": after,
            "max_id": before,
            "sort_order": "asc",
            "page": 2
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "messages": [common::message_json("5", "200", "release notes v2"), {"broken": 1}],
            "channels": [{"id": "200", "type": 0, "guild_id": "100", "name": "general"}],
            "total": 30, "hits_per_page": 25, "page": 2, "cursor": null,
            "something_new": {"a": 1}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let search = MessageSearch::in_guild("100")
        .text("release notes")
        .in_channels("200")
        .from_author("10")
        .mentioning("11")
        .with(search_has::IMAGE)
        .with(search_has::LINK)
        .pinned_only()
        .after_time(1_700_000_000_000)
        .before_time(1_800_000_000_000)
        .oldest_first()
        .page(2);
    let results = c.search_messages(&search).await.unwrap().ready().unwrap();
    assert_eq!(results.messages.len(), 1);
    assert_eq!(results.messages[0].content, "release notes v2");
    assert_eq!(results.channels[0].name.as_deref(), Some("general"));
    assert_eq!(results.total, 30);
    assert!(!results.has_more());

    // Wider scopes and the "indexing" answer.
    Mock::given(method("POST"))
        .and(path("/api/v1/search/messages"))
        .and(body_json(json!({"scope": "all_dms", "content": "hi"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"indexing": true})))
        .expect(1)
        .mount(&server)
        .await;
    let dms = MessageSearch::in_scope(search_scope::ALL_DMS).text("hi");
    assert_eq!(
        c.search_messages(&dms).await.unwrap(),
        SearchOutcome::Indexing { retry_after: None }
    );

    // Bad filters never reach the server.
    let requests = server.received_requests().await.unwrap().len();
    for bad in [
        MessageSearch::default().text("no context"),
        MessageSearch::in_guild("100").text(""),
        MessageSearch::in_guild("100").text("x".repeat(1025)),
        MessageSearch::in_guild("100").from_author("@me"),
        MessageSearch::in_guild("../1"),
        MessageSearch::in_guild("100").with("poll-ish"),
        MessageSearch::in_scope("everything"),
        MessageSearch {
            author_ids: vec!["1".into(); 101],
            ..MessageSearch::in_guild("100")
        },
        MessageSearch {
            sort_by: Some("random".into()),
            ..MessageSearch::in_channel("200")
        },
    ] {
        assert!(
            matches!(c.search_messages(&bad).await, Err(Error::InvalidInput(_))),
            "{bad:?}"
        );
    }
    assert_eq!(server.received_requests().await.unwrap().len(), requests);
}

#[tokio::test]
async fn searches_are_paced_one_per_second() {
    let (server, instance) = common::instance().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/search/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "messages": [], "channels": [], "total": 0, "hits_per_page": 25, "page": 1
        })))
        .mount(&server)
        .await;
    let c = client(&instance, "tok-search-budget");
    // A burst of eight searches (fast typing): four get slots 0-3 s, the
    // rest are refused at once without a request.
    let started = Instant::now();
    let mut tasks = Vec::new();
    for i in 0..8 {
        let c = c.clone();
        tasks.push(tokio::spawn(async move {
            c.search_messages(&MessageSearch::in_channel("200").text(format!("q{i}")))
                .await
        }));
    }
    let mut ok = 0;
    let mut refused = 0;
    for t in tasks {
        match t.await.unwrap() {
            Ok(_) => ok += 1,
            Err(Error::RateLimited { retry_after }) => {
                assert!(retry_after > Duration::from_secs(3));
                refused += 1;
            }
            Err(e) => panic!("{e}"),
        }
    }
    assert_eq!((ok, refused), (4, 4));
    assert!(started.elapsed() >= Duration::from_millis(2_900));

    // The server saw only the four.
    let searches = |reqs: Vec<wiremock::Request>| {
        reqs.iter()
            .filter(|r| r.url.path() == "/api/v1/search/messages")
            .count()
    };
    assert_eq!(searches(server.received_requests().await.unwrap()), 4);
    // Another client of the same token shares the budget: its search waits
    // for the slot one second after the last one.
    let other = client(&instance, "tok-search-budget");
    let t = Instant::now();
    other
        .search_messages(&MessageSearch::in_channel("200").text("later"))
        .await
        .unwrap();
    assert!(
        t.elapsed() >= Duration::from_millis(700),
        "{:?}",
        t.elapsed()
    );
    assert_eq!(searches(server.received_requests().await.unwrap()), 5);
}
