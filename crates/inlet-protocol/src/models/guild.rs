//! Guild parts: roles, members, permission overwrites, custom emoji,
//! stickers and voice states.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::{
    extra_list, lenient_bool, lenient_vec, null_as_default, opt_i64, opt_sf, opt_u64, sf, sf_list,
    u64_or_zero, value_u64, Channel, Guild, GuildReady, Snowflake, User,
};

/// A guild role.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Role {
    /// Role ID. The everyone role has the guild's ID.
    #[serde(deserialize_with = "sf")]
    pub id: Snowflake,
    /// Name.
    #[serde(default, deserialize_with = "null_as_default")]
    pub name: String,
    /// Colour as 24-bit RGB; 0 means no colour.
    #[serde(default, deserialize_with = "u64_or_zero")]
    pub color: u64,
    /// Rank: a larger value ranks higher. The everyone role is 0.
    #[serde(default, deserialize_with = "null_as_default")]
    pub position: i64,
    /// Position of the role's group in the member list, or `None` to follow `position`.
    #[serde(default, deserialize_with = "opt_i64")]
    pub hoist_position: Option<i64>,
    /// Permission bits (see [`crate::permissions`]). Sent as a decimal string.
    #[serde(default, deserialize_with = "u64_or_zero")]
    pub permissions: u64,
    /// Whether members show as their own group in the member list.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub hoist: bool,
    /// Whether anyone can mention the role.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub mentionable: bool,
    /// Role icon hash (see [`crate::Media::role_icon`]).
    #[serde(default)]
    pub icon: Option<String>,
    /// Unicode emoji used as the role icon.
    #[serde(default)]
    pub unicode_emoji: Option<String>,
    /// Every other field.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Permission overwrite types.
pub mod overwrite_type {
    /// For a role.
    pub const ROLE: i64 = 0;
    /// For a member.
    pub const MEMBER: i64 = 1;
}

/// A channel permission overwrite.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionOverwrite {
    /// Role or user ID.
    #[serde(deserialize_with = "sf")]
    pub id: Snowflake,
    /// See [`overwrite_type`].
    #[serde(rename = "type", default, deserialize_with = "null_as_default")]
    pub kind: i64,
    /// Allowed bits.
    #[serde(default, deserialize_with = "u64_or_zero")]
    pub allow: u64,
    /// Denied bits.
    #[serde(default, deserialize_with = "u64_or_zero")]
    pub deny: u64,
}

/// A guild member.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GuildMember {
    /// The account. Missing in MESSAGE_CREATE (the author is the account);
    /// in READY only `id` is set.
    #[serde(default, deserialize_with = "super::message::lenient_opt")]
    pub user: Option<User>,
    /// Guild nickname.
    #[serde(default)]
    pub nick: Option<String>,
    /// Guild avatar hash (see [`crate::Media::member_avatar`]).
    #[serde(default)]
    pub avatar: Option<String>,
    /// Guild banner hash.
    #[serde(default)]
    pub banner: Option<String>,
    /// Guild profile accent colour as 24-bit RGB.
    #[serde(default, deserialize_with = "opt_u64")]
    pub accent_color: Option<u64>,
    /// Role IDs (the everyone role is never listed).
    #[serde(default, deserialize_with = "sf_list")]
    pub roles: Vec<Snowflake>,
    /// When the account joined (ISO 8601).
    #[serde(default)]
    pub joined_at: Option<String>,
    /// Muted in voice.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub mute: bool,
    /// Deafened in voice.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub deaf: bool,
    /// End of a communication timeout (ISO 8601); it can be in the past.
    #[serde(default)]
    pub communication_disabled_until: Option<String>,
    /// Every other field (`guild_id` in events, `presence` in member lists, ...).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl GuildMember {
    /// The user ID, if the member carries a user.
    pub fn user_id(&self) -> Option<&str> {
        self.user.as_ref().map(|u| u.id.as_str())
    }

    /// The name to show: nickname, then display name, then username.
    pub fn display_name(&self) -> Option<&str> {
        self.nick
            .as_deref()
            .filter(|n| !n.is_empty())
            .or_else(|| self.user.as_ref().map(User::display_name))
    }

    /// Whether a communication timeout is running at `now_ms` (Unix milliseconds).
    pub fn is_timed_out_at(&self, now_ms: i64) -> bool {
        self.communication_disabled_until
            .as_deref()
            .and_then(super::parse_timestamp_ms)
            .is_some_and(|until| until > now_ms)
    }

    /// Whether a communication timeout is running now.
    pub fn is_timed_out(&self) -> bool {
        self.is_timed_out_at(super::time::now_ms())
    }

    /// The guild ID in member events.
    pub fn guild_id(&self) -> Option<Snowflake> {
        self.extra
            .get("guild_id")
            .and_then(|v| opt_sf(v.clone()).ok().flatten())
    }

    /// The member's presence in a member list item.
    pub fn presence(&self) -> Option<super::Presence> {
        super::extra_field(&self.extra, "presence")
    }
}

/// A guild's custom emoji.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GuildEmoji {
    /// Emoji ID (see [`crate::Media::emoji`]).
    #[serde(deserialize_with = "sf")]
    pub id: Snowflake,
    /// Name, used as `:name:`.
    #[serde(default, deserialize_with = "null_as_default")]
    pub name: String,
    /// Whether it is animated.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub animated: bool,
    /// Who uploaded it (only from List guild emojis).
    #[serde(default, deserialize_with = "super::message::lenient_opt")]
    pub user: Option<User>,
}

impl GuildEmoji {
    /// As a reaction emoji.
    pub fn as_emoji(&self) -> super::Emoji {
        super::Emoji::custom(self.id.clone(), self.name.clone(), self.animated)
    }
}

/// A guild's sticker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GuildSticker {
    /// Sticker ID (see [`crate::Media::sticker`]).
    #[serde(deserialize_with = "sf")]
    pub id: Snowflake,
    /// Name.
    #[serde(default, deserialize_with = "null_as_default")]
    pub name: String,
    /// Description.
    #[serde(default, deserialize_with = "null_as_default")]
    pub description: String,
    /// Tags.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub tags: Vec<String>,
    /// Whether it is animated.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub animated: bool,
}

/// A voice state (model only; voice itself is not in this crate).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VoiceState {
    /// Guild, `None` in a DM call.
    #[serde(default, deserialize_with = "opt_sf")]
    pub guild_id: Option<Snowflake>,
    /// Voice channel, `None` when the participant left.
    #[serde(default, deserialize_with = "opt_sf")]
    pub channel_id: Option<Snowflake>,
    /// Participant.
    #[serde(default, deserialize_with = "opt_sf")]
    pub user_id: Option<Snowflake>,
    /// Voice connection ID.
    #[serde(default)]
    pub connection_id: Option<String>,
    /// Gateway session that owns the connection.
    #[serde(default)]
    pub session_id: Option<String>,
    /// The participant's member object.
    #[serde(default, deserialize_with = "super::message::lenient_opt")]
    pub member: Option<GuildMember>,
    /// Server mute.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub mute: bool,
    /// Server deafen.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub deaf: bool,
    /// Microphone muted locally.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub self_mute: bool,
    /// Output deafened locally.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub self_deaf: bool,
    /// Sends camera video.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub self_video: bool,
    /// Shares the screen.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub self_stream: bool,
    /// On a mobile app.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub is_mobile: bool,
    /// Suppressed.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub suppress: bool,
    /// Version of this voice state; only goes up.
    #[serde(default, deserialize_with = "u64_or_zero")]
    pub version: u64,
    /// Every other field.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Channel {
    /// The permission overwrites (guild channels other than threads).
    pub fn permission_overwrites(&self) -> Vec<PermissionOverwrite> {
        extra_list(&self.extra, "permission_overwrites")
    }

    /// Whether the channel is a thread.
    pub fn is_thread(&self) -> bool {
        use super::channel_type::*;
        matches!(
            self.kind,
            ANNOUNCEMENT_THREAD | PUBLIC_THREAD | PRIVATE_THREAD
        )
    }

    /// When a message was last pinned (ISO 8601).
    pub fn last_pin_timestamp(&self) -> Option<&str> {
        self.extra.get("last_pin_timestamp")?.as_str()
    }

    /// Channel flags.
    pub fn flag_bits(&self) -> u64 {
        self.extra.get("flags").and_then(value_u64).unwrap_or(0)
    }
}

impl Guild {
    /// The MFA level: 1 (elevated) means moderators need two-factor
    /// authentication for elevated permissions (see
    /// [`crate::PermissionContext::mfa_missing`]).
    pub fn mfa_level(&self) -> i64 {
        self.extra
            .get("mfa_level")
            .and_then(Value::as_i64)
            .unwrap_or(0)
    }

    /// Banner hash (see [`crate::Media::banner`]).
    pub fn banner(&self) -> Option<&str> {
        self.extra.get("banner")?.as_str()
    }

    /// Splash hash (see [`crate::Media::splash`]).
    pub fn splash(&self) -> Option<&str> {
        self.extra.get("splash")?.as_str()
    }
}

impl GuildReady {
    /// The roles, typed.
    pub fn parsed_roles(&self) -> Vec<Role> {
        super::parse_values(&self.roles, "role")
    }

    /// The members in the payload (the session's own member and voice members), typed.
    pub fn parsed_members(&self) -> Vec<GuildMember> {
        super::parse_values(&self.members, "member")
    }

    /// Custom emoji.
    pub fn emojis(&self) -> Vec<GuildEmoji> {
        extra_list(&self.extra, "emojis")
    }

    /// Stickers.
    pub fn stickers(&self) -> Vec<GuildSticker> {
        extra_list(&self.extra, "stickers")
    }

    /// Voice states in channels the session can view.
    pub fn voice_states(&self) -> Vec<VoiceState> {
        extra_list(&self.extra, "voice_states")
    }

    /// Members online.
    pub fn online_count(&self) -> Option<u64> {
        self.extra.get("online_count").and_then(value_u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn roles_members_and_overwrites_parse() {
        let g: GuildReady = serde_json::from_value(json!({
            "id": "100",
            "roles": [{"id": "100", "name": "@everyone", "color": 0, "position": 0, "permissions": "3072"},
                      {"id": "7", "name": "Mod", "color": 3447003, "position": 4, "permissions": 8192,
                       "hoist": true, "hoist_position": null, "icon": null, "unicode_emoji": "🛡"},
                      {"name": "no id"}],
            "members": [{"user": {"id": "10"}, "roles": ["7"], "joined_at": "2026-01-01T00:00:00Z",
                         "communication_disabled_until": "2000-01-01T00:00:00Z"}],
            "emojis": [{"id": "55", "name": "party", "animated": true}],
            "stickers": [{"id": "66", "name": "wave", "description": "", "tags": ["hi"], "animated": false}],
            "voice_states": [{"guild_id": "100", "channel_id": "300", "user_id": "10", "self_mute": true,
                              "version": 3, "connection_id": "c1", "member": null}],
            "online_count": 2
        }))
        .unwrap();
        let roles = g.parsed_roles();
        assert_eq!(roles.len(), 2);
        assert_eq!(roles[0].permissions, 3072);
        assert_eq!(roles[1].permissions, 8192);
        assert_eq!(roles[1].color, 3_447_003);
        assert!(roles[1].hoist);
        let members = g.parsed_members();
        assert_eq!(members[0].user_id(), Some("10"));
        assert_eq!(members[0].roles, vec!["7".to_string()]);
        assert!(!members[0].is_timed_out());
        assert_eq!(g.emojis()[0].as_emoji().path_value(), "party:55");
        assert_eq!(g.stickers()[0].tags, vec!["hi".to_string()]);
        let vs = &g.voice_states()[0];
        assert!(vs.self_mute && vs.version == 3);
        assert_eq!(g.online_count(), Some(2));

        let c: Channel = serde_json::from_value(json!({
            "id": "200", "type": 0, "permission_overwrites": [
                {"id": "100", "type": 0, "allow": "0", "deny": "1024"},
                {"id": "10", "type": 1, "allow": 1024, "deny": null}]
        }))
        .unwrap();
        let ow = c.permission_overwrites();
        assert_eq!(ow[0].deny, 1024);
        assert_eq!(ow[1].allow, 1024);
        assert_eq!(ow[1].kind, overwrite_type::MEMBER);
    }

    #[test]
    fn timeout_is_checked_against_the_clock() {
        let m: GuildMember = serde_json::from_value(json!({
            "user": {"id": "1"}, "nick": "", "communication_disabled_until": "2026-11-10T08:00:00Z"
        }))
        .unwrap();
        assert!(m.is_timed_out_at(1_794_297_599_000));
        assert!(!m.is_timed_out_at(1_794_297_600_000));
        assert_eq!(m.display_name(), Some(""));
    }
}
