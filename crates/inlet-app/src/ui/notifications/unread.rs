// SPDX-License-Identifier: AGPL-3.0-or-later
//! What the sidebar and the rail show for unread state, worked out from the one account
//! state (`Inlet.account`). [`Badges`] holds the small values the views read and is brought
//! up to date from the [`Changes`] of every update of the state, so only what changed is
//! looked at.

use std::collections::HashMap;

use inlet_state::{AccountState, Changes, ChannelId, GuildId, MessageId};

use super::badge::Badge;

/// A channel's badge: its unread state, and whether it is muted at some level or itself.
pub fn channel_badge(state: &AccountState, ch: ChannelId) -> Badge {
    let u = state.unread(ch);
    let dimmed = state.channel(ch).is_some()
        && state
            .guild_settings()
            .channel_muted(state.channels().guild_of(ch), ch, state.now_ms());
    Badge {
        unread: u.unread,
        mentions: u.mentions,
        muted: u.muted || dimmed,
        dimmed,
    }
}

/// A guild's badge for the rail; `None` is the direct messages button.
pub fn guild_badge(state: &AccountState, g: Option<GuildId>) -> Badge {
    let u = match g {
        Some(g) => state.guild_unread(g),
        None => state.private_unread(),
    };
    Badge {
        unread: u.unread,
        mentions: u.mentions,
        muted: u.muted,
        dimmed: false,
    }
}

/// Every channel and category the account has, guild and private.
fn all_channels(state: &AccountState) -> Vec<ChannelId> {
    let mut out = Vec::new();
    for g in state.guilds() {
        out.extend(
            state
                .guild_channels(g.id)
                .iter()
                .filter_map(|c| ChannelId::parse(&c.id)),
        );
    }
    out.extend(
        state
            .private_channels()
            .iter()
            .filter_map(|c| ChannelId::parse(&c.id)),
    );
    out
}

/// What there is to read in a channel: unread messages or unread mentions.
pub fn has_unread(state: &AccountState, ch: ChannelId) -> bool {
    let u = state.unread(ch);
    u.unread || u.mentions > 0
}

/// The channels of a guild (or the direct messages) with something unread, each with its
/// newest message.
pub fn unread_targets(state: &AccountState, guild: Option<GuildId>) -> Vec<(ChannelId, MessageId)> {
    let channels: Vec<ChannelId> = match guild {
        Some(g) => state
            .visible_channels(g)
            .iter()
            .filter_map(|c| ChannelId::parse(&c.id))
            .collect(),
        None => state
            .private_channels()
            .iter()
            .filter_map(|c| ChannelId::parse(&c.id))
            .collect(),
    };
    channels
        .into_iter()
        .filter(|ch| has_unread(state, *ch))
        .filter_map(|ch| Some((ch, state.read_state(ch)?.last_message_id?)))
        .collect()
}

/// The channels of a category with something unread, each with its newest message.
pub fn category_unread_targets(
    state: &AccountState,
    guild: Option<GuildId>,
    category: ChannelId,
) -> Vec<(ChannelId, MessageId)> {
    unread_targets(state, guild)
        .into_iter()
        .filter(|(ch, _)| state.channels().parent_of(*ch) == Some(category))
        .collect()
}

/// Everything unread, in every guild and in the direct messages.
pub fn all_unread_targets(state: &AccountState) -> Vec<(ChannelId, MessageId)> {
    let mut targets: Vec<(ChannelId, MessageId)> = state
        .guilds()
        .flat_map(|g| unread_targets(state, Some(g.id)))
        .collect();
    targets.extend(unread_targets(state, None));
    targets
}

/// The badges the views show. Entries with nothing to show are not kept.
#[derive(Default)]
pub struct Badges {
    channels: HashMap<ChannelId, Badge>,
    guilds: HashMap<GuildId, Badge>,
    dm: Badge,
}

fn set<K: std::hash::Hash + Eq>(map: &mut HashMap<K, Badge>, key: K, badge: Badge) {
    if badge.is_default() {
        map.remove(&key);
    } else {
        map.insert(key, badge);
    }
}

impl Badges {
    /// A channel, category or direct message by ID; nothing for one that is read.
    pub fn channel(&self, id: &str) -> Badge {
        ChannelId::parse(id)
            .and_then(|ch| self.channels.get(&ch))
            .copied()
            .unwrap_or_default()
    }

    /// A guild's rail entry; `None` is the direct messages button.
    pub fn guild(&self, id: Option<&str>) -> Badge {
        match id {
            Some(id) => GuildId::parse(id)
                .and_then(|g| self.guilds.get(&g))
                .copied()
                .unwrap_or_default(),
            None => self.dm,
        }
    }

    pub fn clear(&mut self) {
        *self = Badges::default();
    }

    /// Brings the badges up to date after the state changed. Looks at the channels and
    /// guilds the changes name, or at everything when too much changed to list (a new
    /// READY, a guild changed or removed, mutes that start or end).
    pub fn refresh(&mut self, state: &AccountState, c: &Changes) {
        let all = c.reset
            || c.guilds
            || c.channels.is_all()
            || c.read_state_channels.is_all()
            || c.read_state_guilds.is_all();
        if !(all || c.read_state || !c.channels.is_empty()) {
            return;
        }
        if c.reset {
            self.clear();
        }
        let channels: Vec<ChannelId> = if all {
            let mut ids = all_channels(state);
            // Channels that are gone: their badge goes too.
            ids.extend(self.channels.keys().copied());
            ids.sort();
            ids.dedup();
            ids
        } else {
            // Created, changed or deleted channels are checked too: a deleted one has no
            // badge any more, a new one may be unread.
            c.read_state_channels
                .iter()
                .chain(c.channels.iter())
                .copied()
                .collect()
        };
        for ch in channels {
            set(&mut self.channels, ch, channel_badge(state, ch));
        }
        let guilds: Vec<Option<GuildId>> = if all {
            let mut keys: Vec<Option<GuildId>> = state.guilds().map(|g| Some(g.id)).collect();
            keys.push(None);
            keys.extend(self.guilds.keys().map(|g| Some(*g)));
            keys.sort();
            keys.dedup();
            keys
        } else {
            c.read_state_guilds.iter().copied().collect()
        };
        for g in guilds {
            let badge = guild_badge(state, g);
            match g {
                Some(g) => set(&mut self.guilds, g, badge),
                None => self.dm = badge,
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use inlet_protocol::models::FLUXER_EPOCH_MS;
    use inlet_protocol::permissions::{READ_MESSAGE_HISTORY, SEND_MESSAGES, VIEW_CHANNEL};
    use inlet_protocol::Dispatch;
    use inlet_state::AckKind;
    use serde_json::{json, Value};

    use super::*;

    const ME: &str = "10";
    const OTHER: &str = "20";
    const GUILD: &str = "100";
    const GENERAL: &str = "301";
    const RANDOM: &str = "302";
    const CATEGORY: &str = "300";
    const DM: &str = "900";
    /// 2026-01-01T00:00:00Z, when the account joined the guild.
    const JOINED_MS: u64 = 1_767_225_600_000;
    /// The clock of the test states: an hour after joining.
    const NOW_MS: i64 = JOINED_MS as i64 + 3_600_000;

    /// A message ID `secs` after the join time.
    fn mid(secs: u64) -> u64 {
        ((JOINED_MS + secs * 1000 - FLUXER_EPOCH_MS) << 22) + 1
    }

    fn ch(id: &str) -> ChannelId {
        ChannelId::parse(id).unwrap()
    }

    fn g() -> GuildId {
        GuildId::parse(GUILD).unwrap()
    }

    fn state(read_states: Value, settings: Value) -> AccountState {
        let ready = serde_json::from_value(json!({
        "session_id": "s1",
        "user": {"id": ME, "username": "me", "mfa_enabled": false},
        "guilds": [{
            "id": GUILD,
            "properties": {"id": GUILD, "name": "Hedgerow", "owner_id": "1", "mfa_level": 0,
                           "default_message_notifications": 0},
            "roles": [{"id": GUILD, "name": "@everyone", "position": 0,
                       "permissions": (VIEW_CHANNEL | SEND_MESSAGES | READ_MESSAGE_HISTORY).to_string()}],
            "channels": [
                {"id": CATEGORY, "type": 4, "position": 0, "name": "text"},
                {"id": GENERAL, "type": 0, "position": 1, "name": "general", "parent_id": CATEGORY,
                 "last_message_id": mid(10).to_string()},
                {"id": RANDOM, "type": 0, "position": 2, "name": "random", "parent_id": CATEGORY,
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
    }))
    .unwrap();
        AccountState::from_ready_at(&ready, NOW_MS)
    }

    fn message(id: u64, channel: &str, author: &str, extra: Value) -> Dispatch {
        let mut m = json!({
            "id": id.to_string(), "channel_id": channel, "type": 0,
            "author": {"id": author, "username": format!("u{author}")},
            "content": "hi", "timestamp": "2026-01-01T01:00:00Z", "mentions": [],
        });
        if let (Value::Object(m), Value::Object(e)) = (&mut m, extra) {
            m.extend(e);
        }
        Dispatch::parse("MESSAGE_CREATE", m)
    }

    /// The badges after a READY.
    fn badges_of(s: &AccountState) -> Badges {
        let mut b = Badges::default();
        let mut reset = Changes::default();
        reset.reset = true;
        b.refresh(s, &reset);
        b
    }

    fn badge(unread: bool, mentions: u32, muted: bool, dimmed: bool) -> Badge {
        Badge {
            unread,
            mentions,
            muted,
            dimmed,
        }
    }

    #[test]
    fn ready_shows_what_the_server_says_is_unread() {
        let s = state(
            json!([
                {"id": GENERAL, "mention_count": 2, "last_message_id": mid(5).to_string()},
                {"id": RANDOM, "mention_count": 0, "last_message_id": mid(20).to_string()},
            ]),
            json!([]),
        );
        let b = badges_of(&s);
        assert_eq!(b.channel(GENERAL), badge(true, 2, false, false));
        // Read to the end, and a category: no badge at all.
        assert_eq!(b.channel(RANDOM), Badge::default());
        assert_eq!(b.channel(CATEGORY), Badge::default());
        assert_eq!(b.guild(Some(GUILD)), badge(true, 2, false, false));
        // The direct message has no ack: unread.
        assert!(b.channel(DM).unread);
        assert!(b.guild(None).unread);
        // Anything that is not an ID has none.
        assert_eq!(b.channel("not an id"), Badge::default());
    }

    #[test]
    fn a_message_event_changes_the_badge_through_the_one_state() {
        let mut s = state(json!([]), json!([]));
        let mut b = badges_of(&s);
        assert_eq!(b.channel(GENERAL).mentions, 0);

        // A mention: the state counts it, the badge follows from the state's own changes.
        let changes = s.apply(&message(
            mid(40),
            GENERAL,
            OTHER,
            json!({"mentions": [{"id": ME, "username": "me"}]}),
        ));
        b.refresh(&s, &changes);
        assert_eq!(b.channel(GENERAL), badge(true, 1, false, false));
        assert_eq!(b.guild(Some(GUILD)).mentions, 1);

        // Another plain message changes nothing the views show.
        let changes = s.apply(&message(mid(41), GENERAL, OTHER, json!({})));
        b.refresh(&s, &changes);
        assert_eq!(b.channel(GENERAL), badge(true, 1, false, false));

        // The server's ack from another session clears it.
        let changes = s.apply(&Dispatch::parse(
            "MESSAGE_ACK",
            json!({"channel_id": GENERAL, "message_id": mid(41).to_string(), "mention_count": 0}),
        ));
        b.refresh(&s, &changes);
        assert_eq!(b.channel(GENERAL), Badge::default());

        // Reading it here does the same, and the guild follows.
        let changes = s.mark_read(ch(RANDOM), None, AckKind::Explicit);
        b.refresh(&s, &changes);
        assert_eq!(b.channel(RANDOM), Badge::default());
        assert_eq!(b.guild(Some(GUILD)), Badge::default());
    }

    #[test]
    fn a_muted_server_has_no_pill_and_its_channels_look_muted() {
        let mut s = state(
            json!([]),
            json!([{"guild_id": GUILD, "muted": true,
                "channel_overrides": [{"channel_id": CATEGORY, "muted": true}]}]),
        );
        let mut b = badges_of(&s);
        let guild = b.guild(Some(GUILD));
        assert!(guild.muted && !guild.unread);
        // Muted through the guild and the category, not by themselves.
        assert!(b.channel(GENERAL).muted && !b.channel(GENERAL).dimmed);
        // The category itself is muted: drawn dimmed.
        assert_eq!(b.channel(CATEGORY), badge(false, 0, true, true));

        // A mention still counts on the badge.
        let changes = s.apply(&message(
            mid(40),
            GENERAL,
            OTHER,
            json!({"mentions": [{"id": ME, "username": "me"}]}),
        ));
        b.refresh(&s, &changes);
        assert_eq!(b.guild(Some(GUILD)).mentions, 1);
    }

    #[test]
    fn a_timed_mute_ends_with_the_clock() {
        let mut s = state(json!([]), json!([]));
        let mut b = badges_of(&s);
        assert!(b.guild(Some(GUILD)).unread);
        let end = "2026-01-01T02:00:00Z";
        let changes = s.apply(&Dispatch::parse(
            "USER_GUILD_SETTINGS_UPDATE",
            json!({"guild_id": GUILD, "muted": true, "mute_config": {"end_time": end}}),
        ));
        b.refresh(&s, &changes);
        assert!(b.guild(Some(GUILD)).muted && !b.guild(Some(GUILD)).unread);

        // Nothing at 01:30; at 02:01 the mute is over and the pill is back.
        let changes = s.tick(JOINED_MS as i64 + 5_400_000);
        b.refresh(&s, &changes);
        assert!(b.guild(Some(GUILD)).muted);
        let changes = s.tick(JOINED_MS as i64 + 7_260_000);
        b.refresh(&s, &changes);
        let guild = b.guild(Some(GUILD));
        assert!(!guild.muted && guild.unread);
    }

    #[test]
    fn a_deleted_channel_loses_its_badge() {
        let mut s = state(json!([]), json!([]));
        let mut b = badges_of(&s);
        assert!(b.channel(RANDOM).unread);
        let changes = s.apply(&Dispatch::parse(
            "CHANNEL_DELETE",
            json!({"id": RANDOM, "type": 0, "guild_id": GUILD}),
        ));
        b.refresh(&s, &changes);
        assert_eq!(b.channel(RANDOM), Badge::default());
    }

    #[test]
    fn what_mark_as_read_takes_is_every_unread_channel_with_its_newest_message() {
        let s = state(
            json!([{"id": RANDOM, "mention_count": 0, "last_message_id": mid(20).to_string()}]),
            json!([]),
        );
        // Everything: the unread channel of the guild and the direct message; random is read.
        let mut all = all_unread_targets(&s);
        all.sort();
        assert_eq!(
            all,
            vec![
                (ch(GENERAL), MessageId(mid(10))),
                (ch(DM), MessageId(mid(30)))
            ]
        );
        // One guild leaves the direct messages alone, and the direct messages the guilds.
        assert_eq!(
            unread_targets(&s, Some(g())),
            vec![(ch(GENERAL), MessageId(mid(10)))]
        );
        assert_eq!(unread_targets(&s, None), vec![(ch(DM), MessageId(mid(30)))]);
    }

    // ---- one state ----

    fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read a source directory") {
            let path = entry.expect("a directory entry").path();
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    /// Memory: the app keeps one `AccountState` (`Inlet.account`), not one for the chat and
    /// another for unread state and settings. Only the code that makes it from READY may
    /// build one outside tests.
    #[test]
    fn the_app_builds_one_account_state_only() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rust_files(&src, &mut files);
        let mut found = Vec::new();
        for path in files {
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            if name == "tests.rs" || path.components().any(|c| c.as_os_str() == "demo") {
                continue;
            }
            let source = std::fs::read_to_string(&path).unwrap();
            // Code before the test module of the file.
            let code = source.split("mod tests").next().unwrap_or("");
            let needle = ["AccountState", "::from_ready"].concat();
            for (i, line) in code.lines().enumerate() {
                if line.contains(&needle) && !line.trim_start().starts_with("//") {
                    found.push(format!("{}:{}", path.display(), i + 1));
                }
            }
        }
        assert_eq!(
            found.len(),
            1,
            "exactly one place builds the account state, found {found:?}"
        );
        assert!(found[0].contains("messages.rs"), "{found:?}");
    }

    #[test]
    fn a_category_can_be_muted_and_its_row_and_channels_follow() {
        use crate::ui::notifications::settings::{build_intent, Change, SettingsBatch};
        let mut s = state(json!([]), json!([]));
        let mut b = badges_of(&s);
        assert!(b.guild(Some(GUILD)).unread);

        // The change the settings window makes for a category, applied as the state shows it.
        let intent = build_intent(
            Some(g()),
            Some(ch(CATEGORY)),
            Change::Mute { for_ms: None },
            NOW_MS,
        )
        .unwrap();
        let mut writes = SettingsBatch::default();
        writes.add(
            Some(g()),
            s.guild_settings().get(Some(g())),
            intent,
            std::time::Instant::now(),
        );
        let data = writes.effective(Some(g())).unwrap();
        let changes = s.apply(&Dispatch::Other {
            name: "USER_GUILD_SETTINGS_UPDATE".into(),
            data,
        });
        b.refresh(&s, &changes);

        // The category row is dimmed; its channels are muted through it, and the server loses
        // its pill because everything unread in it is muted.
        assert_eq!(b.channel(CATEGORY), badge(false, 0, true, true));
        assert!(b.channel(GENERAL).muted && !b.channel(GENERAL).dimmed);
        assert!(!b.guild(Some(GUILD)).unread);
    }

    #[test]
    fn mark_category_read_takes_the_channels_under_it_only() {
        let s = state(
            json!([{"id": RANDOM, "mention_count": 0, "last_message_id": mid(20).to_string()}]),
            json!([]),
        );
        // Random is read; general is unread and under the category; the DM is not.
        assert_eq!(
            category_unread_targets(&s, Some(g()), ch(CATEGORY)),
            vec![(ch(GENERAL), MessageId(mid(10)))]
        );
        // A channel that is no category has nothing under it.
        assert!(category_unread_targets(&s, Some(g()), ch(GENERAL)).is_empty());
    }
}
