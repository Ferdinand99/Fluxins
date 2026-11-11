// SPDX-License-Identifier: AGPL-3.0-or-later
//! User profiles: the popout beside the member list (banner, avatar, name, pronouns,
//! bio, roles, member since, the note), the full profile (with mutual guilds and
//! friends) and the user menu (right click). All three are layers over the chat
//! (`desktop::OpenLayers`): they take the keyboard, Escape closes the topmost first,
//! and nothing is marked read behind them.
//!
//! Profiles are fetched when one of them opens, unless a copy younger than
//! [`CACHE_FOR`] (five minutes) is at hand ([`ProfileCache`], at most
//! [`MAX_CACHED`] per account, emptied on an account switch). A profile shows the
//! open guild's profile of the person when they have one (Fluxer's per-guild
//! profile). See `backend::social::profile` for the request budget.

mod menu;
mod modal;
mod mutual;
mod popout;
mod roles;

use std::collections::VecDeque;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{AnyElement, AppContext as _, Context, Entity, Pixels, Point, Subscription, Window};
use inlet_state::{GuildId, UserId};

use crate::app::Inlet;
use crate::backend::social::profile::{Cmd, Ev, Profile};
use crate::text_input::{InputEvent, TextInput};

pub use menu::UserMenu;

/// How long a fetched profile is shown without asking again.
pub const CACHE_FOR: Duration = Duration::from_secs(5 * 60);
/// Profiles kept per account.
pub const MAX_CACHED: usize = 64;
/// Characters of a note (Fluxer allows 256).
pub const MAX_NOTE_CHARS: usize = 256;

/// Whose profile, in which guild (`None`: the account-wide profile).
pub type Key = (UserId, Option<GuildId>);

/// Where a popout was opened from: it opens beside that.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Opener {
    /// Left of the member list.
    MemberList,
}

/// The open popout.
#[derive(Clone, Debug, PartialEq)]
pub struct Popout {
    pub user: UserId,
    pub guild: Option<GuildId>,
    pub at: Point<Pixels>,
    pub opener: Opener,
}

/// The tabs of the full profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    About,
    Guilds,
    Friends,
}

/// The open full profile.
#[derive(Clone, Debug, PartialEq)]
pub struct Full {
    pub user: UserId,
    pub guild: Option<GuildId>,
    pub tab: Tab,
}

/// Fetched profiles, newest last, each for [`CACHE_FOR`].
#[derive(Default)]
pub struct ProfileCache {
    entries: VecDeque<(Key, Instant, Rc<Profile>)>,
}

impl ProfileCache {
    /// A profile fetched less than [`CACHE_FOR`] before `now`.
    pub fn fresh(&self, key: &Key, now: Instant) -> Option<Rc<Profile>> {
        self.entries
            .iter()
            .find(|(k, at, _)| k == key && now.saturating_duration_since(*at) < CACHE_FOR)
            .map(|(.., p)| p.clone())
    }

    /// The newest copy, however old (shown while a fresh one loads).
    pub fn any(&self, key: &Key) -> Option<Rc<Profile>> {
        self.entries
            .iter()
            .find(|(k, ..)| k == key)
            .map(|(.., p)| p.clone())
    }

    pub fn insert(&mut self, key: Key, profile: Profile, now: Instant) {
        self.entries.retain(|(k, ..)| *k != key);
        if self.entries.len() >= MAX_CACHED {
            self.entries.pop_front();
        }
        self.entries.push_back((key, now, Rc::new(profile)));
    }

    /// Updates the note of every cached copy of a user's profile.
    pub fn set_note(&mut self, user: UserId, note: &str) {
        for (k, _, p) in &mut self.entries {
            if k.0 == user {
                let mut copy = (**p).clone();
                copy.note = note.to_string();
                *p = Rc::new(copy);
            }
        }
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// The note field of the profile on screen.
pub struct NoteEditor {
    pub user: UserId,
    pub input: Entity<TextInput>,
    /// The text as last saved (or fetched).
    saved: String,
    _submit: Subscription,
}

/// The profile domain's view state.
#[derive(Default)]
pub struct State {
    pub popout: Option<Popout>,
    pub full: Option<Full>,
    pub menu: Option<UserMenu>,
    pub cache: ProfileCache,
    next_request: u64,
    /// The fetch on its way.
    in_flight: Option<(u64, Key)>,
    /// The last fetch that failed, and why.
    failed: Option<(Key, String)>,
    pub note: Option<NoteEditor>,
    /// Notes saved here, per person: the text written and the text before it. A
    /// profile fetched while the save was on its way shows the written text; a failed
    /// save puts the text before it back. At most [`MAX_CACHED`].
    written: VecDeque<(UserId, String, String)>,
    /// The window's width at the last frame, to place a popout beside the list.
    pub window_width: f32,
    #[cfg(feature = "demo")]
    pub demo_done: bool,
}

impl State {
    /// Whether the popout or the full profile shows `user`.
    pub fn shows(&self, user: UserId) -> bool {
        self.popout.as_ref().is_some_and(|p| p.user == user)
            || self.full.as_ref().is_some_and(|f| f.user == user)
    }

    /// The profile on screen (the full profile is over the popout).
    fn shown(&self) -> Option<Key> {
        self.full
            .as_ref()
            .map(|f| (f.user, f.guild))
            .or_else(|| self.popout.as_ref().map(|p| (p.user, p.guild)))
    }

    /// Sign-out or an account switch: everything was the old account's.
    pub fn reset(&mut self) {
        self.popout = None;
        self.full = None;
        self.menu = None;
        self.cache.clear();
        self.in_flight = None;
        self.failed = None;
        self.note = None;
        self.written.clear();
    }

    /// Whether `key` must be fetched now: no fresh copy, and not on its way already.
    pub fn needs_fetch(&self, key: &Key, now: Instant) -> bool {
        self.cache.fresh(key, now).is_none()
            && !self.in_flight.as_ref().is_some_and(|(_, k)| k == key)
    }
}

impl Inlet {
    /// Opens the popout of `user` (closing the user menu).
    pub(crate) fn open_profile_popout(
        &mut self,
        user: UserId,
        guild: Option<GuildId>,
        at: Point<Pixels>,
        opener: Opener,
        cx: &mut Context<Self>,
    ) {
        self.profile.menu = None;
        self.profile.popout = Some(Popout {
            user,
            guild,
            at,
            opener,
        });
        self.fetch_profile((user, guild));
        cx.notify();
    }

    /// Opens the full profile of `user` (the popout and menu close).
    pub(crate) fn open_full_profile(
        &mut self,
        user: UserId,
        guild: Option<GuildId>,
        cx: &mut Context<Self>,
    ) {
        self.profile.menu = None;
        self.profile.popout = None;
        self.profile.full = Some(Full {
            user,
            guild,
            tab: Tab::About,
        });
        self.fetch_profile((user, guild));
        cx.notify();
    }

    /// Asks for a profile unless a fresh copy is at hand or it is on its way.
    fn fetch_profile(&mut self, key: Key) {
        let now = Instant::now();
        if !self.profile.needs_fetch(&key, now) {
            return;
        }
        self.profile.next_request += 1;
        let request = self.profile.next_request;
        self.profile.in_flight = Some((request, key));
        if self.profile.failed.as_ref().is_some_and(|(k, _)| *k == key) {
            self.profile.failed = None;
        }
        self.backend.send(Cmd::Fetch {
            user_id: key.0.to_string(),
            guild_id: key.1.map(|g| g.to_string()),
            request,
        });
    }

    /// Answers of the profile domain.
    pub(crate) fn on_profile_event(&mut self, event: Ev, cx: &mut Context<Self>) {
        match event {
            Ev::Profile {
                request,
                user_id,
                guild_id,
                result,
            } => {
                let Some(user) = UserId::parse(&user_id) else {
                    return;
                };
                let guild = guild_id.as_deref().and_then(GuildId::parse);
                if self.profile.in_flight.as_ref().map(|(r, _)| *r) != Some(request) {
                    return;
                }
                self.profile.in_flight = None;
                match result {
                    Ok(mut profile) => {
                        // A note saved here wins over the one the answer may still have.
                        if let Some((_, text, _)) =
                            self.profile.written.iter().find(|(u, ..)| *u == user)
                        {
                            profile.note = text.clone();
                        }
                        self.profile
                            .cache
                            .insert((user, guild), *profile, Instant::now())
                    }
                    Err(reason) => self.profile.failed = Some(((user, guild), reason)),
                }
            }
            Ev::NoteFailed { user_id, reason } => {
                if let Some(user) = UserId::parse(&user_id) {
                    // The text before the save is the one on the server: the cache shows
                    // it again, and the field's text counts as not saved (it is sent
                    // again on the next try).
                    let at = self.profile.written.iter().position(|(u, ..)| *u == user);
                    if let Some((_, _, before)) = at.and_then(|i| self.profile.written.remove(i)) {
                        self.profile.cache.set_note(user, &before);
                        if let Some(n) = self.profile.note.as_mut().filter(|n| n.user == user) {
                            n.saved = before;
                        }
                    }
                }
                self.chat.error = Some(t!("profile-note-failed", reason = reason));
            }
        }
        cx.notify();
    }

    /// Once per frame: the note field follows the profile on screen, and a note
    /// that changed is saved when its profile closes. Also keeps the window width for
    /// placing a popout.
    pub(crate) fn profile_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.profile.window_width = f32::from(window.viewport_size().width);
        let shown = self.profile.shown();
        let keep = match (&self.profile.note, shown) {
            (Some(n), Some((u, _))) => n.user == u,
            (None, _) => true,
            (Some(_), None) => false,
        };
        if !keep {
            self.save_note(cx);
            self.profile.note = None;
        }
        #[cfg(feature = "demo")]
        self.demo_profile(cx);
        let Some(key) = shown else {
            return;
        };
        if self.profile.note.is_some() {
            return;
        }
        // The field appears once the profile (and its note) is in, for others only.
        let mine = self.account.as_ref().map(|a| a.read(cx).me_id()) == Some(key.0);
        let Some(profile) = self.profile.cache.any(&key) else {
            return;
        };
        if mine {
            return;
        }
        let note = profile.note.clone();
        let input = cx.new(|cx| {
            TextInput::new(t!("profile-note-placeholder"), cx)
                .max_chars(MAX_NOTE_CHARS)
                .with_text(note.clone())
        });
        let submit = cx.subscribe(&input, |this, _, event: &InputEvent, cx| match event {
            InputEvent::Submit => this.save_note(cx),
        });
        self.profile.note = Some(NoteEditor {
            user: key.0,
            input,
            saved: note,
            _submit: submit,
        });
    }

    /// Saves the note field's text when it changed (one request per change).
    pub(crate) fn save_note(&mut self, cx: &mut Context<Self>) {
        let Some(n) = self.profile.note.as_mut() else {
            return;
        };
        let text: String = n
            .input
            .read(cx)
            .text()
            .chars()
            .take(MAX_NOTE_CHARS)
            .collect();
        if text.trim() == n.saved.trim() {
            return;
        }
        let before = std::mem::replace(&mut n.saved, text.clone());
        let user = n.user;
        let w = &mut self.profile.written;
        // The text before the first unsaved write is the one the server has.
        let before = match w.iter().position(|(u, ..)| *u == user) {
            Some(i) => w.remove(i).map_or(before, |(_, _, b)| b),
            None => before,
        };
        if w.len() >= MAX_CACHED {
            w.pop_front();
        }
        w.push_back((user, text.clone(), before));
        self.profile.cache.set_note(user, &text);
        self.backend.send(Cmd::SetNote {
            user_id: user.to_string(),
            note: text,
        });
    }

    /// The profile overlays, bottom first: the full profile, the popout, the menu.
    pub(crate) fn render_profile_overlays(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut out = Vec::new();
        if let Some(full) = self.profile.full.clone() {
            out.push(self.render_full_profile(&full, cx));
        }
        if let Some(popout) = self.profile.popout.clone() {
            out.push(self.render_profile_popout(&popout, cx));
        }
        if let Some(menu) = self.profile.menu.clone() {
            out.push(self.render_user_menu(&menu, cx));
        }
        out
    }

    /// What the profile views show for `key`: the fetched profile, whether it is on its
    /// way, and why it failed.
    fn profile_view(&self, key: &Key) -> (Option<Rc<Profile>>, bool, Option<String>) {
        let loading = self
            .profile
            .in_flight
            .as_ref()
            .is_some_and(|(_, k)| k == key);
        let failed = self
            .profile
            .failed
            .as_ref()
            .filter(|(k, _)| k == key)
            .map(|(_, r)| r.clone());
        (self.profile.cache.any(key), loading, failed)
    }

    /// Demo scenes open the popout once the member list has its rows.
    #[cfg(feature = "demo")]
    fn demo_profile(&mut self, cx: &mut Context<Self>) {
        use crate::demo::scenes::{current, Scene};
        if self.profile.demo_done {
            return;
        }
        if current() != Scene::ProfilePopout {
            self.profile.demo_done = true;
            return;
        }
        let Some(list) = self.members.list.as_ref().filter(|l| l.loaded) else {
            return;
        };
        let ada = crate::demo::user_id("ada");
        let Some(row) = (0..list.len()).find(|i| {
            matches!(list.row(*i), Some(crate::ui::members::list::Row::Member(m)) if m.user_id.to_string() == ada)
        }) else {
            return;
        };
        self.profile.demo_done = true;
        // As if the row was clicked: the popout opens at its height.
        let y = 56. + 52. + row as f32 * crate::ui::members::ROW_HEIGHT;
        let guild = list.guild;
        let Some(user) = UserId::parse(&ada) else {
            return;
        };
        self.open_profile_popout(
            user,
            Some(guild),
            gpui::point(gpui::px(self.profile.window_width), gpui::px(y)),
            Opener::MemberList,
            cx,
        );
    }
}

/// A date as profiles show it ("1 Jan 2026", local time).
pub fn date(ms: i64) -> String {
    use chrono::Datelike;
    let Some(t) = chrono::DateTime::from_timestamp_millis(ms) else {
        return String::new();
    };
    let t = t.with_timezone(&chrono::Local);
    let month = u8::try_from(t.month())
        .ok()
        .and_then(|m| chrono::Month::try_from(m).ok())
        .map(|m| m.name().chars().take(3).collect::<String>())
        .unwrap_or_default();
    format!("{} {} {}", t.day(), month, t.year())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(u: u64) -> Key {
        (UserId::parse(&u.to_string()).unwrap(), None)
    }

    #[test]
    fn profile_fetch_is_cached_for_five_minutes() {
        let mut s = State::default();
        let t0 = Instant::now();
        let k = key(10);
        // Nothing cached: fetch.
        assert!(s.needs_fetch(&k, t0));
        // On its way: no second fetch.
        s.in_flight = Some((1, k));
        assert!(!s.needs_fetch(&k, t0));
        s.in_flight = None;
        s.cache.insert(k, Profile::default(), t0);
        // Opened again within five minutes: no request.
        assert!(!s.needs_fetch(&k, t0 + Duration::from_secs(60)));
        assert!(!s.needs_fetch(&k, t0 + CACHE_FOR - Duration::from_millis(1)));
        // Five minutes on: asked again, while the old copy still shows.
        assert!(s.needs_fetch(&k, t0 + CACHE_FOR));
        assert!(s.cache.any(&k).is_some());
        // Another guild's profile of the same person is its own entry.
        let in_guild = (k.0, GuildId::parse("5"));
        assert!(s.needs_fetch(&in_guild, t0));
        // The cache is bounded; the oldest go first.
        for u in 100..100 + MAX_CACHED as u64 {
            s.cache.insert(key(u), Profile::default(), t0);
        }
        assert_eq!(s.cache.len(), MAX_CACHED);
        assert!(s.cache.any(&k).is_none());
        // A note saved here shows in the cached copy.
        s.cache.set_note(key(100).0, "hi");
        assert_eq!(s.cache.any(&key(100)).unwrap().note, "hi");
        // An account switch empties it.
        s.reset();
        assert!(s.cache.is_empty());
    }
}
