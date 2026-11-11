//! What an account in the background costs: a large READY (40 guilds with channels,
//! roles, members and presences, 300 DMs, read states for all of them) built into an
//! account state, measured with a counting allocator, then made passive.
//!
//! One test in this file: the allocator counts every thread of the test binary.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering};

use inlet_protocol::{Dispatch, Ready};
use inlet_state::AccountState;
use serde_json::{json, Value};

struct Counting;

static LIVE: AtomicIsize = AtomicIsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LIVE.fetch_add(layout.size() as isize, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size() as isize, Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        LIVE.fetch_add(
            new_size as isize - layout.size() as isize,
            Ordering::Relaxed,
        );
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

fn live() -> isize {
    LIVE.load(Ordering::Relaxed)
}

const GUILDS: u64 = 40;
const CHANNELS_PER_GUILD: u64 = 60;
const ROLES_PER_GUILD: u64 = 40;
const MEMBERS_PER_GUILD: u64 = 250;
const DMS: u64 = 300;

fn user(id: u64) -> Value {
    json!({
        "id": id.to_string(),
        "username": format!("user{id}"),
        "global_name": format!("Some Person {id}"),
        "avatar": format!("{:032x}", id),
        "discriminator": "0001",
        "flags": 0
    })
}

fn ready() -> Ready {
    let me = 1u64;
    let mut guilds = Vec::new();
    let mut read_states = Vec::new();
    let mut presences = Vec::new();
    for g in 0..GUILDS {
        let gid = 1_000_000 + g;
        let channels: Vec<Value> = (0..CHANNELS_PER_GUILD)
            .map(|c| {
                let cid = 10_000_000 + g * 1_000 + c;
                read_states.push(
                    json!({"id": cid.to_string(), "last_message_id": "5000", "mention_count": 0}),
                );
                json!({
                    "id": cid.to_string(), "type": 0, "guild_id": gid.to_string(),
                    "name": format!("channel-{c}"), "position": c,
                    "topic": "A topic that is about as long as many topics are, more or less.",
                    "last_message_id": "9000",
                    "permission_overwrites": [
                        {"id": gid.to_string(), "type": 0, "allow": "0", "deny": "0"}
                    ]
                })
            })
            .collect();
        let roles: Vec<Value> = (0..ROLES_PER_GUILD)
            .map(|r| {
                let rid = if r == 0 {
                    gid
                } else {
                    100_000_000 + g * 1_000 + r
                };
                json!({"id": rid.to_string(), "name": format!("Role {r}"),
                       "permissions": "2251799813685247", "position": r, "color": 0x336699})
            })
            .collect();
        let mut members =
            vec![json!({"user": user(me), "roles": [], "joined_at": "2026-01-01T00:00:00+00:00"})];
        for m in 0..MEMBERS_PER_GUILD {
            let uid = 2_000_000 + g * 10_000 + m;
            members.push(json!({
                "user": user(uid), "roles": [(100_000_000 + g * 1_000 + 1).to_string()],
                "nick": format!("Nick {m}"), "joined_at": "2026-01-01T00:00:00+00:00"
            }));
            if m % 2 == 0 {
                presences.push(json!({"user": {"id": uid.to_string()}, "status": "online",
                                      "guild_id": gid.to_string()}));
            }
        }
        guilds.push(json!({
            "id": gid.to_string(),
            "properties": {"id": gid.to_string(), "name": format!("Guild {g}"), "owner_id": "2"},
            "channels": channels, "roles": roles, "members": members,
            "member_count": 5000, "joined_at": "2026-01-01T00:00:00+00:00"
        }));
    }
    let private: Vec<Value> = (0..DMS)
        .map(|d| {
            let cid = 50_000_000 + d;
            read_states.push(json!({"id": cid.to_string(), "last_message_id": "5000"}));
            json!({"id": cid.to_string(), "type": 1, "last_message_id": "9000",
                   "recipients": [user(3_000_000 + d)]})
        })
        .collect();
    serde_json::from_value(json!({
        "session_id": "s", "user": user(me), "guilds": guilds,
        "private_channels": private, "users": [], "presences": presences,
        "read_states": read_states, "user_guild_settings": []
    }))
    .expect("a valid READY")
}

#[test]
fn a_passive_account_keeps_only_what_its_badges_need() {
    let ready = ready();
    let before = live();
    let mut state = AccountState::from_ready_at(&ready, 1);
    let full = live() - before;
    let unread_before: Vec<_> = state.guilds().map(|g| state.guild_unread(g.id)).collect();
    let dms_before = state.private_unread();

    state.set_passive(true);
    let passive = live() - before;
    let mb = |b: isize| b as f64 / (1024.0 * 1024.0);
    println!(
        "account state: {:.1} MB full, {:.1} MB passive ({} guilds, {} channels, {} members, {} DMs)",
        mb(full),
        mb(passive),
        GUILDS,
        GUILDS * CHANNELS_PER_GUILD,
        GUILDS * MEMBERS_PER_GUILD,
        DMS
    );
    // The badges are the same as before.
    let unread_after: Vec<_> = state.guilds().map(|g| state.guild_unread(g.id)).collect();
    assert_eq!(unread_before, unread_after);
    assert_eq!(dms_before, state.private_unread());
    assert!(state.is_passive());
    assert_eq!(state.member_total(), 0);
    // Members, presences and most users are gone: well under half of the full state.
    assert!(
        passive * 2 < full,
        "passive {passive} bytes vs full {full} bytes"
    );

    // New members and presences do not pile up while passive; a new READY stays passive.
    state.apply(&Dispatch::Ready(Box::new(ready.clone())));
    assert!(state.is_passive());
    assert_eq!(state.member_total(), 0);
    let after_ready = live() - before;
    assert!(after_ready * 2 < full, "{after_ready} vs {full}");

    // Active again: the normal caps.
    state.set_passive(false);
    state.apply(&Dispatch::Ready(Box::new(ready)));
    assert!(state.member_total() > 0);
}
