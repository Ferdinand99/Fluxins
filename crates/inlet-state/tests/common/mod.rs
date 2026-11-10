//! Fixtures: a small account with one guild and one DM, and event builders.
#![allow(dead_code)]

use inlet_protocol::models::{Message, Ready, FLUXER_EPOCH_MS};
use inlet_protocol::permissions::*;
use inlet_protocol::Dispatch;
use inlet_state::{AccountState, ChannelId, GuildId, MessageId, UserId};
use serde_json::{json, Value};

pub const ME: &str = "10";
pub const OTHER: &str = "20";
pub const GUILD: &str = "100";
pub const OWNER: &str = "1";
pub const MOD_ROLE: &str = "7";
pub const OTHER_ROLE: &str = "8";
pub const CATEGORY: &str = "300";
pub const GENERAL: &str = "301";
pub const RANDOM: &str = "302";
pub const VOICE: &str = "303";
pub const DM: &str = "900";

/// 2026-01-01T00:00:00Z, when the account joined the guild.
pub const JOINED_MS: u64 = 1_767_225_600_000;

pub fn g() -> GuildId {
    GuildId::parse(GUILD).unwrap()
}

pub fn ch(id: &str) -> ChannelId {
    ChannelId::parse(id).unwrap()
}

pub fn uid(id: &str) -> UserId {
    UserId::parse(id).unwrap()
}

/// A message ID created `secs` seconds after the join time (plus `n` for uniqueness).
pub fn mid(secs: u64, n: u64) -> MessageId {
    MessageId(((JOINED_MS + secs * 1000 - FLUXER_EPOCH_MS) << 22) + n)
}

pub fn d(name: &str, data: Value) -> Dispatch {
    Dispatch::parse(name, data)
}

pub fn role(id: &str, perms: u64, position: i64) -> Value {
    json!({"id": id, "name": id, "position": position, "permissions": perms.to_string()})
}

pub fn everyone_perms() -> u64 {
    VIEW_CHANNEL | SEND_MESSAGES | READ_MESSAGE_HISTORY | ADD_REACTIONS
}

pub fn channel(
    id: &str,
    kind: i64,
    parent: Option<&str>,
    position: i64,
    overwrites: Value,
) -> Value {
    json!({"id": id, "type": kind, "parent_id": parent, "position": position, "name": id,
           "permission_overwrites": overwrites})
}

pub fn guild_ready(roles: Value, channels: Value, my_roles: &[&str]) -> Value {
    json!({
        "id": GUILD,
        "properties": {"id": GUILD, "name": "Hedgerow", "owner_id": OWNER, "mfa_level": 0,
                       "default_message_notifications": 0},
        "roles": roles,
        "channels": channels,
        "members": [{"user": {"id": ME, "username": "me"}, "roles": my_roles,
                     "joined_at": "2026-01-01T00:00:00Z"}],
        "member_count": 3,
    })
}

pub fn default_channels() -> Value {
    json!([
        channel(CATEGORY, 4, None, 0, json!([])),
        channel(GENERAL, 0, Some(CATEGORY), 1, json!([])),
        channel(RANDOM, 0, Some(CATEGORY), 2, json!([])),
        channel(VOICE, 2, None, 3, json!([])),
    ])
}

pub fn default_roles() -> Value {
    json!([
        role(GUILD, everyone_perms(), 0),
        role(MOD_ROLE, MANAGE_MESSAGES, 2),
        role(OTHER_ROLE, 0, 1)
    ])
}

pub fn ready_json(guild: Value, read_states: Value, guild_settings: Value) -> Value {
    json!({
        "session_id": "s1",
        "user": {"id": ME, "username": "me", "mfa_enabled": false},
        "guilds": [guild],
        "private_channels": [{"id": DM, "type": 1, "recipients": [{"id": OTHER, "username": "ana"}]}],
        "users": [{"id": OTHER, "username": "ana"}],
        "read_states": read_states,
        "user_guild_settings": guild_settings,
        "presences": [{"user": {"id": OTHER}, "status": "online"}],
    })
}

pub fn ready_from(v: Value) -> Ready {
    serde_json::from_value(v).unwrap()
}

/// The default account: guild with roles loaded, the account has the mod role.
pub fn ready() -> Ready {
    ready_from(ready_json(
        guild_ready(default_roles(), default_channels(), &[MOD_ROLE]),
        json!([]),
        json!([]),
    ))
}

pub fn state() -> AccountState {
    AccountState::from_ready_at(&ready(), JOINED_MS as i64 + 3_600_000)
}

pub fn message_json(id: MessageId, channel: &str, author: &str, extra: Value) -> Value {
    let mut m = json!({
        "id": id.to_string(), "channel_id": channel, "type": 0,
        "author": {"id": author, "username": format!("u{author}")},
        "content": "hi", "timestamp": "2026-01-01T01:00:00Z", "mentions": [],
    });
    if let (Value::Object(m), Value::Object(e)) = (&mut m, extra) {
        m.extend(e);
    }
    m
}

pub fn message(id: MessageId, channel: &str, author: &str) -> Message {
    serde_json::from_value(message_json(id, channel, author, json!({}))).unwrap()
}

pub fn create(id: MessageId, channel: &str, author: &str, extra: Value) -> Dispatch {
    d("MESSAGE_CREATE", message_json(id, channel, author, extra))
}
