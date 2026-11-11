// SPDX-License-Identifier: AGPL-3.0-or-later
//! Threads and forum posts on the network side: the messages of the thread open in the
//! side panel, archived threads of a channel, the posts of a forum and their first
//! messages, the gateway's thread subscription and op 28 (forum unread counts). What to
//! ask for, and when, is decided on the UI thread (`ui/threads`); this module sends each
//! request through the account's limiter and answers every one.
//!
//! Request budget (on top of the protocol's limiter, 20 a second):
//!
//! - Thread panel: one newest page per opened thread (opening another aborts it);
//!   older pages one at a time and at most one a second, only while the person scrolls
//!   at the top of the panel.
//! - Archived threads: one page per opening and per click on "Load more", one at a
//!   time and at most one a second for the account, across lists ([`Pace`]); a page
//!   still waiting for its turn when a newer list asks is dropped.
//! - Forum pages: one page per opening and per click on "Load more", one at a time per
//!   list (`inlet_state::ThreadPages`). Forum pages are
//!   thread searches, which the protocol paces with message search (one a second per
//!   account, refused without a call past a 3 s wait).
//! - First messages of forum posts: at most one request per forum opening for the
//!   active posts that lack one (at most 100), never twice for a post.
//! - Op 28: at most one request per forum opening (`inlet_state::ForumUnreadRequests`),
//!   at most 40 posts; the protocol sends at most 4 per 5 s and drops them while the
//!   gateway is down.
//! - Thread subscription (op 14): when the open guild or the panel's thread changes,
//!   never the same subscription twice in a row; the protocol coalesces op 14 over
//!   300 ms and sends it again after a reconnect.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use inlet_protocol as p;
use inlet_state::{MessageId, MessagesPage};
use tokio::task::AbortHandle;

use super::parse::{self, SharedNames};
use super::{Ev as MessagesEv, PAGE_SIZE};
use crate::backend::session::{Failure, FailureKind};
use crate::backend::{report, Active, Backend, Internal, Ui};
use crate::model::{Attachment, AttachmentKind, Message};

/// Threads per archived page and per forum page (the search allows 25 at most).
pub const LIST_PAGE: u8 = 25;
/// Shortest time between two older pages of the panel's thread.
const OLDER_INTERVAL: Duration = Duration::from_secs(1);
/// Posts per first-message request (the API allows 1-100).
pub const POST_DATA_MAX: usize = 100;
/// Characters of a post's first message shown on its card.
const SNIPPET_CHARS: usize = 160;

/// What a forum post's card shows of its first message.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PostPreview {
    /// The author's display name, cleaned.
    pub author: String,
    /// The start of the text on one line, cleaned (no markdown is drawn).
    pub snippet: String,
    /// The first image (or video) of the message, for the media grid.
    pub image: Option<Attachment>,
}

impl PostPreview {
    pub fn of(m: &Message) -> PostPreview {
        PostPreview {
            author: m.author.clone(),
            snippet: crate::model::snippet(&m.content, SNIPPET_CHARS),
            image: m
                .attachments
                .iter()
                .find(|a| matches!(a.kind, AttachmentKind::Image | AttachmentKind::Video))
                .cloned(),
        }
    }
}

/// The one answer to a list request (archived threads, a forum page). Sending the
/// result uses it up; if it is dropped unused (the task was aborted because a newer list
/// request replaced it, or it panicked), it answers with a failure, so no list ever waits
/// for an answer that does not come.
struct ListAnswer {
    events: Ui,
    kind: ListKind,
    id: String,
    request: u64,
    answered: bool,
}

#[derive(Clone, Copy)]
enum ListKind {
    Archived,
    Forum,
}

impl ListAnswer {
    fn new(events: &Ui, kind: ListKind, id: String, request: u64) -> Self {
        ListAnswer {
            events: events.clone(),
            kind,
            id,
            request,
            answered: false,
        }
    }

    fn archived(mut self, result: Result<(Vec<p::Channel>, bool), String>) {
        self.answered = true;
        self.send_archived(result);
    }

    fn forum(mut self, result: Result<ForumPage, String>) {
        self.answered = true;
        self.send_forum(result);
    }

    fn send_archived(&mut self, result: Result<(Vec<p::Channel>, bool), String>) {
        let _ = self.events.unbounded_send(MessagesEv::Thread(Ev::Archived {
            channel_id: std::mem::take(&mut self.id),
            request: self.request,
            result,
        }));
    }

    fn send_forum(&mut self, result: Result<ForumPage, String>) {
        let _ = self
            .events
            .unbounded_send(MessagesEv::Thread(Ev::ForumPage {
                forum_id: std::mem::take(&mut self.id),
                request: self.request,
                result,
            }));
    }
}

impl Drop for ListAnswer {
    fn drop(&mut self) {
        if self.answered {
            return;
        }
        let stopped = t!("threads-error-stopped");
        match self.kind {
            ListKind::Archived => self.send_archived(Err(stopped)),
            ListKind::Forum => self.send_forum(Err(stopped)),
        }
    }
}

/// Thread commands.
pub enum Cmd {
    /// The newest page of the thread in the side panel. Answered with exactly one
    /// [`Ev::Page`] or [`Ev::PageFailed`] with the same `request`.
    OpenThread { thread_id: String, request: u64 },
    /// The page before `before` in the panel's thread. Answered like `OpenThread`.
    LoadOlder {
        thread_id: String,
        before: String,
        request: u64,
    },
    /// A page of a channel's archived public threads, before `before` (an archive
    /// time). Answered with one [`Ev::Archived`].
    Archived {
        channel_id: String,
        before: Option<String>,
        request: u64,
    },
    /// A page of a forum's archived posts. Answered with one [`Ev::ForumPage`].
    ForumPage {
        forum_id: String,
        sort: &'static str,
        tags: Vec<String>,
        tag_setting: &'static str,
        offset: u32,
        request: u64,
    },
    /// The first messages of forum posts; answered with [`Ev::PostData`] when it worked.
    PostData {
        forum_id: String,
        thread_ids: Vec<String>,
    },
    /// Op 28 for a forum's posts; FORUM_UNREADS answers through the gateway.
    ForumUnreads {
        guild_id: String,
        forum_id: String,
        posts: Vec<p::ForumUnreadRequest>,
    },
    /// Thread events for a guild, and the member list of the panel's thread.
    Subscribe {
        guild_id: String,
        member_lists: Vec<String>,
    },
}

/// Thread events.
pub enum Ev {
    /// A page of the panel's thread, with its messages converted.
    Page {
        thread_id: String,
        request: u64,
        page: MessagesPage,
        rendered: Vec<Message>,
    },
    /// Loading a page of the panel's thread failed or was given up.
    PageFailed {
        thread_id: String,
        request: u64,
        older: bool,
    },
    /// A page of archived threads, or why there is none.
    Archived {
        channel_id: String,
        request: u64,
        result: Result<(Vec<p::Channel>, bool), String>,
    },
    /// A page of a forum's posts with the previews of their first messages.
    ForumPage {
        forum_id: String,
        request: u64,
        result: Result<ForumPage, String>,
    },
    /// First messages of forum posts, by post ID.
    PostData {
        forum_id: String,
        previews: Vec<(String, PostPreview)>,
    },
}

/// One page of forum posts.
pub struct ForumPage {
    pub posts: Vec<p::Channel>,
    pub previews: Vec<(String, PostPreview)>,
    pub has_more: bool,
}

/// One request at a time and at most one a second: a request takes its turn after the
/// one before it finished and a second after that one started.
#[derive(Clone, Default)]
pub struct Pace {
    last: Arc<tokio::sync::Mutex<Option<tokio::time::Instant>>>,
}

impl Pace {
    /// Waits for the turn; the turn lasts until the returned guard drops.
    pub async fn turn(&self) -> tokio::sync::OwnedMutexGuard<Option<tokio::time::Instant>> {
        let mut last = self.last.clone().lock_owned().await;
        if let Some(at) = *last {
            tokio::time::sleep_until(at + PACE_INTERVAL).await;
        }
        *last = Some(tokio::time::Instant::now());
        last
    }
}

/// Shortest time between two archived pages.
const PACE_INTERVAL: Duration = Duration::from_secs(1);

/// The thread domain's part of the backend.
#[derive(Default)]
pub struct State {
    /// The newest page of the panel's thread.
    open: Option<AbortHandle>,
    /// The older page of the panel's thread, and when the last one started.
    older: Option<AbortHandle>,
    older_last: Option<Instant>,
    /// The archived page on its way (and whether its request has started), and the
    /// forum page.
    archived: Option<(AbortHandle, Arc<AtomicBool>)>,
    forum: Option<AbortHandle>,
    /// Archived pages go one at a time and at most one a second, across lists.
    archived_pace: Pace,
    /// The last subscription sent: session, guild, member lists.
    subscribed: Option<(u64, String, Vec<String>)>,
}

impl State {
    /// Whether an older page may start now; records it when it may.
    fn admit_older(&mut self, now: Instant) -> bool {
        if self.older.as_ref().is_some_and(|t| !t.is_finished()) {
            return false;
        }
        if self
            .older_last
            .is_some_and(|last| now.saturating_duration_since(last) < OLDER_INTERVAL)
        {
            return false;
        }
        self.older = None;
        self.older_last = Some(now);
        true
    }

    /// The active account changes or ends: its thread requests stop, and its next
    /// subscription is sent again.
    pub fn stop(&mut self) {
        for task in [
            self.open.take(),
            self.older.take(),
            self.archived.take().map(|(t, _)| t),
            self.forum.take(),
        ]
        .into_iter()
        .flatten()
        {
            task.abort();
        }
        self.older_last = None;
        self.subscribed = None;
    }

    /// Whether the subscription differs from the last one of session `gen`; records it.
    fn fresh_subscription(&mut self, gen: u64, guild: &str, lists: &[String]) -> bool {
        let same = self
            .subscribed
            .as_ref()
            .is_some_and(|(g, s, l)| *g == gen && s == guild && l == lists);
        if !same {
            self.subscribed = Some((gen, guild.to_owned(), lists.to_vec()));
        }
        !same
    }
}

/// The one answer to a page request of the panel. Sending the page uses it up; if it
/// is dropped unused (an error, an aborted or panicking task, no session), it sends
/// [`Ev::PageFailed`]. So every `begin_load` the panel makes meets one answer.
struct PageAnswer {
    events: Ui,
    thread_id: String,
    request: u64,
    older: bool,
    answered: bool,
}

impl PageAnswer {
    fn new(events: &Ui, thread_id: String, request: u64, older: bool) -> Self {
        PageAnswer {
            events: events.clone(),
            thread_id,
            request,
            older,
            answered: false,
        }
    }

    fn page(mut self, page: MessagesPage, rendered: Vec<Message>) {
        self.answered = true;
        let _ = self.events.unbounded_send(MessagesEv::Thread(Ev::Page {
            thread_id: std::mem::take(&mut self.thread_id),
            request: self.request,
            page,
            rendered,
        }));
    }
}

impl Drop for PageAnswer {
    fn drop(&mut self) {
        if !self.answered {
            let _ = self
                .events
                .unbounded_send(MessagesEv::Thread(Ev::PageFailed {
                    thread_id: std::mem::take(&mut self.thread_id),
                    request: self.request,
                    older: self.older,
                }));
        }
    }
}

/// The reason for a failure, shown in the list that asked; a revoked token ends the
/// session instead.
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

fn render(list: &[p::Message], names: &SharedNames) -> Vec<Message> {
    list.iter().map(|m| parse::to_message(m, names)).collect()
}

/// Previews of first messages; a post's first message has the post's ID.
fn previews(list: &[p::Message], names: &SharedNames) -> Vec<(String, PostPreview)> {
    list.iter()
        .map(|m| {
            (
                m.channel_id.clone(),
                PostPreview::of(&parse::to_message(m, names)),
            )
        })
        .collect()
}

pub(super) fn handle(backend: &mut Backend, cmd: Cmd) {
    let (client, gen, names, gateway) = match &mut backend.active {
        #[cfg(feature = "demo")]
        Some(Active::Demo(m)) => {
            m.thread_command(cmd);
            return;
        }
        Some(Active::Real(r)) => (
            r.session.client().clone(),
            r.gen,
            r.names.clone(),
            r.gateway.clone(),
        ),
        None => {
            backend.no_thread_session(cmd);
            return;
        }
    };
    let (events, internal) = (backend.events.clone(), backend.internal.clone());
    match cmd {
        Cmd::OpenThread { thread_id, request } => {
            for task in [backend.threads.open.take(), backend.threads.older.take()]
                .into_iter()
                .flatten()
            {
                task.abort();
            }
            let answer = PageAnswer::new(&events, thread_id.clone(), request, false);
            let task = backend.spawn_guarded("thread page", None, async move {
                match client.messages(&thread_id, None, PAGE_SIZE).await {
                    Ok(list) => {
                        let rendered = render(&list, &names);
                        answer.page(MessagesPage::latest(list, PAGE_SIZE), rendered);
                    }
                    // The answer is a `PageFailed` when it drops.
                    Err(err) => report(
                        &events,
                        &internal,
                        gen,
                        |e| t!("error-load-messages", error = e),
                        err,
                    ),
                }
            });
            backend.threads.open = Some(task);
        }
        Cmd::LoadOlder {
            thread_id,
            before,
            request,
        } => {
            let answer = PageAnswer::new(&events, thread_id.clone(), request, true);
            let Some(before_id) = MessageId::parse(&before) else {
                return;
            };
            if !backend.threads.admit_older(Instant::now()) {
                log::debug!("older thread page dropped: one is running or came too soon");
                return;
            }
            let task = backend.spawn_guarded("thread older page", None, async move {
                match client.messages(&thread_id, Some(&before), PAGE_SIZE).await {
                    Ok(list) => {
                        let rendered = render(&list, &names);
                        answer.page(MessagesPage::before(before_id, list, PAGE_SIZE), rendered);
                    }
                    Err(err) => drop(reason(&internal, gen, err)),
                }
            });
            backend.threads.older = Some(task);
        }
        Cmd::Archived {
            channel_id,
            before,
            request,
        } => {
            // A newer list replaces the one before: a request still waiting for its turn
            // is dropped (answered as stopped); one already sent finishes, and the new
            // one waits for it and for the pace.
            if let Some((task, started)) = backend.threads.archived.take() {
                if !started.load(Ordering::Acquire) {
                    task.abort();
                }
            }
            let answer = ListAnswer::new(&events, ListKind::Archived, channel_id.clone(), request);
            let started = Arc::new(AtomicBool::new(false));
            let pace = backend.threads.archived_pace.clone();
            let flag = started.clone();
            let task = backend.spawn_guarded("archived threads", None, async move {
                let _turn = pace.turn().await;
                flag.store(true, Ordering::Release);
                let result = client
                    .public_archived_threads(&channel_id, before.as_deref(), LIST_PAGE)
                    .await
                    .map(|l| (l.threads, l.has_more))
                    .map_err(|err| reason(&internal, gen, err));
                answer.archived(result);
            });
            backend.threads.archived = Some((task, started));
        }
        Cmd::ForumPage {
            forum_id,
            sort,
            tags,
            tag_setting,
            offset,
            request,
        } => {
            if let Some(task) = backend.threads.forum.take() {
                task.abort();
            }
            let answer = ListAnswer::new(&events, ListKind::Forum, forum_id.clone(), request);
            let search = p::ThreadSearch {
                archived: Some(true),
                // Fluxer sends the tag setting only with more than one tag.
                tag_setting: (tags.len() > 1).then(|| tag_setting.to_owned()),
                tags,
                sort_by: Some(sort.to_owned()),
                sort_order: Some("desc".into()),
                limit: Some(LIST_PAGE),
                offset: Some(offset),
                ..p::ThreadSearch::default()
            };
            let task = backend.spawn_guarded("forum posts", None, async move {
                let result = match client.search_threads(&forum_id, &search).await {
                    Ok(p::SearchOutcome::Ready(found)) => Ok(ForumPage {
                        previews: previews(&found.first_messages, &names),
                        posts: found.threads,
                        has_more: found.has_more,
                    }),
                    Ok(_) => Err(t!("threads-error-indexing")),
                    Err(err) => Err(reason(&internal, gen, err)),
                };
                answer.forum(result);
            });
            backend.threads.forum = Some(task);
        }
        Cmd::PostData {
            forum_id,
            mut thread_ids,
        } => {
            thread_ids.truncate(POST_DATA_MAX);
            if thread_ids.is_empty() {
                return;
            }
            backend.spawn_guarded("forum post data", None, async move {
                let ids: Vec<&str> = thread_ids.iter().map(String::as_str).collect();
                match client.forum_post_data(&forum_id, &ids).await {
                    Ok(map) => {
                        let previews = map
                            .into_iter()
                            .filter_map(|(id, data)| {
                                let m = data.first_message?;
                                Some((id, PostPreview::of(&parse::to_message(&m, &names))))
                            })
                            .collect();
                        let _ = events.unbounded_send(MessagesEv::Thread(Ev::PostData {
                            forum_id,
                            previews,
                        }));
                    }
                    Err(err) => drop(reason(&internal, gen, err)),
                }
            });
        }
        Cmd::ForumUnreads {
            guild_id,
            forum_id,
            posts,
        } => {
            gateway.request_forum_unreads(&guild_id, &forum_id, &posts);
        }
        Cmd::Subscribe {
            guild_id,
            member_lists,
        } => {
            if backend
                .threads
                .fresh_subscription(gen, &guild_id, &member_lists)
            {
                let lists: Vec<&str> = member_lists.iter().map(String::as_str).collect();
                gateway.subscribe_threads(&guild_id, &lists);
            }
        }
    }
}

impl Backend {
    /// No session: every request that waits for an answer gets one.
    fn no_thread_session(&self, cmd: Cmd) {
        let offline = || t!("threads-error-offline");
        let ev = match cmd {
            Cmd::OpenThread { thread_id, request } => Ev::PageFailed {
                thread_id,
                request,
                older: false,
            },
            Cmd::LoadOlder {
                thread_id, request, ..
            } => Ev::PageFailed {
                thread_id,
                request,
                older: true,
            },
            Cmd::Archived {
                channel_id,
                request,
                ..
            } => Ev::Archived {
                channel_id,
                request,
                result: Err(offline()),
            },
            Cmd::ForumPage {
                forum_id, request, ..
            } => Ev::ForumPage {
                forum_id,
                request,
                result: Err(offline()),
            },
            Cmd::PostData { .. } | Cmd::ForumUnreads { .. } | Cmd::Subscribe { .. } => return,
        };
        self.emit(MessagesEv::Thread(ev));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn older_pages_of_the_panel_go_one_a_second() {
        let mut s = State::default();
        let t0 = Instant::now();
        assert!(s.admit_older(t0));
        assert!(!s.admit_older(t0 + Duration::from_millis(500)));
        assert!(s.admit_older(t0 + Duration::from_millis(1_000)));
    }

    #[tokio::test(start_paused = true)]
    async fn archived_pages_go_one_at_a_time_and_one_a_second() {
        use tokio::time::Instant;
        let pace = Pace::default();
        let t0 = Instant::now();
        // The first page goes at once.
        let first = pace.turn().await;
        assert_eq!(Instant::now(), t0);
        // A second one (Load More, or the list opened again) waits for the first to
        // finish and for a second after it started.
        let p2 = pace.clone();
        let second = tokio::spawn(async move {
            let _turn = p2.turn().await;
            Instant::now()
        });
        tokio::time::sleep(Duration::from_millis(3_000)).await;
        assert!(
            !second.is_finished(),
            "one at a time: the first is still running"
        );
        drop(first);
        let at = second.await.unwrap();
        assert_eq!(
            at,
            t0 + Duration::from_millis(3_000),
            "right after the first ended"
        );
        // Back to back: a second apart.
        let third = pace.turn().await;
        assert_eq!(Instant::now(), at + PACE_INTERVAL);
        drop(third);
        // After a pause, no wait.
        tokio::time::sleep(Duration::from_secs(5)).await;
        let before = Instant::now();
        let _fourth = pace.turn().await;
        assert_eq!(Instant::now(), before);
    }

    #[test]
    fn the_same_subscription_is_not_sent_twice() {
        let mut s = State::default();
        let lists = vec!["5".to_owned()];
        assert!(s.fresh_subscription(1, "100", &[]));
        assert!(!s.fresh_subscription(1, "100", &[]));
        assert!(
            s.fresh_subscription(1, "100", &lists),
            "the panel opened a thread"
        );
        assert!(!s.fresh_subscription(1, "100", &lists));
        assert!(s.fresh_subscription(1, "200", &[]), "another guild");
        assert!(s.fresh_subscription(2, "200", &[]), "a new session");
        // A switch to another account stops everything and forgets the last one.
        s.stop();
        assert!(s.fresh_subscription(2, "200", &[]));
    }
}
