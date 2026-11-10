//! Account parts: settings, profiles, read states and presences.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::{
    extra_field, extra_list, lenient_bool, lenient_vec, null_as_default, opt_i64, opt_sf, opt_u64,
    sf, sf_list, snowflake_cmp, u64_or_zero, value_u64, CurrentUser, GuildMember, Ready, Snowflake,
    User,
};

/// A custom status.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct CustomStatus {
    /// Text.
    #[serde(default)]
    pub text: Option<String>,
    /// When it expires (ISO 8601).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    /// Custom emoji ID.
    #[serde(
        default,
        deserialize_with = "opt_sf",
        skip_serializing_if = "Option::is_none"
    )]
    pub emoji_id: Option<Snowflake>,
    /// Unicode emoji or custom emoji name.
    #[serde(default)]
    pub emoji_name: Option<String>,
    /// Whether the custom emoji is animated.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub emoji_animated: bool,
}

/// A folder in the guild sidebar.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GuildFolder {
    /// Folder ID, `-1` for the uncategorized folder.
    #[serde(default, deserialize_with = "opt_i64")]
    pub id: Option<i64>,
    /// Name.
    #[serde(default)]
    pub name: Option<String>,
    /// Colour as 24-bit RGB.
    #[serde(default, deserialize_with = "opt_u64")]
    pub color: Option<u64>,
    /// Folder flags.
    #[serde(default, deserialize_with = "u64_or_zero")]
    pub flags: u64,
    /// Icon name (`folder`, `star`, ...).
    #[serde(default, deserialize_with = "null_as_default")]
    pub icon: String,
    /// Guild IDs in sidebar order.
    #[serde(default, deserialize_with = "sf_list")]
    pub guild_ids: Vec<Snowflake>,
}

/// Theme values of [`UserSettings::theme`].
pub mod theme {
    /// Dark.
    pub const DARK: &str = "dark";
    /// Previous dark theme.
    pub const DARK_LEGACY: &str = "dark_legacy";
    /// Coal.
    pub const COAL: &str = "coal";
    /// Light.
    pub const LIGHT: &str = "light";
    /// Follows the operating system.
    pub const SYSTEM: &str = "system";
}

/// Account-wide settings (READY `user_settings`, USER_SETTINGS_UPDATE and
/// `GET /users/@me/settings`). Appearance: `theme`, `locale`,
/// `message_display_compact`, the media switches and the time formats. The
/// accent colour is the profile's ([`CurrentUser::accent_color`]); the
/// instance's brand colour is [`crate::Discovery::theme_color`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserSettings {
    /// `online`, `idle`, `dnd` or `invisible`.
    #[serde(default, deserialize_with = "null_as_default")]
    pub status: String,
    /// See [`theme`].
    #[serde(default, deserialize_with = "null_as_default")]
    pub theme: String,
    /// Interface locale, for example `en-US`.
    #[serde(default, deserialize_with = "null_as_default")]
    pub locale: String,
    /// Regional format for dates and numbers, or `None` to follow `locale`.
    #[serde(default)]
    pub regional_format: Option<String>,
    /// Compact message display.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub message_display_compact: bool,
    /// Show attachment media inline.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub inline_attachment_media: bool,
    /// Show embed media inline.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub inline_embed_media: bool,
    /// Play GIFs automatically.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub gif_auto_play: bool,
    /// Show embeds.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub render_embeds: bool,
    /// Show reactions.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub render_reactions: bool,
    /// Animate custom emoji.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub animate_emoji: bool,
    /// 0 always, 1 on interaction, 2 never.
    #[serde(default, deserialize_with = "u64_or_zero")]
    pub animate_stickers: u64,
    /// 0 always, 1 on click, 2 if moderator.
    #[serde(default, deserialize_with = "u64_or_zero")]
    pub render_spoilers: u64,
    /// 0 auto, 1 twelve-hour, 2 twenty-four-hour.
    #[serde(default, deserialize_with = "u64_or_zero")]
    pub time_format: u64,
    /// 0 auto, 1 ISO 8601, 2 day/month/year, 3 month/day/year.
    #[serde(default, deserialize_with = "u64_or_zero")]
    pub timestamp_format: u64,
    /// Developer mode.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub developer_mode: bool,
    /// Custom status.
    #[serde(default, deserialize_with = "super::message::lenient_opt")]
    pub custom_status: Option<CustomStatus>,
    /// Guild sidebar layout.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub guild_folders: Vec<GuildFolder>,
    /// Idle seconds before the account shows as away.
    #[serde(default, deserialize_with = "opt_u64")]
    pub afk_timeout: Option<u64>,
    /// Every other field (privacy, synced preferences, ...).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Changes for `PATCH /users/@me/settings`. Fields left `None` keep their value.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct UserSettingsUpdate {
    /// Status (not `offline`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Theme.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// Locale (must be a supported locale).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locale: Option<String>,
    /// Compact message display.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_display_compact: Option<bool>,
    /// Show attachment media inline.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inline_attachment_media: Option<bool>,
    /// Show embed media inline.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inline_embed_media: Option<bool>,
    /// Play GIFs automatically.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gif_auto_play: Option<bool>,
    /// Show embeds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub render_embeds: Option<bool>,
    /// Show reactions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub render_reactions: Option<bool>,
    /// Animate custom emoji.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub animate_emoji: Option<bool>,
    /// Time format.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_format: Option<u64>,
    /// Timestamp format.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp_format: Option<u64>,
    /// Custom status: `Some(None)` clears it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_status: Option<Option<CustomStatus>>,
}

/// The account-wide profile.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct UserProfile {
    /// Biography (markdown).
    #[serde(default)]
    pub bio: Option<String>,
    /// Pronouns.
    #[serde(default)]
    pub pronouns: Option<String>,
    /// Banner hash (see [`crate::Media::banner`]).
    #[serde(default)]
    pub banner: Option<String>,
    /// Colour taken from the banner.
    #[serde(default, deserialize_with = "opt_u64")]
    pub banner_color: Option<u64>,
    /// Accent colour as 24-bit RGB.
    #[serde(default, deserialize_with = "opt_u64")]
    pub accent_color: Option<u64>,
}

/// A guild both accounts are in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MutualGuild {
    /// Guild ID.
    #[serde(deserialize_with = "sf")]
    pub id: Snowflake,
    /// The target's nickname there.
    #[serde(default)]
    pub nick: Option<String>,
}

/// `GET /users/{id}/profile`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FullUserProfile {
    /// The account.
    pub user: User,
    /// Account-wide profile.
    #[serde(default, deserialize_with = "null_as_default")]
    pub user_profile: UserProfile,
    /// Membership in the requested guild.
    #[serde(default, deserialize_with = "super::message::lenient_opt")]
    pub guild_member: Option<GuildMember>,
    /// Profile in the requested guild.
    #[serde(default, deserialize_with = "super::message::lenient_opt")]
    pub guild_member_profile: Option<UserProfile>,
    /// Mutual friends (only when asked for).
    #[serde(default, deserialize_with = "lenient_vec")]
    pub mutual_friends: Vec<User>,
    /// Mutual guilds (only when asked for).
    #[serde(default, deserialize_with = "lenient_vec")]
    pub mutual_guilds: Vec<MutualGuild>,
    /// Whether privacy settings hide parts of the profile.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub profile_limited: bool,
    /// Every other field (badges, connections, premium, ...).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// How far the account has read in one channel (READY `read_states`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadState {
    /// Channel ID.
    #[serde(deserialize_with = "sf")]
    pub id: Snowflake,
    /// Unread mentions.
    #[serde(default, deserialize_with = "u64_or_zero")]
    pub mention_count: u64,
    /// The last message read.
    #[serde(default, deserialize_with = "opt_sf")]
    pub last_message_id: Option<Snowflake>,
    /// The last acknowledged pin time (ISO 8601).
    #[serde(default)]
    pub last_pin_timestamp: Option<String>,
    /// Read state flags.
    #[serde(default, deserialize_with = "opt_u64")]
    pub flags: Option<u64>,
}

impl ReadState {
    /// Whether a channel whose newest message is `channel_last_message_id` has unread messages.
    pub fn is_unread(&self, channel_last_message_id: Option<&str>) -> bool {
        unread(self.last_message_id.as_deref(), channel_last_message_id)
    }
}

/// Whether `last_message` is newer than `read_up_to`. A channel with messages
/// and no read state counts as unread.
pub fn unread(read_up_to: Option<&str>, last_message: Option<&str>) -> bool {
    match (read_up_to, last_message) {
        (_, None) => false,
        (None, Some(_)) => true,
        (Some(read), Some(last)) => snowflake_cmp(last, read).is_gt(),
    }
}

/// A user's presence (PRESENCE_UPDATE, READY `presences`, member list items).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Presence {
    /// The user (often only `id`). Missing inside a member list item.
    #[serde(default, deserialize_with = "super::message::lenient_opt")]
    pub user: Option<User>,
    /// `online`, `idle`, `dnd` or `offline`.
    #[serde(default, deserialize_with = "null_as_default")]
    pub status: String,
    /// The active status comes from a mobile session.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub mobile: bool,
    /// Every session is away.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub afk: bool,
    /// Custom status.
    #[serde(default, deserialize_with = "super::message::lenient_opt")]
    pub custom_status: Option<CustomStatus>,
    /// Guild the presence came through.
    #[serde(default, deserialize_with = "opt_sf")]
    pub guild_id: Option<Snowflake>,
}

impl Presence {
    /// The user ID.
    pub fn user_id(&self) -> Option<&str> {
        self.user.as_ref().map(|u| u.id.as_str())
    }
}

/// One live session of the account (READY `sessions`, SESSIONS_REPLACE).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionPresence {
    /// Session ID, or `all` for the combined entry.
    #[serde(default, deserialize_with = "null_as_default")]
    pub session_id: String,
    /// Status.
    #[serde(default, deserialize_with = "null_as_default")]
    pub status: String,
    /// Away.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub afk: bool,
    /// Mobile.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub mobile: bool,
}

impl CurrentUser {
    /// Profile accent colour (24-bit RGB).
    pub fn accent_color(&self) -> Option<u64> {
        self.user.extra.get("accent_color").and_then(value_u64)
    }

    /// Profile banner hash.
    pub fn banner(&self) -> Option<&str> {
        self.user.extra.get("banner")?.as_str()
    }

    /// Biography.
    pub fn bio(&self) -> Option<&str> {
        self.user.extra.get("bio")?.as_str()
    }

    /// Pronouns.
    pub fn pronouns(&self) -> Option<&str> {
        self.user.extra.get("pronouns")?.as_str()
    }

    /// Account traits, used to resolve limits ([`crate::Discovery::resolve_limit`]).
    pub fn traits(&self) -> Vec<String> {
        match self.user.extra.get("traits") {
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Whether the email address is verified (needed to add reactions).
    pub fn verified(&self) -> bool {
        self.user.extra.get("verified") == Some(&Value::Bool(true))
    }

    /// Whether the account can see age-restricted content.
    pub fn nsfw_allowed(&self) -> bool {
        self.user.extra.get("nsfw_allowed") == Some(&Value::Bool(true))
    }
}

impl Ready {
    /// Read states, typed.
    pub fn parsed_read_states(&self) -> Vec<ReadState> {
        super::parse_values(&self.read_states, "read state")
    }

    /// Account settings.
    pub fn user_settings(&self) -> Option<UserSettings> {
        extra_field(&self.extra, "user_settings")
    }

    /// Visible presences at connection time.
    pub fn presences(&self) -> Vec<Presence> {
        extra_list(&self.extra, "presences")
    }

    /// The account's live sessions; the first is the combined `all` entry.
    pub fn sessions(&self) -> Vec<SessionPresence> {
        extra_list(&self.extra, "sessions")
    }

    /// Pinned DM channel IDs.
    pub fn pinned_dms(&self) -> Vec<Snowflake> {
        self.extra
            .get("pinned_dms")
            .and_then(|v| sf_list(v.clone()).ok())
            .unwrap_or_default()
    }

    /// Per-guild notification settings, raw.
    pub fn user_guild_settings(&self) -> Vec<Value> {
        match self.extra.get("user_guild_settings") {
            Some(Value::Array(a)) => a.clone(),
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ready_read_states_settings_and_presences() {
        let r: Ready = serde_json::from_value(json!({
            "session_id": "s", "user": {"id": "1", "username": "me", "accent_color": 255,
                                         "traits": ["premium", 3], "verified": true},
            "read_states": [{"id": "200", "mention_count": 2, "last_message_id": "500"},
                            {"id": "201", "mention_count": null, "last_message_id": null},
                            {"mention_count": 1}],
            "user_settings": {"theme": "light", "locale": "nb-NO", "message_display_compact": true,
                              "custom_status": {"text": "away", "emoji_name": "🌙"},
                              "guild_folders": [{"id": -1, "guild_ids": ["100", 101], "icon": "folder", "flags": 0}],
                              "status": "online", "time_format": 2},
            "presences": [{"user": {"id": "2"}, "status": "dnd", "mobile": false, "afk": false, "custom_status": null}],
            "sessions": [{"session_id": "all", "status": "online", "afk": false, "mobile": false}],
            "pinned_dms": ["9"]
        }))
        .unwrap();
        let rs = r.parsed_read_states();
        assert_eq!(rs.len(), 2);
        assert!(rs[0].is_unread(Some("501")));
        assert!(!rs[0].is_unread(Some("500")));
        assert!(!rs[0].is_unread(Some("49")));
        assert!(rs[1].is_unread(Some("1")));
        assert!(!rs[1].is_unread(None));
        let s = r.user_settings().unwrap();
        assert_eq!(s.theme, theme::LIGHT);
        assert_eq!(s.locale, "nb-NO");
        assert!(s.message_display_compact);
        assert_eq!(s.time_format, 2);
        assert_eq!(s.custom_status.unwrap().text.as_deref(), Some("away"));
        assert_eq!(s.guild_folders[0].guild_ids, vec!["100", "101"]);
        assert_eq!(r.presences()[0].status, "dnd");
        assert_eq!(r.sessions()[0].session_id, "all");
        assert_eq!(r.pinned_dms(), vec!["9"]);
        assert_eq!(r.user.accent_color(), Some(255));
        assert_eq!(r.user.traits(), vec!["premium"]);
        assert!(r.user.verified());
    }

    #[test]
    fn settings_update_serializes_only_set_fields() {
        let u = UserSettingsUpdate {
            theme: Some("dark".into()),
            custom_status: Some(None),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_value(&u).unwrap(),
            json!({"theme": "dark", "custom_status": null})
        );
    }
}
