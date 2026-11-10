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
//! It fails closed: before the roles are loaded nothing is visible (show a
//! loading state), and overwrites that do not parse deny.
//!
//! Also handled: visited guilds (everyone role without `ADMINISTRATOR`, only
//! the everyone overwrite, masked to view and history), the MFA rule for
//! elevated bits, timeouts, and locked, archived and private threads.
//!
//! ```
//! use inlet_protocol::permissions::{self, PermissionContext};
//! # let (roles, member_roles, channels) = (vec![], vec![], vec![]);
//! let ctx = PermissionContext {
//!     guild_id: "100", owner_id: Some("1"), user_id: "10",
//!     roles: &roles, member_roles: &member_roles,
//!     ..Default::default()
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

/// Bits that need two-factor authentication in a guild with the elevated MFA
/// level (Fluxer's `ElevatedPermissions` and the documented list, combined).
pub const ELEVATED: u64 = KICK_MEMBERS
    | BAN_MEMBERS
    | ADMINISTRATOR
    | MANAGE_CHANNELS
    | MANAGE_GUILD
    | MANAGE_ROLES
    | MANAGE_MESSAGES
    | MANAGE_WEBHOOKS
    | MANAGE_EXPRESSIONS
    | MODERATE_MEMBERS
    | MANAGE_THREADS;

/// What a visitor (a guild the account visits without joining) can have.
pub const GUILD_VISITOR_MASK: u64 = VIEW_CHANNEL | READ_MESSAGE_HISTORY;

/// What a member may not do while timed out: send, react, upload, type,
/// change the nickname or start threads. Reading stays allowed.
const BLOCKED_WHILE_TIMED_OUT: u64 = SEND_MESSAGES
    | SEND_MESSAGES_IN_THREADS
    | ADD_REACTIONS
    | CREATE_PUBLIC_THREADS
    | CREATE_PRIVATE_THREADS
    | CHANGE_NICKNAME
    | ATTACH_FILES;

/// Thread bits an administrator gets implicitly.
const THREAD_BITS: u64 =
    MANAGE_THREADS | CREATE_PUBLIC_THREADS | CREATE_PRIVATE_THREADS | SEND_MESSAGES_IN_THREADS;

/// Everything needed to compute the signed-in member's permissions in one guild.
///
/// The calculator fails closed: until the guild's roles are loaded (READY or
/// GUILD_CREATE/GUILD_SYNC, or [`crate::Client::roles`]) there is no
/// everyone role, every result is 0 and every guild channel is hidden. Show
/// a loading state until the roles are there instead of an empty sidebar.
/// Overwrites that do not parse deny everything for their target, and an
/// overwrite list that does not parse hides the channel.
///
/// Build it with `..Default::default()` so new fields do not break callers.
#[derive(Debug, Clone, Copy, Default)]
pub struct PermissionContext<'a> {
    /// The guild (also the everyone role's ID).
    pub guild_id: &'a str,
    /// The guild owner.
    pub owner_id: Option<&'a str>,
    /// The member (the signed-in account).
    pub user_id: &'a str,
    /// Every role in the guild.
    pub roles: &'a [Role],
    /// The member's role IDs.
    pub member_roles: &'a [Snowflake],
    /// Whether a communication timeout is running ([`crate::GuildMember::is_timed_out`]).
    pub timed_out: bool,
    /// The account visits the guild without being a member (`visiting` on the
    /// guild). A visitor has only the everyone role's view and history bits.
    /// Note that a visiting session gets messages only for the channel set as
    /// `focused_channel_id` in a Lazy Request, which this crate does not send yet.
    pub visiting: bool,
    /// The guild's MFA level is elevated (`Guild::mfa_level() == 1`) and the
    /// account has no second factor ([`crate::CurrentUser::mfa_enabled`]). Then
    /// [`ELEVATED`] bits are removed for everyone but the owner.
    pub mfa_missing: bool,
}

/// Overwrites of one channel as the calculator reads them.
struct Overwrites {
    /// `(id, type, allow, deny)`.
    entries: Vec<(Snowflake, i64, u64, u64)>,
    /// Some entry had no usable target: deny everything for everyone.
    unknown_target: bool,
}

/// Reads `permission_overwrites` strictly: an entry whose `allow` or `deny`
/// does not parse becomes "allow nothing, deny everything" for its target; an
/// entry without a usable target, or a field that is not a list, denies
/// everything to everyone.
fn read_overwrites(channel: &Channel) -> Overwrites {
    use crate::models::value_u64;
    use serde_json::Value;
    let mut out = Overwrites {
        entries: Vec::new(),
        unknown_target: false,
    };
    let items = match channel.extra.get("permission_overwrites") {
        None | Some(Value::Null) => return out,
        Some(Value::Array(items)) => items,
        Some(_) => {
            out.unknown_target = true;
            return out;
        }
    };
    for item in items {
        let id = match item.get("id") {
            Some(Value::String(s)) if crate::media::is_snowflake(s) => s.clone(),
            Some(Value::Number(n)) if n.is_u64() => n.to_string(),
            _ => {
                out.unknown_target = true;
                continue;
            }
        };
        let kind = match item.get("type").and_then(Value::as_i64) {
            Some(k @ (overwrite_type::ROLE | overwrite_type::MEMBER)) => k,
            _ => {
                out.unknown_target = true;
                continue;
            }
        };
        let allow = item.get("allow").and_then(value_u64);
        let deny = item.get("deny").and_then(value_u64);
        let (allow, deny) = match (allow, deny) {
            (Some(a), Some(d)) => (a, d),
            _ => (0, u64::MAX),
        };
        out.entries.push((id, kind, allow, deny));
    }
    out
}

fn thread_locked(channel: &Channel) -> bool {
    thread_flag(channel, "locked")
}

fn thread_archived(channel: &Channel) -> bool {
    thread_flag(channel, "archived")
}

fn thread_flag(channel: &Channel, name: &str) -> bool {
    channel
        .extra
        .get("thread_metadata")
        .and_then(|m| m.get(name))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// Whether the account joined the thread: `member` on the thread channel.
/// Fluxer sends it in READY/GUILD_CREATE `threads`, THREAD_CREATE, THREAD_LIST_SYNC
/// and REST answers (a missing `member` there means "not a member"; store
/// those with [`Channel::replace_thread`]), but strips it from THREAD_UPDATE,
/// so the stored thread must keep it across one ([`Channel::merge_thread_update`],
/// [`Channel::set_thread_member`]).
fn thread_member(channel: &Channel) -> bool {
    channel.is_thread_member()
}

impl PermissionContext<'_> {
    fn is_owner(&self) -> bool {
        !self.visiting && self.owner_id.is_some_and(|o| o == self.user_id)
    }

    fn everyone(&self) -> Option<&Role> {
        self.roles.iter().find(|r| r.id == self.guild_id)
    }

    /// Guild-level permissions (before channel overwrites).
    pub fn guild(&self) -> u64 {
        if self.visiting {
            return self
                .everyone()
                .map_or(0, |r| r.permissions & !ADMINISTRATOR)
                & GUILD_VISITOR_MASK;
        }
        if self.is_owner() {
            return ALL;
        }
        if self.everyone().is_none() {
            // Roles not loaded yet: fail closed.
            return 0;
        }
        let mut bits = 0u64;
        for role in self.roles {
            if role.id == self.guild_id || self.member_roles.iter().any(|r| r == &role.id) {
                bits |= role.permissions;
            }
        }
        if bits & ADMINISTRATOR != 0 {
            return self.after_mfa(ALL);
        }
        self.after_timeout(self.after_mfa(bits & ALL))
    }

    fn after_mfa(&self, bits: u64) -> u64 {
        if self.mfa_missing && !self.is_owner() {
            bits & !ELEVATED
        } else {
            bits
        }
    }

    /// Whether a timeout blocks writing: not for the owner or an administrator.
    /// The administrator exemption holds without MFA too: Fluxer checks the
    /// computed permissions, where `ADMINISTRATOR` stays set (only the
    /// elevated actions need the second factor).
    fn communication_blocked(&self) -> bool {
        self.timed_out && !self.is_owner() && !self.has_admin_role()
    }

    /// The member has a role with `ADMINISTRATOR` (before the MFA rule).
    fn has_admin_role(&self) -> bool {
        !self.visiting
            && self.roles.iter().any(|r| {
                (r.id == self.guild_id || self.member_roles.iter().any(|m| m == &r.id))
                    && r.permissions & ADMINISTRATOR != 0
            })
    }

    /// Administrator after the MFA rule (fails closed when MFA is missing).
    fn is_admin(&self) -> bool {
        self.has_admin_role() && !self.mfa_missing
    }

    fn after_timeout(&self, bits: u64) -> u64 {
        if self.communication_blocked() {
            bits & !BLOCKED_WHILE_TIMED_OUT
        } else {
            bits
        }
    }

    /// Permissions in a channel. For a thread pass its parent as `parent`
    /// (overwrites come from the parent; without it a thread gets nothing
    /// unless the account is owner or administrator). A channel without a
    /// guild (DM, group DM, notes) gets every bit. Thread rules for locked,
    /// archived and private threads are applied too.
    pub fn channel(&self, channel: &Channel, parent: Option<&Channel>) -> u64 {
        if channel.guild_id.is_none() {
            return ALL;
        }
        if !channel.is_thread() {
            return self.overwritten(channel);
        }
        let parent_bits = match parent {
            Some(p) => self.overwritten(p),
            None if self.is_owner() || self.is_admin() => self.guild(),
            None => return 0,
        };
        self.thread_bits(channel, parent_bits)
    }

    /// Fluxer's thread rules on top of the parent's permissions.
    fn thread_bits(&self, thread: &Channel, parent_bits: u64) -> u64 {
        let mut bits = if parent_bits & ADMINISTRATOR != 0 {
            parent_bits | THREAD_BITS
        } else {
            parent_bits
        };
        // Threads send with SEND_MESSAGES_IN_THREADS.
        bits &= !SEND_MESSAGES;
        if bits & SEND_MESSAGES_IN_THREADS != 0 {
            bits |= SEND_MESSAGES;
        }
        let moderator = self.is_owner()
            || bits & ADMINISTRATOR != 0
            || (!self.timed_out && bits & MANAGE_THREADS != 0);
        // A private thread needs membership or moderation.
        if thread.kind == crate::models::channel_type::PRIVATE_THREAD
            && !thread_member(thread)
            && !moderator
        {
            return 0;
        }
        if !self.is_owner() && bits & VIEW_CHANNEL == 0 {
            return 0;
        }
        // A locked thread only takes writes from moderators.
        if thread_locked(thread) && !moderator {
            bits &= !(SEND_MESSAGES | SEND_MESSAGES_IN_THREADS | ADD_REACTIONS | PIN_MESSAGES);
        }
        // Reactions and pins need an open thread (sending unarchives it).
        if thread_archived(thread) {
            bits &= !(ADD_REACTIONS | PIN_MESSAGES);
        }
        // Pinning in a thread is blocked by a timeout too.
        if self.communication_blocked() {
            bits &= !PIN_MESSAGES;
        }
        bits
    }

    /// Base permissions with the channel's overwrites applied.
    fn overwritten(&self, channel: &Channel) -> u64 {
        if self.is_owner() {
            return ALL;
        }
        if self.has_admin_role() {
            // Administrators skip overwrites (the MFA rule is already applied).
            return self.guild();
        }
        let base = self.guild();
        let ow = read_overwrites(channel);
        if ow.unknown_target {
            return 0;
        }
        let mut bits = base;
        // 1. The everyone role.
        if let Some((_, _, allow, deny)) = ow
            .entries
            .iter()
            .find(|(id, kind, _, _)| *kind == overwrite_type::ROLE && id == self.guild_id)
        {
            bits = (bits & !deny) | allow;
        }
        if self.visiting {
            // A visitor gets the everyone overwrite only.
            return bits & GUILD_VISITOR_MASK;
        }
        // 2. The member's roles, combined: an allow on one role beats a deny on another.
        let (mut allow, mut deny) = (0u64, 0u64);
        for (_, _, a, d) in ow.entries.iter().filter(|(id, kind, _, _)| {
            *kind == overwrite_type::ROLE
                && id != self.guild_id
                && self.member_roles.iter().any(|r| r == id)
        }) {
            allow |= a;
            deny |= d;
        }
        // A role overwrite that did not parse denies everything (fail closed).
        bits = (bits & !deny) | (allow & !self.unparsed_role_deny(&ow));
        // 3. The member's own overwrite.
        if let Some((_, _, allow, deny)) = ow
            .entries
            .iter()
            .find(|(id, kind, _, _)| *kind == overwrite_type::MEMBER && id == self.user_id)
        {
            bits = (bits & !deny) | allow;
        }
        self.after_timeout(self.after_mfa(bits & ALL))
    }

    /// Bits an unparsable overwrite of one of the member's roles keeps denied.
    fn unparsed_role_deny(&self, ow: &Overwrites) -> u64 {
        let broken = ow.entries.iter().any(|(id, kind, allow, deny)| {
            *kind == overwrite_type::ROLE
                && *allow == 0
                && *deny == u64::MAX
                && self.member_roles.iter().any(|r| r == id)
        });
        if broken {
            u64::MAX
        } else {
            0
        }
    }

    /// Whether the member can see the channel. A category is visible when at
    /// least one channel in it is; use [`Self::visible_channels`] for that.
    pub fn can_view(&self, channel: &Channel, parent: Option<&Channel>) -> bool {
        self.channel(channel, parent) & VIEW_CHANNEL != 0
    }

    /// Whether the composer should be enabled: view and send (threads use
    /// `SEND_MESSAGES_IN_THREADS`; locked threads only for moderators), the
    /// channel takes text messages, and no timeout is running.
    pub fn can_send(&self, channel: &Channel, parent: Option<&Channel>) -> bool {
        let bits = self.channel(channel, parent);
        channel.is_text() && bits & VIEW_CHANNEL != 0 && bits & SEND_MESSAGES != 0
    }

    /// Whether files can be attached (send plus `ATTACH_FILES`).
    pub fn can_attach(&self, channel: &Channel, parent: Option<&Channel>) -> bool {
        self.can_send(channel, parent) && self.has(channel, parent, ATTACH_FILES)
    }

    /// Whether the member can add a new reaction.
    pub fn can_react(&self, channel: &Channel, parent: Option<&Channel>) -> bool {
        let bits = self.channel(channel, parent);
        bits & VIEW_CHANNEL != 0 && bits & ADD_REACTIONS != 0
    }

    /// Whether the member can pin or unpin messages (`PIN_MESSAGES`; in a
    /// thread also not locked, archived or during a timeout). In a DM anyone can.
    pub fn can_pin(&self, channel: &Channel, parent: Option<&Channel>) -> bool {
        let bits = self.channel(channel, parent);
        bits & VIEW_CHANNEL != 0 && bits & PIN_MESSAGES != 0
    }

    /// Whether the member can edit `message`: their own `DEFAULT` or `REPLY`
    /// message, no timeout, and in a thread one that is neither archived nor
    /// locked (unless moderator).
    pub fn can_edit_message(
        &self,
        message: &crate::models::Message,
        channel: &Channel,
        parent: Option<&Channel>,
    ) -> bool {
        if !message.can_edit(self.user_id) || !self.can_view(channel, parent) {
            return false;
        }
        if channel.guild_id.is_some() && self.communication_blocked() {
            return false;
        }
        if channel.is_thread() {
            if thread_archived(channel) {
                return false;
            }
            let bits = self.channel(channel, parent);
            let moderator =
                self.is_owner() || bits & ADMINISTRATOR != 0 || bits & MANAGE_THREADS != 0;
            if thread_locked(channel) && !moderator {
                return false;
            }
        }
        true
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
    use crate::models::{channel_type, Message};
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

    fn thread(id: &str, kind: i64, parent: &str, meta: serde_json::Value) -> Channel {
        serde_json::from_value(json!({
            "id": id, "type": kind, "guild_id": "100", "parent_id": parent,
            "thread_metadata": meta
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
            ..Default::default()
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

        let admin_roles = vec!["9".to_string()];
        let a = ctx(&roles, &admin_roles, "11");
        assert!(a.can_send(&ch, None));
        let owner = ctx(&roles, &[], "1");
        assert_eq!(owner.channel(&ch, None), ALL);
    }

    #[test]
    fn roles_not_loaded_fail_closed() {
        let c = ctx(&[], &[], "10");
        let ch = channel("200", channel_type::GUILD_TEXT, None, json!([]));
        assert_eq!(c.guild(), 0);
        assert!(!c.can_view(&ch, None));
        assert!(c.visible_channels(std::slice::from_ref(&ch)).is_empty());
    }

    #[test]
    fn broken_overwrites_fail_closed() {
        let roles = vec![role("100", VIEW_CHANNEL | SEND_MESSAGES), role("7", 0)];
        let member_roles = vec!["7".to_string()];
        let c = ctx(&roles, &member_roles, "10");
        // Unparsable deny on the member's own overwrite: deny everything.
        let own = channel(
            "200",
            channel_type::GUILD_TEXT,
            None,
            json!([{"id": "10", "type": 1, "allow": "0", "deny": {"new": "format"}}]),
        );
        assert!(!c.can_view(&own, None));
        // Unparsable allow on a role overwrite: that role denies everything,
        // and its allow cannot open the channel.
        let role_ow = channel(
            "201",
            channel_type::GUILD_TEXT,
            None,
            json!([{"id": "7", "type": 0, "allow": "abc", "deny": "0"}]),
        );
        assert!(!c.can_view(&role_ow, None));
        // An entry without a target, or a list that is not a list: hidden.
        for bad in [
            json!([{"type": 0, "allow": "0", "deny": "1024"}]),
            json!([{"id": "100", "type": 7, "allow": "0", "deny": "1024"}]),
            json!({"not": "a list"}),
        ] {
            let ch = channel("202", channel_type::GUILD_TEXT, None, bad);
            assert!(!c.can_view(&ch, None));
        }
        // An unrelated role's broken overwrite does not matter.
        let other = channel(
            "203",
            channel_type::GUILD_TEXT,
            None,
            json!([{"id": "8", "type": 0, "allow": null, "deny": null}]),
        );
        assert!(c.can_send(&other, None));
    }

    #[test]
    fn visitors_get_view_and_history_only() {
        let roles = vec![
            role(
                "100",
                ADMINISTRATOR | VIEW_CHANNEL | SEND_MESSAGES | READ_MESSAGE_HISTORY | ADD_REACTIONS,
            ),
            role("7", ALL),
        ];
        // Member roles and the owner ID do not count for a visitor.
        let member_roles = vec!["7".to_string()];
        let v = PermissionContext {
            visiting: true,
            ..ctx(&roles, &member_roles, "1")
        };
        assert_eq!(v.guild(), VIEW_CHANNEL | READ_MESSAGE_HISTORY);
        let ch = channel(
            "200",
            channel_type::GUILD_TEXT,
            None,
            json!([
                {"id": "100", "type": 0, "allow": SEND_MESSAGES.to_string(), "deny": READ_MESSAGE_HISTORY.to_string()},
                {"id": "7", "type": 0, "allow": READ_MESSAGE_HISTORY.to_string(), "deny": "0"},
                {"id": "1", "type": 1, "allow": READ_MESSAGE_HISTORY.to_string(), "deny": "0"}
            ]),
        );
        assert_eq!(v.channel(&ch, None), VIEW_CHANNEL);
        assert!(v.can_view(&ch, None));
        assert!(!v.can_send(&ch, None));
        assert!(!v.can_react(&ch, None));
        let hidden = channel(
            "201",
            channel_type::GUILD_TEXT,
            None,
            json!([{"id": "100", "type": 0, "allow": "0", "deny": VIEW_CHANNEL.to_string()}]),
        );
        assert!(!v.can_view(&hidden, None));
    }

    #[test]
    fn mfa_removes_elevated_bits_except_for_the_owner() {
        let roles = vec![
            role("100", VIEW_CHANNEL | SEND_MESSAGES),
            role("7", MANAGE_MESSAGES | KICK_MEMBERS | PIN_MESSAGES),
            role("9", ADMINISTRATOR),
        ];
        let mods = vec!["7".to_string()];
        let m = PermissionContext {
            mfa_missing: true,
            ..ctx(&roles, &mods, "10")
        };
        assert_eq!(m.guild(), VIEW_CHANNEL | SEND_MESSAGES | PIN_MESSAGES);
        let admins = vec!["9".to_string()];
        let a = PermissionContext {
            mfa_missing: true,
            ..ctx(&roles, &admins, "11")
        };
        assert_eq!(a.guild(), ALL & !ELEVATED);
        // A timed-out administrator without MFA is still exempt from the
        // timeout (Fluxer checks the computed bits, which keep ADMINISTRATOR).
        let text = channel("302", channel_type::GUILD_TEXT, None, json!([]));
        let a_timed_out = PermissionContext {
            timed_out: true,
            ..a
        };
        assert!(a_timed_out.can_send(&text, None));
        assert!(a_timed_out.can_react(&text, None));
        let edit: Message = serde_json::from_value(json!({
            "id": "1", "channel_id": "302", "author": {"id": "11"}, "type": 0, "content": "x",
            "timestamp": "2026-01-01T00:00:00Z"
        }))
        .unwrap();
        assert!(a_timed_out.can_edit_message(&edit, &text, None));
        // A timed-out moderator is still blocked.
        let m_timed_out = PermissionContext {
            timed_out: true,
            ..m
        };
        assert!(!m_timed_out.can_send(&text, None));
        let owner = PermissionContext {
            mfa_missing: true,
            ..ctx(&roles, &[], "1")
        };
        assert_eq!(owner.guild(), ALL);
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
        let th = channel("304", channel_type::PUBLIC_THREAD, Some("302"), json!([]));
        let all = vec![cat_open, cat_hidden, text, secret, th];
        let ids: Vec<&str> = c
            .visible_channels(&all)
            .iter()
            .map(|c| c.id.as_str())
            .collect();
        assert_eq!(ids, vec!["300", "302", "304"]);
        assert!(!c.can_send(&all[4], Some(&all[2])));
        let roles2 = vec![role("100", VIEW_CHANNEL | SEND_MESSAGES_IN_THREADS)];
        let c2 = ctx(&roles2, &[], "10");
        assert!(c2.can_send(&all[4], Some(&all[2])));
        assert!(!c2.can_send(&all[2], None));
        assert!(!c2.can_view(&all[4], None));

        let dm: Channel = serde_json::from_value(json!({"id": "9", "type": 1})).unwrap();
        assert!(c.can_send(&dm, None));
        assert!(c.can_pin(&dm, None));

        let timed_out = PermissionContext {
            timed_out: true,
            ..c
        };
        assert!(timed_out.can_view(&all[2], None));
        assert!(!timed_out.can_send(&all[2], None));
    }

    #[test]
    fn locked_archived_and_private_threads() {
        let roles = vec![
            role(
                "100",
                VIEW_CHANNEL | SEND_MESSAGES_IN_THREADS | ADD_REACTIONS | PIN_MESSAGES,
            ),
            role("5", MANAGE_THREADS),
        ];
        let member = ctx(&roles, &[], "10");
        let mods = vec!["5".to_string()];
        let moderator = ctx(&roles, &mods, "11");
        let parent = channel("302", channel_type::GUILD_TEXT, None, json!([]));

        let locked = thread(
            "400",
            channel_type::PUBLIC_THREAD,
            "302",
            json!({"locked": true, "archived": false}),
        );
        assert!(member.can_view(&locked, Some(&parent)));
        assert!(!member.can_send(&locked, Some(&parent)));
        assert!(!member.can_react(&locked, Some(&parent)));
        assert!(!member.can_pin(&locked, Some(&parent)));
        assert!(moderator.can_send(&locked, Some(&parent)));
        // A timed-out moderator is not a moderator.
        let tm = PermissionContext {
            timed_out: true,
            ..moderator
        };
        assert!(!tm.can_send(&locked, Some(&parent)));

        let archived = thread(
            "401",
            channel_type::PUBLIC_THREAD,
            "302",
            json!({"locked": false, "archived": true}),
        );
        // Sending unarchives; reacting and pinning do not.
        assert!(member.can_send(&archived, Some(&parent)));
        assert!(!member.can_react(&archived, Some(&parent)));
        assert!(!member.can_pin(&archived, Some(&parent)));

        let private = thread("402", channel_type::PRIVATE_THREAD, "302", json!({}));
        assert!(!member.can_view(&private, Some(&parent)));
        assert!(moderator.can_view(&private, Some(&parent)));
        let mut joined = private.clone();
        joined
            .extra
            .insert("member".into(), json!({"id": "402", "user_id": "10"}));
        assert!(member.can_view(&joined, Some(&parent)));
        // THREAD_UPDATE carries no `member`; merging keeps the membership.
        let mut stored = joined.clone();
        let mut update = private.clone();
        update.name = Some("renamed".into());
        stored.merge_thread_update(update.clone());
        assert!(member.can_view(&stored, Some(&parent)));
        // Replacing it outright would hide the thread (fails closed).
        assert!(!member.can_view(&update, Some(&parent)));
        // Leaving (THREAD_MEMBERS_UPDATE removes the account) hides it again.
        stored.set_thread_member(None);
        assert!(!member.can_view(&stored, Some(&parent)));

        // Pinning in a thread is blocked during a timeout; outside threads it is not.
        let open = thread("403", channel_type::PUBLIC_THREAD, "302", json!({}));
        let t = PermissionContext {
            timed_out: true,
            ..member
        };
        assert!(member.can_pin(&open, Some(&parent)));
        assert!(!t.can_pin(&open, Some(&parent)));
        assert!(t.can_pin(&parent, None));
    }

    #[test]
    fn editing_follows_timeouts_and_thread_state() {
        let roles = vec![role("100", VIEW_CHANNEL | SEND_MESSAGES_IN_THREADS)];
        let c = ctx(&roles, &[], "10");
        let parent = channel("302", channel_type::GUILD_TEXT, None, json!([]));
        let msg: Message = serde_json::from_value(json!({
            "id": "1", "channel_id": "302", "type": 0, "author": {"id": "10"}
        }))
        .unwrap();
        assert!(c.can_edit_message(&msg, &parent, None));
        let other: Message = serde_json::from_value(json!({
            "id": "2", "channel_id": "302", "type": 0, "author": {"id": "11"}
        }))
        .unwrap();
        assert!(!c.can_edit_message(&other, &parent, None));
        let t = PermissionContext {
            timed_out: true,
            ..c
        };
        assert!(!t.can_edit_message(&msg, &parent, None));
        let archived = thread(
            "401",
            channel_type::PUBLIC_THREAD,
            "302",
            json!({"archived": true}),
        );
        assert!(!c.can_edit_message(&msg, &archived, Some(&parent)));
        let locked = thread(
            "402",
            channel_type::PUBLIC_THREAD,
            "302",
            json!({"locked": true}),
        );
        assert!(!c.can_edit_message(&msg, &locked, Some(&parent)));
    }
}
