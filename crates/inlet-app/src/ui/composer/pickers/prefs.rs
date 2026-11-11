// SPDX-License-Identifier: AGPL-3.0-or-later
//! The pickers' share of the synced preferences, as the UI shows them: favourite and
//! recently used emoji, the skin tone and favourite GIFs.
//!
//! The UI keeps the snapshot as the server stored it and shows it with the edits that
//! have not come back yet applied on top, so a click shows at once. Each edit goes to
//! the backend with the stored snapshot as its base; the protocol's single writer per
//! account batches them, merges and retries on a conflict. A failed edit drops out of
//! what is shown.

use std::collections::VecDeque;

use inlet_protocol as p;
use inlet_protocol::api::preferences::Preferences;
use inlet_protocol::proto::preferences::FavoriteGifEntry;

use crate::backend::composer::gif::{self, Cmd, PrefEdit};

/// Recently used emoji shown (three rows).
pub const MAX_RECENT: usize = 27;

#[derive(Default)]
pub struct PickerPrefs {
    /// The snapshot as the server stored it; `None` until READY, an update or a load
    /// brought it.
    stored: Option<p::SyncedPreferences>,
    /// `stored` with the edits on their way applied.
    shown: Preferences,
    /// Edits sent and not answered yet, oldest first.
    pending: VecDeque<PrefEdit>,
    /// A load was asked for.
    pub asked: bool,
}

impl PickerPrefs {
    /// Whether a snapshot is known.
    pub fn known(&self) -> bool {
        self.stored.is_some()
    }

    /// A snapshot from READY, USER_SETTINGS_UPDATE or a load.
    pub fn set_stored(&mut self, stored: p::SyncedPreferences) {
        self.stored = Some(stored);
        self.recompute();
    }

    /// The answer to the oldest edit: the snapshot stored after it, or `None` when it
    /// failed.
    pub fn edited(&mut self, stored: Option<p::SyncedPreferences>) {
        self.pending.pop_front();
        if let Some(s) = stored {
            self.stored = Some(s);
        }
        self.recompute();
    }

    fn recompute(&mut self) {
        self.shown = self
            .stored
            .as_ref()
            .map(|s| s.get().clone())
            .unwrap_or_default();
        for e in &self.pending {
            e.apply(&mut self.shown);
        }
    }

    /// Shows `edit` at once and gives the command that stores it.
    pub fn edit(&mut self, edit: PrefEdit) -> Cmd {
        edit.apply(&mut self.shown);
        self.pending.push_back(edit.clone());
        Cmd::EditPreferences {
            base: Box::new(self.stored.clone().unwrap_or_default()),
            edit,
        }
    }

    /// The favourite emoji keys, in the order they were added.
    pub fn favourites(&self) -> &[String] {
        self.shown
            .emoji_picker
            .as_ref()
            .map_or(&[], |s| s.favorite_emoji_ids.as_slice())
    }

    pub fn is_favourite(&self, key: &str) -> bool {
        self.favourites().iter().any(|k| k == key)
    }

    /// The most used emoji keys, best first (Fluxer's frecency).
    pub fn recent(&self, now_ms: i64, limit: usize) -> Vec<String> {
        let Some(state) = &self.shown.emoji_picker else {
            return Vec::new();
        };
        gif::ranked(
            state.usage.iter().filter(|(k, _)| gif::is_emoji_key(k)),
            now_ms,
        )
        .into_iter()
        .take(limit)
        .cloned()
        .collect()
    }

    /// The skin tone; the default for a value Fluxer would not write.
    pub fn skin_tone(&self) -> inlet_emoji::SkinTone {
        let stored = self
            .shown
            .emoji
            .as_ref()
            .map_or("", |e| e.skin_tone.as_str());
        inlet_emoji::SkinTone::TONES
            .into_iter()
            .find(|t| t.modifier().is_some_and(|m| stored.chars().eq([m])))
            .unwrap_or_default()
    }

    /// The favourite GIFs, oldest first.
    pub fn favourite_gifs(&self) -> &[FavoriteGifEntry] {
        self.shown
            .favorite_gifs
            .as_ref()
            .map_or(&[], |g| g.entries.as_slice())
    }

    pub fn is_favourite_gif(&self, url: &str) -> bool {
        self.favourite_gifs().iter().any(|e| e.url == url)
    }
}

/// The usage key of a Unicode emoji (`unicode:<name>`).
pub fn unicode_key(emoji: &inlet_emoji::Emoji) -> String {
    format!("{}{}", gif::UNICODE_PREFIX, emoji.name())
}

/// The usage key of a custom emoji (`custom:<guild>:<id>`).
pub fn custom_key(guild: &str, id: &str) -> String {
    format!("{}{guild}:{id}", gif::CUSTOM_PREFIX)
}

/// A usage key taken apart: a Unicode emoji, or a custom one by guild and ID.
#[derive(Debug, PartialEq, Eq)]
pub enum Key<'a> {
    Unicode(&'static inlet_emoji::Emoji),
    Custom { guild: &'a str, id: &'a str },
}

pub fn parse_key(key: &str) -> Option<Key<'_>> {
    if let Some(name) = key.strip_prefix(gif::UNICODE_PREFIX) {
        return inlet_emoji::by_shortcode(name).map(Key::Unicode);
    }
    let rest = key.strip_prefix(gif::CUSTOM_PREFIX)?;
    let (guild, id) = rest.rsplit_once(':')?;
    let snowflake =
        |s: &str| !s.is_empty() && s.len() <= 20 && s.bytes().all(|b| b.is_ascii_digit());
    (snowflake(id) && (guild.is_empty() || snowflake(guild))).then_some(Key::Custom { guild, id })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_fluxers() {
        let smile = inlet_emoji::by_shortcode("smile").unwrap();
        assert_eq!(unicode_key(smile), "unicode:smile");
        assert_eq!(parse_key("unicode:smile"), Some(Key::Unicode(smile)));
        assert_eq!(custom_key("1", "2"), "custom:1:2");
        assert_eq!(
            parse_key("custom:1:2"),
            Some(Key::Custom {
                guild: "1",
                id: "2"
            })
        );
        for bad in [
            "custom:1:x",
            "custom::",
            "unicode:no_such_emoji",
            "smile",
            "custom:a:1",
        ] {
            assert_eq!(parse_key(bad), None, "{bad}");
        }
    }

    #[test]
    fn edits_show_at_once_and_a_failed_one_drops_out() {
        let mut prefs = PickerPrefs::default();
        prefs.set_stored(p::SyncedPreferences::empty());
        let cmd = prefs.edit(PrefEdit::FavouriteEmoji {
            key: "unicode:wave".into(),
            on: true,
        });
        assert!(matches!(cmd, Cmd::EditPreferences { .. }));
        assert!(prefs.is_favourite("unicode:wave"));
        prefs.edit(PrefEdit::UsedEmoji {
            key: "unicode:smile".into(),
            at_ms: 1_000,
        });
        assert_eq!(prefs.recent(2_000, MAX_RECENT), ["unicode:smile"]);
        // The first fails: only the second stays shown.
        prefs.edited(None);
        assert!(!prefs.is_favourite("unicode:wave"));
        assert_eq!(prefs.recent(2_000, MAX_RECENT), ["unicode:smile"]);
        prefs.edited(None);
        assert!(prefs.recent(2_000, MAX_RECENT).is_empty());
        // Skin tones: only Fluxer's values.
        prefs.edit(PrefEdit::SkinTone("\u{1F3FE}".into()));
        assert_eq!(prefs.skin_tone(), inlet_emoji::SkinTone::MediumDark);
    }
}
