//! A guild's roles.

use inlet_protocol::Role;

use crate::ids::{GuildId, RoleId};
use crate::limits::MAX_ROLES_PER_GUILD;

/// A guild's roles, kept in ID order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Roles {
    list: Vec<Role>,
}

impl Roles {
    /// Replaces every role (READY, GUILD_CREATE, GUILD_SYNC). Roles without a
    /// snowflake ID are dropped.
    pub(crate) fn replace(&mut self, roles: Vec<Role>) {
        self.list.clear();
        for r in roles {
            self.upsert(r);
        }
    }

    /// Adds or replaces one role. Returns whether anything changed.
    pub(crate) fn upsert(&mut self, mut role: Role) -> bool {
        let Some(id) = RoleId::parse(&role.id) else {
            return false;
        };
        crate::size::cap_role(&mut role);
        match self.position(id) {
            Ok(i) if self.list[i] == role => false,
            Ok(i) => {
                self.list[i] = role;
                true
            }
            Err(i) if self.list.len() < MAX_ROLES_PER_GUILD => {
                self.list.insert(i, role);
                true
            }
            Err(_) => false,
        }
    }

    /// Removes a role. Returns whether it was there.
    pub(crate) fn remove(&mut self, id: RoleId) -> bool {
        match self.position(id) {
            Ok(i) => {
                self.list.remove(i);
                true
            }
            Err(_) => false,
        }
    }

    fn position(&self, id: RoleId) -> Result<usize, usize> {
        self.list
            .binary_search_by_key(&id, |r| RoleId::parse(&r.id).unwrap_or(RoleId(0)))
    }

    /// One role.
    pub fn get(&self, id: RoleId) -> Option<&Role> {
        self.position(id).ok().map(|i| &self.list[i])
    }

    /// Every role, in ID order (the slice the permission calculator takes).
    pub fn as_slice(&self) -> &[Role] {
        &self.list
    }

    /// Roles from highest to lowest rank (`position`, then ID as Fluxer sorts ties).
    pub fn by_rank(&self) -> Vec<&Role> {
        let mut v: Vec<&Role> = self.list.iter().collect();
        v.sort_by(|a, b| {
            b.position.cmp(&a.position).then_with(|| {
                let ia = RoleId::parse(&a.id).unwrap_or(RoleId(0));
                let ib = RoleId::parse(&b.id).unwrap_or(RoleId(0));
                ia.cmp(&ib)
            })
        });
        v
    }

    /// Whether the everyone role (ID = guild ID) is loaded. Until it is,
    /// every permission in the guild is 0.
    pub fn loaded(&self, guild: GuildId) -> bool {
        self.get(RoleId(guild.0)).is_some()
    }

    /// Number of roles.
    pub fn len(&self) -> usize {
        self.list.len()
    }

    /// Whether there are no roles.
    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }
}
