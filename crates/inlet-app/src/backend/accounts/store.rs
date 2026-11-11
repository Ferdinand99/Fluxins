// SPDX-License-Identifier: AGPL-3.0-or-later
//! The signed-in accounts as they are stored: the keychain entries (one per account,
//! `<origin>#<user_id>`, with its token, pins and the pasted flag) and the list without
//! secrets in `accounts.json`. [`Accounts`] is the only place that writes either. The
//! backend keeps it behind an async mutex, so writes happen in order: a sign-out waits
//! for a token that is still being stored.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use inlet_protocol as p;
use inlet_state::UserId;

use super::{notice_forgotten, notice_sign_out, should_revoke, Ev};
use crate::backend::{session, Ui};
use crate::token_store::{self, AccountList, AccountRecord, SecretStore, StoredSession};

/// At most this many accounts are signed in at once.
pub const MAX_ACCOUNTS: usize = 10;

/// An instance origin (`https://fluxer.example`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Origin(String);

impl Origin {
    pub fn new(origin: impl Into<String>) -> Self {
        Origin(origin.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The host part, for showing (`fluxer.example`).
    pub fn host(&self) -> &str {
        let s = self.0.as_str();
        let s = s
            .strip_prefix("https://")
            .or_else(|| s.strip_prefix("http://"))
            .unwrap_or(s);
        s.trim_end_matches('/')
    }
}

impl std::fmt::Display for Origin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Which account: the instance and the user on it. Two accounts on one instance differ
/// by user; one user on two instances is two accounts.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AccountKey {
    pub origin: Origin,
    pub user_id: UserId,
}

impl AccountKey {
    pub fn new(origin: &str, user_id: &str) -> Option<Self> {
        Some(AccountKey {
            origin: Origin::new(origin),
            user_id: UserId::parse(user_id)?,
        })
    }

    /// The keychain entry name, `<origin>#<user_id>`.
    pub fn entry(&self) -> String {
        token_store::entry_name(self.origin.as_str(), &self.user_id.to_string())
    }

    fn of(record: &AccountRecord) -> Option<Self> {
        Self::new(&record.origin, &record.user_id)
    }
}

/// An account as the account switcher shows it (no secrets).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountInfo {
    pub key: AccountKey,
    /// Display name.
    pub name: String,
    /// Account name (`username`).
    pub handle: String,
}

impl AccountInfo {
    fn record(&self) -> AccountRecord {
        AccountRecord {
            origin: self.key.origin.as_str().to_string(),
            user_id: self.key.user_id.to_string(),
            name: self.name.clone(),
            handle: self.handle.clone(),
        }
    }
}

/// A sign-in the person confirmed: the checked session and whose it is.
pub struct SignedIn {
    pub session: p::Session,
    pub user_id: UserId,
    /// Display name.
    pub name: String,
    /// Account name.
    pub handle: String,
}

/// A stored account to start: its key, token and pins.
pub struct Restored {
    pub key: AccountKey,
    pub token: p::Token,
    pub pins: p::SessionPins,
    pub pasted: bool,
}

/// The entry an older Inlet kept for one instance (named after the origin).
pub struct Legacy {
    pub origin: String,
    pub token: p::Token,
    pub pins: p::SessionPins,
    pub pasted: bool,
}

/// What `restore_all` read, before it is handed to the backend.
struct Read {
    /// `None`: `accounts.json` is there but could not be read (it was backed up).
    list: Option<AccountList>,
    /// Each listed account still in the keychain; `None` when the keychain could not be
    /// read (the account stays listed and is tried at the next start).
    sessions: Vec<(AccountRecord, Option<StoredSession>)>,
    /// Some listed account has no usable entry any more.
    dropped: bool,
    legacy: Option<Legacy>,
    /// An older Inlet's entry without pins was dropped (it cannot be used safely).
    pinless_legacy: bool,
}

/// The stored accounts. See the module documentation.
pub struct Accounts {
    store: Arc<dyn SecretStore>,
    /// `accounts.json`; `None` when there is no config folder (nothing is listed then).
    list_path: Option<PathBuf>,
    /// `accounts.json` could not be read this run: it is never written over.
    list_broken: bool,
    list: Vec<AccountInfo>,
    active: Option<AccountKey>,
    restored: Vec<Restored>,
    legacy: Option<Legacy>,
    /// The live session of each account, to end it on the server at sign-out without a
    /// new discovery. Only the backend's own sessions are kept here.
    sessions: HashMap<AccountKey, p::Session>,
    /// Whether each account's token was pasted, as last read or written. Used when the
    /// keychain cannot tell at sign-out (an entry not moved yet); unknown counts as
    /// pasted, so it is never revoked.
    pasted: HashMap<AccountKey, bool>,
    /// Where notices after a sign-out go (`None` in tests).
    events: Option<Ui>,
    /// The origin an older Inlet signed in to (its entry is named after it).
    last_instance: fn() -> Option<String>,
}

impl Accounts {
    pub fn new(
        store: Arc<dyn SecretStore>,
        list_path: Option<PathBuf>,
        events: Option<Ui>,
    ) -> Self {
        Accounts {
            store,
            list_path,
            list_broken: false,
            list: Vec::new(),
            active: None,
            restored: Vec::new(),
            legacy: None,
            sessions: HashMap::new(),
            pasted: HashMap::new(),
            events,
            last_instance: || None,
        }
    }

    /// Where an older Inlet's entry is looked for (the system default reads the
    /// last-instance file).
    pub fn with_last_instance(mut self, f: fn() -> Option<String>) -> Self {
        self.last_instance = f;
        self
    }

    /// The OS keychain and the config folder.
    pub fn system(events: Ui) -> Self {
        Self::new(
            Arc::new(token_store::Keychain),
            token_store::list_file(),
            Some(events),
        )
        .with_last_instance(token_store::last_instance)
    }

    fn notice(&self, text: String) {
        if let Some(events) = &self.events {
            let _ = events.unbounded_send(Ev::Notice(text));
        }
    }

    /// Reads the account list and every account's keychain entry. Accounts whose entry
    /// is gone or was written before pinning are dropped from the list (their entry is
    /// removed). The single entry of an older Inlet is looked for on every start until
    /// it has been moved: a copy of a listed account's entry is deleted, anything else
    /// is picked up as [`Legacy`] and moved once its user ID is known
    /// ([`Accounts::migrate`]). Afterwards `take_restored` gives the accounts to start,
    /// the active one first.
    pub async fn restore_all(&mut self) {
        let store = self.store.clone();
        let path = self.list_path.clone();
        let last_instance = self.last_instance;
        let read =
            tokio::task::spawn_blocking(move || read_all(&*store, path.as_deref(), last_instance))
                .await
                .unwrap_or_else(|_| Read {
                    list: None,
                    sessions: Vec::new(),
                    dropped: false,
                    legacy: None,
                    pinless_legacy: false,
                });
        self.list.clear();
        self.restored.clear();
        self.list_broken = read.list.is_none();
        if self.list_broken {
            self.notice(t!("notice-account-list-unreadable"));
        }
        if read.pinless_legacy {
            self.notice(t!("notice-old-session-dropped"));
        }
        for (record, stored) in read.sessions {
            let Some(key) = AccountKey::of(&record) else {
                continue;
            };
            if self.list.len() >= MAX_ACCOUNTS {
                break;
            }
            self.list.push(AccountInfo {
                key: key.clone(),
                name: record.name,
                handle: record.handle,
            });
            if let Some(StoredSession {
                token,
                pins: Some(pins),
                pasted,
            }) = stored
            {
                self.pasted.insert(key.clone(), pasted);
                self.restored.push(Restored {
                    key,
                    token,
                    pins,
                    pasted,
                });
            }
        }
        let active = read.list.as_ref().and_then(|l| l.active.clone());
        self.active = active
            .as_deref()
            .and_then(|e| self.list.iter().find(|a| a.key.entry() == e))
            .or_else(|| self.list.first())
            .map(|a| a.key.clone());
        if let Some(active) = &self.active {
            if let Some(ix) = self.restored.iter().position(|r| &r.key == active) {
                let first = self.restored.remove(ix);
                self.restored.insert(0, first);
            }
        }
        self.legacy = read.legacy;
        if read.dropped {
            // Some listed account could not be restored: write the list without it.
            self.save().await;
        }
    }

    /// The accounts to start (the active one first) and an older Inlet's entry, once.
    pub fn take_restored(&mut self) -> (Vec<Restored>, Option<Legacy>) {
        (std::mem::take(&mut self.restored), self.legacy.take())
    }

    pub fn list(&self) -> Vec<AccountInfo> {
        self.list.clone()
    }

    pub fn active(&self) -> Option<&AccountKey> {
        self.active.as_ref()
    }

    pub fn contains(&self, k: &AccountKey) -> bool {
        self.list.iter().any(|a| &a.key == k)
    }

    /// A stored account's token and pins, to start it.
    pub async fn load(&self, k: &AccountKey) -> Option<Restored> {
        let store = self.store.clone();
        let entry = k.entry();
        let stored = tokio::task::spawn_blocking(move || {
            token_store::load_session(&*store, &entry).ok().flatten()
        })
        .await
        .ok()
        .flatten()?;
        Some(Restored {
            key: k.clone(),
            token: stored.token,
            pins: stored.pins?,
            pasted: stored.pasted,
        })
    }

    /// Keeps a live session and whether its token was pasted, to end it at sign-out
    /// without a new discovery (or only forget it).
    pub fn remember_session(&mut self, k: AccountKey, session: p::Session, pasted: bool) {
        self.pasted.insert(k.clone(), pasted);
        self.sessions.insert(k, session);
    }

    /// Writes `file` to `accounts.json`, unless the list on disk could not be read this
    /// run (then it is kept as it is).
    fn write_list(path: Option<&std::path::Path>, broken: bool, file: &AccountList) -> Result<()> {
        match path {
            Some(_) if broken => {
                log::warn!("the account list is not written over: it could not be read");
                Ok(())
            }
            Some(path) => token_store::save_list(path, file),
            None => Ok(()),
        }
    }

    /// Stores a confirmed sign-in under its own entry and lists it as the active
    /// account. Signing in again to an account that is already here replaces its token;
    /// the old token is ended on the server unless it was pasted (then it is only
    /// forgotten).
    pub async fn add(&mut self, s: SignedIn, pasted: bool) -> Result<AccountKey> {
        let origin = s.session.instance().origin().to_string();
        let key = AccountKey {
            origin: Origin::new(origin.clone()),
            user_id: s.user_id,
        };
        if !self.contains(&key) && self.list.len() >= MAX_ACCOUNTS {
            anyhow::bail!("too many accounts");
        }
        let info = AccountInfo {
            key: key.clone(),
            name: s.name.clone(),
            handle: s.handle.clone(),
        };
        let mut list = self.list.clone();
        match list.iter_mut().find(|a| a.key == key) {
            Some(existing) => *existing = info,
            None => list.push(info),
        }
        let file = list_file_of(&list, Some(&key));
        let token = s.session.token().clone();
        let pins = s.session.session_pins();
        let entry = key.entry();
        let store = self.store.clone();
        let path = self.list_path.clone();
        let broken = self.list_broken;
        let old =
            tokio::task::spawn_blocking(move || -> Result<Option<(p::Token, p::SessionPins)>> {
                let old = token_store::load_session(&*store, &entry)
                    .ok()
                    .flatten()
                    .filter(|o| should_revoke(o.pasted) && o.token != token)
                    .and_then(|o| Some((o.token, o.pins?)));
                token_store::save_session(&*store, &entry, &token, &pins, pasted)?;
                Self::write_list(path.as_deref(), broken, &file)?;
                token_store::set_last_instance(&origin);
                Ok(old)
            })
            .await
            .context("store the account")??;
        self.list = list;
        self.active = Some(key.clone());
        // An older token of this account would otherwise live on forever.
        if let Some((old, old_pins)) = old {
            if let Ok(old_session) =
                p::Session::from_token_session_pinned(s.session.instance().clone(), old, &old_pins)
            {
                session::revoke(&old_session).await;
            }
        }
        self.pasted.insert(key.clone(), pasted);
        self.sessions.insert(key.clone(), s.session);
        Ok(key)
    }

    /// Signs an account out: ends its token on the server when `revoke` is set and the
    /// token is known not to be pasted, then removes its entry and takes it off the
    /// list. A pasted token is only forgotten. When the keychain cannot be read, nothing
    /// is revoked (the token might be pasted): the account is forgotten here and the
    /// person is told to end the session elsewhere.
    pub async fn remove(&mut self, k: &AccountKey, revoke: bool) {
        let entry = k.entry();
        let store = self.store.clone();
        let read = tokio::task::spawn_blocking(move || token_store::load_session(&*store, &entry))
            .await
            .unwrap_or_else(|_| Err(anyhow::anyhow!("the keychain read stopped")));
        let live = self.sessions.remove(k);
        let known = self.pasted.remove(k);
        let (pasted, unreadable, stored) = match read {
            Ok(Some(s)) => (s.pasted, false, Some(s)),
            // No entry (an older Inlet's entry not moved yet): what was last known, and
            // unknown counts as pasted.
            Ok(None) => (known.unwrap_or(true), false, None),
            Err(err) => {
                log::warn!("the keychain could not be read at sign-out: {err:#}");
                (true, true, None)
            }
        };
        let mut told_server = false;
        if revoke && !unreadable && should_revoke(pasted) {
            let session = match (live, stored) {
                (Some(session), _) => Some(session),
                (
                    None,
                    Some(StoredSession {
                        token,
                        pins: Some(pins),
                        ..
                    }),
                ) => match session::discover(k.origin.as_str()).await {
                    Ok(inst) => p::Session::from_token_session_pinned(inst, token, &pins).ok(),
                    Err(_) => None,
                },
                _ => None,
            };
            if let Some(session) = session {
                told_server = session::revoke(&session).await;
            }
        }
        self.list.retain(|a| &a.key != k);
        if self.active.as_ref() == Some(k) {
            self.active = self.list.first().map(|a| a.key.clone());
        }
        let entry = k.entry();
        let store = self.store.clone();
        let path = self.list_path.clone();
        let broken = self.list_broken;
        let file = list_file_of(&self.list, self.active.as_ref());
        let deleted = tokio::task::spawn_blocking(move || {
            let deleted = token_store::delete_session(&*store, &entry).is_ok();
            if let Err(err) = Self::write_list(path.as_deref(), broken, &file) {
                log::warn!("could not write the account list: {err:#}");
            }
            deleted
        })
        .await
        .unwrap_or(false);
        if let Some(events) = &self.events {
            if revoke && unreadable {
                let mut text = t!("notice-keychain-unreadable");
                if !deleted {
                    text.push(' ');
                    text.push_str(&t!("notice-not-deleted"));
                }
                let _ = events.unbounded_send(Ev::Notice(text));
            } else if revoke && !should_revoke(pasted) {
                notice_forgotten(events, deleted);
            } else if revoke {
                notice_sign_out(events, told_server, deleted);
            } else if !deleted {
                let _ = events.unbounded_send(Ev::Notice(t!("notice-not-deleted")));
            }
        }
    }

    /// Makes `k` the account shown at the next start. Written by [`Accounts::save`].
    pub fn set_active(&mut self, k: &AccountKey) {
        if self.contains(k) {
            self.active = Some(k.clone());
        }
    }

    /// Writes `accounts.json` (never over a list that could not be read).
    pub async fn save(&self) {
        let path = self.list_path.clone();
        let broken = self.list_broken;
        let file = list_file_of(&self.list, self.active.as_ref());
        let saved =
            tokio::task::spawn_blocking(move || Self::write_list(path.as_deref(), broken, &file))
                .await;
        if !matches!(saved, Ok(Ok(()))) {
            log::warn!("could not write the account list");
        }
    }

    /// Moves an older Inlet's entry for `origin` to the account's own entry and lists
    /// the account (see [`token_store::migrate_legacy`] for the order that keeps the
    /// token and never orphans it). Returns whether the account is listed now; on
    /// `false` the old entry is kept and the next start tries again.
    pub async fn migrate(
        &mut self,
        origin: &str,
        info: AccountInfo,
        session: p::Session,
        pasted: bool,
    ) -> bool {
        self.remember_session(info.key.clone(), session, pasted);
        let mut list = self.list.clone();
        list.retain(|a| a.key != info.key);
        list.insert(0, info.clone());
        let file = list_file_of(&list, Some(&info.key));
        let store = self.store.clone();
        let path = self.list_path.clone();
        let broken = self.list_broken;
        let origin = origin.to_string();
        let user_id = info.key.user_id.to_string();
        let moved = tokio::task::spawn_blocking(move || {
            token_store::migrate_legacy(&*store, &origin, &user_id, || {
                if broken {
                    anyhow::bail!("the account list could not be read; not written over");
                }
                Self::write_list(path.as_deref(), false, &file)
            })
        })
        .await;
        match moved {
            Ok(Ok(token_store::Migration::Nothing)) => false,
            Ok(Ok(_)) => {
                // Listed on disk, so listed here too (also when the old entry could
                // not be deleted yet; the next start deletes the copy).
                self.list = list;
                self.active = Some(info.key);
                true
            }
            Ok(Err(err)) => {
                log::warn!("could not move the stored session to its account entry: {err:#}");
                false
            }
            Err(_) => false,
        }
    }

    /// Signs out the session an older Inlet stored for `origin`: ends it on the server
    /// when `revoke` is set and it is known not to be pasted, then removes the entry.
    /// Notices as for [`Accounts::remove`].
    pub async fn forget_legacy(&mut self, origin: &str, revoke: bool) {
        let store = self.store.clone();
        let entry = origin.to_string();
        let read = tokio::task::spawn_blocking(move || token_store::load_session(&*store, &entry))
            .await
            .unwrap_or_else(|_| Err(anyhow::anyhow!("the keychain read stopped")));
        let unreadable = read.is_err();
        let stored = read.ok().flatten();
        let pasted = stored.as_ref().is_none_or(|s| s.pasted);
        let mut told_server = false;
        if let (
            true,
            true,
            Some(StoredSession {
                token,
                pins: Some(pins),
                ..
            }),
        ) = (revoke, should_revoke(pasted), stored)
        {
            if let Ok(inst) = session::discover(origin).await {
                if let Ok(session) = p::Session::from_token_session_pinned(inst, token, &pins) {
                    told_server = session::revoke(&session).await;
                }
            }
        }
        let store = self.store.clone();
        let entry = origin.to_string();
        let deleted = tokio::task::spawn_blocking(move || {
            token_store::delete_session(&*store, &entry).is_ok()
        })
        .await
        .unwrap_or(false);
        if let Some(events) = &self.events {
            if revoke && unreadable {
                let _ = events.unbounded_send(Ev::Notice(t!("notice-keychain-unreadable")));
            } else if revoke && !should_revoke(pasted) {
                notice_forgotten(events, deleted);
            } else if revoke {
                notice_sign_out(events, told_server, deleted);
            }
        }
    }

    /// Writes new pins for a stored account (the media host of an entry written before
    /// it existed).
    pub async fn update_pins(
        &mut self,
        k: &AccountKey,
        token: p::Token,
        pins: p::SessionPins,
        pasted: bool,
    ) {
        let entry = k.entry();
        let store = self.store.clone();
        let saved = tokio::task::spawn_blocking(move || {
            token_store::save_session(&*store, &entry, &token, &pins, pasted)
        })
        .await;
        if !matches!(saved, Ok(Ok(()))) {
            log::warn!("could not store the media pin with the session");
        }
    }
}

fn list_file_of(list: &[AccountInfo], active: Option<&AccountKey>) -> AccountList {
    AccountList {
        accounts: list.iter().map(AccountInfo::record).collect(),
        active: active.map(AccountKey::entry),
    }
}

fn read_all(
    store: &dyn SecretStore,
    path: Option<&std::path::Path>,
    last_instance: fn() -> Option<String>,
) -> Read {
    let list = match path.map(token_store::load_list) {
        None => Some(AccountList::default()),
        Some(Ok(list)) => Some(list),
        Some(Err(err)) => {
            log::warn!("{err:#}");
            None
        }
    };
    let mut sessions = Vec::new();
    let mut dropped = false;
    for record in list.iter().flat_map(|l| &l.accounts) {
        let entry = record.entry();
        match token_store::load_session(store, &entry) {
            Ok(Some(stored)) if stored.pins.is_some() => {
                sessions.push((record.clone(), Some(stored)))
            }
            Ok(Some(_)) => {
                // Written before endpoint pinning: never start without pins.
                let _ = token_store::delete_session(store, &entry);
                dropped = true;
            }
            Ok(None) => {
                log::warn!("a listed account has no keychain entry; dropped");
                dropped = true;
            }
            Err(err) => {
                // Kept on the list: the keychain may be locked right now.
                log::warn!("keychain read failed: {err:#}");
                sessions.push((record.clone(), None));
            }
        }
    }
    let mut pinless_legacy = false;
    // An older Inlet's entry is looked for on every start until it is moved and gone.
    let legacy = last_instance().and_then(|origin| {
        let raw = match store.get(&origin) {
            Ok(Some(raw)) => raw,
            Ok(None) => return None,
            Err(err) => {
                log::warn!("keychain read failed: {err:#}");
                return None;
            }
        };
        // Already moved (the delete failed or the app stopped before it): the listed
        // account of this origin holds the same bytes, so the old copy goes.
        let moved = list
            .iter()
            .flat_map(|l| &l.accounts)
            .filter(|r| r.origin == origin)
            .any(|r| matches!(store.get(&r.entry()), Ok(Some(e)) if *e == *raw));
        if moved {
            if let Err(err) = store.delete(&origin) {
                log::warn!("the old keychain entry could not be deleted yet: {err:#}");
            }
            return None;
        }
        match token_store::load_session(store, &origin) {
            Ok(Some(StoredSession {
                token,
                pins: Some(pins),
                pasted,
            })) => Some(Legacy {
                origin,
                token,
                pins,
                pasted,
            }),
            Ok(Some(_)) => {
                // Written before endpoint pinning: never used, never revoked (that would
                // need discovery without pins). The person is told.
                let _ = token_store::delete_session(store, &origin);
                pinless_legacy = true;
                None
            }
            _ => None,
        }
    });
    Read {
        list,
        sessions,
        dropped,
        legacy,
        pinless_legacy,
    }
}
