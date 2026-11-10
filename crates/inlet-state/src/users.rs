//! Users seen in READY, messages, members and DMs.

use inlet_protocol::User;

use crate::bounded::LruMap;
use crate::ids::UserId;
use crate::limits::MAX_USERS;

/// A bounded cache of users by ID.
#[derive(Debug, Clone, PartialEq)]
pub struct Users {
    cache: LruMap<UserId, User>,
}

impl Default for Users {
    fn default() -> Self {
        Users {
            cache: LruMap::new(MAX_USERS),
        }
    }
}

impl Users {
    /// Stores a user. A partial user (no username, as in presences and
    /// member removals) does not replace a full one. Returns whether
    /// anything changed.
    pub(crate) fn upsert(&mut self, user: &User) -> bool {
        let Some(id) = UserId::parse(&user.id) else {
            return false;
        };
        let mut user = user.clone();
        crate::size::cap_user(&mut user);
        let user = &user;
        match self.cache.get(&id) {
            Some(old) if old == user => {
                self.cache.touch(&id);
                false
            }
            Some(_) if user.username.is_empty() => false,
            _ => {
                self.cache.insert(id, user.clone());
                true
            }
        }
    }

    /// A user, if cached.
    pub fn get(&self, id: UserId) -> Option<&User> {
        self.cache.get(&id)
    }

    /// Number of cached users.
    pub fn len(&self) -> usize {
        self.cache.len()
    }

    /// Whether no user is cached.
    pub fn is_empty(&self) -> bool {
        self.cache.len() == 0
    }
}
