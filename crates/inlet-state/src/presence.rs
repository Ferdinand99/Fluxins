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

/// Longest custom status text: 128 UTF-16 units, as Fluxer counts it
/// (`string.length`).
pub const CUSTOM_STATUS_TEXT_LIMIT: usize = 128;

/// `text` cut to at most `limit` UTF-16 units, at a character boundary.
pub fn truncate_utf16(text: &str, limit: usize) -> &str {
    let mut units = 0;
    for (at, c) in text.char_indices() {
        units += c.len_utf16();
        if units > limit {
            return &text[..at];
        }
    }
    text
}

/// The account's own status: what its settings say (from the server), and a
/// change picked here that waits for the server's answer. The pick shows until
/// the write is confirmed ([`OwnStatus::confirm`]) or failed
/// ([`OwnStatus::drop_pending`]); an echo of an older write does not replace it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OwnStatus {
    /// `online`, `idle`, `dnd` or `invisible` as saved; anything else reads as `online`.
    status: String,
    /// The saved custom status, if any (it may have expired; see [`OwnStatus::custom_at`]).
    custom: Option<CustomStatus>,
    /// A status picked here, not confirmed yet.
    pending_status: Option<String>,
    /// A custom status picked here (`Some(None)`: cleared), not confirmed yet.
    pending_custom: Option<Option<CustomStatus>>,
}

impl OwnStatus {
    /// The status shown: `online`, `idle`, `dnd` or `invisible`.
    pub fn status(&self) -> &str {
        normalize(self.pending_status.as_deref().unwrap_or(&self.status))
    }

    /// The status the server confirmed last.
    pub fn saved_status(&self) -> &str {
        normalize(&self.status)
    }

    /// The custom status shown, expired or not.
    pub fn custom(&self) -> Option<&CustomStatus> {
        match &self.pending_custom {
            Some(c) => c.as_ref(),
            None => self.custom.as_ref(),
        }
    }

    /// The custom status at `now_ms`, or `None` when there is none, it has no
    /// text or emoji, or it expired.
    pub fn custom_at(&self, now_ms: i64) -> Option<&CustomStatus> {
        self.custom().filter(|c| custom_shown_at(c, now_ms))
    }

    /// When the custom status shown expires (Unix milliseconds), if it does.
    pub fn custom_expires_ms(&self) -> Option<i64> {
        self.custom()
            .and_then(|c| c.expires_at.as_deref())
            .and_then(parse_timestamp_ms)
    }

    /// Whether a pick waits for the server.
    pub fn is_pending(&self) -> bool {
        self.pending_status.is_some() || self.pending_custom.is_some()
    }

    fn shown(&self) -> (String, Option<CustomStatus>) {
        (self.status().to_owned(), self.custom().cloned())
    }

    /// A pick made here: the status (`Some`) and the custom status (`Some(custom)`,
    /// where `None` clears it) show at once; a `None` part stays. Returns whether
    /// what shows changed.
    pub(crate) fn set_parts(
        &mut self,
        status: Option<&str>,
        custom: Option<Option<CustomStatus>>,
    ) -> bool {
        let before = self.shown();
        if let Some(s) = status {
            self.pending_status = Some(normalize(s).to_owned());
        }
        if let Some(c) = custom {
            self.pending_custom = Some(c.map(|mut c| {
                crate::size::cap_custom_status(&mut c);
                c
            }));
        }
        before != self.shown()
    }

    /// Takes the status and custom status from the settings (the server's). A
    /// settings object without a status (a partial update) keeps the current
    /// one. A pick that waits still shows. Returns whether what shows changed.
    pub(crate) fn update(&mut self, settings: &UserSettings) -> bool {
        let before = self.shown();
        if !settings.status.is_empty() {
            self.status = normalize(&settings.status).to_owned();
        }
        if !settings.status.is_empty() || settings.custom_status.is_some() {
            self.custom = settings.custom_status.clone().map(|mut c| {
                crate::size::cap_custom_status(&mut c);
                c
            });
        }
        before != self.shown()
    }

    /// The server took every pick: its answer is the saved status, and nothing
    /// waits any more.
    pub(crate) fn confirm(&mut self, settings: &UserSettings) -> bool {
        let before = self.shown();
        self.update(settings);
        self.pending_status = None;
        self.pending_custom = None;
        before != self.shown()
    }

    /// A write failed: the saved status shows again.
    pub(crate) fn drop_pending(&mut self) -> bool {
        let before = self.shown();
        self.pending_status = None;
        self.pending_custom = None;
        before != self.shown()
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
    let has_text = c.text.as_deref().is_some_and(|t| t.chars().any(is_visible));
    let has_emoji = c.emoji_id.is_some() || c.emoji_name.as_deref().is_some_and(|e| !e.is_empty());
    let live = match c.expires_at.as_deref() {
        None => true,
        Some(at) => parse_timestamp_ms(at).is_some_and(|ms| ms > now_ms),
    };
    (has_text || has_emoji) && live
}

/// Whether a character draws something (not a space, a control or a format
/// character such as a zero-width space or a direction mark).
pub fn is_visible(c: char) -> bool {
    !c.is_whitespace()
        && !c.is_control()
        && !matches!(c,
            '\u{00AD}' | '\u{034F}' | '\u{061C}' | '\u{115F}' | '\u{1160}' | '\u{17B4}'
            | '\u{17B5}' | '\u{180B}'..='\u{180F}' | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{206F}' | '\u{3164}'
            | '\u{FE00}'..='\u{FE0F}' | '\u{FEFF}' | '\u{FFA0}' | '\u{E0000}'..='\u{E007F}')
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

    /// The server took the picks (see [`OwnStatus::confirm`]).
    pub(crate) fn confirm_own(&mut self, settings: &UserSettings) -> bool {
        self.own.confirm(settings)
    }

    /// A write failed (see [`OwnStatus::drop_pending`]).
    pub(crate) fn drop_own_pending(&mut self) -> bool {
        self.own.drop_pending()
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
