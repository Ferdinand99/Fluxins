// SPDX-License-Identifier: AGPL-3.0-or-later
//! Keeps session tokens in the OS keychain (macOS Keychain, Windows Credential Manager,
//! Secret Service on Linux). A token is never written to a file and never logged.
//!
//! Keychain layout: service `inlet`, one entry per account named `<origin>#<user_id>`,
//! holding `{token, pins, pasted}` as JSON. The list of accounts (no secrets: origin,
//! user ID, display name, account name, and which one is active) is `accounts.json` in
//! the config folder. Older Inlet builds kept one entry per instance, named after the
//! origin; [`migrate_legacy`] moves it to the new name once the account's user ID is
//! known and removes the old entry only after the copy was read back.
//!
//! On disk besides `accounts.json`: the address of the last instance (the sign-in form's
//! default) and the installation ID. None of them is secret.
//!
//! Every function here blocks (keychain prompts, D-Bus, file IO). Call them from
//! `tokio::task::spawn_blocking` or a plain thread, never from async code directly.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use zeroize::Zeroizing;

const SERVICE: &str = "inlet";

/// Where secrets live. The keychain in the app; an in-memory map in tests.
pub trait SecretStore: Send + Sync {
    /// The value of entry `name`, or `None` when there is none.
    fn get(&self, name: &str) -> Result<Option<Zeroizing<String>>>;
    /// Writes entry `name`.
    fn set(&self, name: &str, value: &str) -> Result<()>;
    /// Removes entry `name`; a missing entry is not an error.
    fn delete(&self, name: &str) -> Result<()>;
}

/// The OS keychain, service `inlet`.
pub struct Keychain;

impl Keychain {
    fn entry(name: &str) -> Result<keyring::Entry> {
        keyring::Entry::new(SERVICE, name).context("open the keychain entry")
    }
}

impl SecretStore for Keychain {
    fn get(&self, name: &str) -> Result<Option<Zeroizing<String>>> {
        match Self::entry(name)?.get_password() {
            Ok(raw) => Ok(Some(Zeroizing::new(raw))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(err) => Err(err).context("read the token"),
        }
    }

    fn set(&self, name: &str, value: &str) -> Result<()> {
        Self::entry(name)?
            .set_password(value)
            .context("store the token")
    }

    fn delete(&self, name: &str) -> Result<()> {
        match Self::entry(name)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(err) => Err(err).context("delete the token"),
        }
    }
}

/// The keychain entry name of an account: `<origin>#<user_id>`.
pub fn entry_name(origin: &str, user_id: &str) -> String {
    format!("{origin}#{user_id}")
}

/// What a keychain entry holds: the token and the hosts it was issued for, as one
/// JSON value. Older entries hold only the bare token, or pins without the media host
/// (those read as `media: None` and get it after the next successful restore).
#[derive(serde::Serialize, serde::Deserialize)]
struct Blob {
    token: String,
    pins: Option<inlet_protocol::SessionPins>,
    /// The token was pasted (it belongs to another client): never sign it out on the
    /// server, only forget it. Entries written before this field count as not pasted.
    #[serde(default)]
    pasted: bool,
}

/// A stored session: the token (wiped from memory on drop) and its pins.
pub struct StoredSession {
    pub token: inlet_protocol::Token,
    /// `None` for an entry written before pinning existed.
    pub pins: Option<inlet_protocol::SessionPins>,
    /// See [`Blob::pasted`].
    pub pasted: bool,
}

fn encode(
    token: &inlet_protocol::Token,
    pins: &inlet_protocol::SessionPins,
    pasted: bool,
) -> Result<Zeroizing<String>> {
    let blob = Blob {
        token: token.expose_secret().to_string(),
        pins: Some(pins.clone()),
        pasted,
    };
    let json = Zeroizing::new(serde_json::to_string(&blob).context("encode the session")?);
    drop(Zeroizing::new(blob.token));
    Ok(json)
}

fn decode(raw: &str) -> StoredSession {
    match serde_json::from_str::<Blob>(raw) {
        Ok(blob) => StoredSession {
            pins: blob.pins,
            pasted: blob.pasted,
            token: inlet_protocol::Token::new(blob.token),
        },
        Err(_) => StoredSession {
            token: inlet_protocol::Token::new(raw.trim().to_string()),
            pins: None,
            pasted: false,
        },
    }
}

/// Stores an account's session under its own entry.
pub fn save_session(
    store: &dyn SecretStore,
    entry: &str,
    token: &inlet_protocol::Token,
    pins: &inlet_protocol::SessionPins,
    pasted: bool,
) -> Result<()> {
    let json = encode(token, pins, pasted)?;
    store.set(entry, &json)
}

/// The session stored under `entry`, if any.
pub fn load_session(store: &dyn SecretStore, entry: &str) -> Result<Option<StoredSession>> {
    Ok(store.get(entry)?.map(|raw| decode(&raw)))
}

pub fn delete_session(store: &dyn SecretStore, entry: &str) -> Result<()> {
    store.delete(entry)
}

/// Moves the entry an older Inlet kept for `origin` (entry name = origin) to the
/// account's own entry `<origin>#<user_id>`, byte for byte.
///
/// Order, so a token is never lost: write the new entry, read it back and compare,
/// then call `listed` (which writes the account list; an error stops here), and only
/// then delete the old entry. If any step before the delete fails, the old entry stays
/// and the next start tries again; repeating the copy is harmless. Returns whether
/// an old entry was moved; `false` when there was none (already migrated).
pub fn migrate_legacy(
    store: &dyn SecretStore,
    origin: &str,
    user_id: &str,
    listed: impl FnOnce() -> Result<()>,
) -> Result<bool> {
    let Some(old) = store.get(origin)? else {
        return Ok(false);
    };
    let new = entry_name(origin, user_id);
    store.set(&new, &old)?;
    match store.get(&new)? {
        Some(back) if *back == *old => {}
        _ => anyhow::bail!("the moved keychain entry did not read back the same"),
    }
    listed()?;
    store.delete(origin)?;
    Ok(true)
}

// ---------- the account list (no secrets) ----------

/// One account in `accounts.json`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AccountRecord {
    pub origin: String,
    pub user_id: String,
    /// Display name, for the account switcher before the account is connected.
    #[serde(default)]
    pub name: String,
    /// Account name (`username`).
    #[serde(default)]
    pub handle: String,
}

impl AccountRecord {
    pub fn entry(&self) -> String {
        entry_name(&self.origin, &self.user_id)
    }
}

/// `accounts.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AccountList {
    #[serde(default)]
    pub accounts: Vec<AccountRecord>,
    /// The entry name of the account shown at start-up.
    #[serde(default)]
    pub active: Option<String>,
}

fn config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("inlet"))
}

/// Where `accounts.json` lives.
pub fn list_file() -> Option<PathBuf> {
    config_dir().map(|d| d.join("accounts.json"))
}

/// Reads the account list. A missing file is an empty list; an unreadable one too
/// (with a warning), as it holds nothing that cannot be rebuilt by signing in.
pub fn load_list(path: &Path) -> AccountList {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|err| {
            log::warn!("accounts.json could not be read: {err}");
            AccountList::default()
        }),
        Err(_) => AccountList::default(),
    }
}

/// Writes the account list (to a temporary file first, then renamed over the old one).
pub fn save_list(path: &Path, list: &AccountList) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).context("create the config folder")?;
    }
    let json = serde_json::to_string_pretty(list).context("encode the account list")?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).context("write the account list")?;
    std::fs::rename(&tmp, path).context("replace the account list")
}

fn state_file() -> Option<PathBuf> {
    config_dir().map(|d| d.join("last-instance"))
}

pub fn last_instance() -> Option<String> {
    let s = std::fs::read_to_string(state_file()?).ok()?;
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

pub fn set_last_instance(instance: &str) {
    if let Some(path) = state_file() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, instance);
    }
}

fn installation_id_file() -> Option<std::path::PathBuf> {
    config_dir().map(|d| d.join("installation-id"))
}

/// The client description sent to the instance, with an installation ID that stays the
/// same between starts (it identifies this install in the account's session list).
pub fn client_info() -> inlet_protocol::ClientInfo {
    let mut info = inlet_protocol::ClientInfo::default();
    let Some(path) = installation_id_file() else {
        return info;
    };
    match std::fs::read_to_string(&path) {
        Ok(id) if !id.trim().is_empty() => info.installation_id = id.trim().to_string(),
        _ => {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&path, &info.installation_id);
        }
    }
    info
}

/// An in-memory [`SecretStore`] for tests. `refuse_writes` makes `set` fail.
#[cfg(test)]
#[derive(Default)]
pub struct MemoryStore {
    pub entries: std::sync::Mutex<std::collections::HashMap<String, String>>,
    pub refuse_writes: std::sync::atomic::AtomicBool,
}

#[cfg(test)]
impl SecretStore for MemoryStore {
    fn get(&self, name: &str) -> Result<Option<Zeroizing<String>>> {
        Ok(self
            .entries
            .lock()
            .unwrap()
            .get(name)
            .map(|v| Zeroizing::new(v.clone())))
    }

    fn set(&self, name: &str, value: &str) -> Result<()> {
        if self.refuse_writes.load(std::sync::atomic::Ordering::SeqCst) {
            anyhow::bail!("refused");
        }
        self.entries
            .lock()
            .unwrap()
            .insert(name.to_string(), value.to_string());
        Ok(())
    }

    fn delete(&self, name: &str) -> Result<()> {
        self.entries.lock().unwrap().remove(name);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pins() -> inlet_protocol::SessionPins {
        inlet_protocol::SessionPins {
            api: "https://fluxer.example".into(),
            gateway: "wss://fluxer.example".into(),
            media: None,
        }
    }

    #[test]
    fn pasted_flag_round_trips() {
        let token = inlet_protocol::Token::new("secret");
        for pasted in [true, false] {
            let stored = decode(&encode(&token, &pins(), pasted).unwrap());
            assert_eq!(stored.pasted, pasted);
            assert_eq!(stored.token.expose_secret(), "secret");
            assert_eq!(stored.pins, Some(pins()));
        }
    }

    #[test]
    fn old_entries_count_as_not_pasted() {
        let old = r#"{"token":"secret","pins":{"api":"https://fluxer.example","gateway":"wss://fluxer.example"}}"#;
        let stored = decode(old);
        assert!(!stored.pasted);
        assert_eq!(stored.pins, Some(pins()));
        let bare = decode("bare-token");
        assert!(!bare.pasted);
        assert!(bare.pins.is_none());
    }

    #[test]
    fn media_pin_is_stored_and_old_entries_read_without_it() {
        let token = inlet_protocol::Token::new("secret");
        let with_media = inlet_protocol::SessionPins {
            media: Some("https://media.fluxer.example".into()),
            ..pins()
        };
        let stored = decode(&encode(&token, &with_media, true).unwrap());
        assert_eq!(stored.pins, Some(with_media));
        assert!(stored.pasted);
        // Written before the media pin existed: no media host, upgraded on the next save.
        let old = r#"{"token":"secret","pins":{"api":"https://fluxer.example","gateway":"wss://fluxer.example"},"pasted":true}"#;
        let stored = decode(old);
        assert_eq!(stored.pins.as_ref().and_then(|p| p.media.clone()), None);
        assert_eq!(stored.pins, Some(pins()));
        assert!(stored.pasted);
    }

    #[test]
    fn a_failed_copy_or_list_write_keeps_the_old_entry() {
        let store = MemoryStore::default();
        let origin = "https://fluxer.example";
        let token = inlet_protocol::Token::new("secret");
        save_session(&store, origin, &token, &pins(), false).unwrap();
        // The keychain refuses the new entry: the old one stays.
        store
            .refuse_writes
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(migrate_legacy(&store, origin, "42", || Ok(())).is_err());
        assert!(store.get(origin).unwrap().is_some());
        store
            .refuse_writes
            .store(false, std::sync::atomic::Ordering::SeqCst);
        // The account list cannot be written: the old entry stays too.
        assert!(migrate_legacy(&store, origin, "42", || anyhow::bail!("disk full")).is_err());
        assert!(store.get(origin).unwrap().is_some());
        // Next start: it goes through.
        assert!(migrate_legacy(&store, origin, "42", || Ok(())).unwrap());
        assert!(store.get(origin).unwrap().is_none());
        let moved = load_session(&store, &entry_name(origin, "42"))
            .unwrap()
            .unwrap();
        assert_eq!(moved.token.expose_secret(), "secret");
    }

    #[test]
    fn the_account_list_round_trips_and_a_bad_file_reads_empty() {
        let dir = std::env::temp_dir().join(format!("inlet-list-{}", std::process::id()));
        let path = dir.join("accounts.json");
        let list = AccountList {
            accounts: vec![AccountRecord {
                origin: "https://fluxer.example".into(),
                user_id: "42".into(),
                name: "Ada".into(),
                handle: "ada".into(),
            }],
            active: Some(entry_name("https://fluxer.example", "42")),
        };
        save_list(&path, &list).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("token"), "no secret in the list");
        assert_eq!(load_list(&path), list);
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(load_list(&path), AccountList::default());
        let _ = std::fs::remove_dir_all(dir);
    }
}
