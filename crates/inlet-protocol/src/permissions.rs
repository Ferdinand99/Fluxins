//! Permission bits and a calculator that follows Fluxer's rules
//! (`http-api/permissions`): owner has everything; otherwise the everyone
//! role (ID = guild ID) plus the member's roles; `ADMINISTRATOR` grants every
//! bit and skips overwrites; in a channel, the everyone overwrite, then the
//! member's role overwrites combined, then the member's own overwrite, each
//! removing denied bits before adding allowed ones. Threads use the parent's
//! permissions with `SEND_MESSAGES_IN_THREADS` in place of `SEND_MESSAGES`.
//!
//! Use it to hide channels the account cannot view and to disable the
//! composer where it cannot send. The server still checks every request.
//!
//! ```
//! use inlet_protocol::permissions::{self, PermissionContext};
//! # let (roles, member_roles, channels) = (vec![], vec![], vec![]);
//! let ctx = PermissionContext {
//!     guild_id: "100", owner_id: Some("1"), user_id: "10",
//!     roles: &roles, member_roles: &member_roles, timed_out: false,
//! };
//! let visible = ctx.visible_channels(&channels);
//! # let _ = visible;
//! ```

use crate::models::{overwrite_type, Channel, Role, Snowflake};

/// Create invites.
pub const CREATE_INSTANT_INVITE: u64 = 1 << 0;
/// Kick members.
pub const KICK_MEMBERS: u64 = 1 << 1;
/// Ban members.
pub const BAN_MEMBERS: u64 = 1 << 2;
/// Every permission; ignores overwrites.
pub const ADMINISTRATOR: u64 = 1 << 3;
/// Manage channels.
pub const MANAGE_CHANNELS: u64 = 1 << 4;
/// Manage the guild.
pub const MANAGE_GUILD: u64 = 1 << 5;
/// Add reactions.
pub const ADD_REACTIONS: u64 = 1 << 6;
/// Read the audit log.
pub const VIEW_AUDIT_LOG: u64 = 1 << 7;
/// Priority speaker.
pub const PRIORITY_SPEAKER: u64 = 1 << 8;
/// Share screen or camera.
pub const STREAM: u64 = 1 << 9;
/// View a channel.
pub const VIEW_CHANNEL: u64 = 1 << 10;
/// Send messages.
pub const SEND_MESSAGES: u64 = 1 << 11;
/// Send text-to-speech messages.
pub const SEND_TTS_MESSAGES: u64 = 1 << 12;
/// Manage others' messages and reactions.
pub const MANAGE_MESSAGES: u64 = 1 << 13;
/// Links become embeds.
pub const EMBED_LINKS: u64 = 1 << 14;
/// Attach files.
pub const ATTACH_FILES: u64 = 1 << 15;
/// Read older messages.
pub const READ_MESSAGE_HISTORY: u64 = 1 << 16;
/// `@everyone` and `@here`.
pub const MENTION_EVERYONE: u64 = 1 << 17;
/// Emoji from other guilds.
pub const USE_EXTERNAL_EMOJIS: u64 = 1 << 18;
/// Connect to voice.
pub const CONNECT: u64 = 1 << 20;
/// Speak in voice.
pub const SPEAK: u64 = 1 << 21;
/// Mute members.
pub const MUTE_MEMBERS: u64 = 1 << 22;
/// Deafen members.
pub const DEAFEN_MEMBERS: u64 = 1 << 23;
/// Move members.
pub const MOVE_MEMBERS: u64 = 1 << 24;
/// Voice activity detection.
pub const USE_VAD: u64 = 1 << 25;
/// Change own nickname.
pub const CHANGE_NICKNAME: u64 = 1 << 26;
/// Change others' nicknames.
pub const MANAGE_NICKNAMES: u64 = 1 << 27;
/// Manage roles and overwrites.
pub const MANAGE_ROLES: u64 = 1 << 28;
/// Manage webhooks.
pub const MANAGE_WEBHOOKS: u64 = 1 << 29;
/// Manage emoji and stickers.
pub const MANAGE_EXPRESSIONS: u64 = 1 << 30;
/// Manage threads.
pub const MANAGE_THREADS: u64 = 1 << 34;
/// Start public threads.
pub const CREATE_PUBLIC_THREADS: u64 = 1 << 35;
/// Start private threads.
pub const CREATE_PRIVATE_THREADS: u64 = 1 << 36;
/// Stickers from other guilds.
pub const USE_EXTERNAL_STICKERS: u64 = 1 << 37;
/// Send messages in threads.
pub const SEND_MESSAGES_IN_THREADS: u64 = 1 << 38;
/// Time out members.
pub const MODERATE_MEMBERS: u64 = 1 << 40;
/// Create emoji and stickers.
pub const CREATE_EXPRESSIONS: u64 = 1 << 43;
/// Pin messages.
pub const PIN_MESSAGES: u64 = 1 << 51;
/// Ignore slowmode.
pub const BYPASS_SLOWMODE: u64 = 1 << 52;
/// Change a voice channel's region.
pub const UPDATE_RTC_REGION: u64 = 1 << 53;
/// View a channel's member list.
pub const VIEW_CHANNEL_MEMBERS: u64 = 1 << 54;

/// Every bit Fluxer defines.
pub const ALL: u64 = CREATE_INSTANT_INVITE
    | KICK_MEMBERS
    | BAN_MEMBERS
    | ADMINISTRATOR
    | MANAGE_CHANNELS
    | MANAGE_GUILD
    | ADD_REACTIONS
    | VIEW_AUDIT_LOG
    | PRIORITY_SPEAKER
    | STREAM
    | VIEW_CHANNEL
    | SEND_MESSAGES
    | SEND_TTS_MESSAGES
    | MANAGE_MESSAGES
    | EMBED_LINKS
    | ATTACH_FILES
    | READ_MESSAGE_HISTORY
    | MENTION_EVERYONE
    | USE_EXTERNAL_EMOJIS
    | CONNECT
    | SPEAK
    | MUTE_MEMBERS
    | DEAFEN_MEMBERS
    | MOVE_MEMBERS
    | USE_VAD
    | CHANGE_NICKNAME
    | MANAGE_NICKNAMES
    | MANAGE_ROLES
    | MANAGE_WEBHOOKS
    | MANAGE_EXPRESSIONS
    | MANAGE_THREADS
    | CREATE_PUBLIC_THREADS
    | CREATE_PRIVATE_THREADS
    | USE_EXTERNAL_STICKERS
    | SEND_MESSAGES_IN_THREADS
    | MODERATE_MEMBERS
    | CREATE_EXPRESSIONS
    | PIN_MESSAGES
    | BYPASS_SLOWMODE
    | UPDATE_RTC_REGION
    | VIEW_CHANNEL_MEMBERS;

/// What a member may do while timed out: read, but not send, react, type or pin.
const BLOCKED_WHILE_TIMED_OUT: u64 = SEND_MESSAGES
    | SEND_MESSAGES_IN_THREADS
    | ADD_REACTIONS
    | CREATE_PUBLIC_THREADS
    | CREATE_PRIVATE_THREADS
    | CHANGE_NICKNAME
    | ATTACH_FILES;

/// Everything needed to compute one member's permissions in one guild.
#[derive(Debug, Clone, Copy)]
pub struct PermissionContext<'a> {
    /// The guild (also the everyone role's ID).
    pub guild_id: &'a str,
    /// The guild owner.
    pub owner_id: Option<&'a str>,
    /// The member.
    pub user_id: &'a str,
    /// Every role in the guild.
    pub roles: &'a [Role],
    /// The member's role IDs.
    pub member_roles: &'a [Snowflake],
    /// Whether a communication timeout is running ([`crate::GuildMember::is_timed_out`]).
    pub timed_out: bool,
}

impl PermissionContext<'_> {
    fn is_owner(&self) -> bool {
        self.owner_id == Some(self.user_id)
    }

    /// Guild-level permissions (before channel overwrites).
    pub fn guild(&self) -> u64 {
        if self.is_owner() {
            return ALL;
        }
        let mut bits = 0u64;
        for role in self.roles {
            if role.id == self.guild_id || self.member_roles.iter().any(|r| r == &role.id) {
                bits |= role.permissions;
            }
        }
        if bits & ADMINISTRATOR != 0 {
            return ALL;
        }
        self.after_timeout(bits & ALL)
    }

    fn after_timeout(&self, bits: u64) -> u64 {
        if self.timed_out && !self.is_owner() && bits & ADMINISTRATOR == 0 {
            bits & !BLOCKED_WHILE_TIMED_OUT
        } else {
            bits
        }
    }

    /// Permissions in a channel. For a thread pass its parent as `parent`
    /// (overwrites come from the parent). A channel without a guild (DM,
    /// group DM, notes) gets every bit.
    pub fn channel(&self, channel: &Channel, parent: Option<&Channel>) -> u64 {
        if channel.guild_id.is_none() {
            return ALL;
        }
        let source = if channel.is_thread() {
            match parent {
                Some(p) => p,
                // Without the parent the thread inherits nothing it can be sure of.
                None => return self.guild_level_for_unknown_parent(),
            }
        } else {
            channel
        };
        let mut bits = self.overwritten(source);
        if channel.is_thread() {
            // Threads send with SEND_MESSAGES_IN_THREADS.
            bits &= !SEND_MESSAGES;
            if bits & SEND_MESSAGES_IN_THREADS != 0 {
                bits |= SEND_MESSAGES;
            }
        }
        bits
    }

    fn guild_level_for_unknown_parent(&self) -> u64 {
        let g = self.guild();
        if g == ALL {
            ALL
        } else {
            0
        }
    }

    /// Base permissions with the channel's overwrites applied.
    fn overwritten(&self, channel: &Channel) -> u64 {
        let base = self.guild();
        if base & ADMINISTRATOR != 0 {
            return ALL;
        }
        let overwrites = channel.permission_overwrites();
        let mut bits = base;
        // 1. The everyone role.
        if let Some(o) = overwrites
            .iter()
            .find(|o| o.kind == overwrite_type::ROLE && o.id == self.guild_id)
        {
            bits = (bits & !o.deny) | o.allow;
        }
        // 2. The member's roles, combined: an allow on one role beats a deny on another.
        let (mut allow, mut deny) = (0u64, 0u64);
        for o in overwrites.iter().filter(|o| {
            o.kind == overwrite_type::ROLE
                && o.id != self.guild_id
                && self.member_roles.iter().any(|r| r == &o.id)
        }) {
            allow |= o.allow;
            deny |= o.deny;
        }
        bits = (bits & !deny) | allow;
        // 3. The member's own overwrite.
        if let Some(o) = overwrites
            .iter()
            .find(|o| o.kind == overwrite_type::MEMBER && o.id == self.user_id)
        {
            bits = (bits & !o.deny) | o.allow;
        }
        self.after_timeout(bits & ALL)
    }

    /// Whether the member can see the channel. A category is visible when at
    /// least one channel in it is; use [`Self::visible_channels`] for that.
    pub fn can_view(&self, channel: &Channel, parent: Option<&Channel>) -> bool {
        self.channel(channel, parent) & VIEW_CHANNEL != 0
    }

    /// Whether the composer should be enabled: view and send (threads use
    /// `SEND_MESSAGES_IN_THREADS`), and the channel takes text messages.
    pub fn can_send(&self, channel: &Channel, parent: Option<&Channel>) -> bool {
        let bits = self.channel(channel, parent);
        channel.is_text() && bits & VIEW_CHANNEL != 0 && bits & SEND_MESSAGES != 0
    }

    /// Whether `perm` (one or more bits) is granted in the channel.
    pub fn has(&self, channel: &Channel, parent: Option<&Channel>, perm: u64) -> bool {
        self.channel(channel, parent) & perm == perm
    }

    /// The channels to show: viewable channels, threads whose parent is
    /// viewable, and categories with at least one visible child. Keeps the
    /// input order.
    pub fn visible_channels<'c>(&self, channels: &'c [Channel]) -> Vec<&'c Channel> {
        use crate::models::channel_type::GUILD_CATEGORY;
        let by_id = |id: &str| channels.iter().find(|c| c.id == id);
        let viewable = |c: &Channel| -> bool {
            let parent = if c.is_thread() {
                c.parent_id.as_deref().and_then(by_id)
            } else {
                None
            };
            self.can_view(c, parent)
        };
        let shown: Vec<bool> = channels
            .iter()
            .map(|c| c.kind != GUILD_CATEGORY && viewable(c))
            .collect();
        channels
            .iter()
            .enumerate()
            .filter(|(i, c)| {
                if c.kind == GUILD_CATEGORY {
                    channels
                        .iter()
                        .zip(&shown)
                        .any(|(child, s)| *s && child.parent_id.as_deref() == Some(c.id.as_str()))
                } else {
                    shown[*i]
                }
            })
            .map(|(_, c)| c)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::channel_type;
    use serde_json::json;

    fn role(id: &str, perms: u64) -> Role {
        serde_json::from_value(
            json!({"id": id, "name": id, "position": 0, "permissions": perms.to_string()}),
        )
        .unwrap()
    }

    fn channel(
        id: &str,
        kind: i64,
        parent: Option<&str>,
        overwrites: serde_json::Value,
    ) -> Channel {
        serde_json::from_value(json!({
            "id": id, "type": kind, "guild_id": "100", "parent_id": parent,
            "permission_overwrites": overwrites
        }))
        .unwrap()
    }

    fn ctx<'a>(
        roles: &'a [Role],
        member_roles: &'a [Snowflake],
        user: &'a str,
    ) -> PermissionContext<'a> {
        PermissionContext {
            guild_id: "100",
            owner_id: Some("1"),
            user_id: user,
            roles,
            member_roles,
            timed_out: false,
        }
    }

    #[test]
    fn overwrite_order_and_admin() {
        let roles = vec![
            role("100", VIEW_CHANNEL | SEND_MESSAGES),
            role("7", 0),
            role("8", 0),
            role("9", ADMINISTRATOR),
        ];
        let member_roles = vec!["7".to_string(), "8".to_string()];
        let c = ctx(&roles, &member_roles, "10");
        assert_eq!(c.guild(), VIEW_CHANNEL | SEND_MESSAGES);

        // Everyone denied view; role 7 allows it; role 8 denies send; member denies nothing.
        let ch = channel(
            "200",
            channel_type::GUILD_TEXT,
            None,
            json!([
                {"id": "100", "type": 0, "allow": "0", "deny": (VIEW_CHANNEL | SEND_MESSAGES).to_string()},
                {"id": "7", "type": 0, "allow": VIEW_CHANNEL.to_string(), "deny": "0"},
                {"id": "8", "type": 0, "allow": "0", "deny": VIEW_CHANNEL.to_string()},
                {"id": "99", "type": 0, "allow": SEND_MESSAGES.to_string(), "deny": "0"}
            ]),
        );
        // Role 7 allow beats role 8 deny; send stays denied by everyone.
        assert!(c.can_view(&ch, None));
        assert!(!c.can_send(&ch, None));

        // A member overwrite comes last.
        let ch2 = channel(
            "201",
            channel_type::GUILD_TEXT,
            None,
            json!([
                {"id": "7", "type": 0, "allow": SEND_MESSAGES.to_string(), "deny": "0"},
                {"id": "10", "type": 1, "allow": "0", "deny": SEND_MESSAGES.to_string()}
            ]),
        );
        assert!(!c.can_send(&ch2, None));
        assert!(c.can_view(&ch2, None));

        // Administrator ignores overwrites; the owner has everything.
        let admin_roles = vec!["9".to_string()];
        let a = ctx(&roles, &admin_roles, "11");
        assert!(a.can_send(&ch, None));
        let owner = ctx(&roles, &[], "1");
        assert_eq!(owner.channel(&ch, None), ALL);
    }

    #[test]
    fn threads_categories_dms_and_timeouts() {
        let roles = vec![role("100", VIEW_CHANNEL | SEND_MESSAGES)];
        let c = ctx(&roles, &[], "10");
        let cat_open = channel("300", channel_type::GUILD_CATEGORY, None, json!([]));
        let cat_hidden = channel("301", channel_type::GUILD_CATEGORY, None, json!([]));
        let text = channel("302", channel_type::GUILD_TEXT, Some("300"), json!([]));
        let secret = channel(
            "303",
            channel_type::GUILD_TEXT,
            Some("301"),
            json!([{"id": "100", "type": 0, "allow": "0", "deny": VIEW_CHANNEL.to_string()}]),
        );
        let thread = channel("304", channel_type::PUBLIC_THREAD, Some("302"), json!([]));
        let all = vec![cat_open, cat_hidden, text, secret, thread];
        let ids: Vec<&str> = c
            .visible_channels(&all)
            .iter()
            .map(|c| c.id.as_str())
            .collect();
        assert_eq!(ids, vec!["300", "302", "304"]);
        // No SEND_MESSAGES_IN_THREADS: the thread composer is off.
        assert!(!c.can_send(&all[4], Some(&all[2])));
        let roles2 = vec![role("100", VIEW_CHANNEL | SEND_MESSAGES_IN_THREADS)];
        let c2 = ctx(&roles2, &[], "10");
        assert!(c2.can_send(&all[4], Some(&all[2])));
        assert!(!c2.can_send(&all[2], None));
        // A thread without its parent is hidden unless admin.
        assert!(!c2.can_view(&all[4], None));

        let dm: Channel = serde_json::from_value(json!({"id": "9", "type": 1})).unwrap();
        assert!(c.can_send(&dm, None));

        let timed_out = PermissionContext {
            timed_out: true,
            ..c
        };
        assert!(timed_out.can_view(&all[2], None));
        assert!(!timed_out.can_send(&all[2], None));
    }
}
