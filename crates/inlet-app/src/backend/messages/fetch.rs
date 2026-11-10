// SPDX-License-Identifier: AGPL-3.0-or-later
//! Loading what a view opens: the channels of a guild (or the DM list), the newest page
//! of a channel and, while the person scrolls up, older pages. Opening another channel
//! aborts the previous loads.
//!
//! Budget for older pages: one request at a time and at most one a second per account
//! (the UI's pager and [`OlderPages`] both hold it), and only while the person scrolls at
//! the top of a channel that has older messages. Nothing is fetched in a loop.

use std::time::{Duration, Instant};

use inlet_protocol as p;
use inlet_state::{MessageId, MessagesPage};
use tokio::task::AbortHandle;

use super::parse::{self, SharedNames};
use super::{Ev, PAGE_SIZE};
use crate::backend::{report, Active, Backend, Ui};
use crate::model::{Channel, Message};

impl Backend {
    pub(super) fn open_guild(&mut self, guild_id: Option<String>) {
        if let Some(task) = self.open_guild.take() {
            task.abort();
        }
        match &mut self.active {
            #[cfg(feature = "demo")]
            Some(Active::Demo(m)) => m.open_guild(guild_id),
            Some(Active::Real(r)) => {
                if let Some(previous) = r.active_guild.take() {
                    r.gateway.set_guild_active(&previous, false);
                }
                if let Some(id) = &guild_id {
                    // Guilds with more than 250 members are passive until marked active.
                    r.gateway.set_guild_active(id, true);
                    r.active_guild = Some(id.clone());
                }
                let cached = r.cache.lock().unwrap().get(&guild_id).cloned();
                let (session, cache, gen) = (r.session.clone(), r.cache.clone(), r.gen);
                let names = r.names.clone();
                let (events, internal) = (self.events.clone(), self.internal.clone());
                if let Some(channels) = cached {
                    self.emit(Ev::Channels { guild_id, channels });
                    return;
                }
                self.open_guild = Some(self.spawn_guarded("open guild", None, async move {
                    let result = match &guild_id {
                        Some(id) => session.client().channels(id).await,
                        None => session.client().dm_channels().await,
                    };
                    match result {
                        Ok(list) => {
                            let channels: Vec<Channel> =
                                list.iter().map(parse::to_channel).collect();
                            parse::remember_channels(&names, &channels);
                            cache
                                .lock()
                                .unwrap()
                                .insert(guild_id.clone(), channels.clone());
                            let _ = events.unbounded_send(Ev::Channels { guild_id, channels });
                        }
                        Err(err) => report(
                            &events,
                            &internal,
                            gen,
                            |e| t!("error-load-channels", error = e),
                            err,
                        ),
                    }
                }));
            }
            None => {}
        }
    }

    /// Loads the newest page of a channel. The UI opened its window with `begin_load`;
    /// the [`PageAnswer`] makes sure exactly one answer comes back.
    pub(super) fn open_channel(&mut self, channel_id: String, request: u64) {
        if let Some(task) = self.open_channel.take() {
            task.abort();
        }
        // A page of older messages for the channel left behind is no longer wanted; its
        // answer (a `PageFailed`) frees the UI's pager.
        self.older.abort();
        let answer = PageAnswer::new(&self.events, channel_id.clone(), request, false);
        match &mut self.active {
            #[cfg(feature = "demo")]
            Some(Active::Demo(m)) => {
                let (page, rendered) = m.newest_page(&channel_id);
                answer.page(page, rendered);
            }
            Some(Active::Real(r)) => {
                let (session, events, internal, gen) = (
                    r.session.clone(),
                    self.events.clone(),
                    self.internal.clone(),
                    r.gen,
                );
                let names = r.names.clone();
                self.open_channel = Some(self.spawn_guarded("open channel", None, async move {
                    match session
                        .client()
                        .messages(&channel_id, None, PAGE_SIZE)
                        .await
                    {
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
                }));
            }
            // No session: the answer drops and says the load failed.
            None => drop(answer),
        }
    }

    /// Loads the page before `before`. One at a time, and at most one a second
    /// ([`OLDER_PAGE_INTERVAL`]); the UI's pager keeps to the same pace, so this only
    /// catches a bug there. Every request gets exactly one answer.
    pub(super) fn load_older(&mut self, channel_id: String, before: String, request: u64) {
        let answer = PageAnswer::new(&self.events, channel_id.clone(), request, true);
        let Some(before_id) = MessageId::parse(&before) else {
            return;
        };
        if !self.older.admit(Instant::now()) {
            log::debug!("older page dropped: one is running or the last was under a second ago");
            return;
        }
        match &mut self.active {
            #[cfg(feature = "demo")]
            Some(Active::Demo(m)) => {
                let (page, rendered) = m.older_page(&channel_id, before_id);
                answer.page(page, rendered);
            }
            Some(Active::Real(r)) => {
                let (session, events, internal, gen) = (
                    r.session.clone(),
                    self.events.clone(),
                    self.internal.clone(),
                    r.gen,
                );
                let names = r.names.clone();
                let task = self.spawn_guarded("older messages", None, async move {
                    match session
                        .client()
                        .messages(&channel_id, Some(&before), PAGE_SIZE)
                        .await
                    {
                        Ok(list) => {
                            let rendered = render(&list, &names);
                            answer.page(
                                MessagesPage::before(before_id, list, PAGE_SIZE),
                                rendered,
                            );
                        }
                        Err(err) => report(
                            &events,
                            &internal,
                            gen,
                            |e| t!("error-load-messages", error = e),
                            err,
                        ),
                    }
                });
                self.older.started(task);
            }
            None => drop(answer),
        }
    }
}

/// Converts and parses the messages of a page (on the backend thread).
fn render(list: &[p::Message], names: &SharedNames) -> Vec<Message> {
    list.iter().map(|m| parse::to_message(m, names)).collect()
}

/// The one answer to a page request. Sending the page uses it up; if it is dropped
/// unused (an error, an aborted or panicking task, no session, a refused request), it
/// sends `PageFailed`. So every `begin_load` in the UI meets exactly one `apply_messages`
/// or `cancel_load`, and the UI's pager is always freed.
pub struct PageAnswer {
    events: Ui,
    channel_id: String,
    request: u64,
    older: bool,
    answered: bool,
}

impl PageAnswer {
    pub fn new(events: &Ui, channel_id: String, request: u64, older: bool) -> Self {
        PageAnswer {
            events: events.clone(),
            channel_id,
            request,
            older,
            answered: false,
        }
    }

    pub fn page(mut self, page: MessagesPage, rendered: Vec<Message>) {
        self.answered = true;
        let _ = self.events.unbounded_send(Ev::Page {
            channel_id: std::mem::take(&mut self.channel_id),
            request: self.request,
            page,
            rendered,
        });
    }
}

impl Drop for PageAnswer {
    fn drop(&mut self) {
        if !self.answered {
            let _ = self.events.unbounded_send(Ev::PageFailed {
                channel_id: std::mem::take(&mut self.channel_id),
                request: self.request,
                older: self.older,
            });
        }
    }
}

/// Shortest time between two requests for older messages.
pub const OLDER_PAGE_INTERVAL: Duration = Duration::from_secs(1);

/// The request for older messages: at most one running, at most one a second.
#[derive(Default)]
pub struct OlderPages {
    task: Option<AbortHandle>,
    last: Option<Instant>,
}

impl OlderPages {
    /// Whether a request may start now; records it when it may.
    fn admit(&mut self, now: Instant) -> bool {
        if self.task.as_ref().is_some_and(|t| !t.is_finished()) {
            return false;
        }
        if self
            .last
            .is_some_and(|last| now.saturating_duration_since(last) < OLDER_PAGE_INTERVAL)
        {
            return false;
        }
        self.task = None;
        self.last = Some(now);
        true
    }

    fn started(&mut self, task: AbortHandle) {
        self.task = Some(task);
    }

    /// Stops a running request (another channel was opened, or the session ended).
    /// Its answer says it failed.
    pub fn abort(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Event;
    use futures::StreamExt;

    #[test]
    fn the_backend_allows_one_older_page_a_second() {
        let mut pages = OlderPages::default();
        let start = Instant::now();
        let mut admitted = 0;
        // A request every 16 ms for ten seconds.
        for i in 0..625u64 {
            if pages.admit(start + Duration::from_millis(i * 16)) {
                admitted += 1;
            }
        }
        assert!((10..=11).contains(&admitted), "{admitted} requests");
    }

    #[tokio::test]
    async fn one_older_page_runs_at_a_time() {
        let mut pages = OlderPages::default();
        let start = Instant::now();
        assert!(pages.admit(start));
        let running = tokio::spawn(std::future::pending::<()>());
        pages.started(running.abort_handle());
        // A second later, still running: refused.
        assert!(!pages.admit(start + Duration::from_secs(5)));
        pages.abort();
        assert!(pages.admit(start + Duration::from_secs(6)));
    }

    fn failed(ev: Option<Event>) -> Option<(String, u64, bool)> {
        match ev {
            Some(Event::Messages(Ev::PageFailed {
                channel_id,
                request,
                older,
            })) => Some((channel_id, request, older)),
            _ => None,
        }
    }

    #[tokio::test]
    async fn every_page_request_gets_one_answer_even_when_aborted_or_panicking() {
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let events = Ui::new(tx);

        // Answered with a page: no failure follows.
        let answer = PageAnswer::new(&events, "1".into(), 1, false);
        answer.page(MessagesPage::latest(Vec::new(), PAGE_SIZE), Vec::new());
        assert!(matches!(
            rx.next().await,
            Some(Event::Messages(Ev::Page { request: 1, .. }))
        ));

        // An aborted task (a reconnect while an older page loads).
        let answer = PageAnswer::new(&events, "2".into(), 2, true);
        let task = tokio::spawn(async move {
            let _answer = answer;
            std::future::pending::<()>().await;
        });
        task.abort();
        let _ = task.await;
        assert_eq!(failed(rx.next().await), Some(("2".into(), 2, true)));

        // A panicking task.
        let answer = PageAnswer::new(&events, "3".into(), 3, false);
        let task = tokio::spawn(async move {
            let _answer = answer;
            panic!("conversion bug");
        });
        assert!(task.await.is_err());
        assert_eq!(failed(rx.next().await), Some(("3".into(), 3, false)));

        // Dropped without a session.
        drop(PageAnswer::new(&events, "4".into(), 4, false));
        assert_eq!(failed(rx.next().await), Some(("4".into(), 4, false)));
    }
}
