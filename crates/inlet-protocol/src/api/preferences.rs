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
//!   a newer Fluxer client has set;
//! - sends `synced_preferences_base_hash`, so the server refuses the write
//!   when another client changed the snapshot in the meantime
//!   (`SYNCED_PREFERENCES_CONFLICT`). [`crate::Client::update_synced_preferences`]
//!   then reads the new snapshot and applies the change again.
//!
//! Limits (Fluxer's): at most 512 KiB decoded and 699,052 base64 characters.

use base64::Engine;
use prost::Message;
use reqwest::Method;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::models::UserSettings;
use crate::proto::{merge_unknown, Schema};
use crate::Client;

/// The generated message (`fluxer.user.preferences.v1.SyncedPreferences`).
pub type Preferences = crate::proto::preferences::SyncedPreferences;

/// Largest decoded snapshot Fluxer accepts.
pub const MAX_PREFERENCES_BYTES: usize = 512 * 1024;
/// Largest base64 snapshot Fluxer accepts.
pub const MAX_PREFERENCES_ENCODED: usize = MAX_PREFERENCES_BYTES.div_ceil(3) * 4;
/// Retries after `SYNCED_PREFERENCES_CONFLICT` (as Fluxer's client: read, apply again, write).
const CONFLICT_RETRIES: usize = 2;

/// A snapshot as the server stored it, decoded.
#[derive(Clone, PartialEq)]
pub struct SyncedPreferences {
    original: Vec<u8>,
    value: Preferences,
}

impl std::fmt::Debug for SyncedPreferences {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncedPreferences")
            .field("bytes", &self.original.len())
            .field("value", &self.value)
            .finish()
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
    /// crate does not know copied from the stored bytes. Fails with
    /// [`Error::InvalidInput`] when that is not possible (the stored bytes
    /// cannot be split into fields) or the result is over 512 KiB.
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
        Ok(merged)
    }
}

impl UserSettings {
    /// The decoded [`SyncedPreferences`], if the settings carry them.
    pub fn synced_preferences(&self) -> Option<Result<SyncedPreferences>> {
        SyncedPreferences::from_settings(self)
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

impl Client {
    /// `GET /users/@me/settings`, only the synced preferences.
    pub async fn synced_preferences(&self) -> Result<SyncedPreferences> {
        self.user_settings()
            .await?
            .synced_preferences()
            .unwrap_or_else(|| Ok(SyncedPreferences::empty()))
    }

    /// `PATCH /users/@me/settings` with `synced_preferences` built from
    /// `changed` on top of `base` (unknown fields kept) and the base hash.
    /// USER_SETTINGS_UPDATE follows on the gateway.
    pub async fn save_synced_preferences(
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

    /// Applies `edit` to `base` and saves it. On a conflict it reads the
    /// snapshot again and applies `edit` to that, up to two more times
    /// (each round is one GET and one PATCH), then gives up with
    /// [`Error::Api`] `SYNCED_PREFERENCES_CONFLICT`. Returns the stored
    /// snapshot; when `edit` changes nothing, no request is made.
    pub async fn update_synced_preferences<F>(
        &self,
        base: &SyncedPreferences,
        edit: F,
    ) -> Result<SyncedPreferences>
    where
        F: Fn(&mut Preferences),
    {
        let mut current = base.clone();
        for attempt in 0..=CONFLICT_RETRIES {
            let mut changed = current.get().clone();
            edit(&mut changed);
            if changed == *current.get() {
                return Ok(current);
            }
            match self.save_synced_preferences(&current, &changed).await? {
                PreferencesSave::Saved(saved) => return Ok(*saved),
                PreferencesSave::Conflict if attempt < CONFLICT_RETRIES => {
                    current = self.synced_preferences().await?;
                }
                PreferencesSave::Conflict => break,
            }
        }
        Err(Error::Api(Box::new(crate::ApiError {
            status: 400,
            code: "SYNCED_PREFERENCES_CONFLICT".into(),
            message: "the synced preferences kept changing while saving".into(),
            errors: None,
        })))
    }
}
