use std::time::{Duration, Instant};

use inlet_protocol::{Dispatch, Message, Ready};
use serde_json::{json, Value};

use super::*;

const NOW: i64 = 1_800_000_000_000;

fn key(user: &str) -> AccountKey {
    AccountKey::new("https://one.example", user).unwrap()
}

/// Account 1 in guild 10 (#general 100, #quiet 101 in category 50, #muted 102) and
/// a DM 300 with Ada (2). `status` is the account's own status.
fn ready(status: &str, settings: Value) -> Ready {
    serde_json::from_value(json!({
        "session_id": "s", "version": 1,
        "user": {"id": "1", "username": "me"},
        "user_settings": {"status": status},
        "guilds": [{
            "id": "10",
            "properties": {"id": "10", "name": "Garden", "owner_id": "9"},
            "channels": [
                {"id": "50", "type": 4, "guild_id": "10", "name": "Cat"},
                {"id": "100", "type": 0, "guild_id": "10", "name": "general"},
                {"id": "101", "type": 0, "guild_id": "10", "name": "quiet", "parent_id": "50"},
                {"id": "102", "type": 0, "guild_id": "10", "name": "muted"}
            ],
            "roles": [{"id": "10", "name": "@everyone", "permissions": "2251799813685247", "position": 0},
                      {"id": "77", "name": "Gardeners", "permissions": "0", "position": 1}],
            "members": [{"user": {"id": "1", "username": "me"}, "roles": ["77"],
                         "joined_at": "2020-01-01T00:00:00+00:00"},
                        {"user": {"id": "2", "username": "ada", "global_name": "Ada"}, "nick": "Countess",
                         "roles": [], "joined_at": "2020-01-01T00:00:00+00:00"}],
            "member_count": 3
        }],
        "private_channels": [{"id": "300", "type": 1,
                              "recipients": [{"id": "2", "username": "ada", "global_name": "Ada"}]}],
        "read_states": [],
        "user_guild_settings": settings
    }))
    .unwrap()
}

fn state_with(status: &str, settings: Value) -> AccountState {
    AccountState::from_ready_at(&ready(status, settings), NOW)
}

fn message(id: u64, channel: &str, author: &str, content: &str, mentions_me: bool) -> Message {
    let guild = (channel != "300").then_some("10");
    serde_json::from_value(json!({
        "id": id.to_string(), "channel_id": channel, "guild_id": guild,
        "author": {"id": author, "username": if author == "1" { "me" } else { "ada" },
                   "global_name": if author == "1" { "Me" } else { "Ada" }},
        "type": 0, "content": content, "timestamp": "2027-01-15T08:00:00Z",
        "mentions": if mentions_me { json!([{"id": "1", "username": "me"}]) } else { json!([]) },
        "attachments": [], "embeds": []
    }))
    .unwrap()
}

/// Message IDs well after the channels were made.
fn id(n: u64) -> u64 {
    (1_700_000_000_000u64 << 22) + n
}

/// Applies the message to the state as the UI does: probe, apply, then the engine.
fn deliver(
    engine: &mut Engine,
    state: &mut AccountState,
    account: Option<&AccountKey>,
    m: &Message,
    focused: Option<ChannelId>,
    now: Instant,
) -> Option<Delivery> {
    let d = Dispatch::MessageCreate(Box::new(m.clone()));
    let p = probe(state, &d).expect("a message probe");
    state.apply(&d);
    engine.on_message(
        Input {
            account,
            account_label: None,
            state,
            probe: p,
            message: m,
            focused,
        },
        now,
    )
}

fn shown(d: &Option<Delivery>) -> Option<&Notification> {
    d.as_ref().and_then(|d| d.notification.as_ref())
}

#[test]
fn dnd_suppresses_sound_and_notification() {
    let a = key("1");
    let mut engine = Engine::new(AlertPrefs::default());
    let mut state = state_with("dnd", json!([]));
    engine.note_status(
        Some(&a),
        &Dispatch::Ready(Box::new(ready("dnd", json!([])))),
    );
    assert!(engine.is_dnd(Some(&a)));
    let t0 = Instant::now();

    // A mention and a DM in DND: no notification, no sound, nothing at all.
    let mention = message(id(1), "100", "2", "hey <@1>", true);
    assert!(deliver(&mut engine, &mut state, Some(&a), &mention, None, t0).is_none());
    let dm = message(id(2), "300", "2", "psst", false);
    assert!(deliver(&mut engine, &mut state, Some(&a), &dm, None, t0).is_none());

    // A settings update without a status changes nothing.
    let partial: inlet_protocol::models::UserSettings =
        serde_json::from_value(json!({"theme": "dark"})).unwrap();
    engine.note_status(Some(&a), &Dispatch::UserSettingsUpdate(Box::new(partial)));
    assert!(engine.is_dnd(Some(&a)));

    // Another account is not in DND: it still notifies, with its sound.
    let b = key("2");
    let mut other = state_with("online", json!([]));
    engine.note_status(
        Some(&b),
        &Dispatch::Ready(Box::new(ready("online", json!([])))),
    );
    let d = deliver(&mut engine, &mut other, Some(&b), &mention, None, t0);
    assert!(shown(&d).is_some());
    assert!(d.as_ref().unwrap().sound);

    // Back online: the next message notifies and plays the sound.
    let online: inlet_protocol::models::UserSettings =
        serde_json::from_value(json!({"status": "online"})).unwrap();
    engine.note_status(Some(&a), &Dispatch::UserSettingsUpdate(Box::new(online)));
    assert!(!engine.is_dnd(Some(&a)));
    let later = message(id(3), "100", "2", "again <@1>", true);
    let d = deliver(
        &mut engine,
        &mut state,
        Some(&a),
        &later,
        None,
        t0 + Duration::from_secs(5),
    );
    assert!(shown(&d).is_some());
    assert!(d.unwrap().sound);
}

#[test]
fn notification_body_respects_the_preview_setting() {
    let secret = "the launch code is 0000";
    let content = format!("<@1> {secret} \u{202E}evil\u{202C} <#101> <:wave:123> ||hidden||");
    let t0 = Instant::now();

    let mut engine = Engine::new(AlertPrefs::default());
    let mut state = state_with("online", json!([]));
    let m = message(id(1), "100", "2", &content, true);
    let d = deliver(&mut engine, &mut state, None, &m, None, t0);
    let n = shown(&d).expect("a notification");
    assert_eq!(n.title(), "Countess (#general, Garden)");
    assert!(n.body().contains(secret), "{}", n.body());
    // Tokens became names, the spoiler is hidden, the direction override is gone.
    assert!(n.body().starts_with("@me "), "{}", n.body());
    assert!(n.body().contains("#quiet"), "{}", n.body());
    assert!(n.body().contains(":wave:"), "{}", n.body());
    assert!(!n.body().contains("hidden"), "{}", n.body());
    assert!(n.body().contains("(spoiler)"), "{}", n.body());
    assert!(!n.body().contains('\u{202E}'));
    assert!(!n.body().contains('<'), "{}", n.body());

    // Preview off: neither the title nor the body carries any of the content.
    let mut engine = Engine::new(AlertPrefs {
        preview: false,
        ..AlertPrefs::default()
    });
    let mut state = state_with("online", json!([]));
    let d = deliver(&mut engine, &mut state, None, &m, None, t0);
    let n = shown(&d).expect("a notification");
    assert_eq!(n.body(), "Mentioned you");
    for text in [n.title(), n.body()] {
        assert!(!text.contains("launch"), "{text}");
        assert!(!text.contains("evil"), "{text}");
        assert!(!text.contains("wave"), "{text}");
    }
    let plain = message(id(2), "300", "2", secret, false);
    let d = deliver(&mut engine, &mut state, None, &plain, None, t0);
    let n = shown(&d).expect("a DM notifies");
    assert_eq!((n.title(), n.body()), ("Ada", "New message"));

    // Long content is cut; an attachment without text says so.
    let mut engine = Engine::new(AlertPrefs::default());
    let long = message(id(3), "300", "2", &"word ".repeat(2000), false);
    let d = deliver(&mut engine, &mut state, None, &long, None, t0);
    let body = shown(&d).unwrap().body().to_string();
    assert!(body.chars().count() <= inlet_platform::BODY_MAX_GRAPHEMES);
    let mut file = message(id(4), "300", "2", "", false);
    file.attachments = serde_json::from_value(json!([{"id": "5", "filename": "a.png"}])).unwrap();
    let d = deliver(
        &mut engine,
        &mut state,
        None,
        &file,
        None,
        t0 + Duration::from_secs(20),
    );
    assert_eq!(shown(&d).unwrap().body(), "Sent an attachment");
}

#[test]
fn mutes_and_levels_decide_what_notifies() {
    let settings = json!([{
        "guild_id": "10", "message_notifications": 1,
        "channel_overrides": [
            {"channel_id": "50", "message_notifications": 0},
            {"channel_id": "102", "muted": true}
        ]
    }]);
    let mut engine = Engine::new(AlertPrefs::default());
    let mut state = state_with("online", settings);
    let t0 = Instant::now();
    let mut at = 0;
    let mut send = |engine: &mut Engine, state: &mut AccountState, ch: &str, mention: bool| {
        at += 1;
        let text = if mention { "<@1> hi" } else { "hi" };
        let m = message(id(at), ch, "2", text, mention);
        // Far apart, so the burst limit never counts.
        let now = t0 + Duration::from_secs(at * 60);
        deliver(engine, state, None, &m, None, now).is_some()
    };
    // The guild is "only mentions".
    assert!(!send(&mut engine, &mut state, "100", false));
    assert!(send(&mut engine, &mut state, "100", true));
    // The category overrides it with "all messages".
    assert!(send(&mut engine, &mut state, "101", false));
    // A muted channel never notifies, mention or not.
    assert!(!send(&mut engine, &mut state, "102", true));
    // DMs notify for every message.
    assert!(send(&mut engine, &mut state, "300", false));

    // The whole guild muted: nothing, not even a mention.
    let mut state = state_with(
        "online",
        json!([{"guild_id": "10", "muted": true, "message_notifications": 0}]),
    );
    assert!(!send(&mut engine, &mut state, "100", true));
    // Level "nothing": no mention either.
    let mut state = state_with(
        "online",
        json!([{"guild_id": "10", "message_notifications": 2}]),
    );
    assert!(!send(&mut engine, &mut state, "100", true));
    // @everyone suppressed by the guild: not a mention, so "only mentions" is quiet.
    let mut state = state_with(
        "online",
        json!([{"guild_id": "10", "message_notifications": 1, "suppress_everyone": true}]),
    );
    at += 1;
    let mut everyone = message(id(at), "100", "2", "@everyone", false);
    everyone
        .extra
        .insert("mention_everyone".into(), json!(true));
    let now = t0 + Duration::from_secs(at * 60);
    assert!(deliver(&mut engine, &mut state, None, &everyone, None, now).is_none());
}

#[test]
fn own_repeated_and_watched_messages_do_not_notify() {
    let mut engine = Engine::new(AlertPrefs::default());
    let mut state = state_with("online", json!([]));
    let t0 = Instant::now();
    let mine = message(id(1), "300", "1", "from me", false);
    assert!(deliver(&mut engine, &mut state, None, &mine, None, t0).is_none());

    let m = message(id(2), "300", "2", "hello", false);
    let later = t0 + Duration::from_secs(60);
    assert!(deliver(&mut engine, &mut state, None, &m, None, later).is_some());
    // The same MESSAGE_CREATE again: once is enough.
    let again = t0 + Duration::from_secs(120);
    assert!(deliver(&mut engine, &mut state, None, &m, None, again).is_none());

    // The channel on screen with focus: no notification and no sound.
    let m = message(id(3), "300", "2", "seen", false);
    let watched = Some(ChannelId(300));
    let then = t0 + Duration::from_secs(180);
    assert!(deliver(&mut engine, &mut state, None, &m, watched, then).is_none());
    // Another channel is on screen: it notifies.
    let m = message(id(4), "300", "2", "elsewhere", false);
    let then = t0 + Duration::from_secs(240);
    let d = deliver(
        &mut engine,
        &mut state,
        None,
        &m,
        Some(ChannelId(100)),
        then,
    );
    assert!(shown(&d).is_some());
}

#[test]
fn a_flood_is_capped_and_the_sound_spaced() {
    let mut engine = Engine::new(AlertPrefs::default());
    let mut state = state_with("online", json!([]));
    let t0 = Instant::now();
    let mut notified = 0;
    let mut sounds = 0;
    for n in 0..50 {
        let m = message(id(n + 1), "300", "2", "spam", false);
        let now = t0 + Duration::from_millis(n * 100);
        if let Some(d) = deliver(&mut engine, &mut state, None, &m, None, now) {
            notified += usize::from(d.notification.is_some());
            sounds += usize::from(d.sound);
        }
    }
    // Five seconds of messages: six notifications at most, a sound every 1.5 s.
    assert_eq!(notified, BURST);
    assert_eq!(sounds, 4);

    // Sound off and notifications off: nothing to do at all.
    let mut quiet = Engine::new(AlertPrefs {
        enabled: false,
        sound: false,
        ..AlertPrefs::default()
    });
    let m = message(id(100), "300", "2", "hello", false);
    assert!(deliver(&mut quiet, &mut state, None, &m, None, t0).is_none());
}

#[test]
fn clicks_find_their_account_and_channel() {
    let a = key("1");
    let mut engine = Engine::new(AlertPrefs::default());
    let mut state = state_with("online", json!([]));
    let t0 = Instant::now();
    let m = message(id(1), "101", "2", "<@1>", true);
    let d = deliver(&mut engine, &mut state, Some(&a), &m, None, t0);
    let n = shown(&d).unwrap().clone();
    let target = engine.target(n.tag()).expect("a target");
    assert_eq!(target.account.as_ref(), Some(&a));
    assert_eq!(target.guild.as_deref(), Some("10"));
    assert_eq!(target.channel, "101");
    assert!(n.tag().len() <= 64);
    assert!(engine.target("m9-1").is_none());
}

#[test]
fn prefs_round_trip_and_keep_other_settings() {
    let dir = std::env::temp_dir().join(format!("inlet-alerts-{}", std::process::id()));
    let path = dir.join("settings.json");
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(load_prefs(&path), AlertPrefs::default());
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(&path, r#"{"desktop": {"zoom": 1.25}}"#).unwrap();
    let prefs = AlertPrefs {
        preview: false,
        sound: false,
        ..AlertPrefs::default()
    };
    save_prefs(&path, &prefs).unwrap();
    assert_eq!(load_prefs(&path), prefs);
    let root: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(root["desktop"]["zoom"], json!(1.25));
    std::fs::write(&path, "{ not json").unwrap();
    assert_eq!(load_prefs(&path), AlertPrefs::default());
    let _ = std::fs::remove_dir_all(&dir);
}
