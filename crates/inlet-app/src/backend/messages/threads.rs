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
//! - Archived threads and forum pages: one page per opening and per click on "Load
//!   more", one at a time per list (`inlet_state::ThreadPages`). Forum pages are
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

/// The thread domain's part of the backend.
#[derive(Default)]
pub struct State {
    /// The newest page of the panel's thread.
    open: Option<AbortHandle>,
    /// The older page of the panel's thread, and when the last one started.
    older: Option<AbortHandle>,
    older_last: Option<Instant>,
    /// The archived page and the forum page on their way.
    archived: Option<AbortHandle>,
    forum: Option<AbortHandle>,
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
            if let Some(task) = backend.threads.archived.take() {
                task.abort();
            }
            let task = backend.spawn_guarded("archived threads", None, async move {
                let result = client
                    .public_archived_threads(&channel_id, before.as_deref(), LIST_PAGE)
                    .await
                    .map(|l| (l.threads, l.has_more))
                    .map_err(|err| reason(&internal, gen, err));
                let _ = events.unbounded_send(MessagesEv::Thread(Ev::Archived {
                    channel_id,
                    request,
                    result,
                }));
            });
            backend.threads.archived = Some(task);
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
                let _ = events.unbounded_send(MessagesEv::Thread(Ev::ForumPage {
                    forum_id,
                    request,
                    result,
                }));
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
    }
}
