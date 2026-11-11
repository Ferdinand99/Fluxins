//! Relationships and the pinned direct messages.
//!
//! Every relationship of the account (friends, incoming and outgoing friend requests,
//! blocks) from READY `relationships` and RELATIONSHIP_ADD, RELATIONSHIP_UPDATE and
//! RELATIONSHIP_REMOVE, and the pinned DM list from READY `pinned_dms` and
//! USER_PINNED_DMS_UPDATE. Blocks live in the same map as the rest, so there is one
//! answer to "is this person blocked?" ([`Relationships::is_blocked`]).
//!
//! Bounded: at most [`MAX_RELATIONSHIPS`] people (the protocol keeps at most as many
//! from one list) and [`MAX_PINNED_DMS`] pins. A block always finds room: when the map
//! is full, it pushes out a record that is not a block. Each stored user is trimmed like
//! every other cached user. The list is kept for an account in the background too: its
//! incoming requests are a DM-list badge, and it is small.

use std::collections::BTreeMap;

use inlet_protocol::api::relationships::relationship_type;
use inlet_protocol::models::{parse_timestamp_ms, User};
use inlet_protocol::{Dispatch, Relationship};

use crate::{Changes, ChannelId, UserId};

/// Most relationships kept.
pub const MAX_RELATIONSHIPS: usize = 5_000;
/// Most pinned DMs kept.
pub const MAX_PINNED_DMS: usize = 1_000;

/// What the account is to another account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Friend,
    Blocked,
    /// The other account sent a friend request.
    Incoming,
    /// This account sent a friend request.
    Outgoing,
}

impl Kind {
    fn of(kind: u64) -> Option<Kind> {
        Some(match kind {
            relationship_type::FRIEND => Kind::Friend,
            relationship_type::BLOCKED => Kind::Blocked,
            relationship_type::INCOMING_REQUEST => Kind::Incoming,
            relationship_type::OUTGOING_REQUEST => Kind::Outgoing,
            _ => return None,
        })
    }
}

/// One relationship.
#[derive(Debug, Clone, PartialEq)]
pub struct Person {
    pub id: UserId,
    pub kind: Kind,
    /// The account's private nickname for a friend.
    pub nickname: Option<String>,
    /// When the record got its type, in Unix milliseconds.
    pub since_ms: Option<i64>,
    /// The other account, when the server sent it (trimmed).
    pub user: Option<User>,
}

/// The account's relationships and pinned DMs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Relationships {
    people: BTreeMap<UserId, Person>,
    pinned: Vec<ChannelId>,
}

impl Relationships {
    /// From READY's relationships and pinned DMs.
    pub fn from_ready(list: &[Relationship], pinned: &[String]) -> Self {
        let mut r = Relationships::default();
        for item in list {
            r.set(item.clone());
        }
        r.set_pinned(pinned);
        r
    }

    /// Applies an event: RELATIONSHIP_* and USER_PINNED_DMS_UPDATE.
    pub fn apply(&mut self, event: &Dispatch) -> Changes {
        let mut c = Changes::default();
        match event {
            Dispatch::RelationshipAdd(r) | Dispatch::RelationshipUpdate(r) => {
                c.relationships = self.set((**r).clone());
            }
            Dispatch::RelationshipRemove(r) => {
                if let Some(id) = UserId::parse(&r.id) {
                    c.relationships = self.people.remove(&id).is_some();
                }
            }
            Dispatch::UserPinnedDmsUpdate(list) => {
                c.relationships = self.set_pinned(list);
            }
            _ => {}
        }
        c
    }

    /// Stores a record (an event keeps the stored `user` when it has none). Returns
    /// whether anything changed.
    fn set(&mut self, r: Relationship) -> bool {
        let Some(id) = UserId::parse(&r.id) else {
            return false;
        };
        let Some(kind) = Kind::of(r.kind) else {
            // An unknown type: whatever was stored no longer holds.
            return self.people.remove(&id).is_some();
        };
        let mut user = r.user;
        if let Some(u) = user.as_mut() {
            crate::size::cap_user(u);
        }
        let user = user.or_else(|| self.people.get(&id).and_then(|p| p.user.clone()));
        let person = Person {
            id,
            kind,
            nickname: r
                .nickname
                .as_deref()
                .filter(|n| !n.trim().is_empty())
                .map(|n| n.chars().take(256).collect()),
            since_ms: r.since.as_deref().and_then(parse_timestamp_ms),
            user,
        };
        if self.people.get(&id) == Some(&person) {
            return false;
        }
        if !self.people.contains_key(&id) && self.people.len() >= MAX_RELATIONSHIPS {
            if kind != Kind::Blocked {
                return false;
            }
            // A block always finds room.
            let Some(out) = self
                .people
                .values()
                .find(|p| p.kind != Kind::Blocked)
                .map(|p| p.id)
            else {
                return false;
            };
            self.people.remove(&out);
        }
        self.people.insert(id, person);
        true
    }

    fn set_pinned(&mut self, list: &[String]) -> bool {
        let mut pinned: Vec<ChannelId> = Vec::new();
        for id in list.iter().filter_map(|s| ChannelId::parse(s)) {
            if pinned.len() >= MAX_PINNED_DMS {
                break;
            }
            if !pinned.contains(&id) {
                pinned.push(id);
            }
        }
        let changed = pinned != self.pinned;
        self.pinned = pinned;
        changed
    }

    /// Whether the account blocked `user`.
    pub fn is_blocked(&self, user: UserId) -> bool {
        self.kind(user) == Some(Kind::Blocked)
    }

    /// What the account is to `user`, if anything.
    pub fn kind(&self, user: UserId) -> Option<Kind> {
        self.people.get(&user).map(|p| p.kind)
    }

    /// The record about `user`.
    pub fn get(&self, user: UserId) -> Option<&Person> {
        self.people.get(&user)
    }

    /// Every relationship of one kind, by user ID.
    pub fn of_kind(&self, kind: Kind) -> impl Iterator<Item = &Person> {
        self.people.values().filter(move |p| p.kind == kind)
    }

    /// Every relationship, by user ID.
    pub fn iter(&self) -> impl Iterator<Item = &Person> {
        self.people.values()
    }

    /// Friend requests waiting for the account (the Friends row's badge).
    pub fn incoming_count(&self) -> usize {
        self.of_kind(Kind::Incoming).count()
    }

    /// The pinned DMs, in the order the server keeps them.
    pub fn pinned_dms(&self) -> &[ChannelId] {
        &self.pinned
    }

    /// When the account became friends with `user` (Unix milliseconds).
    pub fn friend_since_ms(&self, user: UserId) -> Option<i64> {
        self.people
            .get(&user)
            .filter(|p| p.kind == Kind::Friend)
            .and_then(|p| p.since_ms)
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
        let mut r = Relationships::from_ready(&[rel("2", 2), rel("3", 1)], &[]);
        assert!(r.is_blocked(UserId(2)));
        assert!(!r.is_blocked(UserId(3)));
        r.apply(&Dispatch::RelationshipAdd(Box::new(rel("3", 2))));
        assert!(r.is_blocked(UserId(3)));
        // Unblocked and made a friend again.
        r.apply(&Dispatch::RelationshipUpdate(Box::new(rel("3", 1))));
        assert!(!r.is_blocked(UserId(3)));
        assert_eq!(r.kind(UserId(3)), Some(Kind::Friend));
        let remove = serde_json::from_value(json!({"id": "2"})).unwrap();
        r.apply(&Dispatch::RelationshipRemove(remove));
        assert!(!r.is_blocked(UserId(2)));
        assert_eq!(r.kind(UserId(2)), None);
    }

    #[test]
    fn requests_keep_their_user_and_count() {
        let with_user: Relationship = serde_json::from_value(json!({
            "id": "5", "type": 3, "since": "2026-01-02T03:04:05.000Z",
            "user": {"id": "5", "username": "ada", "discriminator": "0001"}
        }))
        .unwrap();
        let mut r = Relationships::default();
        let c = r.apply(&Dispatch::RelationshipAdd(Box::new(with_user)));
        assert!(c.relationships);
        assert_eq!(r.incoming_count(), 1);
        // Accepted: the update has no user, the stored one stays.
        let c = r.apply(&Dispatch::RelationshipUpdate(Box::new(rel("5", 1))));
        assert!(c.relationships);
        let p = r.get(UserId(5)).unwrap();
        assert_eq!(p.kind, Kind::Friend);
        assert_eq!(p.user.as_ref().map(|u| u.username.as_str()), Some("ada"));
        assert_eq!(r.incoming_count(), 0);
        // The same again changes nothing.
        let c = r.apply(&Dispatch::RelationshipUpdate(Box::new(rel("5", 1))));
        assert!(!c.relationships);
    }

    #[test]
    fn pins_follow_ready_and_events_and_a_block_always_fits() {
        let mut r = Relationships::from_ready(&[], &["9".into(), "8".into(), "9".into()]);
        assert_eq!(r.pinned_dms(), &[ChannelId(9), ChannelId(8)]);
        let c = r.apply(&Dispatch::UserPinnedDmsUpdate(vec!["8".into(), "x".into()]));
        assert!(c.relationships);
        assert_eq!(r.pinned_dms(), &[ChannelId(8)]);

        let mut full = Relationships::default();
        for i in 1..=MAX_RELATIONSHIPS as u64 {
            full.set(rel(&i.to_string(), 1));
        }
        assert!(!full.set(rel("999999", 4)), "no room for a request");
        assert!(full.set(rel("999999", 2)), "room for a block");
        assert!(full.is_blocked(UserId(999_999)));
        assert_eq!(full.iter().count(), MAX_RELATIONSHIPS);
    }
}
