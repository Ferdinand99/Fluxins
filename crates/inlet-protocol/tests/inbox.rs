//! Saved messages, recent mentions and the message helpers (around, after,
//! forward, hiding embeds, silent sends) against a mock instance.

mod common;

use inlet_protocol::models::message_flags;
use inlet_protocol::{
    AllowedMentions, Client, Dispatch, Error, ForwardSource, Instance, MentionsQuery, Message,
    OutgoingMessage, Token,
};
use serde_json::json;
use wiremock::matchers::{body_json, method, path, query_param};
use wiremock::{Mock, ResponseTemplate};

fn client(instance: &Instance, token: &str) -> Client {
    Client::new(instance.clone(), Token::new(token))
}

#[tokio::test]
async fn saved_messages_and_recent_mentions() {
    let (server, instance) = common::instance().await;
    let c = client(&instance, "tok-inbox");
    Mock::given(method("GET"))
        .and(path("/api/v1/users/@me/saved-messages"))
        .and(query_param("limit", "100"))
        .and(query_param("before", "900"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": "5", "channel_id": "200", "message_id": "5", "status": "available",
             "message": common::message_json("5", "200", "keep this")},
            {"id": "6", "channel_id": "201", "message_id": "6", "status": "missing_permissions",
             "message": null, "new_field": 1},
            {"channel_id": "no id"}
        ])))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/users/@me/saved-messages"))
        .and(body_json(json!({"channel_id": "200", "message_id": "7"})))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/api/v1/users/@me/saved-messages/7"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/users/@me/mentions"))
        .and(query_param("limit", "25"))
        .and(query_param("roles", "false"))
        .and(query_param("everyone", "true"))
        .and(query_param("before", "800"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!([common::message_json(
                "9",
                "200",
                "hey <@10>"
            )])),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/api/v1/users/@me/mentions/9"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/users/@me/mentions/read"))
        .and(body_json(json!({"message_ids": ["9", "10"]})))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let saved = c.saved_messages(Some("900"), 200).await.unwrap();
    assert_eq!(saved.len(), 2);
    assert!(saved[0].is_available());
    assert!(!saved[1].is_available());
    c.save_message("200", "7").await.unwrap();
    c.unsave_message("7").await.unwrap();
    let mentions = c
        .recent_mentions(&MentionsQuery {
            roles: false,
            before: Some("800".into()),
            ..MentionsQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(mentions[0].content, "hey <@10>");
    c.delete_recent_mention("9").await.unwrap();
    c.mark_mentions_read(&["9", "10"]).await.unwrap();

    let before = server.received_requests().await.unwrap().len();
    assert!(matches!(
        c.mark_mentions_read(&[]).await,
        Err(Error::InvalidInput(_))
    ));
    let many: Vec<&str> = vec!["1"; 101];
    assert!(matches!(
        c.mark_mentions_read(&many).await,
        Err(Error::InvalidInput(_))
    ));
    assert!(matches!(
        c.save_message("200", "x/y").await,
        Err(Error::InvalidInput(_))
    ));
    assert_eq!(server.received_requests().await.unwrap().len(), before);

    // The gateway side.
    assert!(matches!(
        Dispatch::parse("SAVED_MESSAGE_CREATE", common::message_json("7", "200", "x")),
        Dispatch::SavedMessageCreate(m) if m.id == "7"
    ));
    assert!(matches!(
        Dispatch::parse("SAVED_MESSAGE_DELETE", json!({"message_id": "7", "extra": true})),
        Dispatch::SavedMessageDelete(d) if d.message_id == "7"
    ));
    assert!(matches!(
        Dispatch::parse("RECENT_MENTION_DELETE", json!({"message_id": 9})),
        Dispatch::RecentMentionDelete(d) if d.message_id == "9"
    ));
}

#[tokio::test]
async fn jump_forward_suppress_and_silent() {
    let (server, instance) = common::instance().await;
    let c = client(&instance, "tok-msg-extras");
    Mock::given(method("GET"))
        .and(path("/api/v1/channels/200/messages"))
        .and(query_param("around", "500"))
        .and(query_param("limit", "50"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            common::message_json("501", "200", "after"),
            common::message_json("500", "200", "target"),
        ])))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/channels/200/messages"))
        .and(query_param("after", "501"))
        .and(query_param("limit", "100"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/channels/300/messages"))
        .and(body_json(json!({
            "message_reference": {"message_id": "500", "channel_id": "200", "guild_id": "100",
                                  "type": 1, "attachment_ids": ["77"]},
            "nonce": "n1",
            "flags": message_flags::SUPPRESS_NOTIFICATIONS
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json({
            let mut m = common::message_json("600", "300", "");
            m["message_reference"] = json!({"channel_id": "200", "message_id": "500", "type": 1});
            m["message_snapshots"] = json!([{"content": "target", "timestamp": "2026-11-10T08:00:00.000Z",
                                              "type": 0, "flags": 0}]);
            m
        }))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/api/v1/channels/200/messages/500"))
        .and(body_json(json!({"flags": message_flags::SUPPRESS_EMBEDS})))
        .respond_with(ResponseTemplate::new(200).set_body_json({
            let mut m = common::message_json("500", "200", "target");
            m["flags"] = json!(message_flags::SUPPRESS_EMBEDS);
            m
        }))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/channels/200/messages"))
        .and(body_json(json!({
            "content": "<@10> ship it", "nonce": "n2",
            "flags": message_flags::SUPPRESS_NOTIFICATIONS,
            "allowed_mentions": {"users": ["10"]}
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json({
            let mut m = common::message_json("700", "200", "<@10> ship it");
            m["flags"] = json!(message_flags::SUPPRESS_NOTIFICATIONS);
            m
        }))
        .expect(1)
        .mount(&server)
        .await;

    let page = c.messages_around("200", "500", 50).await.unwrap();
    assert!(page.iter().any(|m| m.id == "500"));
    assert!(c
        .messages_after("200", "501", 200)
        .await
        .unwrap()
        .is_empty());

    let mut target: Message = page.into_iter().find(|m| m.id == "500").unwrap();
    target.guild_id = Some("100".into());
    let mut source = ForwardSource::of(&target);
    source.attachment_ids.push("77".into());
    let fwd = c.forward("300", &source, Some("n1"), true).await.unwrap();
    assert!(fwd.is_forward());
    assert_eq!(fwd.message_snapshots()[0].content, "target");

    let hidden = c.set_embeds_suppressed(&target, true).await.unwrap();
    assert!(hidden.embeds_suppressed());

    let text = inlet_protocol::models::strip_silent_prefix("@silent <@10> ship it").unwrap();
    let sent = c
        .send(
            "200",
            &OutgoingMessage::text(text)
                .with_nonce("n2")
                .silent()
                .with_allowed_mentions(AllowedMentions::only(&["10".into()], &[], false)),
        )
        .await
        .unwrap();
    assert!(sent.is_silent());

    // A forward with too many picks is refused locally.
    let mut greedy = ForwardSource::of(&target);
    greedy.embed_indices = (0..11).collect();
    let before = server.received_requests().await.unwrap().len();
    assert!(matches!(
        c.forward("300", &greedy, None, false).await,
        Err(Error::InvalidInput(_))
    ));
    assert_eq!(server.received_requests().await.unwrap().len(), before);
}
