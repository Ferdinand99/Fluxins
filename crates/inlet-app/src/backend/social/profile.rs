// SPDX-License-Identifier: AGPL-3.0-or-later
//! User profiles on the network side: a person's profile (account-wide and in the
//! open guild, with mutual guilds and friends), the note about them, and saving that
//! note. The answers are made ready here, off the UI thread: names cleaned, the bio
//! parsed as markdown, lists capped.
//!
//! Request budget (REST, through the account's limiter):
//!
//! - A profile costs two requests (`GET /users/{id}/profile` and the note; the
//!   account's own profile only the first). The UI asks only when a popout or the
//!   full profile opens and it has no copy younger than five minutes
//!   (`ui::profile::ProfileCache`). Here one fetch runs at a time, two fetches start
//!   at least [`PROFILE_INTERVAL`] apart, and a newer request replaces one still
//!   waiting or running: at most 2 profiles (4 requests) a second however fast the
//!   person clicks through the member list.
//! - A note is saved only when its text changed, when the field is left or Enter is
//!   pressed: one `PUT` per change, one at a time ([`NoteQueue`]), the newest text last.

use std::sync::Arc;
use std::time::Duration;

use inlet_protocol as p;
use tokio::task::AbortHandle;
use tokio::time::Instant;

use crate::backend::session::{Failure, FailureKind};
use crate::backend::{Active, Backend, Internal};
use crate::model::{self, UserAvatar};
use crate::ui::markdown::{self, Block};

/// Shortest time between the starts of two profile fetches.
pub const PROFILE_INTERVAL: Duration = Duration::from_millis(500);
/// Characters of a bio parsed (Fluxer allows 320 in a profile).
const MAX_BIO_CHARS: usize = 1_024;
/// Mutual guilds and friends kept.
const MAX_MUTUALS: usize = 100;
/// Characters of a note kept (Fluxer allows 256).
const MAX_NOTE_CHARS: usize = 256;

/// Profile commands.
#[derive(Clone, Debug, PartialEq)]
pub enum Cmd {
    /// The profile of `user_id` (in `guild_id` when set); answered with one
    /// [`Ev::Profile`] with the same `request`, unless a newer request replaced it.
    Fetch {
        user_id: String,
        guild_id: Option<String>,
        request: u64,
    },
    /// Saves the note about `user_id` (`None` or empty deletes it).
    SetNote { user_id: String, note: String },
}

/// Profile events.
#[derive(Clone, Debug)]
pub enum Ev {
    Profile {
        request: u64,
        user_id: String,
        guild_id: Option<String>,
        result: Result<Box<Profile>, String>,
    },
    /// Saving a note failed; the text says why.
    NoteFailed { user_id: String, reason: String },
}

/// A person's banner: their account's or their profile in a guild.
#[derive(Clone, Debug, PartialEq)]
pub struct Banner {
    pub guild_id: Option<String>,
    pub hash: String,
}

/// A friend both accounts have.
#[derive(Clone, Debug, PartialEq)]
pub struct Friend {
    pub user_id: String,
    pub name: String,
    pub avatar: Option<UserAvatar>,
}

/// A profile as the popout and the full profile show it. Every text is cleaned.
#[derive(Clone, Debug, Default)]
pub struct Profile {
    /// Nickname in the guild, else the display name.
    pub name: String,
    pub username: String,
    pub bot: bool,
    pub avatar: Option<UserAvatar>,
    pub banner: Option<Banner>,
    /// 24-bit RGB: the guild profile's, else the account's, else the banner's.
    pub accent: Option<u32>,
    pub pronouns: Option<String>,
    /// The bio, parsed (the guild profile's when it has one).
    pub bio: Vec<Block>,
    /// When the account was made (Unix milliseconds).
    pub created_ms: Option<i64>,
    /// When the person joined the guild (Unix milliseconds).
    pub joined_ms: Option<i64>,
    /// Role IDs in the guild.
    pub roles: Vec<String>,
    /// Mutual guilds: ID and the person's nickname there.
    pub mutual_guilds: Vec<(String, Option<String>)>,
    pub mutual_friends: Vec<Friend>,
    /// The account's note about the person.
    pub note: String,
    /// Privacy settings hide parts of the profile.
    pub limited: bool,
}

fn text(s: Option<&str>) -> Option<String> {
    s.and_then(model::clean_text)
}

impl Profile {
    /// Makes the profile ready to show from the API's answer.
    pub fn of(full: &p::FullUserProfile, guild_id: Option<&str>, note: Option<String>) -> Profile {
        let user = &full.user;
        let member = full.guild_member.as_ref();
        let guild = full.guild_member_profile.as_ref();
        let account = &full.user_profile;
        let bio_src = guild
            .and_then(|g| g.bio.as_deref())
            .filter(|b| !b.trim().is_empty())
            .or(account.bio.as_deref())
            .unwrap_or_default();
        let bio: String = bio_src.chars().take(MAX_BIO_CHARS).collect();
        let banner = match (guild.and_then(|g| g.banner.as_deref()), guild_id) {
            (Some(h), Some(g)) if !h.is_empty() => Some(Banner {
                guild_id: Some(g.to_string()),
                hash: h.to_string(),
            }),
            _ => account
                .banner
                .as_deref()
                .filter(|h| !h.is_empty())
                .map(|h| Banner {
                    guild_id: None,
                    hash: h.to_string(),
                }),
        }
        .filter(|b| b.hash.len() <= 128);
        let name = member
            .and_then(|m| m.nick.as_deref())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| user.display_name());
        Profile {
            name: model::clean_name(name),
            username: model::clean_name(&user.username),
            bot: user.bot,
            avatar: crate::backend::media::user_avatar(user),
            banner,
            accent: guild
                .and_then(|g| g.accent_color)
                .or(account.accent_color)
                .or(account.banner_color)
                .map(|c| (c & 0xFF_FF_FF) as u32),
            pronouns: text(
                guild
                    .and_then(|g| g.pronouns.as_deref())
                    .filter(|p| !p.trim().is_empty())
                    .or(account.pronouns.as_deref()),
            ),
            bio: if bio.trim().is_empty() {
                Vec::new()
            } else {
                markdown::parse_embed_with(&bio, &model::Names::default())
            },
            created_ms: p::models::snowflake_timestamp_ms(&user.id).map(|t| t as i64),
            joined_ms: member
                .and_then(|m| m.joined_at.as_deref())
                .and_then(p::models::parse_timestamp_ms),
            roles: member
                .map(|m| m.roles.iter().take(MAX_MUTUALS).cloned().collect())
                .unwrap_or_default(),
            mutual_guilds: full
                .mutual_guilds
                .iter()
                .filter(|g| inlet_state::GuildId::parse(&g.id).is_some())
                .take(MAX_MUTUALS)
                .map(|g| (g.id.clone(), text(g.nick.as_deref())))
                .collect(),
            mutual_friends: full
                .mutual_friends
                .iter()
                .take(MAX_MUTUALS)
                .map(|f| Friend {
                    user_id: f.id.clone(),
                    name: model::clean_name(f.display_name()),
                    avatar: crate::backend::media::user_avatar(f),
                })
                .collect(),
            note: note
                .map(|n| n.chars().take(MAX_NOTE_CHARS).collect())
                .unwrap_or_default(),
            limited: full.profile_limited,
        }
    }
}

/// Notes waiting to be saved: the newest text per account session and person, and
/// whether a writer runs. One writer saves them one at a time, so two saves of the
/// same note never race and the newest text is always the last one sent.
#[derive(Debug)]
pub struct NoteQueue<T> {
    pending: Vec<((u64, String), T)>,
    running: bool,
}

impl<T> Default for NoteQueue<T> {
    fn default() -> Self {
        NoteQueue {
            pending: Vec::new(),
            running: false,
        }
    }
}

impl<T> NoteQueue<T> {
    /// Queues a save (it replaces one of the same note not sent yet). Returns whether
    /// a writer must start.
    pub fn push(&mut self, gen: u64, user: String, job: T) -> bool {
        let key = (gen, user);
        self.pending.retain(|(k, _)| *k != key);
        self.pending.push((key, job));
        !std::mem::replace(&mut self.running, true)
    }

    /// The next save for the writer, or `None` (and the writer stops).
    pub fn next(&mut self) -> Option<((u64, String), T)> {
        if self.pending.is_empty() {
            self.running = false;
            return None;
        }
        Some(self.pending.remove(0))
    }
}

/// One note to save, with its account's client and event sender.
type NoteJob = (p::Client, crate::backend::Ui, String);

/// The profile part of the backend: the fetch on its way and when the last started,
/// and the notes waiting to be saved.
#[derive(Default)]
pub struct State {
    fetch: Option<AbortHandle>,
    last_start: Arc<tokio::sync::Mutex<Option<Instant>>>,
    notes: Arc<std::sync::Mutex<NoteQueue<NoteJob>>>,
}

impl State {
    pub fn stop(&mut self) {
        if let Some(t) = self.fetch.take() {
            t.abort();
        }
    }
}

/// Waits for the turn of a fetch: at least [`PROFILE_INTERVAL`] after the last start.
async fn turn(last: &tokio::sync::Mutex<Option<Instant>>) {
    let mut last = last.lock().await;
    if let Some(at) = *last {
        tokio::time::sleep_until(at + PROFILE_INTERVAL).await;
    }
    *last = Some(Instant::now());
}

pub(super) fn handle(backend: &mut Backend, cmd: Cmd) {
    let (client, gen, me) = match &mut backend.active {
        #[cfg(feature = "demo")]
        Some(Active::Demo(m)) => {
            m.profile_command(cmd);
            return;
        }
        Some(Active::Real(r)) => (r.session.client().clone(), r.gen, r.key.user_id.to_string()),
        None => {
            if let Cmd::Fetch {
                user_id,
                guild_id,
                request,
            } = cmd
            {
                backend.emit(super::Ev::Profile(Ev::Profile {
                    request,
                    user_id,
                    guild_id,
                    result: Err(t!("profile-error-offline")),
                }));
            }
            return;
        }
    };
    let (events, internal) = (backend.events.clone(), backend.internal.clone());
    match cmd {
        Cmd::Fetch {
            user_id,
            guild_id,
            request,
        } => {
            backend.social.profile.stop();
            let last = backend.social.profile.last_start.clone();
            let task = backend.spawn_guarded("profile", None, async move {
                turn(&last).await;
                let mine = user_id == me;
                let fetched = client
                    .user_profile(&user_id, guild_id.as_deref(), !mine)
                    .await;
                let result = match fetched {
                    Ok(full) => {
                        // A note that cannot be read leaves the field empty.
                        let note = if mine {
                            None
                        } else {
                            client.note(&user_id).await.ok().flatten()
                        };
                        Ok(Box::new(Profile::of(&full, guild_id.as_deref(), note)))
                    }
                    Err(err) => Err(reason(&internal, gen, err)),
                };
                let _ = events.unbounded_send(super::Ev::Profile(Ev::Profile {
                    request,
                    user_id,
                    guild_id,
                    result,
                }));
            });
            backend.social.profile.fetch = Some(task);
        }
        Cmd::SetNote { user_id, note } => {
            let note: String = note.chars().take(MAX_NOTE_CHARS).collect();
            let queue = backend.social.profile.notes.clone();
            let start = queue.lock().unwrap_or_else(|e| e.into_inner()).push(
                gen,
                user_id,
                (client, events, note),
            );
            if !start {
                // The running writer saves it after the save on its way.
                return;
            }
            backend.spawn_guarded("note", None, async move {
                loop {
                    let next = queue.lock().unwrap_or_else(|e| e.into_inner()).next();
                    let Some(((gen, user_id), (client, events, note))) = next else {
                        break;
                    };
                    let value = Some(note.as_str()).filter(|n| !n.trim().is_empty());
                    if let Err(err) = client.set_note(&user_id, value).await {
                        let reason = reason(&internal, gen, err);
                        let _ = events
                            .unbounded_send(super::Ev::Profile(Ev::NoteFailed { user_id, reason }));
                    }
                }
            });
        }
    }
}

/// The reason of a failure for the UI; an ended session also goes to the loop.
fn reason(
    internal: &tokio::sync::mpsc::UnboundedSender<Internal>,
    gen: u64,
    err: p::Error,
) -> String {
    let failure = Failure::from(err);
    if failure.kind == FailureKind::SessionEnded {
        let _ = internal.send(Internal::Ended {
            gen,
            message: failure.message.clone(),
        });
    }
    failure.message
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_guild_profile_wins_and_texts_are_cleaned() {
        let full: p::FullUserProfile = serde_json::from_value(json!({
            "user": {"id": "175928847299117063", "username": "ada\u{202e}", "global_name": "Ada", "avatar": "abc"},
            "user_profile": {"bio": "account **bio**", "pronouns": "she/her", "banner": "b1", "accent_color": 255},
            "guild_member": {"user": {"id": "175928847299117063"}, "nick": "Countess", "roles": ["7"],
                             "joined_at": "2026-01-01T00:00:00+00:00"},
            "guild_member_profile": {"bio": "guild bio", "pronouns": " ", "banner": "g1", "accent_color": 16777471},
            "mutual_guilds": [{"id": "1", "nick": "x\u{200b}"}, {"id": "nope"}],
            "mutual_friends": [{"id": "5", "username": "bo"}]
        }))
        .unwrap();
        let prof = Profile::of(&full, Some("42"), Some("remember\nthis".into()));
        assert_eq!(prof.name, "Countess");
        assert_eq!(prof.username, "ada");
        assert_eq!(prof.pronouns.as_deref(), Some("she/her"));
        assert_eq!(
            prof.banner,
            Some(Banner {
                guild_id: Some("42".into()),
                hash: "g1".into()
            })
        );
        assert_eq!(prof.accent, Some(0x0000ff));
        assert!(!prof.bio.is_empty());
        assert_eq!(prof.roles, vec!["7".to_string()]);
        assert!(prof.joined_ms.is_some() && prof.created_ms.is_some());
        assert_eq!(prof.mutual_guilds.len(), 1);
        assert_eq!(prof.mutual_friends[0].name, "bo");
        assert_eq!(prof.note, "remember\nthis");
        // Without a guild the account's banner and bio show.
        let plain = Profile::of(&full, None, None);
        assert_eq!(plain.banner.unwrap().guild_id, None);
    }

    /// Two saves of the same note never run at once, and the newest text is sent last:
    /// one writer works through the queue, and a text not sent yet is replaced.
    #[test]
    fn note_saves_go_one_at_a_time_and_the_newest_wins() {
        let mut q = NoteQueue::default();
        // The first save starts the writer; it takes the job.
        assert!(q.push(1, "5".into(), "a"));
        assert_eq!(q.next(), Some(((1, "5".into()), "a")));
        // While "a" is on its way, two more saves of the note: no second writer, and
        // only the newest waits.
        assert!(!q.push(1, "5".into(), "b"));
        assert!(!q.push(1, "5".into(), "c"));
        // Another person's note and the same person's in another session wait too.
        assert!(!q.push(1, "6".into(), "x"));
        assert!(!q.push(2, "5".into(), "y"));
        assert_eq!(q.next(), Some(((1, "5".into()), "c")));
        assert_eq!(q.next(), Some(((1, "6".into()), "x")));
        assert_eq!(q.next(), Some(((2, "5".into()), "y")));
        // Empty: the writer stops, and the next save starts a new one.
        assert_eq!(q.next(), None);
        assert!(q.push(1, "5".into(), "d"));
    }

    #[tokio::test(start_paused = true)]
    async fn profile_fetches_start_at_most_twice_a_second() {
        let last = tokio::sync::Mutex::new(None);
        let t0 = Instant::now();
        for _ in 0..5 {
            turn(&last).await;
        }
        assert!(Instant::now() - t0 >= PROFILE_INTERVAL * 4);
    }
}
