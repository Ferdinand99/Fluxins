//! `SyncedPreferences`: the client preferences an account shares between
//! all its Fluxer clients (emoji favourites and recents, accessibility,
//! favourite GIFs, keybinds, ...).
//!
//! The server keeps them as one base64 protobuf snapshot in the
//! `synced_preferences` field of the user settings (READY `user_settings`,
//! USER_SETTINGS_UPDATE and `GET /users/@me/settings`). A write replaces the
//! whole snapshot, so this crate:
//!
//! - keeps the bytes it read next to the decoded value ([`SyncedPreferences`]);
//! - writes back with the fields it does not know copied unchanged
//!   ([`crate::proto`] has the rules), so Inlet never deletes a setting that
//!   a newer Fluxer client has set, and checks the result before sending it;
//! - sends `synced_preferences_base_hash`, so the server refuses the write
//!   when another client changed the snapshot in the meantime
//!   (`SYNCED_PREFERENCES_CONFLICT`); the crate then reads the new snapshot
//!   and applies the change again.
//!
//! # One writer per account
//!
//! Like Fluxer's client, writes of one account are serialised and batched
//! ([`crate::Client::update_synced_preferences`]): edits that arrive while a
//! write waits are applied together in one PATCH. A write waits
//! [`PREFERENCES_FLUSH_DELAY`] to gather edits, and two PATCH requests of one
//! account start at least [`PREFERENCES_WRITE_INTERVAL`] apart (also the
//! retries after a conflict), so the account stays far below Fluxer's 20 per
//! 10 s for `PATCH /users/@me/settings`. The crate keeps the last snapshot it
//! stored and builds on it when a caller passes a snapshot this account's own
//! writes have replaced, so callers do not conflict with themselves.
//!
//! Limits (Fluxer's): at most 512 KiB decoded and 699,052 base64 characters.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use base64::Engine;
use prost::Message;
use reqwest::Method;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::oneshot;
use tokio::time::Instant;

use crate::error::{ApiError, Error, Result};
use crate::media::{Media, Transform};
use crate::models::UserSettings;
use crate::proto::{merge_unknown, Schema};
use crate::Client;

/// The generated message (`fluxer.user.preferences.v1.SyncedPreferences`).
pub type Preferences = crate::proto::preferences::SyncedPreferences;

/// Largest decoded snapshot Fluxer accepts.
pub const MAX_PREFERENCES_BYTES: usize = 512 * 1024;
/// Largest base64 snapshot Fluxer accepts.
pub const MAX_PREFERENCES_ENCODED: usize = MAX_PREFERENCES_BYTES.div_ceil(3) * 4;
/// How long a write waits for more edits before it sends them together.
pub const PREFERENCES_FLUSH_DELAY: Duration = Duration::from_millis(500);
/// Shortest time between two preference PATCH requests of one account.
pub const PREFERENCES_WRITE_INTERVAL: Duration = Duration::from_secs(1);
/// Retries after `SYNCED_PREFERENCES_CONFLICT` per batch (read, apply again, write).
const CONFLICT_RETRIES: usize = 2;
/// Most edits waiting for one account; more are refused with `RateLimited`.
const MAX_PENDING_EDITS: usize = 256;
/// Snapshots this account's own writes replaced, remembered per account.
const MAX_SUPERSEDED: usize = 16;

/// A snapshot as the server stored it, decoded.
#[derive(Clone, PartialEq)]
pub struct SyncedPreferences {
    original: Vec<u8>,
    value: Preferences,
}

impl std::fmt::Debug for SyncedPreferences {
    // The value holds personal data (who the account mentions, favourite
    // GIFs, spammer lists): only its size is printed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncedPreferences")
            .field("bytes", &self.original.len())
            .finish_non_exhaustive()
    }
}

impl Default for SyncedPreferences {
    fn default() -> Self {
        Self::empty()
    }
}

impl SyncedPreferences {
    /// No snapshot stored (every preference at its default).
    pub fn empty() -> Self {
        Self {
            original: Vec::new(),
            value: Preferences::default(),
        }
    }

    /// Decodes the protobuf bytes of a snapshot.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_PREFERENCES_BYTES {
            return Err(Error::Decode("synced preferences over 512 KiB".into()));
        }
        let value = Preferences::decode(bytes)
            .map_err(|e| Error::Decode(format!("synced preferences: {e}")))?;
        Ok(Self {
            original: bytes.to_vec(),
            value,
        })
    }

    /// Decodes the base64 snapshot of the user settings (standard or
    /// URL-safe, with or without padding; the empty string is no snapshot).
    pub fn from_base64(encoded: &str) -> Result<Self> {
        let encoded = encoded.trim();
        if encoded.is_empty() {
            return Ok(Self::empty());
        }
        if encoded.len() > MAX_PREFERENCES_ENCODED {
            return Err(Error::Decode("synced preferences over 512 KiB".into()));
        }
        use base64::engine::general_purpose::{
            STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD,
        };
        let bytes = [STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD]
            .iter()
            .find_map(|e| e.decode(encoded).ok())
            .ok_or_else(|| Error::Decode("synced preferences are not base64".into()))?;
        Self::decode(&bytes)
    }

    /// The snapshot in user settings, or `None` when the settings have no
    /// `synced_preferences` field. An empty string is an empty snapshot.
    pub fn from_settings(settings: &UserSettings) -> Option<Result<Self>> {
        match settings.extra.get("synced_preferences")? {
            Value::String(s) => Some(Self::from_base64(s)),
            Value::Null => Some(Ok(Self::empty())),
            _ => Some(Err(Error::Decode(
                "synced_preferences is not a string".into(),
            ))),
        }
    }

    /// The decoded preferences.
    pub fn get(&self) -> &Preferences {
        &self.value
    }

    /// The bytes as the server stored them.
    pub fn bytes(&self) -> &[u8] {
        &self.original
    }

    /// `synced_preferences_base_hash`: lowercase hex SHA-256 of the stored bytes.
    pub fn base_hash(&self) -> String {
        hex::encode(Sha256::digest(&self.original))
    }

    /// The bytes to store for `changed`: its encoding with every field this
    /// crate does not know copied from the stored bytes. The result is
    /// decoded again and must encode exactly like `changed`; otherwise, or
    /// when the stored bytes cannot be split into fields, or the result is
    /// over 512 KiB, this fails with [`Error::InvalidInput`] and nothing may be
    /// sent. Linear in the size of the snapshot.
    pub fn encode_change(&self, changed: &Preferences) -> Result<Vec<u8>> {
        let canonical = self.value.encode_to_vec();
        let new = changed.encode_to_vec();
        let merged = merge_unknown(&self.original, &canonical, &new, Schema::SYNCED_PREFERENCES)
            .map_err(|_| {
                Error::InvalidInput(
                    "the stored synced preferences cannot be rewritten without losing fields"
                        .into(),
                )
            })?;
        if merged.len() > MAX_PREFERENCES_BYTES {
            return Err(Error::InvalidInput(
                "synced preferences would be over 512 KiB".into(),
            ));
        }
        // Never send a lossy merge: what the server would decode must be the change.
        let check = Preferences::decode(merged.as_slice()).map(|p| p.encode_to_vec());
        if check.as_deref() != Ok(new.as_slice()) {
            return Err(Error::InvalidInput(
                "the merged synced preferences do not match the change; not sent".into(),
            ));
        }
        Ok(merged)
    }
}

impl UserSettings {
    /// The decoded [`SyncedPreferences`], if the settings carry them.
    pub fn synced_preferences(&self) -> Option<Result<SyncedPreferences>> {
        SyncedPreferences::from_settings(self)
    }
}

impl crate::proto::preferences::FavoriteGifEntry {
    /// A preview of a favourite GIF through the instance's media endpoint,
    /// or `None`. `url` and the formats' `src` are the provider's own URLs:
    /// never load them (`url` is the text to send).
    pub fn preview(&self, media: &Media, t: Transform) -> Option<String> {
        crate::api::gifs::IMAGE_PREVIEW_ORDER
            .iter()
            .filter_map(|name| self.media.get(*name))
            .find_map(|f| media.external(&f.proxy_src, t))
            .or_else(|| media.external(&self.proxy_url, t))
    }
}

/// The answer to [`Client::save_synced_preferences`].
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum PreferencesSave {
    /// Stored; the snapshot as the server now has it.
    Saved(Box<SyncedPreferences>),
    /// Another client changed the snapshot since `base` was read; nothing was
    /// written. Read it again and apply the change to the new snapshot.
    Conflict,
}

type Edit = Box<dyn Fn(&mut Preferences) + Send + Sync>;

/// An edit waiting for the account's writer.
struct Pending {
    base: SyncedPreferences,
    edit: Edit,
    done: oneshot::Sender<Result<SyncedPreferences>>,
}

/// The writer state of one account.
#[derive(Default)]
struct Writer {
    /// The last snapshot this account stored.
    last: Option<SyncedPreferences>,
    /// Hashes of snapshots this account's own writes replaced.
    superseded: VecDeque<String>,
    /// When the last PATCH started.
    last_patch: Option<Instant>,
}

/// Per-account preference writes (kept in the account's rate limiter).
#[derive(Default)]
pub(crate) struct PreferencesState {
    queue: Mutex<Vec<Pending>>,
    writer: tokio::sync::Mutex<Writer>,
}

impl std::fmt::Debug for PreferencesState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreferencesState").finish_non_exhaustive()
    }
}

impl PreferencesState {
    fn take(&self) -> Vec<Pending> {
        std::mem::take(&mut *self.queue.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

fn conflict_error() -> Error {
    Error::Api(Box::new(ApiError {
        status: 400,
        code: "SYNCED_PREFERENCES_CONFLICT".into(),
        message: "the synced preferences kept changing while saving".into(),
        errors: None,
    }))
}

/// A copy of an error for every caller of a batch. A network error becomes
/// [`Error::Decode`] with its text (it cannot be copied).
fn share_error(e: &Error) -> Error {
    match e {
        Error::Api(a) => Error::Api(a.clone()),
        Error::Unauthorized => Error::Unauthorized,
        Error::GlobalRateLimited => Error::GlobalRateLimited,
        Error::RateLimited { retry_after } => Error::RateLimited {
            retry_after: *retry_after,
        },
        Error::InvalidInput(s) => Error::InvalidInput(s.clone()),
        Error::EndpointsChanged { old, new } => Error::EndpointsChanged {
            old: old.clone(),
            new: new.clone(),
        },
        Error::TooLarge { limit } => Error::TooLarge { limit: *limit },
        other => Error::Decode(other.to_string()),
    }
}

impl Writer {
    /// Waits until the next PATCH may start, and records it.
    async fn pace(&mut self) {
        if let Some(t) = self.last_patch {
            tokio::time::sleep_until(t + PREFERENCES_WRITE_INTERVAL).await;
        }
        self.last_patch = Some(Instant::now());
    }

    fn stored(&mut self, replaced: &SyncedPreferences, saved: &SyncedPreferences) {
        self.superseded.push_back(replaced.base_hash());
        while self.superseded.len() > MAX_SUPERSEDED {
            self.superseded.pop_front();
        }
        self.last = Some(saved.clone());
    }

    /// The snapshot to build on: a caller's, unless this account's own
    /// writes have replaced it; then the last stored one.
    fn base_for(&self, batch: &[Pending]) -> SyncedPreferences {
        let fresh = batch
            .iter()
            .map(|p| &p.base)
            .find(|b| !self.superseded.contains(&b.base_hash()));
        match (fresh, &self.last) {
            (Some(b), _) => b.clone(),
            (None, Some(last)) => last.clone(),
            (None, None) => batch.first().map(|p| p.base.clone()).unwrap_or_default(),
        }
    }
}

impl Client {
    /// `GET /users/@me/settings`, only the synced preferences.
    pub async fn synced_preferences(&self) -> Result<SyncedPreferences> {
        self.user_settings()
            .await?
            .synced_preferences()
            .unwrap_or_else(|| Ok(SyncedPreferences::empty()))
    }

    /// One `PATCH /users/@me/settings` with `synced_preferences` and the base hash.
    async fn patch_preferences(
        &self,
        base: &SyncedPreferences,
        changed: &Preferences,
    ) -> Result<PreferencesSave> {
        let bytes = base.encode_change(changed)?;
        let body = json!({
            "synced_preferences": base64::engine::general_purpose::STANDARD.encode(&bytes),
            "synced_preferences_base_hash": base.base_hash(),
        });
        match self
            .raw(Method::PATCH, "/users/@me/settings", Some(&body))
            .await
        {
            Ok(raw) => {
                let settings: UserSettings = raw.json()?;
                let saved = match settings.synced_preferences() {
                    Some(r) => r?,
                    // Older servers may leave the field out; what was sent is stored.
                    None => SyncedPreferences::decode(&bytes)?,
                };
                Ok(PreferencesSave::Saved(Box::new(saved)))
            }
            Err(Error::Api(e)) if e.code == "SYNCED_PREFERENCES_CONFLICT" => {
                Ok(PreferencesSave::Conflict)
            }
            Err(e) => Err(e),
        }
    }

    /// `PATCH /users/@me/settings` with `synced_preferences` built from
    /// `changed` on top of `base` (unknown fields kept) and the base hash,
    /// once, without retries. It waits for the account's writer and the
    /// write interval like [`Client::update_synced_preferences`], which is
    /// what the UI should normally use. USER_SETTINGS_UPDATE follows on the
    /// gateway.
    pub async fn save_synced_preferences(
        &self,
        base: &SyncedPreferences,
        changed: &Preferences,
    ) -> Result<PreferencesSave> {
        let limiter = self.rate_limiter();
        let mut writer = limiter.coalesce.preferences.writer.lock().await;
        // Fail before waiting when the change cannot be sent at all.
        base.encode_change(changed)?;
        writer.pace().await;
        let result = self.patch_preferences(base, changed).await?;
        if let PreferencesSave::Saved(saved) = &result {
            writer.stored(base, saved);
        }
        Ok(result)
    }

    /// Applies `edit` to the newest known snapshot and saves it.
    ///
    /// - `base` is the snapshot the caller knows (from READY,
    ///   USER_SETTINGS_UPDATE or an earlier call). When this account's own
    ///   writes have replaced it, the last stored snapshot is used instead.
    /// - Edits of one account are queued and sent together: one PATCH per
    ///   batch, at least [`PREFERENCES_WRITE_INTERVAL`] apart, after
    ///   [`PREFERENCES_FLUSH_DELAY`]. `edit` may run several times (once per
    ///   attempt), so it should set values rather than toggle them.
    /// - On a conflict it reads the snapshot again (one GET) and applies the
    ///   batch again, at most twice, then gives up with [`Error::Api`]
    ///   `SYNCED_PREFERENCES_CONFLICT`. A batch makes at most 3 PATCH and 2 GET
    ///   requests.
    /// - When the batch changes nothing, no request is made.
    /// - A network error reaches the other callers of the batch as
    ///   [`Error::Decode`] with its text. More than 256 waiting edits are
    ///   refused with [`Error::RateLimited`].
    ///
    /// Returns the snapshot as stored.
    pub async fn update_synced_preferences<F>(
        &self,
        base: &SyncedPreferences,
        edit: F,
    ) -> Result<SyncedPreferences>
    where
        F: Fn(&mut Preferences) + Send + Sync + 'static,
    {
        let limiter = self.rate_limiter();
        let state = &limiter.coalesce.preferences;
        let (done, mut rx) = oneshot::channel();
        {
            let mut queue = state.queue.lock().unwrap_or_else(|e| e.into_inner());
            if queue.len() >= MAX_PENDING_EDITS {
                return Err(Error::RateLimited {
                    retry_after: PREFERENCES_WRITE_INTERVAL,
                });
            }
            queue.push(Pending {
                base: base.clone(),
                edit: Box::new(edit),
                done,
            });
        }
        let mut writer = state.writer.lock().await;
        // A writer before this one may have sent this edit already.
        if let Ok(result) = rx.try_recv() {
            return result;
        }
        // Gather more edits, and keep the interval since the last PATCH.
        let mut start = Instant::now() + PREFERENCES_FLUSH_DELAY;
        if let Some(t) = writer.last_patch {
            start = start.max(t + PREFERENCES_WRITE_INTERVAL);
        }
        tokio::time::sleep_until(start).await;
        let batch = state.take();
        let result = self.write_batch(&mut writer, &batch).await;
        for p in batch {
            let _ = p.done.send(match &result {
                Ok(s) => Ok(s.clone()),
                Err(e) => Err(share_error(e)),
            });
        }
        drop(writer);
        rx.await
            .unwrap_or_else(|_| Err(Error::Decode("the preference write was cancelled".into())))
    }

    /// Applies a batch of edits and stores it (with conflict retries).
    async fn write_batch(
        &self,
        writer: &mut Writer,
        batch: &[Pending],
    ) -> Result<SyncedPreferences> {
        let mut current = writer.base_for(batch);
        for attempt in 0..=CONFLICT_RETRIES {
            let mut changed = current.get().clone();
            for p in batch {
                (p.edit)(&mut changed);
            }
            if changed.encode_to_vec() == current.get().encode_to_vec() {
                return Ok(current);
            }
            // Refuse before waiting when the change cannot be sent at all.
            current.encode_change(&changed)?;
            writer.pace().await;
            match self.patch_preferences(&current, &changed).await? {
                PreferencesSave::Saved(saved) => {
                    writer.stored(&current, &saved);
                    return Ok(*saved);
                }
                PreferencesSave::Conflict if attempt < CONFLICT_RETRIES => {
                    current = self.synced_preferences().await?;
                }
                PreferencesSave::Conflict => break,
            }
        }
        Err(conflict_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_shows_only_the_size() {
        let p = Preferences {
            local_spam_overrides: Some(crate::proto::preferences::LocalUserSpamOverrides {
                spammer_user_ids: vec!["123456789".into()],
                not_spammer_user_ids: Vec::new(),
            }),
            ..Default::default()
        };
        let s = SyncedPreferences::decode(&p.encode_to_vec()).unwrap();
        let dbg = format!("{s:?}");
        assert!(!dbg.contains("123456789"), "{dbg}");
        assert!(dbg.contains("bytes"));
    }

    #[test]
    fn a_lossy_merge_is_refused() {
        // A snapshot whose map holds the same key twice decodes to the last
        // value; the merge then has no safe pairing and must still give exactly
        // the change, or refuse.
        let entry = |k: &str, count: u8| {
            let mut e = vec![0x0a, k.len() as u8];
            e.extend_from_slice(k.as_bytes());
            e.extend_from_slice(&[0x12, 0x02, 0x08, count]);
            let mut r = vec![0x0a, e.len() as u8];
            r.extend(e);
            r
        };
        let mut usage = entry("a", 1);
        usage.extend(entry("a", 2));
        usage.extend(entry("b", 3));
        let mut original = vec![0xa2, 0x01, usage.len() as u8];
        original.extend(usage);
        let base = SyncedPreferences::decode(&original).unwrap();
        let mut changed = base.get().clone();
        changed
            .emoji_picker
            .as_mut()
            .unwrap()
            .usage
            .get_mut("b")
            .unwrap()
            .count = 4;
        let out = base.encode_change(&changed).unwrap();
        assert_eq!(
            Preferences::decode(out.as_slice()).unwrap(),
            changed,
            "duplicate keys fall back to the new value"
        );
    }

    #[test]
    fn favorite_gifs_preview_only_through_the_media_endpoint() {
        use crate::proto::preferences::{FavoriteGifEntry, FavoriteGifMediaFormat};
        let media = Media::new("https://media.example.org").unwrap();
        let mut gif = FavoriteGifEntry {
            url: "https://klipy.example/gifs/x".into(),
            proxy_url: "https://cdn.elsewhere.example/x.gif".into(),
            ..Default::default()
        };
        assert_eq!(gif.preview(&media, Transform::default()), None);
        gif.media.insert(
            "tinygif".into(),
            FavoriteGifMediaFormat {
                src: "https://static.klipy.example/x.gif".into(),
                proxy_src: "https://media.example.org/external/a/x.gif".into(),
                width: 10,
                height: 10,
            },
        );
        assert_eq!(
            gif.preview(&media, Transform::default()).as_deref(),
            Some("https://media.example.org/external/a/x.gif")
        );
    }
}
