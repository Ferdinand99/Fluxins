//! Sign-in flows against a mock instance.

mod common;

use std::time::Duration;

use inlet_protocol::{
    altcha, poll_ip_authorization, sign_in_password, sign_in_totp, Handoff, HandoffStatus, IpAuthorization, MfaMethod,
    Session, SignInResult,
};
use serde_json::json;
use wiremock::matchers::{body_json, header, header_exists, method, path, query_param};
use wiremock::{Mock, Request, Respond, ResponseTemplate};

fn token_response(token: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "token": token, "user_id": "10", "user": common::user_json("10", "ana")
    }))
}

#[tokio::test]
async fn password_sign_in_returns_token() {
    let (server, instance) = common::instance().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/auth/login"))
        .and(body_json(json!({"login": "ana@example.org", "password": "correct horse"})))
        .and(header_exists("X-Fluxer-Client-Properties"))
        .respond_with(token_response("tok-pw"))
        .expect(1)
        .mount(&server)
        .await;
    match sign_in_password(&instance, "ana@example.org", "correct horse").await.unwrap() {
        SignInResult::Ok(s) => {
            assert_eq!(s.token.expose_secret(), "tok-pw");
            assert_eq!(s.user_id, "10");
            assert_eq!(s.user.unwrap().username, "ana");
            assert_eq!(format!("{:?}", s.token), "Token(<redacted>)");
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn wrong_password_is_an_api_error() {
    let (server, instance) = common::instance().await;
    Mock::given(path("/api/v1/auth/login"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "code": "INVALID_FORM_BODY", "message": "Input Validation Error",
            "errors": [{"path": "password", "code": "INVALID_EMAIL_OR_PASSWORD", "message": "Invalid"}]
        })))
        .mount(&server)
        .await;
    match sign_in_password(&instance, "ana@example.org", "nope nope").await.unwrap() {
        SignInResult::Error(e) => assert_eq!(e.field_code(), Some("INVALID_EMAIL_OR_PASSWORD")),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn mfa_then_totp() {
    let (server, instance) = common::instance().await;
    Mock::given(path("/api/v1/auth/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "mfa": true, "ticket": "mfa-ticket-1", "allowed_methods": ["totp", "webauthn", "backup_codes"],
            "totp": true, "webauthn": true, "backup_codes": true
        })))
        .mount(&server)
        .await;
    Mock::given(path("/api/v1/auth/login/mfa/totp"))
        .and(body_json(json!({"ticket": "mfa-ticket-1", "code": "123456"})))
        .respond_with(token_response("tok-mfa"))
        .expect(1)
        .mount(&server)
        .await;
    let (ticket, methods) = match sign_in_password(&instance, "ana@example.org", "pw-pw-pw-pw").await.unwrap() {
        SignInResult::NeedsMfa { ticket, methods } => (ticket, methods),
        other => panic!("{other:?}"),
    };
    assert_eq!(methods, vec![MfaMethod::Totp, MfaMethod::Webauthn, MfaMethod::BackupCodes]);
    assert_eq!(format!("{ticket:?}"), "Ticket(<redacted>)");
    match sign_in_totp(&instance, &ticket, " 123456 ").await.unwrap() {
        SignInResult::Ok(s) => assert_eq!(s.token.expose_secret(), "tok-mfa"),
        other => panic!("{other:?}"),
    }
}

/// Answers like Fluxer: 400 CAPTCHA_REQUIRED without a token, checks the token otherwise.
struct CaptchaGate {
    challenge: serde_json::Value,
}

impl Respond for CaptchaGate {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        match req.headers.get("X-Captcha-Token").and_then(|v| v.to_str().ok()) {
            None => ResponseTemplate::new(400).set_body_json(json!({
                "code": "CAPTCHA_REQUIRED", "message": "Captcha required",
                "captcha_provider": "altcha", "altcha_challenge": self.challenge
            })),
            Some(t) if altcha::verify_test_token(t) => token_response("tok-captcha"),
            Some(_) => ResponseTemplate::new(400).set_body_json(json!({
                "code": "INVALID_CAPTCHA", "message": "bad", "captcha_provider": "altcha",
                "altcha_challenge": self.challenge
            })),
        }
    }
}

#[tokio::test]
async fn captcha_is_solved_and_retried() {
    let (server, instance) = common::instance().await;
    Mock::given(path("/api/v1/auth/login"))
        .respond_with(CaptchaGate {
            challenge: altcha::make_test_challenge(700, 100),
        })
        .expect(2)
        .mount(&server)
        .await;
    match sign_in_password(&instance, "ana@example.org", "correct horse").await.unwrap() {
        SignInResult::Ok(s) => assert_eq!(s.token.expose_secret(), "tok-captcha"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn rejected_captcha_reports_captcha() {
    let (server, instance) = common::instance().await;
    // The server always wants a new captcha: the client gives up after two solves.
    Mock::given(path("/api/v1/auth/login"))
        .respond_with(
            ResponseTemplate::new(400).set_body_json(json!({
                "code": "CAPTCHA_REQUIRED", "message": "Captcha required",
                "captcha_provider": "altcha", "altcha_challenge": altcha::make_test_challenge(3, 10)
            })),
        )
        .expect(3)
        .mount(&server)
        .await;
    let result = sign_in_password(&instance, "ana@example.org", "correct horse").await.unwrap();
    assert!(matches!(result, SignInResult::Captcha), "{result:?}");
}

#[tokio::test]
async fn new_ip_authorization_polls_to_token() {
    let (server, instance) = common::instance().await;
    Mock::given(path("/api/v1/auth/login"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "code": "IP_AUTHORIZATION_REQUIRED", "message": "Check your email", "ticket": "ip-ticket", "email": "a***@example.org"
        })))
        .mount(&server)
        .await;
    Mock::given(path("/api/v1/auth/ip-authorization/poll"))
        .and(query_param("ticket", "ip-ticket"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"completed": false})))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(path("/api/v1/auth/ip-authorization/poll"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "completed": true, "token": "tok-ip", "user_id": "10", "user": common::user_json("10", "ana")
        })))
        .with_priority(2)
        .mount(&server)
        .await;
    let ticket = match sign_in_password(&instance, "ana@example.org", "correct horse").await.unwrap() {
        SignInResult::IpAuthorizationRequired { ticket } => ticket,
        other => panic!("{other:?}"),
    };
    assert!(matches!(poll_ip_authorization(&instance, &ticket).await.unwrap(), IpAuthorization::Pending));
    match poll_ip_authorization(&instance, &ticket).await.unwrap() {
        IpAuthorization::Completed(s) => assert_eq!(s.token.expose_secret(), "tok-ip"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn browser_handoff_flow() {
    let (server, instance) = common::instance().await;
    let web_origin = server.uri();
    Mock::given(method("POST"))
        .and(path("/api/v1/auth/handoff/initiate"))
        .and(header("Origin", web_origin.as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": "ABCDEFGHJKLM", "expires_at": "2026-11-10T09:05:00Z",
            "poll_secret": "secret_poll-1", "return_method": "code"
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/auth/handoff/ABCDEFGHJKLM/status"))
        .and(body_json(json!({"poll_secret": "secret_poll-1"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "pending"})))
        .up_to_n_times(2)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/auth/handoff/ABCDEFGHJKLM/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "completed", "token": "tok-handoff", "user_id": "10", "user": common::user_json("10", "ana")
        })))
        .with_priority(2)
        .mount(&server)
        .await;

    let handoff = Handoff::start(&instance).await.unwrap();
    assert_eq!(handoff.display_code(), "ABCDEF-GHJKLM");
    assert_eq!(handoff.browser_url(), format!("{}/login?handoff=1", server.uri()));
    assert!(!format!("{handoff:?}").contains("secret_poll-1"));
    let status = handoff.wait(Duration::from_millis(1)).await.unwrap();
    let signed_in = match status {
        HandoffStatus::Completed(s) => s,
        other => panic!("{other:?}"),
    };
    assert_eq!(signed_in.token.expose_secret(), "tok-handoff");

    // The token makes a working session.
    Mock::given(path("/api/v1/users/@me"))
        .and(header("Authorization", "tok-handoff"))
        .respond_with(ResponseTemplate::new(200).set_body_json(common::user_json("10", "ana")))
        .mount(&server)
        .await;
    let session = Session::from_signed_in(instance, signed_in);
    assert_eq!(session.client().me().await.unwrap().id, "10");
}

#[tokio::test]
async fn handoff_denied_and_cancel() {
    let (server, instance) = common::instance().await;
    Mock::given(path("/api/v1/auth/handoff/initiate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": "QWERTYUIOPAS", "expires_at": "2026-11-10T09:05:00Z", "poll_secret": "s2"
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/auth/handoff/QWERTYUIOPAS/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "denied"})))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/api/v1/auth/handoff/QWERTYUIOPAS"))
        .and(body_json(json!({"poll_secret": "s2"})))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    let handoff = Handoff::start(&instance).await.unwrap();
    assert!(matches!(handoff.poll().await.unwrap(), HandoffStatus::Denied));
    handoff.cancel().await.unwrap();
}
