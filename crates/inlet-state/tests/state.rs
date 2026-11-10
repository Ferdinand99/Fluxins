//! Behaviour of `AccountState` against the rules in the T7 brief and Fluxer.

mod common;

use common::*;
use inlet_protocol::permissions::*;
use inlet_protocol::Dispatch;
use inlet_state::limits::{MAX_MESSAGES_PER_CHANNEL, MAX_MESSAGE_WINDOWS, TYPING_EXPIRY_MS};
use inlet_state::{AccountState, AckKind, ChannelId, MessageId, MessagesPage, Permissions, Unread};
use serde_json::json;

fn ids(s: &AccountState, c: ChannelId) -> Vec<MessageId> {
    s.messages(c)
        .map(|w| w.iter().map(|m| MessageId::parse(&m.id).unwrap()).collect())
        .unwrap_or_default()
}

#[test]
fn ready_then_resume_gives_the_same_state() {
    let events = vec![
        create(
            mid(10, 0),
            GENERAL,
            OTHER,
            json!({"mentions": [{"id": ME, "username": "me"}]}),
        ),
        create(mid(11, 0), RANDOM, OTHER, json!({"mention_everyone": true})),
        d(
            "GUILD_ROLE_UPDATE",
            json!({"guild_id": GUILD, "role": role(OTHER_ROLE, ATTACH_FILES, 1)}),
        ),
        d(
            "GUILD_MEMBER_ADD",
            json!({"guild_id": GUILD, "user": {"id": "21", "username": "bo"}, "roles": []}),
        ),
        d(
            "PRESENCE_UPDATE",
            json!({"user": {"id": "21"}, "status": "idle"}),
        ),
        d(
            "MESSAGE_ACK",
            json!({"channel_id": RANDOM, "message_id": mid(11, 0).to_string(), "mention_count": 0}),
        ),
        create(mid(12, 0), DM, OTHER, json!({})),
    ];

    let mut live = state();
    live.apply_messages(
        ch(GENERAL),
        MessagesPage::latest(vec![message(mid(5, 0), GENERAL, OTHER)], 50),
    );
    let mut resumed = live.clone();
    for e in &events {
        live.apply(e);
        resumed.apply(e);
    }
    // A Resume changes nothing, and the replayed tail of events is idempotent.
    assert!(resumed.apply(&Dispatch::Resumed).is_empty());
    for e in &events[events.len() - 3..] {
        resumed.apply(e);
    }
    assert_eq!(live, resumed);
    assert_eq!(
        live.unread(ch(GENERAL)),
        Unread {
            unread: true,
            mentions: 1,
            muted: false
        }
    );

    // A new READY with the same snapshot gives the same state as a fresh
    // start, except that message windows are kept (stale) for a refetch.
    let r = ready();
    let mut again = live.clone();
    let c = again.apply(&Dispatch::Ready(Box::new(r.clone())));
    assert!(c.reset);
    let w = again.messages(ch(GENERAL)).expect("window kept");
    assert!(!w.has_newest(), "a kept window waits for the newest page");
    again.drop_message_windows();
    let mut fresh = AccountState::from_ready_at(&r, again.now_ms());
    fresh.drop_message_windows();
    assert_eq!(again, fresh);
}

#[test]
fn channel_delete_removes_it_and_its_messages() {
    let mut s = state();
    let page = vec![
        message(mid(2, 0), GENERAL, OTHER),
        message(mid(1, 0), GENERAL, OTHER),
    ];
    s.apply_messages(ch(GENERAL), MessagesPage::latest(page.clone(), 50));
    s.apply(&d(
        "TYPING_START",
        json!({"channel_id": GENERAL, "user_id": OTHER, "timestamp": 1}),
    ));
    assert_eq!(ids(&s, ch(GENERAL)).len(), 2);
    assert!(s.permissions(g(), ch(GENERAL)).can_view());

    let c = s.apply(&d(
        "CHANNEL_DELETE",
        json!({"id": GENERAL, "type": 0, "guild_id": GUILD}),
    ));
    assert!(c.channels.contains(&ch(GENERAL)) && c.messages.contains(&ch(GENERAL)));
    assert!(s.channel(ch(GENERAL)).is_none());
    assert!(s.messages(ch(GENERAL)).is_none());
    assert!(s.typing(ch(GENERAL)).is_empty());
    assert!(s.read_state(ch(GENERAL)).is_none());
    assert_eq!(s.permissions(g(), ch(GENERAL)), Permissions::NONE);
    assert!(s.guild_channels(g()).iter().all(|c| c.id != GENERAL));

    // Late events and a late page for the deleted channel change nothing.
    assert!(s
        .apply_messages(ch(GENERAL), MessagesPage::latest(page, 50))
        .is_empty());
    assert!(s
        .apply(&create(mid(3, 0), GENERAL, OTHER, json!({})))
        .messages
        .is_empty());
    assert!(s.messages(ch(GENERAL)).is_none());
    // Deleting again is harmless.
    assert!(s
        .apply(&d(
            "CHANNEL_DELETE",
            json!({"id": GENERAL, "type": 0, "guild_id": GUILD})
        ))
        .is_empty());

    // A closed DM loses its window but keeps its ack.
    s.apply_messages(
        ch(DM),
        MessagesPage::latest(vec![message(mid(4, 0), DM, OTHER)], 50),
    );
    s.mark_read(ch(DM), None, AckKind::Explicit);
    s.apply(&d("CHANNEL_DELETE", json!({"id": DM, "type": 1})));
    assert!(s.channel(ch(DM)).is_none() && s.messages(ch(DM)).is_none());
    assert_eq!(
        s.read_state(ch(DM)).unwrap().ack_message_id,
        Some(mid(4, 0))
    );
}

#[test]
fn permission_overwrites_follow_fluxers_order() {
    // permissions.mdx: everyone overwrite, then the member's role overwrites
    // combined (an allow on one role beats a deny on another), then the
    // member's own overwrite; deny before allow at each step.
    let overwrites = json!([
        {"id": GUILD, "type": 0, "allow": "0", "deny": (VIEW_CHANNEL | SEND_MESSAGES).to_string()},
        {"id": MOD_ROLE, "type": 0, "allow": VIEW_CHANNEL.to_string(), "deny": "0"},
        {"id": OTHER_ROLE, "type": 0, "allow": SEND_MESSAGES.to_string(), "deny": VIEW_CHANNEL.to_string()},
        {"id": ME, "type": 1, "allow": "0", "deny": SEND_MESSAGES.to_string()},
    ]);
    let roles = json!([
        role(GUILD, everyone_perms(), 0),
        role(MOD_ROLE, 0, 2),
        role(OTHER_ROLE, 0, 1)
    ]);
    let channels = json!([channel(GENERAL, 0, None, 0, overwrites.clone())]);
    let r = ready_from(ready_json(
        guild_ready(roles, channels, &[MOD_ROLE, OTHER_ROLE]),
        json!([]),
        json!([]),
    ));
    let mut s = AccountState::from_ready_at(&r, JOINED_MS as i64);
    let p = s.permissions(g(), ch(GENERAL));
    // Role allow beats role deny for VIEW; the member's own deny wins for SEND.
    assert!(p.can_view());
    assert!(!p.has(SEND_MESSAGES));
    assert!(p.has(READ_MESSAGE_HISTORY));

    // Dropping the member overwrite: the role allow for SEND applies after the everyone deny.
    let without_me = json!([overwrites[0], overwrites[1], overwrites[2]]);
    let c = s.apply(&d(
        "CHANNEL_UPDATE",
        json!({"id": GENERAL, "type": 0, "guild_id": GUILD,
        "permission_overwrites": without_me}),
    ));
    assert!(c.permissions, "an overwrite change recomputes permissions");
    assert!(s.permissions(g(), ch(GENERAL)).can_send());

    // Losing the mod role: only the other role's deny is left, so the channel hides.
    let c = s.apply(&d(
        "GUILD_MEMBER_UPDATE",
        json!({"guild_id": GUILD,
        "user": {"id": ME, "username": "me"}, "roles": [OTHER_ROLE]}),
    ));
    assert!(c.permissions && c.members);
    assert!(!s.permissions(g(), ch(GENERAL)).can_view());
    assert!(s.visible_channels(g()).is_empty());

    // A role update that grants ADMINISTRATOR skips every overwrite.
    let c = s.apply(&d(
        "GUILD_ROLE_UPDATE",
        json!({"guild_id": GUILD, "role": role(OTHER_ROLE, ADMINISTRATOR, 1)}),
    ));
    assert!(c.permissions);
    assert_eq!(s.permissions(g(), ch(GENERAL)), Permissions::ALL);
    // Deleting the role takes it away again.
    s.apply(&d(
        "GUILD_ROLE_DELETE",
        json!({"guild_id": GUILD, "role_id": OTHER_ROLE}),
    ));
    assert!(!s.permissions(g(), ch(GENERAL)).can_view());
}

#[test]
fn everyone_role_and_admin_short_circuit() {
    // No roles yet: everything is denied (fail closed), nothing is visible.
    let r = ready_from(ready_json(
        guild_ready(json!([]), default_channels(), &[]),
        json!([]),
        json!([]),
    ));
    let mut s = AccountState::from_ready_at(&r, JOINED_MS as i64);
    assert!(!s.guild(g()).unwrap().roles_loaded());
    assert_eq!(s.permissions(g(), ch(GENERAL)), Permissions::NONE);
    assert_eq!(s.guild_permissions(g()), Permissions::NONE);
    assert!(s.visible_channels(g()).is_empty());

    // The everyone role arrives: its bits apply to every member.
    let c = s.apply(&d(
        "GUILD_ROLE_CREATE",
        json!({"guild_id": GUILD, "role": role(GUILD, VIEW_CHANNEL, 0)}),
    ));
    assert!(c.permissions);
    let p = s.permissions(g(), ch(GENERAL));
    assert!(p.can_view() && !p.has(SEND_MESSAGES));
    let names: Vec<&str> = s
        .visible_channels(g())
        .iter()
        .map(|c| c.id.as_str())
        .collect();
    assert_eq!(names, vec![CATEGORY, GENERAL, RANDOM, VOICE]);

    // An overwrite that hides a channel from everyone...
    s.apply(&d("CHANNEL_UPDATE", json!({"id": RANDOM, "type": 0, "guild_id": GUILD, "parent_id": CATEGORY,
        "permission_overwrites": [{"id": GUILD, "type": 0, "allow": "0", "deny": VIEW_CHANNEL.to_string()}]})));
    assert!(!s.permissions(g(), ch(RANDOM)).can_view());
    // ...does not stop an administrator.
    s.apply(&d(
        "GUILD_ROLE_CREATE",
        json!({"guild_id": GUILD, "role": role("9", ADMINISTRATOR, 3)}),
    ));
    s.apply(&d(
        "GUILD_MEMBER_UPDATE",
        json!({"guild_id": GUILD, "user": {"id": ME}, "roles": ["9"]}),
    ));
    assert_eq!(s.permissions(g(), ch(RANDOM)), Permissions::ALL);
    assert_eq!(s.guild_permissions(g()), Permissions::ALL);

    // The owner has everything whatever the roles say.
    s.apply(&d(
        "GUILD_MEMBER_UPDATE",
        json!({"guild_id": GUILD, "user": {"id": ME}, "roles": []}),
    ));
    assert!(!s.permissions(g(), ch(RANDOM)).can_view());
    s.apply(&d(
        "GUILD_UPDATE",
        json!({"id": GUILD, "name": "Hedgerow", "owner_id": ME}),
    ));
    assert_eq!(s.permissions(g(), ch(RANDOM)), Permissions::ALL);

    // Being removed from the guild denies everything.
    s.apply(&d(
        "GUILD_MEMBER_REMOVE",
        json!({"guild_id": GUILD, "user": {"id": ME}}),
    ));
    assert_eq!(s.permissions(g(), ch(GENERAL)), Permissions::NONE);
    // A DM always allows everything; an unknown channel nothing.
    assert_eq!(s.channel_permissions(ch(DM)), Permissions::ALL);
    assert_eq!(s.channel_permissions(ch("12345")), Permissions::NONE);
}

#[test]
fn unread_counts_mentions_of_me_my_roles_everyone_and_here() {
    let mut s = state();
    let c = ch(GENERAL);
    assert_eq!(s.unread(c), Unread::default());

    // A plain message: unread, no mention.
    assert!(
        s.apply(&create(mid(1, 0), GENERAL, OTHER, json!({})))
            .read_state
    );
    assert_eq!(
        s.unread(c),
        Unread {
            unread: true,
            mentions: 0,
            muted: false
        }
    );
    // The account, one of its roles, @everyone and @here each count.
    s.apply(&create(
        mid(2, 0),
        GENERAL,
        OTHER,
        json!({"mentions": [{"id": ME, "username": "me"}]}),
    ));
    s.apply(&create(
        mid(3, 0),
        GENERAL,
        OTHER,
        json!({"mention_roles": [MOD_ROLE]}),
    ));
    s.apply(&create(
        mid(4, 0),
        GENERAL,
        OTHER,
        json!({"mention_everyone": true, "content": "@everyone"}),
    ));
    s.apply(&create(
        mid(5, 0),
        GENERAL,
        OTHER,
        json!({"mention_everyone": true, "content": "@here"}),
    ));
    assert_eq!(s.unread(c).mentions, 4);
    // A role the account does not have, and a mention of someone else, do not.
    s.apply(&create(
        mid(6, 0),
        GENERAL,
        OTHER,
        json!({"mention_roles": [OTHER_ROLE]}),
    ));
    s.apply(&create(
        mid(7, 0),
        GENERAL,
        OTHER,
        json!({"mentions": [{"id": "21", "username": "bo"}]}),
    ));
    assert_eq!(s.unread(c).mentions, 4);
    // A repeated MESSAGE_CREATE is counted once.
    s.apply(&create(
        mid(2, 0),
        GENERAL,
        OTHER,
        json!({"mentions": [{"id": ME, "username": "me"}]}),
    ));
    assert_eq!(s.unread(c).mentions, 4);
    // The guild badge sums the channel's mentions.
    assert_eq!(
        s.guild_unread(g()),
        Unread {
            unread: true,
            mentions: 4,
            muted: false
        }
    );

    // The account's own message acks the channel.
    s.apply(&create(mid(8, 0), GENERAL, ME, json!({})));
    assert_eq!(s.unread(c), Unread::default());
    // A message the ack already covers changes nothing.
    assert!(
        !s.apply(&create(
            mid(7, 1),
            GENERAL,
            OTHER,
            json!({"mention_everyone": true})
        ))
        .read_state
    );
    assert_eq!(s.unread(c).mentions, 0);

    // With @everyone suppressed only the explicit mention counts.
    s.apply(&d(
        "USER_GUILD_SETTINGS_UPDATE",
        json!({"guild_id": GUILD, "suppress_everyone": true, "suppress_roles": true}),
    ));
    s.apply(&create(
        mid(9, 0),
        GENERAL,
        OTHER,
        json!({"mention_everyone": true}),
    ));
    s.apply(&create(
        mid(10, 0),
        GENERAL,
        OTHER,
        json!({"mention_roles": [MOD_ROLE]}),
    ));
    s.apply(&create(
        mid(11, 0),
        GENERAL,
        OTHER,
        json!({"mentions": [{"id": ME, "username": "me"}]}),
    ));
    assert_eq!(
        s.unread(c),
        Unread {
            unread: true,
            mentions: 1,
            muted: false
        }
    );

    // Every message in a DM is a mention.
    s.apply(&create(mid(12, 0), DM, OTHER, json!({})));
    s.apply(&create(mid(13, 0), DM, OTHER, json!({})));
    assert_eq!(s.unread(ch(DM)).mentions, 2);
    assert_eq!(
        s.private_unread(),
        Unread {
            unread: true,
            mentions: 2,
            muted: false
        }
    );

    // MESSAGE_ACK from another session: older is ignored, newer moves the ack.
    s.apply(&d(
        "MESSAGE_ACK",
        json!({"channel_id": DM, "message_id": mid(12, 0).to_string(), "mention_count": 1}),
    ));
    assert_eq!(
        s.unread(ch(DM)),
        Unread {
            unread: true,
            mentions: 1,
            muted: false
        }
    );
    s.apply(&d(
        "MESSAGE_ACK",
        json!({"channel_id": DM, "message_id": mid(1, 0).to_string(), "mention_count": 0}),
    ));
    assert_eq!(s.unread(ch(DM)).mentions, 1);
    // "Mark unread" (manual) may move it back.
    s.apply(&d("MESSAGE_ACK", json!({"channel_id": DM, "message_id": mid(1, 0).to_string(), "mention_count": 2, "manual": true})));
    assert_eq!(
        s.read_state(ch(DM)).unwrap().ack_message_id,
        Some(mid(1, 0))
    );
    assert_eq!(s.unread(ch(DM)).mentions, 2);

    // An automatic ack does nothing while "mark unread" holds; an explicit
    // ack clears it and, as Fluxer's applyAck, zeroes the mentions.
    s.apply_messages(
        ch(DM),
        MessagesPage::latest(
            vec![
                message(mid(13, 0), DM, OTHER),
                message(mid(12, 0), DM, OTHER),
            ],
            50,
        ),
    );
    assert!(s
        .mark_read(ch(DM), None, AckKind::Automatic)
        .read_state_channels
        .is_empty());
    assert_eq!(s.unread(ch(DM)).mentions, 2);
    let c = s.mark_read(ch(DM), Some(mid(12, 0)), AckKind::Explicit);
    assert!(c.read_state_channels.contains(&ch(DM)) && c.read_state_guilds.contains(&None));
    assert_eq!(
        s.unread(ch(DM)),
        Unread {
            unread: true,
            mentions: 0,
            muted: false
        }
    );
    assert!(!s.read_state(ch(DM)).unwrap().manual);
    s.mark_read(ch(DM), None, AckKind::Automatic);
    assert_eq!(s.unread(ch(DM)), Unread::default());
}

#[test]
fn muted_channel_has_unread_but_no_badge() {
    let r = ready_from(ready_json(
        guild_ready(default_roles(), default_channels(), &[MOD_ROLE]),
        json!([]),
        json!([{"guild_id": GUILD, "channel_overrides": [{"channel_id": RANDOM, "muted": true}]},
               {"guild_id": null, "channel_overrides": {"900": {"muted": true}}}]),
    ));
    let mut s = AccountState::from_ready_at(&r, JOINED_MS as i64 + 3_600_000);
    s.apply(&create(mid(1, 0), RANDOM, OTHER, json!({})));
    // The channel is unread (shown dimmed)...
    assert_eq!(
        s.unread(ch(RANDOM)),
        Unread {
            unread: true,
            mentions: 0,
            muted: true
        }
    );
    // ...but gives the guild no unread pill.
    assert_eq!(s.guild_unread(g()), Unread::default());
    // A direct mention still counts, and shows on the guild.
    s.apply(&create(
        mid(2, 0),
        RANDOM,
        OTHER,
        json!({"mentions": [{"id": ME, "username": "me"}]}),
    ));
    assert_eq!(s.unread(ch(RANDOM)).mentions, 1);
    assert_eq!(
        s.guild_unread(g()),
        Unread {
            unread: false,
            mentions: 1,
            muted: false
        }
    );
    // An unmuted channel gives the pill.
    s.apply(&create(mid(3, 0), GENERAL, OTHER, json!({})));
    assert!(s.guild_unread(g()).unread);

    // A muted DM: unread, but its messages are not mentions.
    s.apply(&create(mid(4, 0), DM, OTHER, json!({})));
    assert_eq!(
        s.unread(ch(DM)),
        Unread {
            unread: true,
            mentions: 0,
            muted: true
        }
    );

    // A muted category mutes its channels; a timed mute ends with the clock.
    let end = "2026-01-01T02:00:00Z";
    let c = s.apply(&d("USER_GUILD_SETTINGS_UPDATE", json!({"guild_id": GUILD,
        "channel_overrides": [{"channel_id": CATEGORY, "muted": true, "mute_config": {"end_time": end}}]})));
    assert!(c.settings);
    assert!(s.unread(ch(GENERAL)).muted);
    let c = s.tick(JOINED_MS as i64 + 2 * 3_600_000);
    assert!(c.settings && c.read_state);
    assert!(!s.unread(ch(GENERAL)).muted);

    // A whole muted guild: no pill, mentions still summed.
    s.apply(&d(
        "USER_GUILD_SETTINGS_UPDATE",
        json!({"guild_id": GUILD, "muted": true}),
    ));
    let gu = s.guild_unread(g());
    assert!(!gu.unread && gu.muted && gu.mentions == 1);
}

#[test]
fn message_page_merges_with_live_messages() {
    let mut s = state();
    let c = ch(GENERAL);
    // The newest page: 1, 2, 3 (newest first, as the API sends it).
    let first = vec![
        message(mid(3, 0), GENERAL, OTHER),
        message(mid(2, 0), GENERAL, OTHER),
        message(mid(1, 0), GENERAL, OTHER),
    ];
    let ch1 = s.apply_messages(c, MessagesPage::latest(first, 3));
    assert!(ch1.messages.contains(&c));
    assert_eq!(ids(&s, c), vec![mid(1, 0), mid(2, 0), mid(3, 0)]);
    assert!(s.messages(c).unwrap().has_more_before());

    // While a refetch is on its way: message 5 arrives, 3 is deleted, 2 is edited.
    s.apply(&create(mid(5, 0), GENERAL, OTHER, json!({})));
    s.apply(&d(
        "MESSAGE_DELETE",
        json!({"id": mid(3, 0).to_string(), "channel_id": GENERAL}),
    ));
    s.apply(&d(
        "MESSAGE_UPDATE",
        message_json(
            mid(2, 0),
            GENERAL,
            OTHER,
            json!({"content": "edited", "edited_timestamp": "2026-01-01T01:30:00Z"}),
        ),
    ));
    s.apply(&d(
        "MESSAGE_REACTION_ADD",
        json!({"user_id": OTHER, "channel_id": GENERAL,
        "message_id": mid(2, 0).to_string(), "emoji": {"name": "👍"}}),
    ));

    // The page was read before all that: 2 (old text), 3, 4.
    let stale = vec![
        message(mid(4, 0), GENERAL, OTHER),
        message(mid(3, 0), GENERAL, OTHER),
        message(mid(2, 0), GENERAL, OTHER),
    ];
    s.apply_messages(c, MessagesPage::latest(stale, 3));
    // Live 5 stays, deleted 3 does not come back, 1 stays (contiguous), 4 is new.
    assert_eq!(ids(&s, c), vec![mid(1, 0), mid(2, 0), mid(4, 0), mid(5, 0)]);
    let w = s.messages(c).unwrap();
    let two = w.get(mid(2, 0)).unwrap();
    assert_eq!(two.content, "edited");
    assert_eq!(two.reactions()[0].count, 1);
    assert!(w.has_newest());

    // An older page continues the window from its oldest message.
    let older: Vec<_> = (0..2)
        .map(|i| message(mid(0, 10 - i), GENERAL, OTHER))
        .collect();
    s.apply_messages(c, MessagesPage::before(mid(1, 0), older, 50));
    assert_eq!(ids(&s, c).len(), 6);
    assert!(
        !s.messages(c).unwrap().has_more_before(),
        "fewer than the limit: the start"
    );
    // A page for a window that moved on is dropped.
    let changes = s.apply_messages(
        c,
        MessagesPage::before(mid(99, 0), vec![message(mid(0, 1), GENERAL, OTHER)], 50),
    );
    assert!(changes.messages.is_empty());
    // Messages of another channel in a page are ignored.
    s.apply_messages(
        c,
        MessagesPage::latest(
            vec![
                message(mid(6, 0), RANDOM, OTHER),
                message(mid(5, 0), GENERAL, OTHER),
            ],
            50,
        ),
    );
    assert!(s.messages(c).unwrap().get(mid(6, 0)).is_none());

    // The window never holds more than the cap; live messages push out the oldest.
    for i in 0..(MAX_MESSAGES_PER_CHANNEL as u64 + 20) {
        s.apply(&create(mid(100 + i, 0), GENERAL, OTHER, json!({})));
    }
    let w = s.messages(c).unwrap();
    assert_eq!(w.len(), MAX_MESSAGES_PER_CHANNEL);
    assert!(w.has_more_before());
    assert_eq!(
        w.newest_id(),
        Some(mid(100 + MAX_MESSAGES_PER_CHANNEL as u64 + 19, 0))
    );
}

#[test]
fn windows_evict_least_recently_used_channel() {
    let count = MAX_MESSAGE_WINDOWS + 2;
    let mut channels = vec![];
    for i in 0..count {
        channels.push(channel(
            &(1000 + i).to_string(),
            0,
            None,
            i as i64,
            json!([]),
        ));
    }
    let r = ready_from(ready_json(
        guild_ready(default_roles(), json!(channels), &[]),
        json!([]),
        json!([]),
    ));
    let mut s = AccountState::from_ready_at(&r, JOINED_MS as i64);
    let id = |i: usize| ch(&(1000 + i).to_string());
    let page = |i: usize| {
        MessagesPage::latest(
            vec![message(mid(1, i as u64), &(1000 + i).to_string(), OTHER)],
            50,
        )
    };
    for i in 0..MAX_MESSAGE_WINDOWS {
        s.apply_messages(id(i), page(i));
    }
    assert_eq!(s.message_windows().len(), MAX_MESSAGE_WINDOWS);
    // Channel 0 is used again, so channel 1 is now the least recently used.
    s.touch_channel(id(0));
    s.apply_messages(id(MAX_MESSAGE_WINDOWS), page(MAX_MESSAGE_WINDOWS));
    assert_eq!(s.message_windows().len(), MAX_MESSAGE_WINDOWS);
    assert!(s.messages(id(0)).is_some());
    assert!(s.messages(id(1)).is_none());
    // Live messages do not count as use: channel 2 goes next even though it is busy.
    s.apply(&create(mid(2, 0), &(1002).to_string(), OTHER, json!({})));
    s.apply_messages(id(MAX_MESSAGE_WINDOWS + 1), page(MAX_MESSAGE_WINDOWS + 1));
    assert!(s.messages(id(2)).is_none());
    // An evicted channel has no window; its live messages are not collected.
    let c = s.apply(&create(mid(3, 0), &(1001).to_string(), OTHER, json!({})));
    assert!(c.messages.is_empty() && c.read_state);
}

#[test]
fn guild_delete_clears_members_and_read_state() {
    let mut s = state();
    s.apply(&d(
        "GUILD_MEMBER_ADD",
        json!({"guild_id": GUILD, "user": {"id": "21", "username": "bo"}, "roles": []}),
    ));
    s.apply(&create(
        mid(1, 0),
        GENERAL,
        OTHER,
        json!({"mentions": [{"id": ME, "username": "me"}]}),
    ));
    s.apply_messages(
        ch(GENERAL),
        MessagesPage::latest(vec![message(mid(1, 0), GENERAL, OTHER)], 50),
    );
    assert!(s.member(g(), uid("21")).is_some());
    assert_eq!(s.guild_unread(g()).mentions, 1);

    // Unavailable (an outage) keeps everything.
    s.apply(&d(
        "GUILD_DELETE",
        json!({"id": GUILD, "unavailable": true}),
    ));
    assert!(s.guild(g()).unwrap().unavailable);
    assert!(s.member(g(), uid("21")).is_some());
    // Removed (left or kicked) clears the guild and everything in it.
    let c = s.apply(&d("GUILD_DELETE", json!({"id": GUILD})));
    assert!(c.guilds && c.members && c.read_state);
    assert!(s.guild(g()).is_none());
    assert!(s.member(g(), uid("21")).is_none());
    assert!(s.member(g(), uid(ME)).is_none());
    for c in [GENERAL, RANDOM, VOICE, CATEGORY] {
        assert!(s.read_state(ch(c)).is_none(), "{c}");
        assert!(s.channel(ch(c)).is_none());
        assert_eq!(s.unread(ch(c)), Unread::default());
    }
    assert!(s.messages(ch(GENERAL)).is_none());
    assert_eq!(s.guild_unread(g()), Unread::default());
    // The DM is untouched.
    assert!(s.channel(ch(DM)).is_some());
    // Events for the gone guild are ignored.
    assert!(s
        .apply(&d(
            "GUILD_ROLE_CREATE",
            json!({"guild_id": GUILD, "role": role("5", 0, 1)})
        ))
        .is_empty());
    assert!(s
        .apply(&d(
            "CHANNEL_CREATE",
            json!({"id": GENERAL, "type": 0, "guild_id": GUILD})
        ))
        .is_empty());
    // Joining again brings it back.
    let c = s.apply(&d(
        "GUILD_CREATE",
        guild_ready(default_roles(), default_channels(), &[]),
    ));
    assert!(c.guilds);
    assert!(s.permissions(g(), ch(GENERAL)).can_view());
}

#[test]
fn typing_expires_and_ends_with_the_message() {
    let mut s = state();
    let now = s.now_ms();
    let c = s.apply(&d(
        "TYPING_START",
        json!({"channel_id": GENERAL, "user_id": OTHER, "timestamp": 1}),
    ));
    assert!(c.typing.contains(&ch(GENERAL)) && c.typing.iter().count() == 1);
    // The account's own typing and typing in unknown channels are ignored.
    assert!(s
        .apply(&d(
            "TYPING_START",
            json!({"channel_id": GENERAL, "user_id": ME})
        ))
        .is_empty());
    assert!(s
        .apply(&d(
            "TYPING_START",
            json!({"channel_id": "77", "user_id": OTHER})
        ))
        .is_empty());
    assert_eq!(s.typing(ch(GENERAL)), vec![uid(OTHER)]);
    let c = s.tick(now + TYPING_EXPIRY_MS);
    assert!(c.typing.contains(&ch(GENERAL)) && c.typing.iter().count() == 1);
    assert!(s.typing(ch(GENERAL)).is_empty());
    s.apply(&d(
        "TYPING_START",
        json!({"channel_id": GENERAL, "user_id": OTHER}),
    ));
    let c = s.apply(&create(mid(1, 0), GENERAL, OTHER, json!({})));
    assert!(c.typing.contains(&ch(GENERAL)));
    assert!(s.typing(ch(GENERAL)).is_empty());
}

#[test]
fn timeout_blocks_sending_until_it_ends() {
    let mut s = state();
    let until = "2026-01-01T02:00:00Z";
    let c = s.apply(&d(
        "GUILD_MEMBER_UPDATE",
        json!({"guild_id": GUILD, "user": {"id": ME},
        "roles": [MOD_ROLE], "communication_disabled_until": until}),
    ));
    assert!(c.permissions);
    let p = s.permissions(g(), ch(GENERAL));
    assert!(p.can_view() && !p.can_send());
    let c = s.tick(JOINED_MS as i64 + 2 * 3_600_000);
    assert!(c.permissions);
    assert!(s.permissions(g(), ch(GENERAL)).can_send());
}

#[test]
fn visited_guild_has_no_read_state_and_viewer_bits() {
    let mut gr = guild_ready(default_roles(), default_channels(), &[]);
    gr["visiting"] = json!(true);
    gr["members"] = json!([]);
    let r = ready_from(ready_json(gr, json!([]), json!([])));
    let mut s = AccountState::from_ready_at(&r, JOINED_MS as i64);
    let p = s.permissions(g(), ch(GENERAL));
    assert_eq!(p.bits(), VIEW_CHANNEL | READ_MESSAGE_HISTORY);
    s.apply(&create(
        mid(1, 0),
        GENERAL,
        OTHER,
        json!({"mention_everyone": true}),
    ));
    assert_eq!(s.unread(ch(GENERAL)), Unread::default());
    assert_eq!(s.guild_unread(g()), Unread::default());
}

#[test]
fn reactions_follow_add_remove_and_clear() {
    let mut s = state();
    s.apply_messages(
        ch(GENERAL),
        MessagesPage::latest(vec![message(mid(1, 0), GENERAL, OTHER)], 50),
    );
    let ev = |name: &str, user: &str| {
        d(
            name,
            json!({"user_id": user, "channel_id": GENERAL,
        "message_id": mid(1, 0).to_string(), "emoji": {"name": "🦔"}}),
        )
    };
    s.apply(&ev("MESSAGE_REACTION_ADD", OTHER));
    s.apply(&ev("MESSAGE_REACTION_ADD", ME));
    s.apply(&ev("MESSAGE_REACTION_ADD", ME));
    let r = &s
        .messages(ch(GENERAL))
        .unwrap()
        .get(mid(1, 0))
        .unwrap()
        .reactions()[0];
    assert_eq!((r.count, r.me), (2, true));
    s.apply(&ev("MESSAGE_REACTION_REMOVE", ME));
    s.apply(&ev("MESSAGE_REACTION_REMOVE", ME));
    let r = &s
        .messages(ch(GENERAL))
        .unwrap()
        .get(mid(1, 0))
        .unwrap()
        .reactions()[0];
    assert_eq!((r.count, r.me), (1, false));
    s.apply(&d(
        "MESSAGE_REACTION_REMOVE_ALL",
        json!({"channel_id": GENERAL, "message_id": mid(1, 0).to_string()}),
    ));
    assert!(s
        .messages(ch(GENERAL))
        .unwrap()
        .get(mid(1, 0))
        .unwrap()
        .reactions()
        .is_empty());
}

#[test]
fn unknown_and_malformed_events_change_nothing() {
    let mut s = state();
    let before = s.clone();
    for e in [
        d("SOMETHING_NEW", json!({"x": 1})),
        d("MESSAGE_CREATE", json!({"id": "not a number"})),
        d("CHANNEL_CREATE", json!({"id": "abc", "type": 0})),
        d("CHANNEL_CREATE", json!({"id": "555", "type": 0})),
        d("GUILD_DELETE", json!({"id": "4242"})),
        d(
            "MESSAGE_ACK",
            json!({"channel_id": "4242", "message_id": "1"}),
        ),
        d(
            "PASSIVE_UPDATES",
            json!({"guild_id": "4242", "channels": {RANDOM: "99999999999"}}),
        ),
        d("USER_GUILD_SETTINGS_UPDATE", json!("nope")),
        d(
            "GUILD_MEMBER_UPDATE",
            json!({"guild_id": "4242", "user": {"id": "1"}}),
        ),
        d(
            "USER_UPDATE",
            json!({"id": "999", "username": "someone else"}),
        ),
        Dispatch::Resumed,
    ] {
        assert!(s.apply(&e).is_empty(), "{e:?}");
    }
    assert_eq!(s, before);
}

#[test]
fn huge_messages_stay_within_the_byte_budget() {
    use inlet_state::limits::MAX_WINDOW_BYTES;
    let mut s = state();
    s.apply_messages(ch(GENERAL), MessagesPage::latest(vec![], 50));
    let big = "x".repeat(200 * 1024);
    for i in 0..40 {
        s.apply(&create(
            mid(1 + i, 0),
            GENERAL,
            OTHER,
            json!({"content": big}),
        ));
    }
    let w = s.messages(ch(GENERAL)).unwrap();
    assert!(w.approx_bytes() <= MAX_WINDOW_BYTES);
    assert!(w.len() > 1 && w.len() < 40);
    assert!(w.has_more_before());
    assert_eq!(w.newest_id(), Some(mid(40, 0)));
    // A single message over the per-message cap is kept as a stub, and still counts as unread.
    let huge = "x".repeat(300 * 1024);
    s.apply(&create(
        mid(100, 0),
        GENERAL,
        OTHER,
        json!({"content": huge, "mention_everyone": true}),
    ));
    let stub = s.messages(ch(GENERAL)).unwrap().get(mid(100, 0)).unwrap();
    assert!(inlet_state::messages::is_oversized(stub) && stub.content.is_empty());
    assert_eq!(s.unread(ch(GENERAL)).mentions, 1);
}

fn window_ids(s: &AccountState, c: &str) -> Vec<MessageId> {
    ids(s, ch(c))
}

#[test]
fn create_and_delete_during_the_first_load_are_kept() {
    let mut s = state();
    let c = s.begin_load(ch(GENERAL));
    assert!(c.messages.contains(&ch(GENERAL)));
    assert!(!s.messages(ch(GENERAL)).unwrap().has_newest());
    // While GET /messages is in flight: a new message, a delete and an edit.
    s.apply(&create(mid(10, 0), GENERAL, OTHER, json!({})));
    s.apply(&d(
        "MESSAGE_DELETE",
        json!({"id": mid(2, 0).to_string(), "channel_id": GENERAL}),
    ));
    s.apply(&d(
        "MESSAGE_UPDATE",
        message_json(
            mid(3, 0),
            GENERAL,
            OTHER,
            json!({"content": "edited", "edited_timestamp": "2026-01-01T01:30:00Z"}),
        ),
    ));
    assert_eq!(s.messages(ch(GENERAL)).unwrap().pending_len(), 2);
    // The page was read before all of it.
    let page = vec![
        message(mid(3, 0), GENERAL, OTHER),
        message(mid(2, 0), GENERAL, OTHER),
        message(mid(1, 0), GENERAL, OTHER),
    ];
    s.apply_messages(ch(GENERAL), MessagesPage::latest(page, 50));
    assert_eq!(
        window_ids(&s, GENERAL),
        vec![mid(1, 0), mid(3, 0), mid(10, 0)]
    );
    let w = s.messages(ch(GENERAL)).unwrap();
    assert_eq!(w.get(mid(3, 0)).unwrap().content, "edited");
    assert!(w.has_newest() && w.pending_len() == 0);
}

#[test]
fn delete_during_a_refetch_after_ready_stays_deleted() {
    let mut s = state();
    let first = vec![
        message(mid(3, 0), GENERAL, OTHER),
        message(mid(2, 0), GENERAL, OTHER),
        message(mid(1, 0), GENERAL, OTHER),
    ];
    s.apply_messages(ch(GENERAL), MessagesPage::latest(first.clone(), 50));
    s.apply(&Dispatch::Ready(Box::new(ready())));
    assert!(!s.messages(ch(GENERAL)).unwrap().has_newest());
    // Between READY and the refetch: one delete, one new message.
    s.apply(&d(
        "MESSAGE_DELETE",
        json!({"id": mid(2, 0).to_string(), "channel_id": GENERAL}),
    ));
    s.apply(&create(mid(5, 0), GENERAL, OTHER, json!({})));
    s.apply_messages(ch(GENERAL), MessagesPage::latest(first, 50));
    assert_eq!(
        window_ids(&s, GENERAL),
        vec![mid(1, 0), mid(3, 0), mid(5, 0)]
    );
    assert!(s.messages(ch(GENERAL)).unwrap().has_newest());
}

#[test]
fn an_overflowing_live_buffer_asks_for_another_page() {
    use inlet_state::limits::MAX_PENDING_LIVE;
    let mut s = state();
    s.begin_load(ch(GENERAL));
    let n = MAX_PENDING_LIVE as u64 + 5;
    for i in 0..n {
        s.apply(&create(mid(10 + i, 0), GENERAL, OTHER, json!({})));
    }
    s.apply_messages(
        ch(GENERAL),
        MessagesPage::latest(vec![message(mid(1, 0), GENERAL, OTHER)], 50),
    );
    let w = s.messages(ch(GENERAL)).unwrap();
    assert!(!w.has_newest(), "the app must load the newest page again");
    assert_eq!(window_ids(&s, GENERAL), vec![mid(1, 0)]);
    // The next newest page completes the window.
    let page: Vec<_> = (0..50)
        .rev()
        .map(|i| message(mid(10 + n - 50 + i, 0), GENERAL, OTHER))
        .collect();
    s.apply_messages(ch(GENERAL), MessagesPage::latest(page, 50));
    assert!(s.messages(ch(GENERAL)).unwrap().has_newest());
}

#[test]
fn a_hidden_channel_keeps_its_ack_and_takes_its_threads() {
    let mut s = state();
    s.apply(&create(mid(1, 0), GENERAL, OTHER, json!({})));
    s.mark_read(ch(GENERAL), None, AckKind::Explicit);
    s.apply(&d(
        "CHANNEL_CREATE",
        json!({"id": "304", "type": 11, "guild_id": GUILD, "parent_id": GENERAL}),
    ));
    assert!(s.channel(ch("304")).is_some());
    // Access lost: Fluxer deletes the channel; its thread goes with it.
    let c = s.apply(&d(
        "CHANNEL_DELETE",
        json!({"id": GENERAL, "type": 0, "guild_id": GUILD}),
    ));
    assert!(c.channels.contains(&ch("304")));
    assert!(s.channel(ch("304")).is_none());
    assert!(s
        .guild(g())
        .unwrap()
        .channels
        .iter()
        .all(|c| *c != ch("304")));
    assert_eq!(s.permissions(g(), ch("304")), Permissions::NONE);
    // Access regained: the ack comes back, so the channel is not unread.
    s.apply(&d(
        "CHANNEL_CREATE",
        json!({"id": GENERAL, "type": 0, "guild_id": GUILD,
        "last_message_id": mid(1, 0).to_string()}),
    ));
    assert_eq!(
        s.read_state(ch(GENERAL)).unwrap().ack_message_id,
        Some(mid(1, 0))
    );
    assert_eq!(s.unread(ch(GENERAL)), Unread::default());
}

#[test]
fn changes_name_the_channels_and_guilds() {
    let mut s = state();
    let c = s.apply(&create(mid(1, 0), GENERAL, OTHER, json!({})));
    assert!(c.read_state_channels.contains(&ch(GENERAL)));
    assert!(c.read_state_guilds.contains(&Some(g())) && !c.read_state_guilds.contains(&None));
    let c = s.apply(&d(
        "GUILD_ROLE_UPDATE",
        json!({"guild_id": GUILD, "role": role(GUILD, VIEW_CHANNEL, 0)}),
    ));
    assert!(c.permission_guilds.contains(&g()));
    let c = s.apply(&d(
        "GUILD_MEMBER_ADD",
        json!({"guild_id": GUILD, "user": {"id": "21", "username": "bo"}, "roles": []}),
    ));
    assert!(c.member_guilds.contains(&g()) && c.members);
    // A settings change can touch every channel: the lists say "all".
    let c = s.apply(&d(
        "USER_GUILD_SETTINGS_UPDATE",
        json!({"guild_id": GUILD, "muted": true}),
    ));
    assert!(c.read_state_guilds.is_all() && c.read_state_guilds.contains(&Some(g())));
}

#[test]
fn a_message_member_never_overwrites_the_own_member() {
    let mut s = state();
    // A replayed message carries an old member without the mod role.
    s.apply(&create(
        mid(1, 0),
        GENERAL,
        ME,
        json!({"member": {"roles": [], "nick": "old"}}),
    ));
    let me = s.member(g(), uid(ME)).unwrap();
    assert_eq!(me.roles, vec![MOD_ROLE.to_string()]);
    assert!(me.nick.is_none());
    // A missing member is filled in, and not replaced by a later copy.
    s.apply(&create(
        mid(2, 0),
        GENERAL,
        OTHER,
        json!({"member": {"roles": [OTHER_ROLE], "nick": "ana"}}),
    ));
    assert_eq!(
        s.member(g(), uid(OTHER)).unwrap().nick.as_deref(),
        Some("ana")
    );
    s.apply(&create(
        mid(3, 0),
        GENERAL,
        OTHER,
        json!({"member": {"roles": [], "nick": "older"}}),
    ));
    assert_eq!(
        s.member(g(), uid(OTHER)).unwrap().nick.as_deref(),
        Some("ana")
    );
}

#[test]
fn a_thread_with_a_parent_outside_the_guild_gets_nothing() {
    let mut s = state();
    s.apply(&d(
        "CHANNEL_CREATE",
        json!({"id": "305", "type": 11, "guild_id": GUILD, "parent_id": DM}),
    ));
    s.apply(&d(
        "CHANNEL_CREATE",
        json!({"id": "306", "type": 11, "guild_id": GUILD, "parent_id": GENERAL}),
    ));
    assert_eq!(s.permissions(g(), ch("305")), Permissions::NONE);
    assert!(s.permissions(g(), ch("306")).can_view());
}

#[test]
fn a_large_guild_defaults_to_only_mentions() {
    use inlet_state::guild_settings::notifications::{ALL_MESSAGES, ONLY_MENTIONS};
    let mut gr = guild_ready(default_roles(), default_channels(), &[MOD_ROLE]);
    gr["member_count"] = json!(300);
    let r = ready_from(ready_json(gr, json!([]), json!([])));
    let s = AccountState::from_ready_at(&r, JOINED_MS as i64);
    let guild = s.guild(g()).unwrap();
    assert!(guild.is_large());
    assert_eq!(
        s.guild_settings()
            .guild_notifications(Some(g()), guild.default_notifications()),
        ONLY_MENTIONS
    );
    let small = state();
    let guild = small.guild(g()).unwrap();
    assert_eq!(
        small
            .guild_settings()
            .guild_notifications(Some(g()), guild.default_notifications()),
        ALL_MESSAGES
    );
}

#[test]
fn oversized_objects_are_trimmed_and_overwrite_floods_hide_the_channel() {
    let mut s = state();
    s.apply(&d(
        "GUILD_MEMBER_ADD",
        json!({"guild_id": GUILD,
        "user": {"id": "21", "username": "bo", "junk": "x".repeat(100_000)},
        "roles": [], "junk": "y".repeat(100_000)}),
    ));
    let m = s.member(g(), uid("21")).unwrap();
    assert!(m.extra.is_empty() && m.user.as_ref().unwrap().extra.is_empty());
    assert!(s.user(uid("21")).unwrap().extra.is_empty());
    let ow: Vec<_> = (0..inlet_state::limits::MAX_OVERWRITES + 1)
        .map(|i| json!({"id": (50_000 + i).to_string(), "type": 1, "allow": "0", "deny": "0"}))
        .collect();
    s.apply(&d(
        "CHANNEL_UPDATE",
        json!({"id": RANDOM, "type": 0, "guild_id": GUILD, "permission_overwrites": ow}),
    ));
    assert!(
        !s.permissions(g(), ch(RANDOM)).can_view(),
        "an unreadable overwrite list fails closed"
    );
}

#[test]
fn the_member_cache_has_an_account_wide_cap() {
    use inlet_state::limits::{MAX_MEMBERS_PER_GUILD, MAX_MEMBERS_TOTAL};
    let mut s = state();
    let guilds = MAX_MEMBERS_TOTAL / MAX_MEMBERS_PER_GUILD + 2;
    for gi in 0..guilds as u64 {
        let gid = (60_000 + gi).to_string();
        s.apply(&d(
            "GUILD_CREATE",
            json!({"id": gid, "roles": [role(&gid, VIEW_CHANNEL, 0)],
            "members": [{"user": {"id": ME}, "roles": []}]}),
        ));
        let members: Vec<_> = (0..MAX_MEMBERS_PER_GUILD as u64)
            .map(|i| json!({"user": {"id": (1_000_000 + gi * 10_000 + i).to_string(), "username": "m"}, "roles": []}))
            .collect();
        s.apply(&d(
            "GUILD_MEMBERS_CHUNK",
            json!({"guild_id": gid, "members": members}),
        ));
    }
    let total: usize = s.guilds().map(|g| g.members.others_len()).sum();
    assert_eq!(total, MAX_MEMBERS_TOTAL);
    assert!(s.guilds().all(|g| g.members.me().is_some()));
}

#[test]
fn many_overwrites_stay_visible_and_too_many_hide_the_channel() {
    let mut s = state();
    // 300 member overwrites for other users, plus one that denies sending to the account.
    let mut ow: Vec<_> = (0..300)
        .map(|i| json!({"id": (50_000 + i).to_string(), "type": 1, "allow": "0", "deny": VIEW_CHANNEL.to_string()}))
        .collect();
    ow.push(json!({"id": ME, "type": 1, "allow": "0", "deny": SEND_MESSAGES.to_string()}));
    s.apply(&d(
        "CHANNEL_UPDATE",
        json!({"id": RANDOM, "type": 0, "guild_id": GUILD, "permission_overwrites": ow}),
    ));
    let p = s.permissions(g(), ch(RANDOM));
    assert!(p.can_view() && !p.has(SEND_MESSAGES));
    assert_eq!(s.channels().overwrites(ch(RANDOM)).unwrap().len(), 301);
    // The stored channel carries an unreadable marker, so running the
    // protocol calculator on it directly fails closed.
    assert_eq!(
        s.channel(ch(RANDOM)).unwrap().extra["permission_overwrites"],
        json!(false)
    );

    // Fluxer's limit is 10,000; one more hides the channel.
    let ow: Vec<_> = (0..inlet_state::limits::MAX_OVERWRITES as u64 + 1)
        .map(|i| json!({"id": (50_000 + i).to_string(), "type": 1, "allow": "0", "deny": "0"}))
        .collect();
    s.apply(&d(
        "CHANNEL_UPDATE",
        json!({"id": RANDOM, "type": 0, "guild_id": GUILD, "permission_overwrites": ow}),
    ));
    assert!(s.channels().overwrites(ch(RANDOM)).is_none());
    assert!(!s.permissions(g(), ch(RANDOM)).can_view());
}

#[test]
fn reactions_while_loading_are_applied_after_the_page() {
    let mut s = state();
    s.begin_load(ch(GENERAL));
    let react = |name: &str, user: &str| {
        d(
            name,
            json!({"user_id": user, "channel_id": GENERAL,
        "message_id": mid(1, 0).to_string(), "emoji": {"name": "🦔"}}),
        )
    };
    s.apply(&react("MESSAGE_REACTION_ADD", OTHER));
    s.apply(&react("MESSAGE_REACTION_ADD", ME));
    s.apply(&react("MESSAGE_REACTION_ADD", ME));
    s.apply_messages(
        ch(GENERAL),
        MessagesPage::latest(vec![message(mid(1, 0), GENERAL, OTHER)], 50),
    );
    let r = &s
        .messages(ch(GENERAL))
        .unwrap()
        .get(mid(1, 0))
        .unwrap()
        .reactions()[0];
    assert_eq!((r.count, r.me), (2, true));
}

#[test]
fn too_many_deletes_while_loading_ask_for_another_page() {
    use inlet_state::limits::MAX_RECENTLY_DELETED;
    let mut s = state();
    s.begin_load(ch(GENERAL));
    for i in 0..(MAX_RECENTLY_DELETED as u64 + 1) {
        s.apply(&d(
            "MESSAGE_DELETE",
            json!({"id": mid(1, i).to_string(), "channel_id": GENERAL}),
        ));
    }
    // The first delete was forgotten; its message is in the page.
    s.apply_messages(
        ch(GENERAL),
        MessagesPage::latest(vec![message(mid(1, 0), GENERAL, OTHER)], 50),
    );
    assert!(
        !s.messages(ch(GENERAL)).unwrap().has_newest(),
        "load the newest page again"
    );
}

#[test]
fn loads_are_never_refused_and_end_by_page_timeout_or_ready() {
    use inlet_state::limits::LOAD_TIMEOUT_MS;
    let count = MAX_MESSAGE_WINDOWS + 2;
    let channels: Vec<_> = (0..count)
        .map(|i| channel(&(1000 + i).to_string(), 0, None, i as i64, json!([])))
        .collect();
    let r = ready_from(ready_json(
        guild_ready(default_roles(), json!(channels), &[]),
        json!([]),
        json!([]),
    ));
    let start = JOINED_MS as i64;
    let mut s = AccountState::from_ready_at(&r, start);
    let id = |i: usize| ch(&(1000 + i).to_string());
    // 31 loads whose pages never arrive: the newest still opens a window,
    // and the least recently used loading window makes room.
    for i in 0..=MAX_MESSAGE_WINDOWS {
        s.begin_load(id(i));
    }
    let w = s
        .messages(id(MAX_MESSAGE_WINDOWS))
        .expect("the newest load works");
    assert!(w.is_loading());
    assert!(s.messages(id(0)).is_none());
    // An idle window goes before a loading one.
    s.cancel_load(id(5));
    let c = s.begin_load(id(MAX_MESSAGE_WINDOWS + 1));
    assert!(c.messages.contains(&id(5)) && s.messages(id(5)).is_none());
    assert!(s.messages(id(1)).unwrap().is_loading());
    // A page that lands ends the loading state.
    s.apply_messages(id(1), MessagesPage::latest(vec![], 50));
    assert!(!s.messages(id(1)).unwrap().is_loading());
    // Without a page, the load ends after the timeout.
    let c = s.tick(start + LOAD_TIMEOUT_MS - 1);
    assert!(c.messages.is_empty());
    let c = s.tick(start + LOAD_TIMEOUT_MS);
    assert!(c.messages.contains(&id(2)));
    assert!(!s.messages(id(2)).unwrap().is_loading());
    // A new READY ends every load.
    s.begin_load(id(2));
    s.apply(&Dispatch::Ready(Box::new(r.clone())));
    assert!(s
        .message_windows()
        .channels()
        .all(|c| !s.messages(c).unwrap().is_loading()));
}

#[test]
fn a_window_scrolled_back_applies_reactions_at_once() {
    let mut s = state();
    let page: Vec<_> = (0..50)
        .rev()
        .map(|i| message(mid(100 + i, 0), GENERAL, OTHER))
        .collect();
    s.apply_messages(ch(GENERAL), MessagesPage::latest(page, 50));
    for i in 0..(MAX_MESSAGES_PER_CHANNEL as u64 - 50) {
        s.apply(&create(mid(200 + i, 0), GENERAL, OTHER, json!({})));
    }
    // An older page pushes the newest messages out: scrolled back, not stale.
    let older: Vec<_> = (0..50)
        .rev()
        .map(|i| message(mid(10 + i, 0), GENERAL, OTHER))
        .collect();
    s.apply_messages(ch(GENERAL), MessagesPage::before(mid(100, 0), older, 50));
    let w = s.messages(ch(GENERAL)).unwrap();
    assert!(!w.has_newest() && !w.is_loading());
    let c = s.apply(&d(
        "MESSAGE_REACTION_ADD",
        json!({"user_id": OTHER, "channel_id": GENERAL,
        "message_id": mid(10, 0).to_string(), "emoji": {"name": "🦔"}}),
    ));
    assert!(c.messages.contains(&ch(GENERAL)));
    let m = s.messages(ch(GENERAL)).unwrap().get(mid(10, 0)).unwrap();
    assert_eq!(m.reactions()[0].count, 1);
}

#[test]
fn guild_record_changes_refresh_the_pill() {
    let mut s = state();
    let c = s.apply(&d(
        "GUILD_UPDATE",
        json!({"id": GUILD, "name": "Hedgerow", "owner_id": OWNER,
        "default_message_notifications": 1}),
    ));
    assert!(c.read_state_guilds.contains(&Some(g())));
    let c = s.apply(&d("GUILD_MEMBER_UPDATE", json!({"guild_id": GUILD,
        "user": {"id": ME, "username": "me"}, "roles": [MOD_ROLE], "joined_at": "2026-01-02T00:00:00Z"})));
    assert!(c.read_state_guilds.contains(&Some(g())) && c.read_state_channels.is_all());
    let c = s.apply(&d(
        "GUILD_CREATE",
        guild_ready(default_roles(), default_channels(), &[MOD_ROLE]),
    ));
    assert!(c.read_state_guilds.contains(&Some(g())));
}
