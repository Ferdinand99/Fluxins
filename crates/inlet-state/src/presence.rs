//! Presences: one per user, across guilds.
//!
//! Fluxer keeps a user's presence per user, not per guild, so the cache is
//! account-wide with one cap ([`crate::limits::MAX_PRESENCES`]). `offline`
//! is stored as "no entry", which keeps the cache to the people who are on.

use inlet_protocol::Presence;

use crate::bounded::LruMap;
use crate::ids::UserId;
use crate::limits::MAX_PRESENCES;

/// A bounded presence cache.
#[derive(Debug, Clone, PartialEq)]
pub struct Presences {
    by_user: LruMap<UserId, Presence>,
}

impl Default for Presences {
    fn default() -> Self {
        Presences {
            by_user: LruMap::new(MAX_PRESENCES),
        }
    }
}

impl Presences {
    /// Applies one presence. Returns whether anything changed.
    pub(crate) fn upsert(&mut self, p: &Presence) -> bool {
        let Some(id) = p.user_id().and_then(UserId::parse) else {
            return false;
        };
        self.upsert_for(id, p)
    }

    /// Applies a presence that carries no user (member list items).
    pub(crate) fn upsert_for(&mut self, id: UserId, p: &Presence) -> bool {
        if p.status.is_empty() || p.status == "offline" || p.status == "invisible" {
            return self.by_user.remove(&id).is_some();
        }
        // Keep only what the UI shows; the user object lives in the user cache.
        let mut slim = Presence {
            user: None,
            guild_id: None,
            ..p.clone()
        };
        crate::size::cap_presence(&mut slim);
        if self.by_user.get(&id) == Some(&slim) {
            return false;
        }
        self.by_user.insert(id, slim);
        true
    }

    /// A user's presence, or `None` when offline or unknown.
    pub fn get(&self, id: UserId) -> Option<&Presence> {
        self.by_user.get(&id)
    }

    /// A user's status: `online`, `idle`, `dnd`, or `offline` when unknown.
    pub fn status(&self, id: UserId) -> &str {
        self.get(id).map_or("offline", |p| p.status.as_str())
    }

    /// Number of stored presences.
    pub fn len(&self) -> usize {
        self.by_user.len()
    }

    /// Whether nobody is online.
    pub fn is_empty(&self) -> bool {
        self.by_user.len() == 0
    }
}
