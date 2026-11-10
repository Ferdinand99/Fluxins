//! Size caps per kind of object.
//!
//! Server content is untrusted (rule 5), and the protocol crate keeps every
//! unknown field in `extra` as raw JSON. Count caps alone multiply into large
//! numbers, so every object is also trimmed to a byte budget before it is
//! stored: long strings are cut, unknown `extra` keys are dropped when they
//! are over the budget, and lists that matter for permissions fail closed
//! instead of being cut.

use inlet_protocol::models::{CustomStatus, GuildEmoji, GuildSticker};
use inlet_protocol::{Channel, Guild, GuildMember, Presence, Role, User};
use serde_json::{Map, Value};

use crate::limits::{
    MAX_CHANNEL_EXTRA_BYTES, MAX_GUILD_EXTRA_BYTES, MAX_MEMBER_EXTRA_BYTES, MAX_ROLES_PER_MEMBER,
    MAX_ROLE_EXTRA_BYTES, MAX_USER_EXTRA_BYTES,
};

/// Approximate heap size of a JSON value. It undercounts the real use of
/// `serde_json` maps (about 2×), which is fine for a relative cap.
pub(crate) fn value_size(v: &Value) -> usize {
    match v {
        Value::String(s) => 24 + s.len(),
        Value::Array(a) => 24 + a.iter().map(value_size).sum::<usize>(),
        Value::Object(o) => 48 + map_size(o),
        _ => 16,
    }
}

/// Approximate heap size of a JSON object's entries.
pub(crate) fn map_size(o: &Map<String, Value>) -> usize {
    o.iter().map(|(k, v)| 24 + k.len() + value_size(v)).sum()
}

pub(crate) fn opt_len(s: &Option<String>) -> usize {
    s.as_ref().map_or(0, String::len)
}

/// Cuts `s` to at most `max` bytes, on a character boundary.
pub(crate) fn truncate(s: &mut String, max: usize) {
    if s.len() > max {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
}

fn truncate_opt(s: &mut Option<String>, max: usize) {
    if let Some(s) = s {
        truncate(s, max);
    }
}

/// Keeps only `keep` keys of `extra` when it is over `budget`.
fn trim_extra(extra: &mut Map<String, Value>, budget: usize, keep: &[&str]) {
    if map_size(extra) > budget {
        extra.retain(|k, _| keep.contains(&k.as_str()));
        // Even the kept keys can be huge: drop them too then.
        if map_size(extra) > budget {
            extra.clear();
        }
    }
}

const NAME: usize = 512;
const HASH: usize = 128;
const TEXT: usize = 4096;

pub(crate) fn cap_user(u: &mut User) {
    truncate(&mut u.username, NAME);
    truncate(&mut u.discriminator, 16);
    truncate_opt(&mut u.global_name, NAME);
    truncate_opt(&mut u.avatar, HASH);
    trim_extra(
        &mut u.extra,
        MAX_USER_EXTRA_BYTES,
        &["mfa_enabled", "accent_color", "banner", "verified"],
    );
}

/// `own` keeps every role of the account's own member (cutting them would
/// drop role overwrites that deny).
pub(crate) fn cap_member(m: &mut GuildMember, own: bool) {
    if let Some(u) = &mut m.user {
        cap_user(u);
    }
    truncate_opt(&mut m.nick, NAME);
    truncate_opt(&mut m.avatar, HASH);
    truncate_opt(&mut m.banner, HASH);
    truncate_opt(&mut m.joined_at, 64);
    truncate_opt(&mut m.communication_disabled_until, 64);
    if !own {
        m.roles.truncate(MAX_ROLES_PER_MEMBER);
    }
    trim_extra(&mut m.extra, MAX_MEMBER_EXTRA_BYTES, &[]);
}

fn cap_custom_status(s: &mut CustomStatus) {
    truncate_opt(&mut s.text, 512);
    truncate_opt(&mut s.emoji_name, 64);
    truncate_opt(&mut s.expires_at, 64);
}

pub(crate) fn cap_presence(p: &mut Presence) {
    truncate(&mut p.status, 32);
    if let Some(s) = &mut p.custom_status {
        cap_custom_status(s);
    }
}

pub(crate) fn cap_role(r: &mut Role) {
    truncate(&mut r.name, NAME);
    truncate_opt(&mut r.icon, HASH);
    truncate_opt(&mut r.unicode_emoji, 64);
    trim_extra(&mut r.extra, MAX_ROLE_EXTRA_BYTES, &[]);
}

/// Trims a channel. The permission overwrites are budgeted on their own
/// (by count, [`crate::limits::MAX_OVERWRITES`], and stored compactly by
/// [`crate::channels::Channels`]), so a channel with many overwrites keeps
/// them. When the rest of `extra` must be dropped, a thread keeps fail-closed
/// thread metadata (locked and archived) and loses its membership.
pub(crate) fn cap_channel(c: &mut Channel) {
    truncate_opt(&mut c.name, NAME);
    truncate_opt(&mut c.topic, TEXT);
    c.recipients.truncate(32);
    for u in &mut c.recipients {
        cap_user(u);
    }
    let overwrites = c.extra.remove("permission_overwrites");
    let had_thread_metadata = c.extra.contains_key("thread_metadata");
    trim_extra(
        &mut c.extra,
        MAX_CHANNEL_EXTRA_BYTES,
        &[
            "last_pin_timestamp",
            "flags",
            "thread_metadata",
            "member",
            "owner_id",
        ],
    );
    if had_thread_metadata && !c.extra.contains_key("thread_metadata") {
        c.extra.insert(
            "thread_metadata".into(),
            serde_json::json!({"locked": true, "archived": true}),
        );
    }
    if let Some(ow) = overwrites {
        c.extra.insert("permission_overwrites".into(), ow);
    }
}

pub(crate) fn cap_guild(g: &mut Guild) {
    truncate(&mut g.name, NAME);
    truncate_opt(&mut g.icon, HASH);
    g.features.truncate(64);
    for f in &mut g.features {
        truncate(f, 64);
    }
    trim_extra(
        &mut g.extra,
        MAX_GUILD_EXTRA_BYTES,
        &[
            "mfa_level",
            "default_message_notifications",
            "joined_at",
            "banner",
            "splash",
        ],
    );
}

pub(crate) fn cap_emoji(e: &mut GuildEmoji) {
    truncate(&mut e.name, 64);
    if let Some(u) = &mut e.user {
        cap_user(u);
    }
}

pub(crate) fn cap_sticker(s: &mut GuildSticker) {
    truncate(&mut s.name, 64);
    truncate(&mut s.description, 512);
    s.tags.truncate(16);
    for t in &mut s.tags {
        truncate(t, 64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn caps_trim_strings_and_extra_but_keep_overwrites() {
        let mut s = "aé".repeat(10);
        truncate(&mut s, 2);
        assert_eq!(s, "a");

        let mut u: User = serde_json::from_value(json!({
            "id": "1", "username": "x".repeat(10_000),
            "mfa_enabled": true, "junk": "y".repeat(100_000)
        }))
        .unwrap();
        cap_user(&mut u);
        assert_eq!(u.username.len(), NAME);
        assert!(!u.extra.contains_key("junk"));
        assert_eq!(u.extra.get("mfa_enabled"), Some(&json!(true)));

        // Overwrites are not part of the channel budget; thread metadata
        // that has to go is replaced by a locked, archived marker.
        let ow: Vec<Value> = (0..300)
            .map(|i| json!({"id": (1000 + i).to_string(), "type": 0, "allow": "0", "deny": "0"}))
            .collect();
        let mut c: Channel = serde_json::from_value(json!({
            "id": "5", "type": 11, "guild_id": "1", "permission_overwrites": ow,
            "thread_metadata": {"locked": false, "pad": "q".repeat(100_000)},
            "huge": "z".repeat(200_000)
        }))
        .unwrap();
        cap_channel(&mut c);
        assert!(!c.extra.contains_key("huge"));
        assert_eq!(
            c.extra["permission_overwrites"].as_array().unwrap().len(),
            300
        );
        assert_eq!(
            c.extra["thread_metadata"],
            json!({"locked": true, "archived": true})
        );
    }
}
