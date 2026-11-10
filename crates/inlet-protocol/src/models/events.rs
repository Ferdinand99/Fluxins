//! Payloads of gateway events, and the member list state.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    lenient_bool, lenient_vec, null_as_default, opt_sf, sf, sf_list, u64_or_zero, Channel, Emoji,
    GuildEmoji, GuildMember, GuildSticker, Presence, Role, Snowflake, User, VoiceState,
};

/// MESSAGE_ACK: the account's read state moved, usually from another session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageAck {
    /// Channel.
    #[serde(deserialize_with = "sf")]
    pub channel_id: Snowflake,
    /// The read state now points at this message.
    #[serde(deserialize_with = "sf")]
    pub message_id: Snowflake,
    /// Mentions left.
    #[serde(default, deserialize_with = "u64_or_zero")]
    pub mention_count: u64,
    /// Marked by hand (for example "mark unread").
    #[serde(default, deserialize_with = "lenient_bool")]
    pub manual: bool,
}

/// MESSAGE_REACTION_ADD and MESSAGE_REACTION_REMOVE.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReactionEvent {
    /// Who reacted (or whose reaction was removed).
    #[serde(deserialize_with = "sf")]
    pub user_id: Snowflake,
    /// Channel.
    #[serde(deserialize_with = "sf")]
    pub channel_id: Snowflake,
    /// Message.
    #[serde(deserialize_with = "sf")]
    pub message_id: Snowflake,
    /// Emoji.
    pub emoji: Emoji,
    /// Guild.
    #[serde(default, deserialize_with = "opt_sf")]
    pub guild_id: Option<Snowflake>,
    /// The user's member object in a guild channel.
    #[serde(default, deserialize_with = "super::message::lenient_opt")]
    pub member: Option<GuildMember>,
}

/// One reaction in MESSAGE_REACTION_ADD_MANY.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReactionAddition {
    /// Who reacted.
    #[serde(deserialize_with = "sf")]
    pub user_id: Snowflake,
    /// Emoji.
    pub emoji: Emoji,
}

/// MESSAGE_REACTION_ADD_MANY: DM reactions grouped over 650 ms.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReactionAddMany {
    /// Channel.
    #[serde(deserialize_with = "sf")]
    pub channel_id: Snowflake,
    /// Message.
    #[serde(deserialize_with = "sf")]
    pub message_id: Snowflake,
    /// Guild.
    #[serde(default, deserialize_with = "opt_sf")]
    pub guild_id: Option<Snowflake>,
    /// Reactions, oldest first.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub reactions: Vec<ReactionAddition>,
}

/// MESSAGE_REACTION_REMOVE_ALL and MESSAGE_REACTION_REMOVE_EMOJI (`emoji` set).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReactionClear {
    /// Channel.
    #[serde(deserialize_with = "sf")]
    pub channel_id: Snowflake,
    /// Message.
    #[serde(deserialize_with = "sf")]
    pub message_id: Snowflake,
    /// Guild.
    #[serde(default, deserialize_with = "opt_sf")]
    pub guild_id: Option<Snowflake>,
    /// Only reactions with this emoji were removed.
    #[serde(default, deserialize_with = "super::message::lenient_opt")]
    pub emoji: Option<Emoji>,
}

/// MESSAGE_DELETE_BULK.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageDeleteBulk {
    /// Deleted messages.
    #[serde(default, deserialize_with = "sf_list")]
    pub ids: Vec<Snowflake>,
    /// Channel.
    #[serde(deserialize_with = "sf")]
    pub channel_id: Snowflake,
    /// Guild.
    #[serde(default, deserialize_with = "opt_sf")]
    pub guild_id: Option<Snowflake>,
}

/// CHANNEL_PINS_UPDATE.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelPinsUpdate {
    /// Channel.
    #[serde(deserialize_with = "sf")]
    pub channel_id: Snowflake,
    /// Newest pin time, `None` when nothing is pinned.
    #[serde(default)]
    pub last_pin_timestamp: Option<String>,
    /// Guild.
    #[serde(default, deserialize_with = "opt_sf")]
    pub guild_id: Option<Snowflake>,
}

/// GUILD_ROLE_CREATE and GUILD_ROLE_UPDATE.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GuildRoleEvent {
    /// Guild.
    #[serde(deserialize_with = "sf")]
    pub guild_id: Snowflake,
    /// The role.
    pub role: Role,
}

/// GUILD_ROLE_UPDATE_BULK.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GuildRolesUpdate {
    /// Guild.
    #[serde(deserialize_with = "sf")]
    pub guild_id: Snowflake,
    /// Every changed role.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub roles: Vec<Role>,
}

/// GUILD_ROLE_DELETE.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GuildRoleDelete {
    /// Guild.
    #[serde(deserialize_with = "sf")]
    pub guild_id: Snowflake,
    /// Deleted role.
    #[serde(deserialize_with = "sf")]
    pub role_id: Snowflake,
}

/// GUILD_MEMBER_REMOVE.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GuildMemberRemove {
    /// Guild.
    #[serde(deserialize_with = "sf")]
    pub guild_id: Snowflake,
    /// The user (only `id`).
    pub user: User,
}

/// GUILD_EMOJIS_UPDATE: replace the stored emoji.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GuildEmojisUpdate {
    /// Guild.
    #[serde(deserialize_with = "sf")]
    pub guild_id: Snowflake,
    /// Every emoji.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub emojis: Vec<GuildEmoji>,
}

/// GUILD_STICKERS_UPDATE: replace the stored stickers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GuildStickersUpdate {
    /// Guild.
    #[serde(deserialize_with = "sf")]
    pub guild_id: Snowflake,
    /// Every sticker.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub stickers: Vec<GuildSticker>,
}

/// CHANNEL_UPDATE_BULK.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelUpdateBulk {
    /// Guild.
    #[serde(default, deserialize_with = "opt_sf")]
    pub guild_id: Option<Snowflake>,
    /// Changed channels.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub channels: Vec<Channel>,
}

/// PASSIVE_UPDATES: new `last_message_id`s in a passive large guild.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PassiveUpdates {
    /// Guild.
    #[serde(deserialize_with = "sf")]
    pub guild_id: Snowflake,
    /// Channel ID to its new `last_message_id`.
    #[serde(default, deserialize_with = "null_as_default")]
    pub channels: BTreeMap<String, Value>,
    /// Changed voice states.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub voice_states: Vec<VoiceState>,
}

impl PassiveUpdates {
    /// `(channel_id, last_message_id)` pairs.
    pub fn last_message_ids(&self) -> Vec<(Snowflake, Snowflake)> {
        self.channels
            .iter()
            .filter_map(|(c, v)| {
                let id = match v {
                    Value::String(s) => s.clone(),
                    Value::Number(n) if n.is_u64() => n.to_string(),
                    _ => return None,
                };
                Some((c.clone(), id))
            })
            .collect()
    }
}

/// PRESENCE_UPDATE_BULK.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PresenceUpdateBulk {
    /// Guild.
    #[serde(default, deserialize_with = "opt_sf")]
    pub guild_id: Option<Snowflake>,
    /// Presences.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub presences: Vec<Presence>,
}

/// GUILD_MEMBERS_CHUNK: the answer to Request Guild Members.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GuildMembersChunk {
    /// Guild.
    #[serde(deserialize_with = "sf")]
    pub guild_id: Snowflake,
    /// Up to 1,000 members.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub members: Vec<GuildMember>,
    /// Index of this chunk.
    #[serde(default, deserialize_with = "u64_or_zero")]
    pub chunk_index: u64,
    /// Number of chunks.
    #[serde(default, deserialize_with = "u64_or_zero")]
    pub chunk_count: u64,
    /// Presences, when asked for.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub presences: Vec<Presence>,
    /// The request nonce.
    #[serde(default)]
    pub nonce: Option<String>,
}

/// A group header in a member list: a hoisted role ID, `online` or `offline`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberListGroup {
    /// Role ID, `online` or `offline`.
    #[serde(default, deserialize_with = "group_id")]
    pub id: String,
    /// Members in the group.
    #[serde(default, deserialize_with = "u64_or_zero")]
    pub count: u64,
}

fn group_id<'de, D>(d: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match Value::deserialize(d)? {
        Value::String(s) => s,
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    })
}

/// One row of a member list.
#[derive(Debug, Clone, PartialEq)]
pub enum MemberListItem {
    /// A group header.
    Group(MemberListGroup),
    /// A member, with `presence` in its `extra` ([`GuildMember::presence`]).
    Member(Box<GuildMember>),
}

impl<'de> Deserialize<'de> for MemberListItem {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let v = Value::deserialize(d)?;
        if let Some(g) = v.get("group") {
            return MemberListGroup::deserialize(g)
                .map(MemberListItem::Group)
                .map_err(D::Error::custom);
        }
        if let Some(m) = v.get("member") {
            return GuildMember::deserialize(m)
                .map(|m| MemberListItem::Member(Box::new(m)))
                .map_err(D::Error::custom);
        }
        Err(D::Error::custom("member list item without group or member"))
    }
}

/// One operation in GUILD_MEMBER_LIST_UPDATE.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MemberListOp {
    /// `SYNC` (the only kind Fluxer sends today), or `INVALIDATE`.
    #[serde(default, deserialize_with = "null_as_default")]
    pub op: String,
    /// Inclusive `[start, end]` range.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub range: Vec<u64>,
    /// Replacement items for the range.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub items: Vec<MemberListItem>,
}

/// GUILD_MEMBER_LIST_UPDATE: a subscribed member list changed.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct GuildMemberListUpdate {
    /// Guild.
    #[serde(deserialize_with = "sf")]
    pub guild_id: Snowflake,
    /// List ID (the channel ID).
    #[serde(default, deserialize_with = "group_id")]
    pub id: String,
    /// Channel the list belongs to.
    #[serde(default, deserialize_with = "opt_sf")]
    pub channel_id: Option<Snowflake>,
    /// Members in the list.
    #[serde(default, deserialize_with = "u64_or_zero")]
    pub member_count: u64,
    /// Online members.
    #[serde(default, deserialize_with = "u64_or_zero")]
    pub online_count: u64,
    /// Group headers in order.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub groups: Vec<MemberListGroup>,
    /// Operations.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub ops: Vec<MemberListOp>,
}

/// Rows a member list keeps at most (Fluxer's highest range end is 100,000).
const MAX_MEMBER_LIST_ROWS: u64 = 100_001;

/// A member list as the UI shows it: make it with the ranges passed to
/// [`crate::GatewayHandle::subscribe_member_list`], apply every
/// GUILD_MEMBER_LIST_UPDATE for that channel, then read `rows`. Rows outside
/// the subscribed ranges stay `None` (not loaded), even if the server sends
/// them, so a hostile server cannot fill memory with members nobody asked for.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MemberList {
    /// Subscribed ranges (inclusive). Empty means the first 100 rows.
    pub ranges: Vec<[u32; 2]>,
    /// Guild.
    pub guild_id: Snowflake,
    /// List ID (channel ID).
    pub id: String,
    /// Members in the list.
    pub member_count: u64,
    /// Online members.
    pub online_count: u64,
    /// Group headers.
    pub groups: Vec<MemberListGroup>,
    /// Rows in list order.
    pub rows: Vec<Option<MemberListItem>>,
}

impl MemberList {
    /// An empty list for a guild's channel and the subscribed ranges.
    pub fn new(guild_id: &str, list_id: &str, ranges: &[[u32; 2]]) -> Self {
        MemberList {
            guild_id: guild_id.to_owned(),
            id: list_id.to_owned(),
            ranges: ranges.to_vec(),
            ..Default::default()
        }
    }

    /// Changes the subscribed ranges (after a new
    /// [`crate::GatewayHandle::subscribe_member_list`]); rows outside them are dropped.
    pub fn set_ranges(&mut self, ranges: &[[u32; 2]]) {
        self.ranges = ranges.to_vec();
        for i in 0..self.rows.len() {
            if !self.subscribed(i as u64) {
                self.rows[i] = None;
            }
        }
    }

    fn subscribed(&self, row: u64) -> bool {
        if self.ranges.is_empty() {
            return row <= 99;
        }
        self.ranges
            .iter()
            .any(|[s, e]| u64::from(*s) <= row && row <= u64::from(*e))
    }

    /// Applies one update. Updates for another guild or list are ignored and
    /// return `false`.
    pub fn apply(&mut self, update: &GuildMemberListUpdate) -> bool {
        if self.guild_id.is_empty() && self.id.is_empty() {
            self.guild_id = update.guild_id.clone();
            self.id = update.id.clone();
        } else if self.guild_id != update.guild_id || self.id != update.id {
            return false;
        }
        self.member_count = update.member_count;
        self.online_count = update.online_count;
        self.groups = update.groups.clone();
        // Rows = members + group headers.
        let total = update
            .member_count
            .saturating_add(update.groups.len() as u64)
            .min(MAX_MEMBER_LIST_ROWS);
        self.rows.resize(total as usize, None);
        for op in &update.ops {
            // Fluxer's ranges span at most 100 rows and end by 100,000.
            let (start, end) = match op.range.as_slice() {
                [s, e] if s <= e && e.saturating_sub(*s) <= 99 && *e < MAX_MEMBER_LIST_ROWS => {
                    (*s, *e)
                }
                _ => continue,
            };
            match op.op.as_str() {
                "SYNC" => {
                    let needed = (end + 1) as usize;
                    if self.rows.len() < needed {
                        self.rows.resize(needed, None);
                    }
                    for i in start..=end {
                        let idx = i as usize;
                        if idx >= self.rows.len() {
                            break;
                        }
                        if !self.subscribed(i) {
                            continue;
                        }
                        self.rows[idx] = op.items.get((i - start) as usize).cloned();
                    }
                }
                "INVALIDATE" => {
                    for i in start..=end {
                        match self.rows.get_mut(i as usize) {
                            Some(row) => *row = None,
                            None => break,
                        }
                    }
                }
                other => tracing::debug!(op = other, "unknown member list op"),
            }
        }
        true
    }

    /// The loaded members, in list order.
    pub fn members(&self) -> impl Iterator<Item = &GuildMember> {
        self.rows.iter().filter_map(|r| match r {
            Some(MemberListItem::Member(m)) => Some(m.as_ref()),
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn update(ops: Value, count: u64) -> GuildMemberListUpdate {
        serde_json::from_value(json!({
            "guild_id": "100", "id": "200", "channel_id": "200",
            "member_count": count, "online_count": 1,
            "groups": [{"id": "7", "count": 1}, {"id": "offline", "count": count - 1}],
            "ops": ops
        }))
        .unwrap()
    }

    #[test]
    fn member_list_applies_sync_and_invalidate() {
        let mut list = MemberList::default();
        let u = update(
            json!([{"op": "SYNC", "range": [0, 3], "items": [
                {"group": {"id": "7", "count": 1}},
                {"member": {"user": {"id": "10", "username": "ana"}, "roles": ["7"],
                            "presence": {"status": "online", "mobile": false, "afk": false}}},
                {"group": {"id": "offline", "count": 2}},
                {"member": {"user": {"id": "11", "username": "bo"}, "roles": []}},
                {"neither": true}
            ]}, {"op": "WEIRD", "range": [0, 0]}, {"op": "SYNC", "range": [5, 2]}]),
            3,
        );
        assert!(list.apply(&u));
        assert_eq!(list.rows.len(), 5);
        let names: Vec<_> = list.members().map(|m| m.user_id().unwrap()).collect();
        assert_eq!(names, vec!["10", "11"]);
        let first = list.members().next().unwrap();
        assert_eq!(first.presence().unwrap().status, "online");
        assert!(list.rows[4].is_none());

        let inv = update(json!([{"op": "INVALIDATE", "range": [3, 99]}]), 3);
        assert!(list.apply(&inv));
        assert_eq!(list.members().count(), 1);

        let mut other = inv.clone();
        other.id = "201".into();
        assert!(!list.apply(&other));

        // A hostile range cannot allocate past the cap.
        let huge = update(
            json!([{"op": "SYNC", "range": [0, u64::MAX], "items": []},
                   {"op": "SYNC", "range": [999_999, 1_000_000], "items": []}]),
            3,
        );
        assert!(list.apply(&huge));
        assert_eq!(list.rows.len(), 5);
    }

    #[test]
    fn rows_outside_the_subscribed_ranges_are_ignored() {
        let mut list = MemberList::new("100", "200", &[[0, 1]]);
        let u = update(
            json!([{"op": "SYNC", "range": [0, 3], "items": [
                {"group": {"id": "7", "count": 1}},
                {"member": {"user": {"id": "10"}, "roles": []}},
                {"group": {"id": "offline", "count": 2}},
                {"member": {"user": {"id": "11"}, "roles": []}}
            ]}]),
            3,
        );
        assert!(list.apply(&u));
        assert_eq!(list.members().count(), 1);
        assert!(list.rows[3].is_none());
        list.set_ranges(&[[2, 3]]);
        assert_eq!(list.members().count(), 0);
        assert!(list.apply(&u));
        assert_eq!(list.members().next().unwrap().user_id(), Some("11"));
        // A huge member_count does not overflow.
        let mut huge = u.clone();
        huge.member_count = u64::MAX;
        assert!(list.apply(&huge));
        assert_eq!(list.rows.len() as u64, MAX_MEMBER_LIST_ROWS);
    }

    #[test]
    fn reaction_and_ack_events_parse() {
        let r: ReactionEvent = serde_json::from_value(json!({
            "user_id": "1", "channel_id": "2", "message_id": "3",
            "emoji": {"id": "4", "name": "x", "animated": true}, "guild_id": "5",
            "member": {"user": {"id": "1"}, "roles": []}
        }))
        .unwrap();
        assert!(r.emoji.is_custom());
        let many: ReactionAddMany = serde_json::from_value(json!({
            "channel_id": "2", "message_id": "3",
            "reactions": [{"user_id": "1", "emoji": {"name": "👍"}}, {"bad": 1}]
        }))
        .unwrap();
        assert_eq!(many.reactions.len(), 1);
        let ack: MessageAck = serde_json::from_value(json!({
            "channel_id": "2", "message_id": "3", "mention_count": 0, "version": "1"
        }))
        .unwrap();
        assert!(!ack.manual);
        let p: PassiveUpdates = serde_json::from_value(json!({
            "guild_id": "1", "channels": {"2": "30", "3": 40, "4": null}
        }))
        .unwrap();
        assert_eq!(p.last_message_ids().len(), 2);
    }
}
