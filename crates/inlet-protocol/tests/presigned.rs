//! Pre-signed attachment uploads against a mock instance and a mock storage.

mod common;

use bytes::Bytes;
use inlet_protocol::{Client, Error, OutgoingMessage, PresignFile, Token, UploadedFile};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, Request, ResponseTemplate};

fn file(id: u32, name: &str, size: u64) -> PresignFile {
    PresignFile {
        id,
        filename: name.into(),
        file_size: size,
        content_type: "image/png".into(),
    }
}

#[tokio::test]
async fn a_file_goes_to_storage_without_the_token_and_the_message_names_its_key() {
    let (server, instance) = common::instance().await;
    let storage = format!("{}/storage/key-1?X-Amz-Signature=secret", server.uri());
    Mock::given(method("POST"))
        .and(path("/api/v1/channels/301/attachments"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"attachments": [{
                "upload_mode": "singlepart", "id": 0, "filename": "cat.png",
                "upload_filename": "key-1", "upload_url": storage,
                "file_size": 3, "content_type": "image/png"
            }]})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/storage/key-1"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/channels/301/messages"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(common::message_json("900", "301", "look")),
        )
        .expect(1)
        .mount(&server)
        .await;

    let client = Client::new(instance, Token::new("tok-presigned-1"));
    // A full local path is never sent.
    let plans = client
        .presign_attachments("301", &[file(0, "C:\\Users\\me\\Pictures\\cat.png", 3)])
        .await
        .unwrap();
    assert_eq!(plans.len(), 1);
    let url = plans[0].part_url(0).unwrap();
    assert!(!format!("{url:?}").contains("secret"));
    client
        .put_upload(url, Bytes::from_static(b"abc"), "image/png", None)
        .await
        .unwrap();
    let uploaded = UploadedFile::of(&plans[0], "cat.png", Some("a cat".into()), true);
    let message = OutgoingMessage::text("look").with_nonce("777");
    client
        .send_with_uploads("301", &message, &[uploaded])
        .await
        .unwrap();

    let requests: Vec<Request> = server.received_requests().await.unwrap();
    let presign = requests
        .iter()
        .find(|r| r.url.path() == "/api/v1/channels/301/attachments")
        .unwrap();
    let asked: Value = serde_json::from_slice(&presign.body).unwrap();
    assert_eq!(asked["attachments"][0]["filename"], "cat.png");
    assert_eq!(asked["attachments"][0]["file_size"], 3);
    let put = requests
        .iter()
        .find(|r| r.method.as_str() == "PUT")
        .unwrap();
    assert_eq!(put.body, b"abc");
    assert!(
        put.headers.get("authorization").is_none(),
        "no token to storage"
    );
    assert!(
        put.headers.get("x-fluxer-client-installation-id").is_none(),
        "no instance headers to storage"
    );
    let post = requests
        .iter()
        .find(|r| r.url.path() == "/api/v1/channels/301/messages")
        .unwrap();
    let sent: Value = serde_json::from_slice(&post.body).unwrap();
    assert_eq!(sent["nonce"], "777");
    assert_eq!(
        sent["attachments"],
        json!([{"id": 0, "filename": "cat.png", "upload_filename": "key-1", "file_size": 3,
                "content_type": "image/png", "description": "a cat", "flags": 8}])
    );
}

#[tokio::test]
async fn storage_redirects_are_not_followed_and_multipart_is_completed() {
    let (server, instance) = common::instance().await;
    Mock::given(method("PUT"))
        .and(path("/storage/moved"))
        .respond_with(
            ResponseTemplate::new(307)
                .insert_header("Location", format!("{}/elsewhere", server.uri())),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/channels/301/attachments/complete"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"uploads": [{"upload_filename": "k"}]})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let client = Client::new(instance, Token::new("tok-presigned-2"));
    let url = inlet_protocol::UploadUrl::parse(&format!("{}/storage/moved", server.uri())).unwrap();
    let err = client
        .put_upload(&url, Bytes::from_static(b"x"), "image/png", None)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Decode(_)), "{err:?}");
    let requests = server.received_requests().await.unwrap();
    assert!(
        !requests.iter().any(|r| r.url.path() == "/elsewhere"),
        "the redirect was not followed"
    );
    client
        .complete_multipart_uploads("301", &[("k".into(), "u".into())])
        .await
        .unwrap();
    let complete = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.url.path().ends_with("/complete"))
        .unwrap();
    let body: Value = serde_json::from_slice(&complete.body).unwrap();
    assert_eq!(
        body,
        json!({"uploads": [{"upload_filename": "k", "upload_id": "u"}]})
    );
}

#[tokio::test]
async fn a_plan_over_the_limits_is_refused_before_any_request() {
    let (server, instance) = common::instance().await;
    let client = Client::new(instance, Token::new("tok-presigned-3"));
    let too_big = client.upload_limits().max_file_size + 1;
    let err = client
        .presign_attachments("301", &[file(0, "huge.bin", too_big)])
        .await
        .unwrap_err();
    assert!(matches!(err, Error::TooLarge { .. }), "{err:?}");
    let many: Vec<PresignFile> = (0..11).map(|i| file(i, "a.png", 1)).collect();
    assert!(client.presign_attachments("301", &many).await.is_err());
    let requests = server.received_requests().await.unwrap();
    assert!(
        requests
            .iter()
            .all(|r| r.url.path() == "/.well-known/fluxer"),
        "nothing but discovery was asked"
    );
}
