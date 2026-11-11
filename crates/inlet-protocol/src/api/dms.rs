//! Direct messages and group DMs: open, create, add and remove
//! participants, rename, leave, and pin in the DM list.
//!
//! Creating a group DM and adding someone to one are CAPTCHA-gated on
//! instances that turn the check on. The crate solves the ALTCHA
//! proof-of-work itself ([`crate::altcha`]): on a blocking thread, one solve
//! at a time per process, at most [`GROUP_DM_CAPTCHA_TIMEOUT`] each and at
//! most two solves per call (so at most three requests). When it cannot pass
//! the check, the call returns [`GroupDmOutcome::Captcha`] and the UI tells
//! the person to do it in the browser. Fluxer allows 10 group DM creations per
//! hour, and every attempt counts.
//!
//! The pinned DM list is in READY ([`crate::Ready::pinned_dms`]) and
//! USER_PINNED_DMS_UPDATE ([`crate::Dispatch::UserPinnedDmsUpdate`]).

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tokio::sync::Semaphore;

use crate::api::profiles::ImageData;
use crate::error::{Error, Result};
use crate::models::{null_as_default, sf, Channel, Snowflake};
use crate::rest::{send, snowflake, RawResponse, Request};
use crate::Client;

/// Most other participants in a new group DM (Fluxer's default limit is 50
/// including the creator).
pub const MAX_GROUP_DM_RECIPIENTS: usize = 49;
/// Time one CAPTCHA solve may take, waiting for the solver included.
pub const GROUP_DM_CAPTCHA_TIMEOUT: Duration = Duration::from_secs(30);
/// Solves per call: the first challenge, and one more after `INVALID_CAPTCHA`.
const MAX_SOLVES: usize = 2;

/// One solve at a time in the whole process, so many clicks or accounts
/// cannot take every core.
fn solver_slot() -> Arc<Semaphore> {
    static SLOT: OnceLock<Arc<Semaphore>> = OnceLock::new();
    SLOT.get_or_init(|| Arc::new(Semaphore::new(1))).clone()
}

/// A recipient that could not be added to a new group DM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnaddableRecipient {
    /// The user.
    #[serde(deserialize_with = "sf")]
    pub user_id: Snowflake,
    /// `unknown_user`, `not_friends`, `blocked` or `group_dm_add_disabled`.
    #[serde(default, deserialize_with = "null_as_default")]
    pub reason: String,
}

/// The result of a CAPTCHA-gated group DM call.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum GroupDmOutcome<T> {
    /// Done.
    Done(T),
    /// The instance wants a CAPTCHA this crate could not pass (too hard, too
    /// slow, unknown algorithm or refused). Nothing was created; ask the
    /// person to do it in the browser.
    Captcha,
    /// Some recipients cannot be added (`GROUP_DM_RECIPIENTS_NOT_ADDABLE`);
    /// nothing was created.
    NotAddable {
        /// Who, and why.
        unaddable: Vec<UnaddableRecipient>,
        /// The recipients that could be added.
        addable: Vec<Snowflake>,
    },
}

/// Changes to a group DM ([`Client::edit_group_dm`]). `None` keeps a field.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GroupDmEdit {
    /// Name (1-100 characters); `Some(None)` or an empty string clears it,
    /// and a name of only spaces is refused.
    pub name: Option<Option<String>>,
    /// Icon; `Some(None)` clears it.
    pub icon: Option<Option<ImageData>>,
    /// New owner (owner only).
    pub owner_id: Option<Snowflake>,
}

impl GroupDmEdit {
    fn to_json(&self) -> Result<Value> {
        let mut body = Map::new();
        if let Some(name) = &self.name {
            // "" clears like `None`; a name of only spaces is refused, not
            // taken as a clear.
            let name = name.as_deref().filter(|n| !n.is_empty());
            if let Some(n) = name {
                if n.trim().is_empty() {
                    return Err(Error::InvalidInput("group DM name is blank".into()));
                }
                crate::api::text(n, 100, "group DM name")?;
            }
            body.insert("name".into(), json!(name));
        }
        if let Some(icon) = &self.icon {
            body.insert(
                "icon".into(),
                icon.as_ref().map_or(Value::Null, |i| json!(i.data_url())),
            );
        }
        if let Some(owner) = &self.owner_id {
            body.insert("owner_id".into(), json!(snowflake(owner)?));
        }
        if body.is_empty() {
            return Err(Error::InvalidInput(
                "group DM edit: nothing to change".into(),
            ));
        }
        Ok(Value::Object(body))
    }
}

/// Solves a challenge on a blocking thread, one solve at a time, within the
/// timeout. The slot is held by the blocking work itself, so it is only
/// freed once that work has stopped, even when this future gives up first.
async fn solve_bounded(challenge: Value) -> Result<String> {
    let started = tokio::time::Instant::now();
    let permit = tokio::time::timeout(GROUP_DM_CAPTCHA_TIMEOUT, solver_slot().acquire_owned())
        .await
        .map_err(|_| Error::Captcha("another CAPTCHA is being solved".into()))?
        .map_err(|_| Error::Captcha("solver closed".into()))?;
    let left = GROUP_DM_CAPTCHA_TIMEOUT.saturating_sub(started.elapsed());
    // The workers stop on their own after `left`; the outer timeout also covers
    // the hand-off, and dropping the inner future cancels the workers.
    tokio::time::timeout(left, crate::altcha::solve_holding(challenge, left, permit))
        .await
        .unwrap_or_else(|_| Err(Error::Captcha("CAPTCHA timed out".into())))
}

impl Client {
    /// Sends a CAPTCHA-gated request, solving challenges on the way.
    async fn send_with_captcha(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<GroupDmOutcome<RawResponse>> {
        let mut token: Option<String> = None;
        let mut solves = 0;
        loop {
            let mut headers: Vec<(&'static str, String)> = Vec::new();
            if let Some(t) = &token {
                headers.push(("X-Captcha-Token", t.clone()));
            }
            let limiter = self.rate_limiter();
            let raw = send(
                self.instance(),
                &limiter,
                Some(self.token()),
                Request {
                    method: method.clone(),
                    path,
                    body,
                    headers: &headers,
                    upload: None,
                },
            )
            .await?;
            if raw.is_success() {
                return Ok(GroupDmOutcome::Done(raw));
            }
            let value = raw.value();
            if raw.status == 400 {
                if let Some(challenge) = crate::altcha::challenge_from_error(&value) {
                    if solves >= MAX_SOLVES {
                        return Ok(GroupDmOutcome::Captcha);
                    }
                    solves += 1;
                    match solve_bounded(challenge).await {
                        Ok(t) => token = Some(t),
                        Err(e) => {
                            tracing::warn!("group DM CAPTCHA not solved: {e}");
                            return Ok(GroupDmOutcome::Captcha);
                        }
                    }
                    continue;
                }
                if value.get("code").and_then(Value::as_str)
                    == Some("GROUP_DM_RECIPIENTS_NOT_ADDABLE")
                {
                    let unaddable = match value.get("unaddable_recipients") {
                        Some(Value::Array(items)) => {
                            crate::models::lenient_list(items.clone(), "unaddable recipient")
                        }
                        _ => Vec::new(),
                    };
                    let addable = value
                        .get("addable_recipients")
                        .and_then(|v| crate::models::sf_list(v.clone()).ok())
                        .unwrap_or_default();
                    return Ok(GroupDmOutcome::NotAddable { unaddable, addable });
                }
            }
            return Err(Error::Api(Box::new(raw.api_error())));
        }
    }

    /// `POST /users/@me/channels` with `recipient_id`: opens (or returns) the
    /// DM with one account.
    pub async fn open_dm(&self, user_id: &str) -> Result<Channel> {
        self.raw(
            Method::POST,
            "/users/@me/channels",
            Some(&json!({ "recipient_id": snowflake(user_id)? })),
        )
        .await?
        .json()
    }

    /// `POST /users/@me/channels` with `recipients`: creates a group DM with
    /// 1-49 other accounts (friends whose settings allow it). CAPTCHA-gated;
    /// see the module docs.
    pub async fn create_group_dm(&self, recipients: &[String]) -> Result<GroupDmOutcome<Channel>> {
        if recipients.is_empty() {
            return Err(Error::InvalidInput("group DM: no recipients".into()));
        }
        crate::api::id_list(recipients, MAX_GROUP_DM_RECIPIENTS, "group DM recipients")?;
        let mut unique: Vec<&String> = Vec::with_capacity(recipients.len());
        for r in recipients {
            if !unique.contains(&r) {
                unique.push(r);
            }
        }
        let body = json!({ "recipients": unique });
        Ok(
            match self
                .send_with_captcha(Method::POST, "/users/@me/channels", Some(&body))
                .await?
            {
                GroupDmOutcome::Done(raw) => GroupDmOutcome::Done(raw.json()?),
                GroupDmOutcome::Captcha => GroupDmOutcome::Captcha,
                GroupDmOutcome::NotAddable { unaddable, addable } => {
                    GroupDmOutcome::NotAddable { unaddable, addable }
                }
            },
        )
    }

    /// `PUT /channels/{channel}/recipients/{user}`: adds a friend to a group
    /// DM. CAPTCHA-gated; see the module docs.
    pub async fn add_group_dm_recipient(
        &self,
        channel_id: &str,
        user_id: &str,
    ) -> Result<GroupDmOutcome<()>> {
        let path = format!(
            "/channels/{}/recipients/{}",
            snowflake(channel_id)?,
            snowflake(user_id)?
        );
        Ok(
            match self.send_with_captcha(Method::PUT, &path, None).await? {
                GroupDmOutcome::Done(_) => GroupDmOutcome::Done(()),
                GroupDmOutcome::Captcha => GroupDmOutcome::Captcha,
                GroupDmOutcome::NotAddable { unaddable, addable } => {
                    GroupDmOutcome::NotAddable { unaddable, addable }
                }
            },
        )
    }

    /// `DELETE /channels/{channel}/recipients/{user}`: removes someone from a
    /// group DM (the owner can remove anyone, everyone can remove themself).
    /// `silent` skips the system message.
    pub async fn remove_group_dm_recipient(
        &self,
        channel_id: &str,
        user_id: &str,
        silent: bool,
    ) -> Result<()> {
        let path = format!(
            "/channels/{}/recipients/{}?silent={silent}",
            snowflake(channel_id)?,
            snowflake(user_id)?
        );
        self.raw(Method::DELETE, &path, None).await?;
        Ok(())
    }

    /// `DELETE /channels/{channel}`: closes a DM or leaves a group DM.
    /// `silent` skips the "left the group" message. (Deleting the account's
    /// own messages on the way out needs sudo mode and is not offered.)
    pub async fn close_dm(&self, channel_id: &str, silent: bool) -> Result<()> {
        let path = format!("/channels/{}?silent={silent}", snowflake(channel_id)?);
        self.raw(Method::DELETE, &path, None).await?;
        Ok(())
    }

    /// `PATCH /channels/{channel}`: renames a group DM, changes its icon or
    /// hands it to another participant.
    pub async fn edit_group_dm(&self, channel_id: &str, edit: &GroupDmEdit) -> Result<Channel> {
        let body = edit.to_json()?;
        self.raw(
            Method::PATCH,
            &format!("/channels/{}", snowflake(channel_id)?),
            Some(&body),
        )
        .await?
        .json()
    }

    /// `PUT /users/@me/channels/{channel}/pin`: pins a DM or group DM at the
    /// end of the pinned list.
    pub async fn pin_dm(&self, channel_id: &str) -> Result<()> {
        self.raw(
            Method::PUT,
            &format!("/users/@me/channels/{}/pin", snowflake(channel_id)?),
            None,
        )
        .await?;
        Ok(())
    }

    /// `DELETE /users/@me/channels/{channel}/pin`: unpins a DM or group DM.
    pub async fn unpin_dm(&self, channel_id: &str) -> Result<()> {
        self.raw(
            Method::DELETE,
            &format!("/users/@me/channels/{}/pin", snowflake(channel_id)?),
            None,
        )
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_solver_slot_is_held_until_the_blocking_work_stops() {
        // A challenge nothing can solve keeps the workers busy until cancelled.
        let mut c = crate::altcha::make_test_challenge(1, 1);
        c["parameters"]["keyPrefix"] = json!("0123456789abcdef0123456789abcdef");
        let slot = Arc::new(Semaphore::new(1));
        let permit = slot.clone().acquire_owned().await.unwrap();
        let fut = crate::altcha::solve_holding(c, Duration::from_secs(600), permit);
        // Give up early: the future is dropped while the workers still run.
        assert!(tokio::time::timeout(Duration::from_millis(100), fut)
            .await
            .is_err());
        // The slot comes back only once the blocking work has stopped.
        let back = tokio::time::timeout(Duration::from_secs(10), slot.acquire())
            .await
            .expect("the cancelled solve released the slot");
        drop(back);
    }

    #[test]
    fn group_dm_edit_needs_a_change_and_valid_values() {
        assert!(GroupDmEdit::default().to_json().is_err());
        let rename = GroupDmEdit {
            name: Some(Some("Trip".into())),
            ..Default::default()
        };
        assert_eq!(rename.to_json().unwrap(), json!({"name": "Trip"}));
        let blank = GroupDmEdit {
            name: Some(Some("  ".into())),
            ..Default::default()
        };
        assert!(blank.to_json().is_err());
        let clear = GroupDmEdit {
            name: Some(Some(String::new())),
            icon: Some(None),
            ..Default::default()
        };
        assert_eq!(
            clear.to_json().unwrap(),
            json!({"name": null, "icon": null})
        );
        let bad_owner = GroupDmEdit {
            owner_id: Some("@me".into()),
            ..Default::default()
        };
        assert!(bad_owner.to_json().is_err());
    }
}
