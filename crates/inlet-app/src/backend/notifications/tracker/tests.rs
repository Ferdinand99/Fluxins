use std::time::Duration;

use futures::channel::mpsc::{unbounded, UnboundedReceiver};
use inlet_protocol::models::FLUXER_EPOCH_MS;
use inlet_protocol::permissions::{READ_MESSAGE_HISTORY, SEND_MESSAGES, VIEW_CHANNEL};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use super::*;
use crate::backend::Event;

const ME: &str = "10";
const OTHER: &str = "20";
const GUILD: &str = "100";
const GENERAL: &str = "301";
const RANDOM: &str = "302";
const DM: &str = "900";
/// 2026-01-01T00:00:00Z, when the account joined the guild.
const JOINED_MS: u64 = 1_767_225_600_000;

/// A message ID `secs` after the join time.
fn mid(secs: u64) -> u64 {
    ((JOINED_MS + secs * 1000 - FLUXER_EPOCH_MS) << 22) + 1
}

fn ready(read_states: Value, settings: Value) -> Value {
    json!({
        "session_id": "s1",
        "user": {"id": ME, "username": "me", "mfa_enabled": false},
        "guilds": [{
            "id": GUILD,
            "properties": {"id": GUILD, "name": "Hedgerow", "owner_id": "1", "mfa_level": 0,
                           "default_message_notifications": 0},
            "roles": [{"id": GUILD, "name": "@everyone", "position": 0,
                       "permissions": (VIEW_CHANNEL | SEND_MESSAGES | READ_MESSAGE_HISTORY).to_string()}],
            "channels": [
                {"id": GENERAL, "type": 0, "position": 1, "name": "general",
                 "last_message_id": mid(10).to_string()},
                {"id": RANDOM, "type": 0, "position": 2, "name": "random",
                 "last_message_id": mid(20).to_string()},
            ],
            "members": [{"user": {"id": ME, "username": "me"}, "roles": [],
                         "joined_at": "2026-01-01T00:00:00Z"}],
            "member_count": 3,
        }],
        "private_channels": [{"id": DM, "type": 1, "recipients": [{"id": OTHER, "username": "ana"}],
                              "last_message_id": mid(30).to_string()}],
        "users": [{"id": OTHER, "username": "ana"}],
        "read_states": read_states,
        "user_guild_settings": settings,
        "presences": [],
    })
}

fn message(id: u64, channel: &str, author: &str, extra: Value) -> p::Dispatch {
    let mut m = json!({
        "id": id.to_string(), "channel_id": channel, "type": 0,
        "author": {"id": author, "username": format!("u{author}")},
        "content": "hi", "timestamp": "2026-01-01T01:00:00Z", "mentions": [],
    });
    if let (Value::Object(m), Value::Object(e)) = (&mut m, extra) {
        m.extend(e);
    }
    p::Dispatch::parse("MESSAGE_CREATE", m)
}

fn discovery(base: &str) -> Value {
    json!({
        "api_code_version": 1,
        "endpoints": {
            "api": format!("{base}/api"), "api_client": format!("{base}/api"),
            "api_public": format!("{base}/api"), "gateway": "ws://127.0.0.1:1/gateway",
            "media": format!("{base}/media"), "webapp": base, "invite": format!("{base}/invite")
        },
        "captcha": {"provider": "altcha"},
        "features": {"account_identity": "email", "voice_enabled": true},
        "limits": {"version": 2, "rules": [{"id": "default", "overrides": {"max_message_length": 4000}}]},
        "app_public": {"branding": {"product_name": "Mock Social"}},
        "sso": {"enabled": false},
        "domain_migration": {"enabled": false}
    })
}

struct Fixture {
    server: MockServer,
    client: p::Client,
    tracker: ReadTracker,
    events: UnboundedReceiver<Event>,
    base: Instant,
}

impl Fixture {
    async fn new(read_states: Value, settings: Value) -> Fixture {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.well-known/fluxer"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("ETag", "\"v1\"")
                    .set_body_json(discovery(&server.uri())),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/read-states/ack"))
            .respond_with(|req: &Request| {
                let body: Value = serde_json::from_slice(&req.body).unwrap();
                let states: Vec<Value> = body["read_states"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|e| {
                        json!({"id": e["channel_id"], "mention_count": 0,
                                    "last_message_id": e["message_id"]})
                    })
                    .collect();
                ResponseTemplate::new(200).set_body_json(json!({ "read_states": states }))
            })
            .mount(&server)
            .await;
        let info = p::ClientInfo {
            installation_id: "00000000-0000-4000-a000-000000000000".into(),
            ..p::ClientInfo::default()
        };
        let instance = p::Instance::discover_with(&server.uri(), info)
            .await
            .expect("discovery");
        let client = p::Client::new(instance, p::Token::new("tok-read"));
        let (tx, events) = unbounded();
        let mut tracker = ReadTracker::new(client.clone(), Ui::new(tx));
        let base = Instant::now();
        let ready: p::Ready = serde_json::from_value(ready(read_states, settings)).unwrap();
        tracker.on_dispatch(
            &p::Dispatch::Ready(Box::new(ready)),
            Fixture::at_base(base, 0),
        );
        Fixture {
            server,
            client,
            tracker,
            events,
            base,
        }
    }

    fn at_base(base: Instant, ms: u64) -> Now {
        Now {
            at: base + Duration::from_millis(ms),
            ms: JOINED_MS as i64 + 3_600_000 + ms as i64,
        }
    }

    /// The time `ms` after the READY.
    fn at(&self, ms: u64) -> Now {
        Fixture::at_base(self.base, ms)
    }

    fn drain(&mut self) -> Vec<Ev> {
        let mut out = Vec::new();
        while let Ok(ev) = self.events.try_recv() {
            if let Event::Notifications(ev) = ev {
                out.push(ev);
            }
        }
        out
    }

    async fn requests(&self, m: &str, p: &str) -> Vec<Request> {
        self.server
            .received_requests()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|r| r.method.as_str() == m && r.url.path() == p)
            .collect()
    }
}

fn badges(events: &[Ev]) -> Vec<&BadgeUpdate> {
    events
        .iter()
        .filter_map(|e| match e {
            Ev::Badges(b) => Some(b),
            _ => None,
        })
        .collect()
}

fn channel_badge_of<'a>(update: &'a BadgeUpdate, id: &str) -> Option<&'a Badge> {
    update
        .channels
        .iter()
        .find(|(c, _)| c == id)
        .map(|(_, b)| b)
}

fn guild_badge_of<'a>(update: &'a BadgeUpdate, id: Option<&str>) -> Option<&'a Badge> {
    update
        .guilds
        .iter()
        .find(|(g, _)| g.as_deref() == id)
        .map(|(_, b)| b)
}

#[tokio::test]
async fn ready_shows_what_the_server_says_is_unread() {
    let mut f = Fixture::new(
        json!([
            {"id": GENERAL, "mention_count": 2, "last_message_id": mid(5).to_string()},
            {"id": RANDOM, "mention_count": 0, "last_message_id": mid(20).to_string()},
        ]),
        json!([]),
    )
    .await;
    let events = f.drain();
    let all = badges(&events);
    assert_eq!(all.len(), 1);
    let first = all[0];
    assert!(first.reset);
    assert_eq!(
        channel_badge_of(first, GENERAL),
        Some(&Badge {
            unread: true,
            mentions: 2,
            muted: false,
            dimmed: false
        })
    );
    // Read to the end: no badge at all.
    assert_eq!(channel_badge_of(first, RANDOM), None);
    assert_eq!(
        guild_badge_of(first, Some(GUILD)),
        Some(&Badge {
            unread: true,
            mentions: 2,
            muted: false,
            dimmed: false
        })
    );
    // The direct message has no ack: unread.
    assert!(channel_badge_of(first, DM).is_some_and(|b| b.unread));
    assert!(guild_badge_of(first, None).is_some_and(|b| b.unread));
}

#[tokio::test]
async fn only_changed_badges_reach_the_ui() {
    let mut f = Fixture::new(json!([]), json!([])).await;
    f.drain();
    // A mention in an unread channel changes the count.
    f.tracker.on_dispatch(
        &message(
            mid(40),
            GENERAL,
            OTHER,
            json!({"mentions": [{"id": ME, "username": "me"}]}),
        ),
        f.at(100),
    );
    let events = f.drain();
    let update = badges(&events)[0];
    assert!(!update.reset);
    assert_eq!(channel_badge_of(update, GENERAL).unwrap().mentions, 1);
    assert_eq!(guild_badge_of(update, Some(GUILD)).unwrap().mentions, 1);

    // Another plain message changes nothing the UI shows: no event.
    f.tracker
        .on_dispatch(&message(mid(41), GENERAL, OTHER, json!({})), f.at(200));
    assert!(badges(&f.drain()).is_empty());

    // Typing and presence are not even applied.
    f.tracker.on_dispatch(
        &p::Dispatch::parse(
            "TYPING_START",
            json!({"channel_id": GENERAL, "user_id": OTHER}),
        ),
        f.at(300),
    );
    assert!(f.drain().is_empty());

    // The server's ack from another session clears it.
    f.tracker.on_dispatch(
        &p::Dispatch::parse(
            "MESSAGE_ACK",
            json!({"channel_id": GENERAL, "message_id": mid(41).to_string(), "mention_count": 0}),
        ),
        f.at(400),
    );
    let events = f.drain();
    let update = badges(&events)[0];
    assert_eq!(channel_badge_of(update, GENERAL), Some(&Badge::default()));
}

#[tokio::test]
async fn mark_all_read_is_one_bulk_call() {
    let mut f = Fixture::new(json!([]), json!([])).await;
    f.drain();
    // Two channels of the guild and a direct message are unread.
    f.tracker.mark_all_read(f.at(10));
    let events = f.drain();
    let update = badges(&events)[0];
    assert_eq!(channel_badge_of(update, GENERAL), Some(&Badge::default()));
    assert_eq!(channel_badge_of(update, RANDOM), Some(&Badge::default()));
    assert_eq!(channel_badge_of(update, DM), Some(&Badge::default()));
    assert_eq!(guild_badge_of(update, Some(GUILD)), Some(&Badge::default()));

    // The protocol sends all three in one request.
    f.client.flush_acks().await.unwrap();
    let sent = f.requests("POST", "/api/v1/read-states/ack").await;
    assert_eq!(sent.len(), 1, "one bulk call");
    let body: Value = serde_json::from_slice(&sent[0].body).unwrap();
    let mut entries: Vec<(String, String)> = body["read_states"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["channel_id"].as_str().unwrap().to_string(),
                e["message_id"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    entries.sort();
    assert_eq!(
        entries,
        vec![
            (GENERAL.to_string(), mid(10).to_string()),
            (RANDOM.to_string(), mid(20).to_string()),
            (DM.to_string(), mid(30).to_string()),
        ]
    );

    // Everything is read now: pressing it again sends nothing.
    f.tracker.mark_all_read(f.at(2_000));
    f.client.flush_acks().await.unwrap();
    assert_eq!(f.requests("POST", "/api/v1/read-states/ack").await.len(), 1);
}

#[tokio::test]
async fn mark_guild_read_leaves_other_guilds_and_direct_messages_alone() {
    let mut f = Fixture::new(json!([]), json!([])).await;
    f.drain();
    f.tracker.mark_guild_read(GuildId::parse(GUILD), f.at(10));
    f.client.flush_acks().await.unwrap();
    let sent = f.requests("POST", "/api/v1/read-states/ack").await;
    let body: Value = serde_json::from_slice(&sent[0].body).unwrap();
    assert_eq!(body["read_states"].as_array().unwrap().len(), 2);
    let events = f.drain();
    let update = badges(&events)[0];
    assert!(
        channel_badge_of(update, DM).is_none(),
        "the DM is untouched"
    );
}

#[tokio::test]
async fn an_open_channel_is_acknowledged_once_for_its_last_message() {
    let mut f = Fixture::new(json!([]), json!([])).await;
    f.drain();
    let general = ChannelId::parse(GENERAL);

    // Opening reads what the channel holds, and says where the person had read to.
    f.tracker.watch(general, f.at(100));
    let events = f.drain();
    assert!(events.iter().any(|e| matches!(
        e,
        Ev::Opened { channel_id, last_read: None, .. } if channel_id == GENERAL
    )));
    let update = badges(&events)[0];
    assert_eq!(channel_badge_of(update, GENERAL), Some(&Badge::default()));

    // Five messages arrive; they are read as they come, so no badge shows.
    for i in 0..5 {
        f.tracker.on_dispatch(
            &message(mid(40 + i), GENERAL, OTHER, json!({})),
            f.at(200 + i * 50),
        );
    }
    assert!(badges(&f.drain()).is_empty());
    // And one in a channel nobody is looking at: unread, never acknowledged.
    f.tracker
        .on_dispatch(&message(mid(50), RANDOM, OTHER, json!({})), f.at(500));

    f.tracker.tick(f.at(600));
    f.client.flush_acks().await.unwrap();
    assert!(
        f.requests("POST", "/api/v1/read-states/ack")
            .await
            .is_empty(),
        "still inside the quiet time"
    );
    f.tracker.tick(f.at(1_300));
    f.client.flush_acks().await.unwrap();
    let sent = f.requests("POST", "/api/v1/read-states/ack").await;
    assert_eq!(sent.len(), 1);
    let body: Value = serde_json::from_slice(&sent[0].body).unwrap();
    assert_eq!(
        body["read_states"],
        json!([{"channel_id": GENERAL, "message_id": mid(44).to_string()}])
    );

    // Nothing more, however long it runs.
    for s in 2..30 {
        f.tracker.tick(f.at(s * 1_000));
    }
    f.client.flush_acks().await.unwrap();
    assert_eq!(f.requests("POST", "/api/v1/read-states/ack").await.len(), 1);

    // Leaving the window (not watched): the next message stays unread.
    f.tracker.watch(None, f.at(31_000));
    f.tracker
        .on_dispatch(&message(mid(60), GENERAL, OTHER, json!({})), f.at(31_100));
    let events = f.drain();
    let update = badges(&events)[0];
    assert!(channel_badge_of(update, GENERAL).is_some_and(|b| b.unread));
}

#[tokio::test]
async fn a_channel_nobody_looked_at_is_never_acknowledged() {
    let mut f = Fixture::new(json!([]), json!([])).await;
    for s in 0..20 {
        f.tracker.tick(f.at(s * 1_000));
    }
    f.client.flush_acks().await.unwrap();
    assert!(f
        .requests("POST", "/api/v1/read-states/ack")
        .await
        .is_empty());
}

#[tokio::test]
async fn settings_changes_show_at_once_and_go_out_as_one_patch() {
    let mut f = Fixture::new(
        json!([]),
        json!([{"guild_id": GUILD, "channel_overrides": [
            {"channel_id": GENERAL, "collapsed": true, "message_notifications": 3, "muted": false}]}]),
    )
    .await;
    Mock::given(method("PATCH"))
        .and(path(format!("/api/v1/users/@me/guilds/{GUILD}/settings")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"guild_id": GUILD})))
        .mount(&f.server)
        .await;
    f.drain();
    let channel = |id: &str| Target::Channel {
        guild_id: Some(GUILD.to_string()),
        channel_id: id.to_string(),
    };

    f.tracker.change(
        channel(RANDOM),
        Change::Mute {
            for_ms: Some(900_000),
        },
        f.at(100),
    );
    // The row is dimmed straight away.
    let events = f.drain();
    let update = badges(&events)[0];
    assert_eq!(
        channel_badge_of(update, RANDOM),
        Some(&Badge {
            unread: true,
            mentions: 0,
            muted: true,
            dimmed: true
        })
    );
    f.tracker
        .change(channel(RANDOM), Change::Level(1), f.at(150));
    f.tracker.change(
        Target::Guild(Some(GUILD.to_string())),
        Change::SuppressEveryone(true),
        f.at(200),
    );
    // A change for a channel the account does not have is refused.
    f.tracker
        .change(channel("999"), Change::Level(2), f.at(220));
    f.tracker.change(
        Target::Channel {
            guild_id: Some("555".to_string()),
            channel_id: GENERAL.to_string(),
        },
        Change::Level(2),
        f.at(230),
    );

    f.tracker.tick(f.at(400));
    tokio::time::sleep(Duration::from_millis(100)).await;
    let path = format!("/api/v1/users/@me/guilds/{GUILD}/settings");
    assert!(f.requests("PATCH", &path).await.is_empty(), "merging");
    f.tracker.tick(f.at(700));
    for _ in 0..40 {
        if !f.requests("PATCH", &path).await.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let sent = f.requests("PATCH", &path).await;
    assert_eq!(sent.len(), 1, "one request for four changes");
    let body: Value = serde_json::from_slice(&sent[0].body).unwrap();
    assert_eq!(body["suppress_everyone"], json!(true));
    let overrides = &body["channel_overrides"];
    assert_eq!(overrides[RANDOM]["muted"], json!(true));
    assert_eq!(overrides[RANDOM]["message_notifications"], json!(1));
    assert_eq!(
        overrides[RANDOM]["mute_config"]["selected_time_window"],
        json!(900_000)
    );
    // The other override travels whole, folded state included.
    assert_eq!(overrides[GENERAL]["collapsed"], json!(true));
}

#[tokio::test]
async fn a_timed_mute_ends_with_the_clock() {
    let mut f = Fixture::new(json!([]), json!([])).await;
    f.drain();
    f.tracker.change(
        Target::Guild(Some(GUILD.to_string())),
        Change::Mute {
            for_ms: Some(60_000),
        },
        f.at(0),
    );
    let events = f.drain();
    // Muted: no pill on the rail.
    assert!(guild_badge_of(badges(&events)[0], Some(GUILD)).is_some_and(|b| b.muted && !b.unread));
    f.tracker.tick(f.at(30_000));
    assert!(badges(&f.drain()).is_empty());
    f.tracker.tick(f.at(61_000));
    let events = f.drain();
    let update = badges(&events)[0];
    assert!(guild_badge_of(update, Some(GUILD)).is_some_and(|b| b.unread && !b.muted));
}

#[tokio::test]
async fn the_settings_window_gets_the_values_and_every_change() {
    let mut f = Fixture::new(json!([]), json!([])).await;
    f.drain();
    f.tracker
        .show_settings(GuildId::parse(GUILD), ChannelId::parse(RANDOM));
    let events = f.drain();
    let Some(Ev::Settings(view)) = events.iter().find(|e| matches!(e, Ev::Settings(_))) else {
        panic!("no settings view");
    };
    assert!(!view.guild_muted && !view.channel_muted);
    assert_eq!(view.channel_level, 3);
    assert_eq!(view.guild_effective_level, 0);

    f.tracker.change(
        Target::Guild(Some(GUILD.to_string())),
        Change::Level(1),
        f.at(10),
    );
    let events = f.drain();
    let Some(Ev::Settings(view)) = events.iter().find(|e| matches!(e, Ev::Settings(_))) else {
        panic!("no settings view after a change");
    };
    assert_eq!(view.guild_level, 1);
    assert_eq!(view.channel_inherited_level, 1);

    f.tracker.hide_settings();
    f.tracker.change(
        Target::Guild(Some(GUILD.to_string())),
        Change::Level(2),
        f.at(20),
    );
    assert!(!f.drain().iter().any(|e| matches!(e, Ev::Settings(_))));
}
