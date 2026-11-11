//! Relationships: for now only who the account blocked, from READY `relationships` and
//! RELATIONSHIP_ADD, RELATIONSHIP_UPDATE and RELATIONSHIP_REMOVE. Friends, requests and
//! notes are filled in by a later task.

use std::collections::BTreeSet;

use inlet_protocol::{Dispatch, Relationship};

use crate::{Changes, UserId};

/// Most blocked accounts kept (the protocol keeps at most 5,000 relationships).
const MAX_BLOCKED: usize = 5_000;

/// Who the account blocked.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Relationships {
    blocked: BTreeSet<UserId>,
}

impl Relationships {
    /// From READY's relationships.
    pub fn from_ready(list: &[Relationship]) -> Self {
        let mut r = Relationships::default();
        for item in list {
            r.set(item);
        }
        r
    }

    /// Applies an event. Blocking changes nothing that is drawn yet, so the changes are
    /// empty.
    pub fn apply(&mut self, event: &Dispatch) -> Changes {
        match event {
            Dispatch::RelationshipAdd(r) | Dispatch::RelationshipUpdate(r) => self.set(r),
            Dispatch::RelationshipRemove(r) => {
                if let Some(id) = UserId::parse(&r.id) {
                    self.blocked.remove(&id);
                }
            }
            _ => {}
        }
        Changes::default()
    }

    fn set(&mut self, r: &Relationship) {
        let Some(id) = UserId::parse(&r.id) else {
            return;
        };
        if !r.is_blocked() {
            self.blocked.remove(&id);
        } else if self.blocked.len() < MAX_BLOCKED {
            self.blocked.insert(id);
        }
    }

    /// Whether the account blocked `user`.
    pub fn is_blocked(&self, user: UserId) -> bool {
        self.blocked.contains(&user)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rel(id: &str, kind: u64) -> Relationship {
        serde_json::from_value(json!({"id": id, "type": kind})).unwrap()
    }

    #[test]
    fn blocks_follow_ready_and_events() {
        let mut r = Relationships::from_ready(&[rel("2", 2), rel("3", 1)]);
        assert!(r.is_blocked(UserId(2)));
        assert!(!r.is_blocked(UserId(3)));
        r.apply(&Dispatch::RelationshipAdd(Box::new(rel("3", 2))));
        assert!(r.is_blocked(UserId(3)));
        // Unblocked and made a friend again.
        r.apply(&Dispatch::RelationshipUpdate(Box::new(rel("3", 1))));
        assert!(!r.is_blocked(UserId(3)));
        let remove = serde_json::from_value(json!({"id": "2"})).unwrap();
        r.apply(&Dispatch::RelationshipRemove(remove));
        assert!(!r.is_blocked(UserId(2)));
    }
}
