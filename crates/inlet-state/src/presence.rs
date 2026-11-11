//! Presences: one per user, across guilds.
//!
//! Fluxer keeps a user's presence per user, not per guild, so the cache is
//! account-wide with one cap ([`crate::limits::MAX_PRESENCES`]). `offline`
//! is stored as "no entry", which keeps the cache to the people who are on.
//!
//! The account's own status is apart ([`OwnStatus`]): the one chosen in the
//! account settings and the custom status, from READY `user_settings` and
//! USER_SETTINGS_UPDATE.

use inlet_protocol::models::parse_timestamp_ms;
use inlet_protocol::models::CustomStatus;
use inlet_protocol::{Presence, UserSettings};

use crate::bounded::LruMap;
use crate::ids::UserId;
use crate::limits::MAX_PRESENCES;

/// The statuses a person can pick (Fluxer's order in the status menu).
pub const STATUSES: [&str; 4] = ["online", "idle", "dnd", "invisible"];

/// Longest custom status text (Fluxer's `CUSTOM_STATUS_TEXT_LIMIT`, in characters).
pub const CUSTOM_STATUS_TEXT_LIMIT: usize = 128;

/// The account's own status, as saved in its settings.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OwnStatus {
    /// `online`, `idle`, `dnd` or `invisible`; anything else reads as `online`.
    status: String,
    /// The custom status, if any (it may have expired; see [`OwnStatus::custom_at`]).
    custom: Option<CustomStatus>,
}

impl OwnStatus {
    /// The chosen status: `online`, `idle`, `dnd` or `invisible`.
    pub fn status(&self) -> &str {
        normalize(&self.status)
    }

    /// The custom status at `now_ms`, or `None` when there is none, it has no
    /// text or emoji, or it expired.
    pub fn custom_at(&self, now_ms: i64) -> Option<&CustomStatus> {
        self.custom.as_ref().filter(|c| custom_shown_at(c, now_ms))
    }

    /// When the custom status expires (Unix milliseconds), if it does.
    pub fn custom_expires_ms(&self) -> Option<i64> {
        self.custom
            .as_ref()
            .and_then(|c| c.expires_at.as_deref())
            .and_then(parse_timestamp_ms)
    }

    /// Sets the status (`Some`) and the custom status (`Some(custom)`, where `None`
    /// clears it); a `None` part stays. Returns whether anything changed.
    pub(crate) fn set_parts(
        &mut self,
        status: Option<&str>,
        custom: Option<Option<CustomStatus>>,
    ) -> bool {
        let mut next = self.clone();
        if let Some(s) = status {
            next.status = normalize(s).to_owned();
        }
        if let Some(c) = custom {
            next.custom = c.map(|mut c| {
                crate::size::cap_custom_status(&mut c);
                c
            });
        }
        if next == *self {
            return false;
        }
        *self = next;
        true
    }

    /// The custom status as stored, expired or not.
    pub fn custom(&self) -> Option<&CustomStatus> {
        self.custom.as_ref()
    }

    /// Takes the status and custom status from the settings. A settings object
    /// without a status (a partial update) keeps the current one. Returns
    /// whether anything changed.
    pub(crate) fn update(&mut self, settings: &UserSettings) -> bool {
        let mut next = self.clone();
        if !settings.status.is_empty() {
            next.status = normalize(&settings.status).to_owned();
        }
        if !settings.status.is_empty() || settings.custom_status.is_some() {
            next.custom = settings.custom_status.clone().map(|mut c| {
                crate::size::cap_custom_status(&mut c);
                c
            });
        }
        if next == *self {
            return false;
        }
        *self = next;
        true
    }
}

/// A status Fluxer knows, or `online`.
pub fn normalize(status: &str) -> &str {
    match status {
        "idle" => "idle",
        "dnd" => "dnd",
        "invisible" | "offline" => "invisible",
        _ => "online",
    }
}

/// Whether a custom status shows at `now_ms`: it has text or an emoji and did
/// not expire. An expiry that cannot be read counts as expired (fail closed:
/// nothing old is shown).
pub fn custom_shown_at(c: &CustomStatus, now_ms: i64) -> bool {
    let has_text = c.text.as_deref().is_some_and(|t| !t.trim().is_empty());
    let has_emoji = c.emoji_id.is_some() || c.emoji_name.as_deref().is_some_and(|e| !e.is_empty());
    let live = match c.expires_at.as_deref() {
        None => true,
        Some(at) => parse_timestamp_ms(at).is_some_and(|ms| ms > now_ms),
    };
    (has_text || has_emoji) && live
}

/// A bounded presence cache.
#[derive(Debug, Clone, PartialEq)]
pub struct Presences {
    by_user: LruMap<UserId, Presence>,
    own: OwnStatus,
}

impl Default for Presences {
    fn default() -> Self {
        Presences {
            by_user: LruMap::new(MAX_PRESENCES),
            own: OwnStatus::default(),
        }
    }
}

impl Presences {
    /// Applies one presence. Returns whether anything changed.
    pub(crate) fn upsert(&mut self, p: &Presence) -> bool {
        let Some(id) = p.user_id().and_then(UserId::parse) else {
            return false;
        };
        self.upsert_for(id, p)
    }

    /// Applies a presence that carries no user (member list items).
    pub(crate) fn upsert_for(&mut self, id: UserId, p: &Presence) -> bool {
        if p.status.is_empty() || p.status == "offline" || p.status == "invisible" {
            return self.by_user.remove(&id).is_some();
        }
        // Keep only what the UI shows; the user object lives in the user cache.
        let mut slim = Presence {
            user: None,
            guild_id: None,
            ..p.clone()
        };
        crate::size::cap_presence(&mut slim);
        if self.by_user.get(&id) == Some(&slim) {
            return false;
        }
        self.by_user.insert(id, slim);
        true
    }

    /// The account's own status from its settings. Returns whether it changed.
    pub(crate) fn set_own(&mut self, settings: &UserSettings) -> bool {
        self.own.update(settings)
    }

    /// Changes parts of the account's own status (see [`OwnStatus::set_parts`]).
    pub(crate) fn set_own_parts(
        &mut self,
        status: Option<&str>,
        custom: Option<Option<CustomStatus>>,
    ) -> bool {
        self.own.set_parts(status, custom)
    }

    /// The account's own chosen status and custom status.
    pub fn own(&self) -> &OwnStatus {
        &self.own
    }

    /// A new cap (lower for an account in the background).
    pub(crate) fn set_cap(&mut self, cap: usize) {
        self.by_user.set_cap(cap);
    }

    /// A user's presence, or `None` when offline or unknown.
    pub fn get(&self, id: UserId) -> Option<&Presence> {
        self.by_user.get(&id)
    }

    /// A user's status: `online`, `idle`, `dnd`, or `offline` when unknown.
    pub fn status(&self, id: UserId) -> &str {
        self.get(id).map_or("offline", |p| p.status.as_str())
    }

    /// Number of stored presences.
    pub fn len(&self) -> usize {
        self.by_user.len()
    }

    /// Whether nobody is online.
    pub fn is_empty(&self) -> bool {
        self.by_user.len() == 0
    }
}
