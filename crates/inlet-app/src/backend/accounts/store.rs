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
    list: AccountList,
    /// Each listed account still in the keychain; `None` when the keychain could not be
    /// read (the account stays listed and is tried at the next start).
    sessions: Vec<(AccountRecord, Option<StoredSession>)>,
    /// Some listed account has no usable entry any more.
    dropped: bool,
    legacy: Option<Legacy>,
}

/// The stored accounts. See the module documentation.
pub struct Accounts {
    store: Arc<dyn SecretStore>,
    /// `accounts.json`; `None` when there is no config folder (nothing is listed then).
    list_path: Option<PathBuf>,
    list: Vec<AccountInfo>,
    active: Option<AccountKey>,
    restored: Vec<Restored>,
    legacy: Option<Legacy>,
    /// The live session of each account, to end it on the server at sign-out without a
    /// new discovery. Only the backend's own sessions are kept here.
    sessions: HashMap<AccountKey, p::Session>,
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
            list: Vec::new(),
            active: None,
            restored: Vec::new(),
            legacy: None,
            sessions: HashMap::new(),
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

    /// Reads the account list and every account's keychain entry. Accounts whose entry
    /// is gone or was written before pinning are dropped from the list (their entry is
    /// removed). With no list yet, the single entry of an older Inlet is picked up as
    /// [`Legacy`]; it is moved once its user ID is known ([`Accounts::migrate`]).
    /// Afterwards `take_restored` gives the accounts to start, the active one first.
    pub async fn restore_all(&mut self) {
        let store = self.store.clone();
        let path = self.list_path.clone();
        let last_instance = self.last_instance;
        let read =
            tokio::task::spawn_blocking(move || read_all(&*store, path.as_deref(), last_instance))
                .await
                .unwrap_or_else(|_| Read {
                    list: AccountList::default(),
                    sessions: Vec::new(),
                    dropped: false,
                    legacy: None,
                });
        self.list.clear();
        self.restored.clear();
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
                self.restored.push(Restored {
                    key,
                    token,
                    pins,
                    pasted,
                });
            }
        }
        self.active = read
            .list
            .active
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

    /// Keeps a live session, to end it at sign-out without a new discovery.
    pub fn remember_session(&mut self, k: AccountKey, session: p::Session) {
        self.sessions.insert(k, session);
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
        let old =
            tokio::task::spawn_blocking(move || -> Result<Option<(p::Token, p::SessionPins)>> {
                let old = token_store::load_session(&*store, &entry)
                    .ok()
                    .flatten()
                    .filter(|o| should_revoke(o.pasted) && o.token != token)
                    .and_then(|o| Some((o.token, o.pins?)));
                token_store::save_session(&*store, &entry, &token, &pins, pasted)?;
                if let Some(path) = &path {
                    token_store::save_list(path, &file)?;
                }
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
        self.sessions.insert(key.clone(), s.session);
        Ok(key)
    }

    /// Signs an account out: ends its token on the server when `revoke` is set and the
    /// token was not pasted (a pasted token is only forgotten), then removes its entry
    /// and takes it off the list. With `revoke` set, the person gets a notice when the
    /// server could not be told or the keychain kept the entry.
    pub async fn remove(&mut self, k: &AccountKey, revoke: bool) {
        let entry = k.entry();
        let store = self.store.clone();
        let stored = tokio::task::spawn_blocking(move || {
            token_store::load_session(&*store, &entry).ok().flatten()
        })
        .await
        .ok()
        .flatten();
        let live = self.sessions.remove(k);
        let pasted = stored.as_ref().is_some_and(|s| s.pasted);
        let mut told_server = false;
        if revoke && should_revoke(pasted) {
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
        let file = list_file_of(&self.list, self.active.as_ref());
        let deleted = tokio::task::spawn_blocking(move || {
            let deleted = token_store::delete_session(&*store, &entry).is_ok();
            if let Some(path) = &path {
                if let Err(err) = token_store::save_list(path, &file) {
                    log::warn!("could not write the account list: {err:#}");
                }
            }
            deleted
        })
        .await
        .unwrap_or(false);
        if let Some(events) = &self.events {
            if revoke && !should_revoke(pasted) {
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

    /// Writes `accounts.json`.
    pub async fn save(&self) {
        let Some(path) = self.list_path.clone() else {
            return;
        };
        let file = list_file_of(&self.list, self.active.as_ref());
        let saved = tokio::task::spawn_blocking(move || token_store::save_list(&path, &file)).await;
        if !matches!(saved, Ok(Ok(()))) {
            log::warn!("could not write the account list");
        }
    }

    /// Moves an older Inlet's entry for `origin` to the account's own entry and lists
    /// the account (see [`token_store::migrate_legacy`] for the order that keeps the
    /// token). Returns whether it moved; on `false` the old entry is kept for the next
    /// start.
    pub async fn migrate(&mut self, origin: &str, info: AccountInfo, session: p::Session) -> bool {
        let mut list = self.list.clone();
        list.retain(|a| a.key != info.key);
        list.insert(0, info.clone());
        let file = list_file_of(&list, Some(&info.key));
        let store = self.store.clone();
        let path = self.list_path.clone();
        let origin = origin.to_string();
        let user_id = info.key.user_id.to_string();
        let moved = tokio::task::spawn_blocking(move || {
            token_store::migrate_legacy(&*store, &origin, &user_id, || match &path {
                Some(path) => token_store::save_list(path, &file),
                None => Ok(()),
            })
        })
        .await;
        match moved {
            Ok(Ok(false)) => false,
            Ok(Ok(true)) => {
                self.list = list;
                self.active = Some(info.key.clone());
                self.sessions.insert(info.key, session);
                true
            }
            Ok(Err(err)) => {
                log::warn!("could not move the stored session to its account entry: {err:#}");
                false
            }
            Err(_) => false,
        }
    }

    /// Signs out the session an older Inlet stored for `origin` (no account list yet):
    /// ends it on the server when `revoke` is set and it was not pasted, then removes
    /// the entry. Notices as for [`Accounts::remove`].
    pub async fn forget_legacy(&mut self, origin: &str, revoke: bool) {
        let store = self.store.clone();
        let entry = origin.to_string();
        let stored = tokio::task::spawn_blocking(move || {
            token_store::load_session(&*store, &entry).ok().flatten()
        })
        .await
        .ok()
        .flatten();
        let pasted = stored.as_ref().is_some_and(|s| s.pasted);
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
            if revoke && !should_revoke(pasted) {
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
    let list = path.map(token_store::load_list).unwrap_or_default();
    let mut sessions = Vec::new();
    let mut dropped = false;
    for record in &list.accounts {
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
    let legacy = if list.accounts.is_empty() {
        last_instance().and_then(|origin| match token_store::load_session(store, &origin) {
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
                let _ = token_store::delete_session(store, &origin);
                None
            }
            Ok(None) => None,
            Err(err) => {
                log::warn!("keychain read failed: {err:#}");
                None
            }
        })
    } else {
        None
    };
    Read {
        list,
        sessions,
        dropped,
        legacy,
    }
}
