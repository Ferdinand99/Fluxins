//! Unread state and notification settings as the sidebar and the guild rail
//! read them: from READY, from live events and from settings changes.

mod common;

use common::*;
use inlet_state::{AccountState, AckKind, Unread};
use serde_json::{json, Value};

fn with_last(mut channel: Value, last: inlet_state::MessageId) -> Value {
    channel["last_message_id"] = json!(last.to_string());
    channel
}

/// READY as the server sends it after a day of use: some channels read, some
/// not, a mention count on one, and a direct message nobody has acked.
fn ready_with_read_states() -> AccountState {
    let channels = json!([
        channel(CATEGORY, 4, None, 0, json!([])),
        with_last(
            channel(GENERAL, 0, Some(CATEGORY), 1, json!([])),
            mid(10, 0)
        ),
        with_last(channel(RANDOM, 0, Some(CATEGORY), 2, json!([])), mid(20, 0)),
        channel(VOICE, 2, None, 3, json!([])),
    ]);
    let mut v = ready_json(
        guild_ready(default_roles(), channels, &[MOD_ROLE]),
        json!([
            // Read up to message 5 of 10, with three unread mentions.
            {"id": GENERAL, "mention_count": 3, "last_message_id": mid(5, 0).to_string()},
            // Read to the end.
            {"id": RANDOM, "mention_count": 0, "last_message_id": mid(20, 0).to_string()},
        ]),
        json!([]),
    );
    v["private_channels"][0]["last_message_id"] = json!(mid(30, 0).to_string());
    AccountState::from_ready_at(&ready_from(v), JOINED_MS as i64 + 3_600_000)
}

#[test]
fn read_state_from_ready_matches_server() {
    let mut s = ready_with_read_states();

    // The server's ack and mention count are what the sidebar shows.
    assert_eq!(
        s.unread(ch(GENERAL)),
        Unread {
            unread: true,
            mentions: 3,
            muted: false
        }
    );
    assert_eq!(s.unread(ch(RANDOM)), Unread::default());
    // Categories and voice channels have no read state.
    assert_eq!(s.unread(ch(CATEGORY)), Unread::default());
    assert_eq!(s.unread(ch(VOICE)), Unread::default());
    // A DM with no ack counts from when the channel was created.
    assert!(s.unread(ch(DM)).unread);
    assert_eq!(s.unread(ch(DM)).mentions, 0);

    // The guild pill and the DM button add the channels up.
    assert_eq!(
        s.guild_unread(g()),
        Unread {
            unread: true,
            mentions: 3,
            muted: false
        }
    );
    assert!(s.private_unread().unread);

    // The channel list only needs to refresh what READY named: all of it.
    let c = s.apply(&d("READY", ready_json_for_apply()));
    assert!(c.reset);
    // The same server data gives the same answers.
    assert_eq!(
        s.unread(ch(GENERAL)),
        Unread {
            unread: true,
            mentions: 3,
            muted: false
        }
    );
    assert_eq!(s.unread(ch(RANDOM)), Unread::default());
    assert_eq!(
        s.guild_unread(g()),
        Unread {
            unread: true,
            mentions: 3,
            muted: false
        }
    );

    // Reading the channel does what the server will say back: ack at the
    // newest message, mentions to zero.
    let c = s.mark_read(ch(GENERAL), None, AckKind::Explicit);
    assert!(c.read_state_channels.contains(&ch(GENERAL)));
    assert!(c.read_state_guilds.contains(&Some(g())));
    assert_eq!(s.unread(ch(GENERAL)), Unread::default());
    assert_eq!(s.guild_unread(g()), Unread::default());
    // The server's own echo (MESSAGE_ACK) changes nothing more.
    let c = s.apply(&d(
        "MESSAGE_ACK",
        json!({"channel_id": GENERAL, "message_id": mid(10, 0).to_string(), "mention_count": 0}),
    ));
    assert!(c.read_state_channels.is_empty());
    assert_eq!(s.unread(ch(GENERAL)), Unread::default());
}

/// A READY like the first one, to show a second READY (a new Identify) gives
/// the same answers from the same server data.
fn ready_json_for_apply() -> Value {
    let channels = json!([
        channel(CATEGORY, 4, None, 0, json!([])),
        with_last(
            channel(GENERAL, 0, Some(CATEGORY), 1, json!([])),
            mid(10, 0)
        ),
        with_last(channel(RANDOM, 0, Some(CATEGORY), 2, json!([])), mid(20, 0)),
        channel(VOICE, 2, None, 3, json!([])),
    ]);
    ready_json(
        guild_ready(default_roles(), channels, &[MOD_ROLE]),
        json!([
            {"id": GENERAL, "mention_count": 3, "last_message_id": mid(5, 0).to_string()},
            {"id": RANDOM, "mention_count": 0, "last_message_id": mid(20, 0).to_string()},
        ]),
        json!([]),
    )
}

#[test]
fn muted_guild_shows_no_badge_but_mentions_count() {
    // The whole guild is muted from READY.
    let r = ready_from(ready_json(
        guild_ready(default_roles(), default_channels(), &[MOD_ROLE]),
        json!([]),
        json!([{"guild_id": GUILD, "muted": true}]),
    ));
    let mut s = AccountState::from_ready_at(&r, JOINED_MS as i64 + 3_600_000);

    // Plain messages in an unmuted channel of a muted guild: no pill.
    s.apply(&create(mid(1, 0), GENERAL, OTHER, json!({})));
    s.apply(&create(mid(2, 0), RANDOM, OTHER, json!({})));
    assert_eq!(
        s.guild_unread(g()),
        Unread {
            unread: false,
            mentions: 0,
            muted: true
        }
    );

    // A mention still counts on the badge, and only on the badge.
    s.apply(&create(
        mid(3, 0),
        GENERAL,
        OTHER,
        json!({"mentions": [{"id": ME, "username": "me"}]}),
    ));
    s.apply(&create(
        mid(4, 0),
        RANDOM,
        OTHER,
        json!({"mention_everyone": true, "content": "@everyone"}),
    ));
    assert_eq!(
        s.guild_unread(g()),
        Unread {
            unread: false,
            mentions: 2,
            muted: true
        }
    );
    // The channels inside still say what they hold; the guild decides what the rail shows.
    assert_eq!(s.unread(ch(GENERAL)).mentions, 1);
    assert!(s.unread(ch(GENERAL)).unread);

    // Suppressing @everyone takes that mention off the badge for new messages.
    let c = s.apply(&d(
        "USER_GUILD_SETTINGS_UPDATE",
        json!({"guild_id": GUILD, "suppress_everyone": true}),
    ));
    assert!(c.settings);
    s.apply(&create(
        mid(5, 0),
        RANDOM,
        OTHER,
        json!({"mention_everyone": true, "content": "@everyone"}),
    ));
    assert_eq!(s.guild_unread(g()).mentions, 2);

    // A timed mute that ends brings the pill back with the clock.
    let end = "2026-01-01T02:00:00Z";
    s.apply(&d(
        "USER_GUILD_SETTINGS_UPDATE",
        json!({"guild_id": GUILD, "muted": true, "mute_config": {"end_time": end, "selected_time_window": 3_600_000}}),
    ));
    assert!(!s.guild_unread(g()).unread);
    let c = s.tick(JOINED_MS as i64 + 2 * 3_600_000);
    assert!(c.settings && c.read_state_guilds.contains(&Some(g())));
    let gu = s.guild_unread(g());
    assert!(gu.unread && !gu.muted && gu.mentions == 2);

    // Unmuting by hand does the same.
    s.apply(&d(
        "USER_GUILD_SETTINGS_UPDATE",
        json!({"guild_id": GUILD, "muted": true}),
    ));
    assert!(!s.guild_unread(g()).unread);
    s.apply(&d(
        "USER_GUILD_SETTINGS_UPDATE",
        json!({"guild_id": GUILD, "muted": false, "mute_config": null}),
    ));
    assert!(s.guild_unread(g()).unread);
}
