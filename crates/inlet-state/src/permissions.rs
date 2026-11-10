//! The account's permissions, cached per guild and channel.
//!
//! The bits come from the protocol crate's calculator
//! ([`inlet_protocol::permissions::PermissionContext`]), which follows
//! Fluxer's `permissions.mdx`: owner, everyone role plus the member's roles,
//! `ADMINISTRATOR` short-circuit, then the everyone overwrite, the member's
//! role overwrites combined and the member's own overwrite; visitors, the
//! MFA rule, timeouts and thread rules on top. This module keeps the result
//! per channel and recomputes a guild whenever its roles, channels (and so
//! overwrites), the account's member, the guild's owner or MFA level, or the
//! account's MFA change, and when a timeout ends.
//!
//! It fails closed: until the everyone role and the account's own member are
//! loaded, and after the account left the guild, every bit is 0.

use std::collections::BTreeMap;

use inlet_protocol::permissions::{self as bits, PermissionContext};
use inlet_protocol::CurrentUser;

use crate::channels::Channels;
use crate::guilds::GuildState;
use crate::ids::ChannelId;

/// Permission bits in one channel (see [`inlet_protocol::permissions`] for the bits).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Permissions(pub u64);

impl Permissions {
    /// Nothing.
    pub const NONE: Permissions = Permissions(0);
    /// Everything (DMs, the owner).
    pub const ALL: Permissions = Permissions(bits::ALL);

    /// The raw bits.
    pub fn bits(self) -> u64 {
        self.0
    }

    /// Whether every bit of `perm` is set.
    pub fn has(self, perm: u64) -> bool {
        self.0 & perm == perm
    }

    /// The channel is visible.
    pub fn can_view(self) -> bool {
        self.has(bits::VIEW_CHANNEL)
    }

    /// The composer can send (thread rules are already folded into
    /// `SEND_MESSAGES`). The channel must also take text messages.
    pub fn can_send(self) -> bool {
        self.has(bits::VIEW_CHANNEL | bits::SEND_MESSAGES)
    }

    /// Files can be attached.
    pub fn can_attach(self) -> bool {
        self.can_send() && self.has(bits::ATTACH_FILES)
    }

    /// New reactions can be added.
    pub fn can_react(self) -> bool {
        self.has(bits::VIEW_CHANNEL | bits::ADD_REACTIONS)
    }

    /// Messages can be pinned.
    pub fn can_pin(self) -> bool {
        self.has(bits::VIEW_CHANNEL | bits::PIN_MESSAGES)
    }

    /// Others' messages can be deleted.
    pub fn can_manage_messages(self) -> bool {
        self.has(bits::VIEW_CHANNEL | bits::MANAGE_MESSAGES)
    }
}

/// Runs `f` with the guild's permission context, or returns `None` when the
/// guild fails closed (no everyone role, no own member unless visiting, or
/// the account left).
fn with_context<T>(
    guild: &GuildState,
    me: &CurrentUser,
    now_ms: i64,
    f: impl FnOnce(&PermissionContext<'_>) -> T,
) -> Option<T> {
    let own = guild.members.me();
    if !guild.roles_loaded() || guild.left || (own.is_none() && !guild.visiting) {
        return None;
    }
    let guild_id = guild.id.to_string();
    let mfa_missing = guild
        .properties
        .as_ref()
        .is_some_and(|p| p.mfa_level() == 1)
        && !me.mfa_enabled();
    let ctx = PermissionContext {
        guild_id: &guild_id,
        owner_id: guild.owner_id(),
        user_id: &me.id,
        roles: guild.roles.as_slice(),
        member_roles: own.map_or(&[][..], |m| m.roles.as_slice()),
        timed_out: own.is_some_and(|m| m.is_timed_out_at(now_ms)),
        visiting: guild.visiting,
        mfa_missing,
    };
    Some(f(&ctx))
}

fn channel_bits(ctx: &PermissionContext<'_>, channels: &Channels, id: ChannelId) -> Option<u64> {
    let ch = channels.get(id)?;
    let parent = if ch.is_thread() {
        channels.parent_of(id).and_then(|p| channels.get(p))
    } else {
        None
    };
    Some(ctx.channel(ch, parent))
}

/// Recomputes the account's permissions in `guild`. Returns whether any bit changed.
pub(crate) fn recompute(
    guild: &mut GuildState,
    channels: &Channels,
    me: &CurrentUser,
    now_ms: i64,
) -> bool {
    let (guild_bits, perms) = with_context(guild, me, now_ms, |ctx| {
        let perms: BTreeMap<ChannelId, u64> = guild
            .channels
            .iter()
            .filter_map(|id| Some((*id, channel_bits(ctx, channels, *id)?)))
            .collect();
        (ctx.guild(), perms)
    })
    .unwrap_or_default();
    guild.timed_out = guild
        .members
        .me()
        .is_some_and(|m| m.is_timed_out_at(now_ms));
    let changed = perms != guild.perms || guild_bits != guild.guild_perms;
    guild.perms = perms;
    guild.guild_perms = guild_bits;
    changed
}

/// Recomputes only `ids` (a changed or deleted channel and its threads):
/// channel events leave roles, member and guild alone. Returns whether any
/// bit changed.
pub(crate) fn recompute_channels(
    guild: &mut GuildState,
    channels: &Channels,
    me: &CurrentUser,
    now_ms: i64,
    ids: &[ChannelId],
) -> bool {
    let fresh: Vec<(ChannelId, Option<u64>)> = with_context(guild, me, now_ms, |ctx| {
        ids.iter()
            .map(|id| {
                let bits = if guild.channels.contains(id) {
                    channel_bits(ctx, channels, *id)
                } else {
                    None
                };
                (*id, bits)
            })
            .collect()
    })
    .unwrap_or_else(|| ids.iter().map(|id| (*id, None)).collect());
    let mut changed = false;
    for (id, bits) in fresh {
        let old = match bits {
            Some(b) => guild.perms.insert(id, b),
            None => guild.perms.remove(&id),
        };
        changed |= old != bits;
    }
    changed
}
