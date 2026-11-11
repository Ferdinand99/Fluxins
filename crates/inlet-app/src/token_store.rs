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

/// What [`migrate_legacy`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Migration {
    /// There was no old entry (already migrated).
    Nothing,
    /// Moved and listed; the old entry is gone.
    Moved,
    /// Moved and listed, but the old entry could not be deleted. It is a copy of the
    /// listed account's entry now, and the next start deletes it.
    MovedOldKept,
    /// The account's own entry already held another token (a newer sign-in): that one
    /// is kept and listed, and the old entry is dropped (not revoked).
    KeptNewer,
}

/// Moves the entry an older Inlet kept for `origin` (entry name = origin) to the
/// account's own entry `<origin>#<user_id>`, byte for byte.
///
/// Order, so a token is never lost or orphaned: write the new entry (unless it is
/// there already), read it back and compare, then call `listed` (which writes the
/// account list; an error stops here), and only then delete the old entry. If any step
/// up to the list fails, the old entry stays and the next start, which looks for it
/// whatever the list holds, tries again; repeating the copy is harmless. A failed
/// delete after the list was written is not an error: the account is listed.
pub fn migrate_legacy(
    store: &dyn SecretStore,
    origin: &str,
    user_id: &str,
    listed: impl FnOnce() -> Result<()>,
) -> Result<Migration> {
    let Some(old) = store.get(origin)? else {
        return Ok(Migration::Nothing);
    };
    let new = entry_name(origin, user_id);
    let newer = match store.get(&new)? {
        Some(existing) if *existing != *old => true,
        Some(_) => false,
        None => {
            store.set(&new, &old)?;
            match store.get(&new)? {
                Some(back) if *back == *old => false,
                _ => anyhow::bail!("the moved keychain entry did not read back the same"),
            }
        }
    };
    listed()?;
    if let Err(err) = store.delete(origin) {
        log::warn!("the old keychain entry could not be deleted yet: {err:#}");
        return Ok(Migration::MovedOldKept);
    }
    if newer {
        log::warn!("an old keychain entry was dropped; the newer entry of the account is kept");
        return Ok(Migration::KeptNewer);
    }
    Ok(Migration::Moved)
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

/// Reads the account list. A missing file is an empty list. A file that is there but
/// cannot be read or parsed is an error: it is copied to `accounts.json.bad`, and the
/// caller must not write a new list over it (the accounts it names would be lost
/// from view; the keychain cannot be listed on every system).
pub fn load_list(path: &Path) -> Result<AccountList> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(AccountList::default()),
        Err(err) => {
            backup_bad_list(path);
            return Err(err).context("read the account list");
        }
    };
    serde_json::from_str(&text).map_err(|err| {
        backup_bad_list(path);
        anyhow::anyhow!("the account list is not valid: {err}")
    })
}

fn backup_bad_list(path: &Path) {
    let bad = path.with_extension("json.bad");
    if let Err(err) = std::fs::copy(path, &bad) {
        log::warn!("the unreadable account list could not be backed up: {err}");
    }
}

/// Writes the account list: to a temporary file, synced to disk, then renamed over the
/// old one, so a crash or power cut leaves either the old list or the new one.
pub fn save_list(path: &Path, list: &AccountList) -> Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).context("create the config folder")?;
    }
    let json = serde_json::to_string_pretty(list).context("encode the account list")?;
    let tmp = path.with_extension("json.tmp");
    {
        let mut file = std::fs::File::create(&tmp).context("write the account list")?;
        file.write_all(json.as_bytes())
            .context("write the account list")?;
        file.sync_all().context("write the account list")?;
    }
    std::fs::rename(&tmp, path).context("replace the account list")?;
    #[cfg(unix)]
    if let Some(dir) = path.parent() {
        if let Ok(dir) = std::fs::File::open(dir) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
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

/// An in-memory [`SecretStore`] for tests. The `refuse_*` flags make the calls fail.
#[cfg(test)]
#[derive(Default)]
pub struct MemoryStore {
    pub entries: std::sync::Mutex<std::collections::HashMap<String, String>>,
    pub refuse_writes: std::sync::atomic::AtomicBool,
    pub refuse_reads: std::sync::atomic::AtomicBool,
    pub refuse_deletes: std::sync::atomic::AtomicBool,
}

#[cfg(test)]
impl MemoryStore {
    pub fn refuse(flag: &std::sync::atomic::AtomicBool, on: bool) {
        flag.store(on, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(test)]
impl SecretStore for MemoryStore {
    fn get(&self, name: &str) -> Result<Option<Zeroizing<String>>> {
        if self.refuse_reads.load(std::sync::atomic::Ordering::SeqCst) {
            anyhow::bail!("locked");
        }
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
        if self
            .refuse_deletes
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            anyhow::bail!("refused");
        }
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
        assert_eq!(
            migrate_legacy(&store, origin, "42", || Ok(())).unwrap(),
            Migration::Moved
        );
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
        assert_eq!(load_list(&path).unwrap(), list);
        // A broken file is an error (never read as an empty list) and is backed up.
        std::fs::write(&path, "not json").unwrap();
        assert!(load_list(&path).is_err());
        assert_eq!(
            std::fs::read_to_string(path.with_extension("json.bad")).unwrap(),
            "not json"
        );
        // A missing file is an empty list.
        std::fs::remove_file(&path).unwrap();
        assert_eq!(load_list(&path).unwrap(), AccountList::default());
        let _ = std::fs::remove_dir_all(dir);
    }
}
