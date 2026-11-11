// SPDX-License-Identifier: AGPL-3.0-or-later
//! Demo accounts for the account switcher: the demo's own account, a second account on
//! the same instance and one on another instance. The other two only send a READY,
//! tagged with their account like a background gateway's events, so the switcher shows
//! their unread messages and mentions. Switching to one shows its guild list (no
//! messages); switching back shows the demo data again.

use inlet_protocol as p;
use serde_json::{json, Value};

use super::{at, message_id, user, user_id, ME};
use crate::backend::accounts::{self, AccountInfo, AccountKey};
use crate::backend::{messages as msgs, Ui};
use crate::model::Guild;

/// The demo instance (the user panel shows it as is).
pub const INSTANCE: &str = "demo";
/// The other instance of the third account.
pub const OTHER_INSTANCE: &str = "https://chat.example.org";

/// The demo's own account.
pub fn main_key() -> AccountKey {
    AccountKey::new(INSTANCE, &user_id(ME)).expect("demo IDs are snowflakes")
}

/// A second account on the demo instance (grace).
fn same_instance_key() -> AccountKey {
    AccountKey::new(INSTANCE, &user_id("grace")).expect("demo IDs are snowflakes")
}

/// An account on another instance; its user ID is the demo account's, which is fine:
/// on another instance it is another account.
fn other_instance_key() -> AccountKey {
    AccountKey::new(OTHER_INSTANCE, &user_id(ME)).expect("demo IDs are snowflakes")
}

/// The accounts the switcher lists, the demo's own first.
pub fn list() -> Vec<AccountInfo> {
    vec![
        AccountInfo {
            key: main_key(),
            name: ME.to_string(),
            handle: ME.to_string(),
        },
        AccountInfo {
            key: same_instance_key(),
            name: "Grace Hopper".to_string(),
            handle: "grace".to_string(),
        },
        AccountInfo {
            key: other_instance_key(),
            name: "Demo at Example".to_string(),
            handle: "demo.example".to_string(),
        },
    ]
}

/// A background account's READY: one guild with an unread channel and a DM, with
/// `mentions` unread mentions in the channel.
fn ready(key: &AccountKey, name: &str, guild: &str, mentions: u64) -> p::Ready {
    let me = json!({
        "id": key.user_id.to_string(),
        "username": name.to_lowercase().replace(' ', "."),
        "global_name": name,
    });
    let guild_id = super::snowflake(&format!("{guild}-{}", key.origin));
    let channel = super::snowflake(&format!("general-{guild}-{}", key.origin));
    let dm = super::snowflake(&format!("dm-{}", key.origin));
    let newest = message_id(at("11:30"), 1);
    let read = message_id(at("09:00"), 1);
    let guilds: Vec<Value> = vec![json!({
        "id": guild_id,
        "properties": {"id": guild_id, "name": guild, "owner_id": key.user_id.to_string()},
        "channels": [{"id": channel, "type": 0, "guild_id": guild_id, "name": "general",
                      "position": 0, "last_message_id": newest}],
        "roles": [{"id": guild_id, "name": "@everyone", "permissions": "2251799813685247", "position": 0}],
        "members": [{"user": me, "roles": [], "joined_at": "2026-01-01T00:00:00+00:00"}],
        "member_count": 3
    })];
    serde_json::from_value(json!({
        "session_id": "demo-background",
        "user": me,
        "guilds": guilds,
        "private_channels": [{"id": dm, "type": 1, "recipients": [user("ada")],
                              "last_message_id": newest}],
        "users": [],
        "read_states": [
            {"id": channel, "last_message_id": read, "mention_count": mentions},
            {"id": dm, "last_message_id": read, "mention_count": 1}
        ],
        "user_guild_settings": [],
    }))
    .expect("the demo background READY is valid")
}

/// The READY of a demo account other than the main one.
fn ready_of(key: &AccountKey) -> Option<p::Ready> {
    if key == &same_instance_key() {
        Some(ready(key, "Grace Hopper", "Compilers", 2))
    } else if key == &other_instance_key() {
        Some(ready(key, "Demo at Example", "Example Club", 0))
    } else {
        None
    }
}

/// The guild list of a READY.
fn guilds_of(ready: &p::Ready) -> Vec<Guild> {
    ready
        .guilds
        .iter()
        .filter_map(|g| {
            Some(Guild {
                id: g.id.clone(),
                name: g.name()?.to_string(),
            })
        })
        .collect()
}

/// Lists the demo accounts and starts the two in the background (their READY, tagged).
pub fn start_background(events: &Ui) {
    let _ = events.unbounded_send(accounts::Ev::Accounts(list()));
    for info in list().into_iter().skip(1) {
        if let Some(ready) = ready_of(&info.key) {
            let _ = events
                .for_account(info.key.clone())
                .unbounded_send(msgs::Ev::Dispatch {
                    dispatch: Box::new(p::Dispatch::Ready(Box::new(ready))),
                    rendered: Vec::new(),
                });
        }
    }
}

/// Switching to a background demo account: what it shows. `None` for the main one.
pub fn switch_to(key: &AccountKey) -> Option<(String, p::Ready, Vec<Guild>)> {
    let info = list().into_iter().find(|a| &a.key == key)?;
    let ready = ready_of(key)?;
    let guilds = guilds_of(&ready);
    Some((info.name, ready, guilds))
}
