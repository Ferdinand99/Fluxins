//! The plain social, profile, folder and meme routes: paths, bodies and
//! answers against a mock instance.

mod common;

use inlet_protocol::api::expressions::{MemeEdit, NewMeme};
use inlet_protocol::models::GuildFolder;
use inlet_protocol::{
    Client, Error, GroupDmEdit, ImageData, Instance, MemberProfileEdit, ProfileEdit, Token,
};
use serde_json::json;
use wiremock::matchers::{body_json, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(instance: &Instance, token: &str) -> Client {
    Client::new(instance.clone(), Token::new(token))
}

async fn mock(
    server: &MockServer,
    m: &str,
    p: &str,
    body: Option<serde_json::Value>,
    reply: ResponseTemplate,
) {
    let mut builder = Mock::given(method(m)).and(path(p));
    if let Some(b) = body {
        builder = builder.and(body_json(b));
    }
    builder.respond_with(reply).expect(1).mount(server).await;
}

fn ok(v: serde_json::Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(v)
}

fn no_content() -> ResponseTemplate {
    ResponseTemplate::new(204)
}

fn relationship(id: &str, kind: u64) -> serde_json::Value {
    json!({"id": id, "type": kind, "user": common::user_json(id, "bo"), "nickname": null})
}

#[tokio::test]
async fn relationships_and_notes() {
    let (server, instance) = common::instance().await;
    let base = "/api/v1/users/@me";
    mock(
        &server,
        "GET",
        &format!("{base}/relationships"),
        None,
        ok(json!([relationship("11", 1), relationship("12", 3), {"type": 1}])),
    )
    .await;
    mock(
        &server,
        "POST",
        &format!("{base}/relationships"),
        Some(json!({"username": "bo", "discriminator": "0042"})),
        ok(relationship("11", 4)),
    )
    .await;
    mock(
        &server,
        "POST",
        &format!("{base}/relationships/13"),
        Some(json!({})),
        ok(relationship("13", 4)),
    )
    .await;
    mock(
        &server,
        "PUT",
        &format!("{base}/relationships/12"),
        Some(json!({})),
        ok(relationship("12", 1)),
    )
    .await;
    mock(
        &server,
        "PUT",
        &format!("{base}/relationships/14"),
        Some(json!({"type": 2})),
        ok(relationship("14", 2)),
    )
    .await;
    mock(
        &server,
        "DELETE",
        &format!("{base}/relationships/14"),
        None,
        no_content(),
    )
    .await;
    mock(
        &server,
        "PATCH",
        &format!("{base}/relationships/11"),
        Some(json!({"nickname": null})),
        ok(relationship("11", 1)),
    )
    .await;
    mock(
        &server,
        "GET",
        &format!("{base}/notes"),
        None,
        ok(json!({"11": "climbing", "12": ""})),
    )
    .await;
    mock(
        &server,
        "GET",
        &format!("{base}/notes/11"),
        None,
        ok(json!({"note": "climbing"})),
    )
    .await;
    mock(
        &server,
        "GET",
        &format!("{base}/notes/12"),
        None,
        ResponseTemplate::new(404).set_body_json(json!({"code": "UNKNOWN_USER", "message": "no"})),
    )
    .await;
    mock(
        &server,
        "PUT",
        &format!("{base}/notes/11"),
        Some(json!({"note": "climbing"})),
        no_content(),
    )
    .await;
    mock(
        &server,
        "PUT",
        &format!("{base}/notes/12"),
        Some(json!({"note": null})),
        no_content(),
    )
    .await;
    let c = client(&instance, "tok-rel");

    let rels = c.relationships().await.unwrap();
    assert_eq!(rels.len(), 2);
    assert!(rels[0].is_friend() && rels[1].is_incoming_request());
    assert!(c
        .send_friend_request_by_tag("bo", Some("0042"))
        .await
        .unwrap()
        .is_outgoing_request());
    c.send_friend_request("13").await.unwrap();
    assert!(c.accept_friend_request("12").await.unwrap().is_friend());
    assert!(c.block_user("14").await.unwrap().is_blocked());
    c.remove_relationship("14").await.unwrap();
    c.set_friend_nickname("11", Some("")).await.unwrap();
    assert_eq!(c.notes().await.unwrap().len(), 1);
    assert_eq!(c.note("11").await.unwrap().as_deref(), Some("climbing"));
    assert_eq!(c.note("12").await.unwrap(), None);
    c.set_note("11", Some("climbing")).await.unwrap();
    c.set_note("12", None).await.unwrap();

    // Checked before any request.
    let long = "x".repeat(257);
    assert!(matches!(
        c.set_note("11", Some(&long)).await,
        Err(Error::InvalidInput(_))
    ));
    assert!(matches!(
        c.block_user("@me").await,
        Err(Error::InvalidInput(_))
    ));
    assert!(matches!(
        c.send_friend_request_by_tag("bo", Some("12345")).await,
        Err(Error::InvalidInput(_))
    ));
    assert!(matches!(
        c.send_friend_request_by_tag("", None).await,
        Err(Error::InvalidInput(_))
    ));
}

#[tokio::test]
async fn dms_profiles_folders_and_memes() {
    let (server, instance) = common::instance().await;
    let dm = json!({"id": "600", "type": 1, "recipients": [common::user_json("11", "bo")]});
    mock(
        &server,
        "POST",
        "/api/v1/users/@me/channels",
        Some(json!({"recipient_id": "11"})),
        ok(dm),
    )
    .await;
    mock(
        &server,
        "PUT",
        "/api/v1/users/@me/channels/600/pin",
        None,
        no_content(),
    )
    .await;
    mock(
        &server,
        "DELETE",
        "/api/v1/users/@me/channels/600/pin",
        None,
        no_content(),
    )
    .await;
    Mock::given(method("DELETE"))
        .and(path("/api/v1/channels/500"))
        .and(query_param("silent", "true"))
        .respond_with(no_content())
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/api/v1/channels/500/recipients/12"))
        .and(query_param("silent", "false"))
        .respond_with(no_content())
        .expect(1)
        .mount(&server)
        .await;
    mock(
        &server,
        "PATCH",
        "/api/v1/channels/500",
        Some(json!({"name": "Trip", "icon": "data:image/png;base64,AQID", "owner_id": "11"})),
        ok(json!({"id": "500", "type": 3, "name": "Trip"})),
    )
    .await;
    mock(
        &server,
        "PATCH",
        "/api/v1/users/@me",
        Some(json!({"global_name": "Ana", "bio": "hi\nthere", "accent_color": null})),
        ok(common::user_json("10", "ana")),
    )
    .await;
    mock(
        &server,
        "PATCH",
        "/api/v1/guilds/300/members/@me",
        Some(json!({"nick": "ana (mod)", "pronouns": "she/her"})),
        ok(json!({"user": common::user_json("10", "ana"), "nick": "ana (mod)", "roles": []})),
    )
    .await;
    mock(&server, "PATCH", "/api/v1/users/@me/settings",
         Some(json!({"guild_folders": [
             {"id": -1, "name": null, "color": 0, "flags": 0, "icon": "folder", "guild_ids": ["300"]},
             {"id": 4, "name": "Games", "color": 3908956, "flags": 1, "icon": "star",
              "guild_ids": ["301", "302"]}
         ]})),
         ok(json!({"status": "online", "guild_folders": [
             {"id": -1, "guild_ids": ["300"]}, {"id": 4, "name": "Games", "guild_ids": ["301", "302"]}
         ]}))).await;
    let meme = json!({"id": "70", "name": "parrot", "tags": ["party"],
                      "url": format!("{}/media/attachments/1/2/p.gif?ex=1", server.uri()),
                      "content_type": "image/gif", "size": 40213, "is_gifv": true});
    mock(
        &server,
        "GET",
        "/api/v1/users/@me/memes",
        None,
        ok(json!([meme.clone()])),
    )
    .await;
    mock(
        &server,
        "POST",
        "/api/v1/channels/200/messages/9/memes",
        Some(json!({"name": "parrot", "attachment_id": "77", "tags": ["party"]})),
        ok(meme.clone()),
    )
    .await;
    mock(
        &server,
        "PATCH",
        "/api/v1/users/@me/memes/70",
        Some(json!({"tags": ["party", "bird"]})),
        ok(meme.clone()),
    )
    .await;
    mock(
        &server,
        "DELETE",
        "/api/v1/users/@me/memes/70",
        None,
        no_content(),
    )
    .await;
    mock(
        &server,
        "POST",
        "/api/v1/channels/200/messages",
        Some(json!({"favorite_meme_id": "70", "nonce": "123"})),
        ok(common::message_json("9", "200", "")),
    )
    .await;
    mock(
        &server,
        "GET",
        "/api/v1/emojis/88/metadata",
        None,
        ok(
            json!({"id": "88", "guild_id": "300", "name": "blob", "animated": true,
                   "allow_cloning": false}),
        ),
    )
    .await;
    mock(
        &server,
        "GET",
        "/api/v1/stickers/89/source",
        None,
        ok(json!({"id": "300", "name": "Blobs", "icon": null, "features": ["DISCOVERABLE"]})),
    )
    .await;
    let c = client(&instance, "tok-social-routes");

    assert_eq!(c.open_dm("11").await.unwrap().id, "600");
    c.pin_dm("600").await.unwrap();
    c.unpin_dm("600").await.unwrap();
    c.close_dm("500", true).await.unwrap();
    c.remove_group_dm_recipient("500", "12", false)
        .await
        .unwrap();
    let edit = GroupDmEdit {
        name: Some(Some("Trip".into())),
        icon: Some(Some(ImageData::new("image/png", vec![1, 2, 3]).unwrap())),
        owner_id: Some("11".into()),
    };
    assert_eq!(
        c.edit_group_dm("500", &edit).await.unwrap().name.as_deref(),
        Some("Trip")
    );

    let me = c
        .edit_profile(&ProfileEdit {
            global_name: Some(Some("Ana".into())),
            bio: Some(Some("hi\nthere".into())),
            accent_color: Some(None),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(me.id, "10");
    let member = c
        .edit_member_profile(
            "300",
            &MemberProfileEdit {
                nick: Some(Some("ana (mod)".into())),
                pronouns: Some(Some("she/her".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(member.nick.as_deref(), Some("ana (mod)"));

    let mut games = GuildFolder::new(4, Some("Games"), vec!["301".into(), "302".into()]);
    games.color = Some(0x3B_A55C);
    games.flags = 1;
    games.icon = "star".into();
    let settings = c
        .set_guild_folders(&[GuildFolder::uncategorized(vec!["300".into()]), games])
        .await
        .unwrap();
    assert_eq!(settings.guild_folders.len(), 2);

    let memes = c.memes().await.unwrap();
    assert_eq!(memes[0].tags, ["party"]);
    let media = instance.media().unwrap();
    assert!(memes[0].proxied(&media, Default::default()).is_some());
    let new = NewMeme {
        name: "parrot".into(),
        attachment_id: Some("77".into()),
        tags: vec!["party".into()],
        ..Default::default()
    };
    c.save_meme_from_message("200", "9", &new).await.unwrap();
    let edit = MemeEdit {
        tags: Some(vec!["party".into(), "bird".into()]),
        ..Default::default()
    };
    c.edit_meme("70", &edit).await.unwrap();
    c.delete_meme("70").await.unwrap();
    c.send_meme("200", "70", Some("123")).await.unwrap();
    let meta = c.emoji_metadata("88").await.unwrap();
    assert_eq!((meta.guild_id.as_str(), meta.animated), ("300", true));
    assert_eq!(
        c.sticker_source("89").await.unwrap().features,
        ["DISCOVERABLE"]
    );

    // Checked before any request.
    assert!(matches!(
        c.open_dm("../1").await,
        Err(Error::InvalidInput(_))
    ));
    assert!(matches!(
        c.close_dm("@me", false).await,
        Err(Error::InvalidInput(_))
    ));
    assert!(matches!(
        c.edit_group_dm("500", &GroupDmEdit::default()).await,
        Err(Error::InvalidInput(_))
    ));
    assert!(matches!(
        c.set_guild_folders(&vec![GuildFolder::uncategorized(vec![]); 101])
            .await,
        Err(Error::InvalidInput(_))
    ));
    assert!(matches!(
        c.emoji_metadata("x").await,
        Err(Error::InvalidInput(_))
    ));
}
