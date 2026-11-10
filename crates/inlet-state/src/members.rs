//! A guild's members: the account's own member, and a bounded cache of others.

use inlet_protocol::GuildMember;

use crate::bounded::LruMap;
use crate::ids::UserId;
use crate::limits::MAX_MEMBERS_PER_GUILD;

/// Members of one guild.
#[derive(Debug, Clone, PartialEq)]
pub struct Members {
    me: Option<GuildMember>,
    others: LruMap<UserId, GuildMember>,
}

impl Default for Members {
    fn default() -> Self {
        Members {
            me: None,
            others: LruMap::new(MAX_MEMBERS_PER_GUILD),
        }
    }
}

impl Members {
    /// Stores a member. `me` is the signed-in account's ID. Returns whether
    /// anything changed; members without a usable user ID are ignored.
    pub(crate) fn upsert(&mut self, member: &GuildMember, me: UserId) -> bool {
        let Some(id) = member.user_id().and_then(UserId::parse) else {
            return false;
        };
        // Member list items carry their presence; it lives in the presence cache.
        let mut member = member.clone();
        member.extra.remove("presence");
        member.extra.remove("guild_id");
        crate::size::cap_member(&mut member, id == me);
        if id == me {
            if self.me.as_ref() == Some(&member) {
                return false;
            }
            self.me = Some(member);
            return true;
        }
        if self.others.get(&id) == Some(&member) {
            self.others.touch(&id);
            return false;
        }
        self.others.insert(id, member);
        true
    }

    /// Stores a member only when none is cached for the user (Fluxer's
    /// `hydrateIfMissing`, for the `member` on messages and typing): such a
    /// copy may be older than the cached one.
    pub(crate) fn insert_if_missing(&mut self, member: &GuildMember, me: UserId) -> bool {
        let Some(id) = member.user_id().and_then(UserId::parse) else {
            return false;
        };
        if self.get(id, me).is_some() {
            return false;
        }
        self.upsert(member, me)
    }

    /// Evicts the least recently used cached member (never the account's own).
    pub(crate) fn evict_oldest(&mut self) -> bool {
        self.others.pop_oldest().is_some()
    }

    /// Number of cached members other than the account.
    pub fn others_len(&self) -> usize {
        self.others.len()
    }

    /// Removes a member. Returns whether it was there.
    pub(crate) fn remove(&mut self, id: UserId, me: UserId) -> bool {
        if id == me {
            return self.me.take().is_some();
        }
        self.others.remove(&id).is_some()
    }

    /// The account's own member.
    pub fn me(&self) -> Option<&GuildMember> {
        self.me.as_ref()
    }

    /// A member, if cached.
    pub fn get(&self, id: UserId, me: UserId) -> Option<&GuildMember> {
        if id == me {
            self.me.as_ref()
        } else {
            self.others.get(&id)
        }
    }

    /// Cached members other than the account, in user ID order.
    pub fn others(&self) -> impl Iterator<Item = (UserId, &GuildMember)> {
        self.others.iter().map(|(k, v)| (*k, v))
    }

    /// Number of cached members, the account's own included.
    pub fn len(&self) -> usize {
        self.others.len() + usize::from(self.me.is_some())
    }

    /// Whether no member is cached.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
