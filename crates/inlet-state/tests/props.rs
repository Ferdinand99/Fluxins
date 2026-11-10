//! Property tests: random event sequences (with duplicates, events out of
//! order, unknown and malformed events) never panic, keep every cap, keep
//! the permission cache equal to a fresh computation, and are deterministic.
//!
//! The generator is a seeded xorshift, so every failure names its seed.

mod common;

use common::*;
use inlet_protocol::permissions::*;
use inlet_protocol::{Dispatch, PermissionContext};
use inlet_state::limits::*;
use inlet_state::{AccountState, ChannelId, GuildId, MessageId, MessagesPage, Permissions};
use serde_json::{json, Value};

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len() as u64) as usize]
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

const GUILDS: &[&str] = &["100", "101", "4242"];
const CHANNELS: &[&str] = &["300", "301", "302", "303", "310", "311", "900", "999"];
const USERS: &[&str] = &["10", "20", "21", "22"];
const ROLES: &[&str] = &["100", "101", "7", "8", "9"];

#[derive(Debug, Clone)]
enum Op {
    Event(Dispatch),
    Page(ChannelId, MessagesPage),
    Read(ChannelId, Option<MessageId>),
    Tick(i64),
}

fn perms(r: &mut Rng) -> u64 {
    let bits = [
        VIEW_CHANNEL,
        SEND_MESSAGES,
        ADMINISTRATOR,
        MANAGE_MESSAGES,
        ADD_REACTIONS,
        SEND_MESSAGES_IN_THREADS,
    ];
    bits.iter().filter(|_| r.chance(35)).fold(0, |a, b| a | b)
}

fn overwrites(r: &mut Rng) -> Value {
    let mut v = vec![];
    for _ in 0..r.below(4) {
        let id = if r.chance(50) {
            r.pick(ROLES)
        } else {
            r.pick(USERS)
        };
        let kind = if r.chance(85) { r.below(2) as i64 } else { 7 };
        v.push(json!({"id": id, "type": kind, "allow": perms(r).to_string(), "deny": perms(r).to_string()}));
    }
    if r.chance(3) {
        return json!({"broken": true});
    }
    json!(v)
}

fn guild_of(channel: &str) -> Option<&'static str> {
    match channel {
        "300" | "301" | "302" | "303" => Some("100"),
        "310" | "311" => Some("101"),
        _ => None,
    }
}

fn channel_json(r: &mut Rng, id: &str) -> Value {
    let guild = guild_of(id);
    let kind = match (guild, id) {
        (None, _) => 1,
        (_, "300") => 4,
        (_, "303") => *[0i64, 11, 12].get(r.below(3) as usize).unwrap(),
        _ => 0,
    };
    let mut c = json!({"id": id, "type": kind, "position": r.below(5), "permission_overwrites": overwrites(r)});
    if let Some(g) = guild {
        c["guild_id"] = json!(if r.chance(90) { g } else { "100" });
        c["parent_id"] = json!(if id == "303" { "301" } else { "300" });
    }
    if r.chance(50) {
        c["last_message_id"] = json!(mid(r.below(60), r.below(3)).to_string());
    }
    c
}

fn member_json(r: &mut Rng, guild: &str, user: &str) -> Value {
    let roles: Vec<&str> = ROLES.iter().copied().filter(|_| r.chance(30)).collect();
    let mut m = json!({"guild_id": guild, "user": {"id": user, "username": format!("u{user}")}, "roles": roles});
    if r.chance(15) {
        // A timeout that ends somewhere in the next two hours.
        let end = JOINED_MS + 3_600_000 + r.below(7_200) * 1000;
        m["communication_disabled_until"] = json!(iso(end));
    }
    m
}

/// Unix ms to ISO 8601 (UTC), days after 2026-01-01 only.
fn iso(ms: u64) -> String {
    let secs = (ms - JOINED_MS) / 1000;
    let (day, rem) = (secs / 86_400, secs % 86_400);
    format!(
        "2026-01-{:02}T{:02}:{:02}:{:02}Z",
        1 + day.min(27),
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

fn message_value(r: &mut Rng, channel: &str) -> Value {
    let id = mid(r.below(60), r.below(3));
    let author = r.pick(USERS);
    let mut extra = json!({});
    if r.chance(25) {
        extra["mentions"] = json!([{"id": r.pick(USERS), "username": "x"}]);
    }
    if r.chance(15) {
        extra["mention_everyone"] = json!(true);
    }
    if r.chance(15) {
        extra["mention_roles"] = json!([r.pick(ROLES)]);
    }
    if r.chance(10) {
        extra["edited_timestamp"] = json!(iso(JOINED_MS + r.below(3600) * 1000));
    }
    message_json(id, channel, author, extra)
}

fn guild_create(r: &mut Rng, guild: &str) -> Value {
    let mut roles = vec![];
    if r.chance(80) {
        roles.push(role(guild, perms(r), 0));
    }
    for id in ["7", "8", "9"] {
        if r.chance(50) {
            roles.push(role(id, perms(r), r.below(5) as i64));
        }
    }
    let mut channels: Vec<Value> = vec![];
    for c in CHANNELS {
        if guild_of(c) == Some(guild) && r.chance(85) {
            channels.push(channel_json(r, c));
        }
    }
    let mut members = vec![];
    if r.chance(85) {
        members.push(member_json(r, guild, ME));
    }
    json!({
        "id": guild, "unavailable": r.chance(5), "visiting": r.chance(10),
        "properties": {"id": guild, "name": "g", "owner_id": r.pick(&["1", ME]), "mfa_level": r.below(2)},
        "roles": roles, "channels": channels, "members": members,
    })
}

fn op(r: &mut Rng) -> Op {
    let channel = r.pick(CHANNELS);
    let guild = r.pick(GUILDS);
    let user = r.pick(USERS);
    let msg = mid(r.below(60), r.below(3)).to_string();
    let event = |name: &str, data: Value| Op::Event(d(name, data));
    match r.below(30) {
        0..=4 => event("MESSAGE_CREATE", message_value(r, channel)),
        5 => event("MESSAGE_UPDATE", message_value(r, channel)),
        6 => event("MESSAGE_DELETE", json!({"id": msg, "channel_id": channel})),
        7 => event(
            "MESSAGE_DELETE_BULK",
            json!({"ids": [msg, mid(r.below(60), 0).to_string()], "channel_id": channel}),
        ),
        8 => event(
            "TYPING_START",
            json!({"channel_id": channel, "user_id": user, "timestamp": 1}),
        ),
        9 => event(
            "MESSAGE_ACK",
            json!({"channel_id": channel, "message_id": msg,
            "mention_count": r.below(4), "manual": r.chance(20)}),
        ),
        10 => {
            let name = r.pick(&["MESSAGE_REACTION_ADD", "MESSAGE_REACTION_REMOVE"]);
            event(
                name,
                json!({"user_id": user, "channel_id": channel, "message_id": msg,
                "emoji": {"name": r.pick(&["👍", "🦔"])}}),
            )
        }
        11 => event(
            r.pick(&[
                "MESSAGE_REACTION_REMOVE_ALL",
                "MESSAGE_REACTION_REMOVE_EMOJI",
            ]),
            json!({"channel_id": channel, "message_id": msg, "emoji": {"name": "👍"}}),
        ),
        12 => event(
            r.pick(&["CHANNEL_CREATE", "CHANNEL_UPDATE"]),
            channel_json(r, channel),
        ),
        13 => event(
            "CHANNEL_DELETE",
            json!({"id": channel, "type": 0, "guild_id": guild_of(channel)}),
        ),
        14 => {
            let target = r.pick(&["100", "101"]);
            event("GUILD_CREATE", guild_create(r, target))
        }
        15 => event(
            "GUILD_DELETE",
            json!({"id": guild, "unavailable": r.chance(50)}),
        ),
        16 => event(
            r.pick(&["GUILD_ROLE_CREATE", "GUILD_ROLE_UPDATE"]),
            json!({"guild_id": guild, "role": role(r.pick(ROLES), perms(r), r.below(5) as i64)}),
        ),
        17 => event(
            "GUILD_ROLE_DELETE",
            json!({"guild_id": guild, "role_id": r.pick(ROLES)}),
        ),
        18 => event(
            r.pick(&["GUILD_MEMBER_ADD", "GUILD_MEMBER_UPDATE"]),
            member_json(r, guild, user),
        ),
        19 => event(
            "GUILD_MEMBER_REMOVE",
            json!({"guild_id": guild, "user": {"id": user}}),
        ),
        20 => event(
            "PRESENCE_UPDATE",
            json!({"user": {"id": user}, "status": r.pick(&["online", "idle", "offline"])}),
        ),
        21 => event(
            "PASSIVE_UPDATES",
            json!({"guild_id": guild, "channels": {channel: msg}}),
        ),
        22 => event(
            "USER_GUILD_SETTINGS_UPDATE",
            json!({"guild_id": if r.chance(20) { Value::Null } else { json!(guild) },
            "muted": r.chance(30), "suppress_everyone": r.chance(30),
            "channel_overrides": [{"channel_id": channel, "muted": r.chance(50)}]}),
        ),
        23 => event(
            r.pick(&["SOMETHING_NEW", "MESSAGE_CREATE", "GUILD_CREATE"]),
            json!({"junk": [1, 2]}),
        ),
        24 => {
            if r.chance(10) {
                Op::Event(Dispatch::Ready(Box::new(ready())))
            } else {
                Op::Event(Dispatch::Resumed)
            }
        }
        25 | 26 => {
            let mut msgs = vec![];
            for _ in 0..r.below(6) {
                let target = if r.chance(90) { channel } else { "301" };
                msgs.push(serde_json::from_value(message_value(r, target)).unwrap());
            }
            let before = if r.chance(40) {
                Some(mid(r.below(60), r.below(3)))
            } else {
                None
            };
            Op::Page(
                ChannelId::parse(channel).unwrap(),
                MessagesPage {
                    messages: msgs,
                    before,
                    limit: r.below(8) as u8,
                },
            )
        }
        27 => Op::Read(
            ChannelId::parse(channel).unwrap(),
            r.chance(50).then(|| mid(r.below(60), r.below(3))),
        ),
        28 => Op::Tick(JOINED_MS as i64 + r.below(4 * 3600) as i64 * 1000),
        _ => event(
            "GUILD_MEMBER_LIST_UPDATE",
            json!({"guild_id": guild, "id": channel, "member_count": r.below(5),
            "online_count": 1, "groups": [], "ops": [{"op": "SYNC", "range": [0, 1], "items": [
                {"member": {"user": {"id": user}, "roles": [], "presence": {"status": "online"}}}]}]}),
        ),
    }
}

fn run(s: &mut AccountState, op: &Op) {
    match op {
        Op::Event(e) => {
            s.apply(e);
        }
        Op::Page(c, p) => {
            s.apply_messages(*c, p.clone());
        }
        Op::Read(c, m) => {
            s.mark_read(*c, *m);
        }
        Op::Tick(t) => {
            s.tick(*t);
        }
    }
}

/// The permission cache must equal a fresh computation from the state.
fn expected_permissions(s: &AccountState, g: GuildId, c: ChannelId) -> u64 {
    let Some(guild) = s.guild(g) else { return 0 };
    let Some(channel) = s.channel(c) else {
        return 0;
    };
    if !guild.channels.contains(&c) {
        return 0;
    }
    let me = guild.members.me();
    if !guild.roles_loaded() || guild.left || (me.is_none() && !guild.visiting) {
        return 0;
    }
    let gid = g.to_string();
    let ctx = PermissionContext {
        guild_id: &gid,
        owner_id: guild.owner_id(),
        user_id: &s.me().id,
        roles: guild.roles.as_slice(),
        member_roles: me.map_or(&[][..], |m| m.roles.as_slice()),
        timed_out: me.is_some_and(|m| m.is_timed_out_at(s.now_ms())),
        visiting: guild.visiting,
        mfa_missing: guild
            .properties
            .as_ref()
            .is_some_and(|p| p.mfa_level() == 1)
            && !s.me().mfa_enabled(),
    };
    let parent = if channel.is_thread() {
        s.channels().parent_of(c).and_then(|p| s.channel(p))
    } else {
        None
    };
    ctx.channel(channel, parent)
}

fn check_invariants(s: &AccountState, seed: u64, step: usize) {
    let at = format!("seed {seed} step {step}");
    let windows = s.message_windows();
    assert!(windows.len() <= MAX_MESSAGE_WINDOWS, "{at}");
    for c in windows.channels() {
        assert!(
            s.channel(c).is_some(),
            "{at}: window for unknown channel {c}"
        );
        let w = s.messages(c).unwrap();
        assert!(w.len() <= MAX_MESSAGES_PER_CHANNEL, "{at}");
        let bytes: usize = w.iter().map(inlet_state::messages::approx_size).sum();
        assert_eq!(w.approx_bytes(), bytes, "{at}: byte count drifted");
        let ids: Vec<MessageId> = w.iter().map(|m| MessageId::parse(&m.id).unwrap()).collect();
        assert!(
            ids.windows(2).all(|p| p[0] < p[1]),
            "{at}: window out of order"
        );
        assert!(
            w.iter().all(|m| ChannelId::parse(&m.channel_id) == Some(c)),
            "{at}"
        );
        assert!(
            w.iter().all(|m| m.reactions().iter().all(|r| r.count > 0)),
            "{at}"
        );
    }
    for c in CHANNELS {
        let c = ChannelId::parse(c).unwrap();
        assert!(!s.typing(c).contains(&uid(ME)), "{at}");
        assert!(s.typing(c).len() <= MAX_TYPING_PER_CHANNEL, "{at}");
        if s.channel(c).is_none() {
            assert_eq!(s.channel_permissions(c), Permissions::NONE, "{at}");
        }
        for g in GUILDS {
            let g = GuildId::parse(g).unwrap();
            assert_eq!(
                s.permissions(g, c).bits(),
                expected_permissions(s, g, c),
                "{at}: perms {g}/{c}"
            );
        }
        // Unread never panics and a visited guild never counts.
        let u = s.unread(c);
        if let Some(gs) = s.channels().guild_of(c).and_then(|g| s.guild(g)) {
            if gs.visiting {
                assert!(!u.unread && u.mentions == 0, "{at}");
            }
        }
    }
    for g in s.guilds() {
        assert!(g.members.len() <= MAX_MEMBERS_PER_GUILD + 1, "{at}");
        for c in &g.channels {
            assert_eq!(
                s.channels().guild_of(*c),
                Some(g.id),
                "{at}: channel {c} in the wrong guild"
            );
        }
        let _ = s.guild_unread(g.id);
    }
    let _ = s.private_unread();
    assert!(s.presences().len() <= MAX_PRESENCES, "{at}");
}

fn ops(seed: u64, n: usize) -> Vec<Op> {
    let mut r = Rng::new(seed);
    (0..n).map(|_| op(&mut r)).collect()
}

#[test]
fn random_sequences_keep_invariants_and_never_panic() {
    for seed in 0..400 {
        let mut s = state();
        for (i, o) in ops(seed, 250).iter().enumerate() {
            run(&mut s, o);
            check_invariants(&s, seed, i);
        }
    }
}

#[test]
fn the_same_events_give_the_same_state() {
    for seed in 1000..1060 {
        let seq = ops(seed, 300);
        let (mut a, mut b) = (state(), state());
        for o in &seq {
            run(&mut a, o);
        }
        for o in &seq {
            run(&mut b, o);
        }
        assert_eq!(a, b, "seed {seed}");
    }
}

#[test]
fn repeated_events_are_idempotent() {
    // Every operation applied twice in a row gives the same state as once,
    // except reactions from other users, which Fluxer's events cannot tell apart.
    for seed in 2000..2080 {
        let seq: Vec<Op> = ops(seed, 200)
            .into_iter()
            .filter(|o| {
                !matches!(
                    o,
                    Op::Event(Dispatch::MessageReactionAdd(_) | Dispatch::MessageReactionRemove(_))
                )
            })
            .collect();
        let (mut once, mut twice) = (state(), state());
        for (i, o) in seq.iter().enumerate() {
            run(&mut once, o);
            run(&mut twice, o);
            run(&mut twice, o);
            assert_eq!(once, twice, "seed {seed} step {i}: {o:?}");
        }
    }
}

#[test]
fn shuffled_events_keep_invariants() {
    // The same events in a different order: no panic, every invariant holds.
    for seed in 3000..3060 {
        let mut seq = ops(seed, 200);
        let mut r = Rng::new(seed ^ 0xABCD);
        for i in (1..seq.len()).rev() {
            let j = r.below(i as u64 + 1) as usize;
            seq.swap(i, j);
        }
        let mut s = state();
        for (i, o) in seq.iter().enumerate() {
            run(&mut s, o);
            check_invariants(&s, seed, i);
        }
    }
}

#[test]
fn floods_stay_within_the_caps() {
    let mut s = state();
    // Many members, presences, users and typing users.
    for i in 0..(MAX_MEMBERS_PER_GUILD as u64 + 300) {
        let user = (5000 + i).to_string();
        s.apply(&d(
            "GUILD_MEMBER_ADD",
            json!({"guild_id": GUILD, "user": {"id": user, "username": "x"}, "roles": []}),
        ));
        s.apply(&d(
            "PRESENCE_UPDATE",
            json!({"user": {"id": user}, "status": "online"}),
        ));
        s.apply(&d(
            "TYPING_START",
            json!({"channel_id": GENERAL, "user_id": user}),
        ));
    }
    let guild = s.guild(g()).unwrap();
    assert_eq!(guild.members.len(), MAX_MEMBERS_PER_GUILD + 1);
    assert!(
        guild.members.me().is_some(),
        "the account's own member is never evicted"
    );
    assert!(s.typing(ch(GENERAL)).len() <= MAX_TYPING_PER_CHANNEL);
    for i in 0..(MAX_PRESENCES as u64 + 10) {
        s.apply(&d(
            "PRESENCE_UPDATE",
            json!({"user": {"id": (100_000 + i).to_string()}, "status": "idle"}),
        ));
    }
    assert_eq!(s.presences().len(), MAX_PRESENCES);
    // Many roles and channels in one guild.
    let roles: Vec<Value> = (0..MAX_ROLES_PER_GUILD as u64 + 50)
        .map(|i| role(&(7000 + i).to_string(), 0, 1))
        .collect();
    s.apply(&d(
        "GUILD_ROLE_UPDATE_BULK",
        json!({"guild_id": GUILD, "roles": roles}),
    ));
    assert!(s.guild(g()).unwrap().roles.len() <= MAX_ROLES_PER_GUILD);
    for i in 0..(MAX_CHANNELS_PER_GUILD as u64 + 10) {
        s.apply(&d(
            "CHANNEL_CREATE",
            json!({"id": (20_000 + i).to_string(), "type": 0, "guild_id": GUILD}),
        ));
    }
    assert!(s.guild(g()).unwrap().channels.len() <= MAX_CHANNELS_PER_GUILD);
    // Many reactions on one message.
    s.apply_messages(
        ch(GENERAL),
        MessagesPage::latest(vec![message(mid(1, 0), GENERAL, OTHER)], 50),
    );
    for i in 0..(MAX_REACTIONS_PER_MESSAGE + 20) {
        s.apply(&d("MESSAGE_REACTION_ADD", json!({"user_id": OTHER, "channel_id": GENERAL,
            "message_id": mid(1, 0).to_string(), "emoji": {"id": (1 + i).to_string(), "name": "e"}})));
    }
    let m = s.messages(ch(GENERAL)).unwrap().get(mid(1, 0)).unwrap();
    assert_eq!(m.reactions().len(), MAX_REACTIONS_PER_MESSAGE);
}
