// SPDX-License-Identifier: AGPL-3.0-or-later
//! The relationship and group DM budget, and the CAPTCHA and rate-limit answers, against
//! a mock instance.

use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use super::*;
use crate::backend::accounts::testkit::{mock_instance, requests};

fn key(user: &str) -> AccountKey {
    AccountKey::new("https://mock.test", user).unwrap()
}

#[test]
fn the_worst_clicking_stays_within_the_budget() {
    let t0 = Instant::now();
    let mut g = Gate::default();
    // 100 clicks on 100 people within 5 s: at most 10 calls start in any 10 s.
    let mut started = Vec::new();
    for i in 0..100u64 {
        let now = t0 + Duration::from_millis(50 * i);
        if g.admit(&format!("user:{i}"), Budget::Write, now).is_ok() {
            started.push(now);
            g.finish(&format!("user:{i}"));
        }
    }
    assert_eq!(started.len(), WRITES_PER_WINDOW);
    // The same person twice while the first call runs: one call.
    let later = t0 + WRITE_WINDOW * 2;
    assert!(g.admit("user:1", Budget::Write, later).is_ok());
    assert_eq!(g.admit("user:1", Budget::Write, later), Err(Refusal::Busy));
    g.finish("user:1");
    // Group DM creates: three an hour, whatever the clicks.
    let mut creates = 0;
    for i in 0..60u64 {
        let now = later + Duration::from_secs(60 * i);
        if g.admit("group-dm", Budget::GroupCreate, now).is_ok() {
            creates += 1;
            g.finish("group-dm");
        }
    }
    assert_eq!(creates, GROUP_CALLS_PER_HOUR);
    // Adds have their own three.
    let now = later + Duration::from_secs(10);
    for _ in 0..GROUP_CALLS_PER_HOUR {
        assert!(g.admit("group:5", Budget::GroupAdd, now).is_ok());
        g.finish("group:5");
    }
    match g.admit("group:5", Budget::GroupAdd, now) {
        Err(Refusal::GroupLimit { wait }) => assert!(wait > Duration::from_secs(3500)),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn group_dm_create_handles_captcha_result() {
    let (server, instance) = mock_instance("ws://127.0.0.1:9", &[]).await;
    // A challenge Inlet cannot solve (an algorithm it does not know).
    Mock::given(method("POST"))
        .and(path("/api/v1/users/@me/channels"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "code": "CAPTCHA_REQUIRED", "message": "Captcha required",
            "captcha_provider": "altcha",
            "altcha_challenge": {"parameters": {
                "algorithm": "ARGON2ID", "cost": 1, "keyLength": 32, "keyPrefix": "a1b2c3",
                "nonce": "00112233445566778899aabbccddeeff",
                "salt": "00112233445566778899aabbccddeeff", "expiresAt": 4_102_444_800u64
            }, "signature": "x"}
        })))
        .expect(1)
        .mount(&server)
        .await;
    let client = p::Client::new(instance, p::Token::new("tok-captcha"));
    let (internal, _irx) = tokio::sync::mpsc::unbounded_channel();
    let account = key("1");
    let cmd = Cmd::CreateGroupDm {
        recipients: vec!["11".into(), "12".into()],
    };
    let ev = run(&client, &internal, 1, &account, &cmd.key(), cmd.clone()).await;
    assert_eq!(
        ev,
        Ev::GroupDm {
            key: "group-dm".into(),
            result: GroupDm::Captcha
        }
    );
    // One request, and nothing is tried again on its own.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let posts = requests(&server)
        .await
        .into_iter()
        .filter(|r| r.method.as_str() == "POST")
        .count();
    assert_eq!(posts, 1);

    // Over Fluxer's hourly limit: the real wait, and no more group calls until it ends.
    let (server, instance) = mock_instance("ws://127.0.0.1:9", &[]).await;
    Mock::given(method("POST"))
        .and(path("/api/v1/users/@me/channels"))
        .respond_with(ResponseTemplate::new(429).set_body_json(json!({
            "code": "RATE_LIMITED", "message": "slow", "global": false, "retry_after": 3000
        })))
        .expect(1)
        .mount(&server)
        .await;
    let client = p::Client::new(instance, p::Token::new("tok-limit"));
    let account = key("2");
    let ev = run(&client, &internal, 1, &account, &cmd.key(), cmd.clone()).await;
    assert_eq!(
        ev,
        Ev::GroupDm {
            key: "group-dm".into(),
            result: GroupDm::RateLimited {
                retry_after: Duration::from_secs(3000)
            }
        }
    );
    assert_eq!(
        wait_text(Duration::from_secs(3000)),
        t!("social-wait-minutes", count = 50)
    );
    match with_gate(&account, |g| {
        g.admit("group-dm", Budget::GroupCreate, Instant::now())
    }) {
        Err(Refusal::GroupLimit { wait }) => assert!(wait > Duration::from_secs(2900)),
        other => panic!("{other:?}"),
    }
    // Other writes still go.
    assert!(with_gate(&account, |g| g.admit(
        "user:5",
        Budget::Write,
        Instant::now()
    ))
    .is_ok());
}
