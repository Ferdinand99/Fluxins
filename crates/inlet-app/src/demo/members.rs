// SPDX-License-Identifier: AGPL-3.0-or-later
//! The member list and profiles of the demo: the members of `g1` in their groups
//! (Moderators, Contributors, online, offline) with statuses and custom statuses, and
//! profiles with bio, pronouns, roles, mutual guilds and friends and a note. Answers
//! go out as the gateway and the API would send them.

use inlet_protocol as p;
use serde_json::{json, Value};

use super::{snowflake, user, user_id, MockSession, ME, ROLE_CONTRIBUTORS, ROLE_MODERATORS};
use crate::backend::messages as msgs;
use crate::backend::social::members::Cmd as MembersCmd;
use crate::backend::social::profile::{Cmd as ProfileCmd, Ev as ProfileEv, Profile};
use crate::backend::social::Ev as SocialEv;

/// The demo's people: name, roles in `g1`, status, custom status.
const PEOPLE: [(&str, &[&str], &str, Option<&str>); 13] = [
    (
        ME,
        &[ROLE_MODERATORS],
        "online",
        Some("Building a Fluxer client"),
    ),
    ("ada", &[ROLE_MODERATORS], "online", None),
    ("grace", &[ROLE_CONTRIBUTORS], "idle", Some("Finding bugs")),
    (
        "linus",
        &[],
        "dnd",
        Some("Do not disturb: kernel merge window"),
    ),
    ("Sylo", &[], "online", None),
    ("aase", &[], "online", None),
    ("Inlet", &[], "offline", None),
    ("alan", &[], "offline", None),
    ("barbara", &[], "offline", None),
    ("edsger", &[], "offline", None),
    ("ken", &[], "offline", None),
    ("margaret", &[], "offline", None),
    (
        "a-very-long-name-that-does-not-fit-in-the-member-list",
        &[],
        "offline",
        None,
    ),
];

fn member(name: &str, roles: &[&str], status: &str, custom: Option<&str>) -> Value {
    let mut u = user(name);
    if name == "aase" {
        u["global_name"] = json!("\u{c5}se \u{d8}deg\u{e5}rd");
    }
    let mut presence = json!({"status": status, "mobile": false, "afk": false});
    if let Some(text) = custom {
        presence["custom_status"] = json!({ "text": text });
    }
    json!({"member": {"user": u, "roles": roles, "joined_at": "2026-01-01T00:00:00+00:00",
        "presence": presence}})
}

/// The rows of the demo member list of a guild, and its groups.
fn rows(guild: &str) -> (Vec<Value>, Vec<Value>) {
    let mut groups: Vec<(String, Vec<Value>)> = Vec::new();
    let first = guild == snowflake("g1");
    for (name, roles, status, custom) in PEOPLE {
        // Another guild has fewer people and no hoisted roles.
        if !first && !matches!(name, ME | "ada" | "linus" | "Inlet") {
            continue;
        }
        let roles: &[&str] = if first { roles } else { &[] };
        let group = if status == "offline" {
            "offline".to_string()
        } else if let Some(r) = roles.first() {
            (*r).to_string()
        } else {
            "online".to_string()
        };
        let item = member(name, roles, status, custom);
        match groups.iter_mut().find(|(g, _)| *g == group) {
            Some((_, list)) => list.push(item),
            None => groups.push((group, vec![item])),
        }
    }
    let mut items = Vec::new();
    let mut heads = Vec::new();
    for (id, list) in groups {
        let g = json!({"id": id, "count": list.len()});
        heads.push(g.clone());
        items.push(json!({ "group": g }));
        items.extend(list);
    }
    (items, heads)
}

impl MockSession {
    /// Member list subscriptions answer at once with the whole (small) list.
    pub fn member_command(&mut self, cmd: MembersCmd) {
        let MembersCmd::Window {
            guild_id,
            channel_id,
            ..
        } = cmd
        else {
            return;
        };
        let (items, groups) = rows(&guild_id);
        let online = items
            .iter()
            .filter(|i| {
                i["member"]["presence"]["status"]
                    .as_str()
                    .is_some_and(|s| s != "offline")
            })
            .count();
        let total = items.iter().filter(|i| i.get("member").is_some()).count();
        let end = items.len().saturating_sub(1);
        let update: p::GuildMemberListUpdate = match serde_json::from_value(json!({
            "guild_id": guild_id, "id": channel_id, "channel_id": channel_id,
            "member_count": total, "online_count": online, "groups": groups,
            "ops": [{"op": "SYNC", "range": [0, end], "items": items}]
        })) {
            Ok(u) => u,
            Err(e) => {
                log::warn!("demo member list: {e}");
                return;
            }
        };
        let _ = self.events.unbounded_send(msgs::Ev::Dispatch {
            dispatch: Box::new(p::Dispatch::GuildMemberListUpdate(Box::new(update))),
            rendered: Vec::new(),
        });
    }

    /// Profiles answer at once; notes are kept nowhere.
    pub fn profile_command(&mut self, cmd: ProfileCmd) {
        let ProfileCmd::Fetch {
            user_id: id,
            guild_id,
            request,
        } = cmd
        else {
            return;
        };
        let result = demo_profile(&id, guild_id.as_deref())
            .map(Box::new)
            .ok_or_else(|| t!("profile-error-offline"));
        let _ = self
            .events
            .unbounded_send(SocialEv::Profile(ProfileEv::Profile {
                request,
                user_id: id,
                guild_id,
                result,
            }));
    }
}

/// A demo person's profile, as the API would send it, made ready.
pub fn demo_profile(id: &str, guild: Option<&str>) -> Option<Profile> {
    let (name, roles, ..) = PEOPLE.iter().find(|(n, ..)| user_id(n) == id)?;
    let (bio, pronouns, accent, banner) = match *name {
        "ada" => (
            "Analyst of engines. Writes the **first** programs.\nAsk me about _Bernoulli numbers_.",
            "she/her",
            0x963C78,
            Some("adabanner"),
        ),
        "grace" => (
            "It is easier to ask forgiveness than permission.",
            "she/her",
            0x1E8264,
            None,
        ),
        "linus" => ("Talk is cheap. Show me the code.", "he/him", 0x286EAA, None),
        ME => ("The account the demo signs in with.", "", 0x504696, None),
        _ => ("", "", 0x504696, None),
    };
    let first = guild == Some(snowflake("g1").as_str());
    let full: p::FullUserProfile = serde_json::from_value(json!({
        "user": user(name),
        "user_profile": {"bio": bio, "pronouns": pronouns, "accent_color": accent, "banner": banner},
        "guild_member": guild.map(|_| json!({"user": user(name),
            "roles": if first { roles.to_vec() } else { Vec::new() },
            "joined_at": "2026-01-01T00:00:00+00:00"})),
        "mutual_guilds": if *name == ME { json!([]) } else {
            json!([{"id": snowflake("g1")}, {"id": snowflake("g2"), "nick": "Countess"}])
        },
        "mutual_friends": if matches!(*name, "ada" | "grace") { json!([user("linus"), user("Sylo")]) } else { json!([]) },
    }))
    .ok()?;
    let note = (*name == "ada").then(|| "Met at the demo meetup. Likes engines.".to_string());
    Some(Profile::of(&full, guild, note))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_demo_list_and_profiles_parse() {
        let (items, groups) = rows(&snowflake("g1"));
        assert_eq!(groups.len(), 4);
        assert_eq!(items.len(), PEOPLE.len() + 4);
        let (items, groups) = rows(&snowflake("g2"));
        assert_eq!((items.len(), groups.len()), (6, 2));
        let ada = demo_profile(&user_id("ada"), Some(&snowflake("g1"))).unwrap();
        assert_eq!(ada.roles, vec![ROLE_MODERATORS.to_string()]);
        assert!(!ada.bio.is_empty() && ada.banner.is_some());
        assert_eq!(ada.mutual_friends.len(), 2);
        assert!(demo_profile("1", None).is_none());
    }
}
