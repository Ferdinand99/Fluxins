//! Friends, friend requests, blocks and private notes.
//!
//! A relationship is one account's record about another: a friendship
//! ([`relationship_type::FRIEND`] on both sides), a pending request
//! (`OUTGOING_REQUEST` on the sender, `INCOMING_REQUEST` on the recipient) or
//! a block (`BLOCKED`, on the blocker only). READY has them in
//! [`crate::Ready::relationships`]; RELATIONSHIP_ADD, RELATIONSHIP_UPDATE and
//! RELATIONSHIP_REMOVE keep them current ([`crate::Dispatch::RelationshipAdd`]).
//!
//! Notes are private text about another account (1-256 characters), in READY
//! `notes` ([`crate::Ready::notes`]) and USER_NOTE_UPDATE.
//!
//! Only user tokens can call these routes.

use std::collections::BTreeMap;

use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::error::{Error, Result};
use crate::models::{lenient_bool, null_as_default, sf, u64_or_zero, Ready, Snowflake, User};
use crate::rest::snowflake;
use crate::Client;

/// Values of [`Relationship::kind`].
pub mod relationship_type {
    /// The two accounts are friends.
    pub const FRIEND: u64 = 1;
    /// This account blocked the other.
    pub const BLOCKED: u64 = 2;
    /// The other account sent a friend request.
    pub const INCOMING_REQUEST: u64 = 3;
    /// This account sent a friend request.
    pub const OUTGOING_REQUEST: u64 = 4;
}

/// Longest private note or friend nickname, in characters.
pub const MAX_NOTE_CHARS: usize = 256;

/// One relationship (also the RELATIONSHIP_ADD and RELATIONSHIP_UPDATE payload).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Relationship {
    /// The other account's ID.
    #[serde(deserialize_with = "sf")]
    pub id: Snowflake,
    /// A [`relationship_type`] value.
    #[serde(rename = "type", default, deserialize_with = "u64_or_zero")]
    pub kind: u64,
    /// The other account. Missing in READY (look it up in `users`;
    /// [`Ready::relationships`] does that).
    #[serde(default, deserialize_with = "crate::models::lenient_opt")]
    pub user: Option<User>,
    /// When the record got its current type (ISO 8601).
    #[serde(default)]
    pub since: Option<String>,
    /// This account's private nickname for the other.
    #[serde(default)]
    pub nickname: Option<String>,
    /// Whether this account shares voice activity with the other.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub share_voice_activity: bool,
    /// Every other field.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Relationship {
    /// Friends.
    pub fn is_friend(&self) -> bool {
        self.kind == relationship_type::FRIEND
    }

    /// Blocked by this account.
    pub fn is_blocked(&self) -> bool {
        self.kind == relationship_type::BLOCKED
    }

    /// A request waiting for this account to accept or ignore.
    pub fn is_incoming_request(&self) -> bool {
        self.kind == relationship_type::INCOMING_REQUEST
    }

    /// A request this account sent.
    pub fn is_outgoing_request(&self) -> bool {
        self.kind == relationship_type::OUTGOING_REQUEST
    }

    /// Applies a RELATIONSHIP_UPDATE (or a repeated RELATIONSHIP_ADD) to the
    /// stored record: every field comes from `update`, except a missing
    /// `user`, which keeps the stored one.
    pub fn apply_update(&mut self, update: Relationship) {
        let user = update.user.or_else(|| self.user.take());
        *self = Relationship { user, ..update };
    }
}

fn check_text(value: &str, max: usize, what: &str) -> Result<()> {
    crate::api::text(value, max, what)
}

impl Ready {
    /// The account's relationships, with `user` filled in from `users`.
    pub fn relationships(&self) -> Vec<Relationship> {
        let mut list: Vec<Relationship> = crate::models::extra_list(&self.extra, "relationships");
        for r in &mut list {
            if r.user.is_none() {
                r.user = self.users.iter().find(|u| u.id == r.id).cloned();
            }
        }
        list
    }

    /// Private notes by user ID.
    pub fn notes(&self) -> BTreeMap<Snowflake, String> {
        notes_from(self.extra.get("notes"))
    }
}

fn notes_from(v: Option<&Value>) -> BTreeMap<Snowflake, String> {
    let mut out = BTreeMap::new();
    if let Some(Value::Object(map)) = v {
        for (k, v) in map {
            if let (true, Some(note)) = (crate::media::is_snowflake(k), v.as_str()) {
                if !note.is_empty() {
                    out.insert(k.clone(), note.to_owned());
                }
            }
        }
    }
    out
}

#[derive(Deserialize)]
struct NoteBody {
    #[serde(default, deserialize_with = "null_as_default")]
    note: String,
}

impl Client {
    /// `GET /users/@me/relationships`: every relationship, in no order.
    pub async fn relationships(&self) -> Result<Vec<Relationship>> {
        self.get_list("/users/@me/relationships", "relationship")
            .await
    }

    /// `POST /users/@me/relationships/{user}`: sends a friend request, or
    /// accepts the user's pending request.
    pub async fn send_friend_request(&self, user_id: &str) -> Result<Relationship> {
        self.raw(
            Method::POST,
            &format!("/users/@me/relationships/{}", snowflake(user_id)?),
            Some(&json!({})),
        )
        .await?
        .json()
    }

    /// `POST /users/@me/relationships`: sends a friend request by username
    /// and discriminator (the discriminator may be left out only on an
    /// instance without tags).
    pub async fn send_friend_request_by_tag(
        &self,
        username: &str,
        discriminator: Option<&str>,
    ) -> Result<Relationship> {
        check_text(username, 32, "username")?;
        let mut body = json!({ "username": username });
        if let Some(d) = discriminator {
            if d.is_empty() || d.len() > 4 || !d.bytes().all(|b| b.is_ascii_digit()) {
                return Err(Error::InvalidInput(
                    "discriminator: 1-4 decimal digits".into(),
                ));
            }
            body["discriminator"] = json!(d);
        }
        self.raw(Method::POST, "/users/@me/relationships", Some(&body))
            .await?
            .json()
    }

    /// `PUT /users/@me/relationships/{user}`: accepts a pending incoming request.
    pub async fn accept_friend_request(&self, user_id: &str) -> Result<Relationship> {
        self.raw(
            Method::PUT,
            &format!("/users/@me/relationships/{}", snowflake(user_id)?),
            Some(&json!({})),
        )
        .await?
        .json()
    }

    /// `PUT /users/@me/relationships/{user}` with `type: BLOCKED`: blocks the
    /// account (ends a friendship or pending request).
    pub async fn block_user(&self, user_id: &str) -> Result<Relationship> {
        self.raw(
            Method::PUT,
            &format!("/users/@me/relationships/{}", snowflake(user_id)?),
            Some(&json!({ "type": relationship_type::BLOCKED })),
        )
        .await?
        .json()
    }

    /// `DELETE /users/@me/relationships/{user}`: removes a friend, cancels or
    /// ignores a request, or unblocks.
    pub async fn remove_relationship(&self, user_id: &str) -> Result<()> {
        self.raw(
            Method::DELETE,
            &format!("/users/@me/relationships/{}", snowflake(user_id)?),
            None,
        )
        .await?;
        Ok(())
    }

    /// `PATCH /users/@me/relationships/{user}`: sets (or with `None` clears)
    /// the private nickname for a friend.
    pub async fn set_friend_nickname(
        &self,
        user_id: &str,
        nickname: Option<&str>,
    ) -> Result<Relationship> {
        let nickname = nickname.filter(|n| !n.is_empty());
        if let Some(n) = nickname {
            check_text(n, MAX_NOTE_CHARS, "nickname")?;
        }
        self.raw(
            Method::PATCH,
            &format!("/users/@me/relationships/{}", snowflake(user_id)?),
            Some(&json!({ "nickname": nickname })),
        )
        .await?
        .json()
    }

    /// `GET /users/@me/notes`: every private note by user ID.
    pub async fn notes(&self) -> Result<BTreeMap<Snowflake, String>> {
        let v: Value = self
            .raw(Method::GET, "/users/@me/notes", None)
            .await?
            .json()?;
        Ok(notes_from(Some(&v)))
    }

    /// `GET /users/@me/notes/{user}`: the note about one account, or `None`.
    pub async fn note(&self, user_id: &str) -> Result<Option<String>> {
        let path = format!("/users/@me/notes/{}", snowflake(user_id)?);
        match self.raw(Method::GET, &path, None).await {
            Ok(raw) => {
                let body: NoteBody = raw.json()?;
                Ok(Some(body.note).filter(|n| !n.is_empty()))
            }
            Err(Error::Api(e)) if e.status == 404 => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// `PUT /users/@me/notes/{user}`: sets the note (1-256 characters), or
    /// deletes it with `None` or an empty string.
    pub async fn set_note(&self, user_id: &str, note: Option<&str>) -> Result<()> {
        let note = note.filter(|n| !n.is_empty());
        if let Some(n) = note {
            if n.chars().count() > MAX_NOTE_CHARS {
                return Err(Error::InvalidInput(format!(
                    "note: at most {MAX_NOTE_CHARS} characters"
                )));
            }
        }
        self.raw(
            Method::PUT,
            &format!("/users/@me/notes/{}", snowflake(user_id)?),
            Some(&json!({ "note": note })),
        )
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_skip_bad_keys_and_empty_text() {
        let v = json!({"5": "met at the meetup", "../x": "no", "6": "", "7": 3});
        let notes = notes_from(Some(&v));
        assert_eq!(notes.len(), 1);
        assert_eq!(notes["5"], "met at the meetup");
    }
}
