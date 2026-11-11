// SPDX-License-Identifier: AGPL-3.0-or-later
//! Message search on the network side. The quick switcher needs no requests: it ranks
//! what the account states already hold.
//!
//! A search is one `POST /search/messages` through the account's limiter. The UI builds
//! the request from typed filters (`ui/search/filters.rs`), never from a query string,
//! and the protocol checks every field against Fluxer's limits before anything is sent.
//!
//! Request budget (on top of the protocol's own: one search slot a second per account,
//! and a search whose slot is more than three seconds away is refused without a
//! request), kept by the UI (`ui/search/results.rs`):
//!
//! - A search starts only on Enter, on another page or on another sort, never while
//!   typing.
//! - One search at a time, and at most one a second. A newer search waits for its turn
//!   and replaces an older one that waits; it never runs beside it.
//! - A failed search waits for the person (Enter, or "Try again").
//!
//! Every command names its account. One for an account that is not the active one is
//! answered as failed without a request; every search is answered exactly once
//! ([`Answer`]), so the UI never waits for a lost one.

use inlet_protocol as p;

use super::accounts::AccountKey;
use super::messages::inbox::{item, Item};
use super::messages::parse::SharedNames;
use super::session::{Failure, FailureKind};
use super::{Active, Backend, Internal, Ui};

/// Results per page (the most the API gives).
pub const PAGE_SIZE: u8 = 25;

pub enum Cmd {
    /// One page of a message search, for `account`.
    Messages {
        account: AccountKey,
        request: u64,
        search: Box<p::MessageSearch>,
    },
}

/// One page of results, converted and parsed here, off the UI thread.
pub struct Page {
    pub items: Vec<Item>,
    /// Threads that hold a result, with the account's membership attached.
    pub threads: Vec<p::Channel>,
    /// Matches in total.
    pub total: u64,
    pub page: u16,
    pub more: bool,
}

pub enum Ev {
    Results {
        request: u64,
        page: Page,
    },
    /// The instance is still indexing the messages; search again later.
    Indexing {
        request: u64,
    },
    /// The search failed (`reason`), or was given up without one (another account, no
    /// session, the session ended).
    Failed {
        request: u64,
        reason: Option<String>,
    },
}

/// The one answer to a search. Sending the result uses it up; dropped unused (an error,
/// a panicking or aborted task, no session) it sends `Failed`.
pub struct Answer {
    events: Ui,
    request: u64,
    reason: Option<String>,
    answered: bool,
}

impl Answer {
    pub fn new(events: &Ui, request: u64) -> Self {
        Answer {
            events: events.clone(),
            request,
            reason: None,
            answered: false,
        }
    }

    pub fn request(&self) -> u64 {
        self.request
    }

    pub fn send(mut self, ev: Ev) {
        self.answered = true;
        let _ = self.events.unbounded_send(ev);
    }

    /// The search failed with `reason` (`None`: the session ended).
    pub fn fail(mut self, reason: Option<String>) {
        self.reason = reason;
    }
}

impl Drop for Answer {
    fn drop(&mut self) {
        if !self.answered {
            let _ = self.events.unbounded_send(Ev::Failed {
                request: self.request,
                reason: self.reason.take(),
            });
        }
    }
}

/// The reason to show for a failed search, or `None` when the session ended (the loop
/// is told, and signs out).
fn reason(
    internal: &tokio::sync::mpsc::UnboundedSender<Internal>,
    gen: u64,
    err: p::Error,
) -> Option<String> {
    if matches!(err, p::Error::RateLimited { .. }) {
        return Some(t!("search-too-fast"));
    }
    let failure = Failure::from(err);
    if failure.kind == FailureKind::SessionEnded {
        let _ = internal.send(Internal::Ended {
            gen,
            message: failure.message,
        });
        return None;
    }
    Some(failure.message)
}

/// A page of results as the UI shows it.
pub(crate) fn page_of(found: &p::MessageSearchResults, names: &SharedNames) -> Page {
    Page {
        items: found.messages.iter().map(|m| item(m, names)).collect(),
        threads: found.threads.clone(),
        total: found.total,
        page: u16::try_from(found.page.max(1)).unwrap_or(1),
        more: found.has_more(),
    }
}

pub(super) fn handle(backend: &mut Backend, cmd: Cmd) {
    match cmd {
        Cmd::Messages {
            account,
            request,
            search,
        } => {
            let answer = Answer::new(&backend.events, request);
            let (client, gen, names) = match &backend.active {
                Some(Active::Real(r)) if r.key == account => {
                    (r.session.client().clone(), r.gen, r.names.clone())
                }
                #[cfg(feature = "demo")]
                Some(Active::Demo(m)) => return m.search(&search, answer),
                // Another account, or none: answered as failed, without a request.
                _ => return,
            };
            let internal = backend.internal.clone();
            backend.spawn_guarded("message search", None, async move {
                match client.search_messages(&search).await {
                    Ok(p::SearchOutcome::Ready(found)) => {
                        let request = answer.request();
                        answer.send(Ev::Results {
                            request,
                            page: page_of(&found, &names),
                        });
                    }
                    Ok(_) => {
                        let request = answer.request();
                        answer.send(Ev::Indexing { request });
                    }
                    Err(err) => answer.fail(reason(&internal, gen, err)),
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::backend::accounts::Accounts;
    use crate::backend::Event;
    use crate::token_store::MemoryStore;
    use futures::StreamExt;

    fn failed(ev: Option<Event>) -> Option<(u64, Option<String>)> {
        match ev {
            Some(Event::Search(Ev::Failed { request, reason })) => Some((request, reason)),
            _ => None,
        }
    }

    #[tokio::test]
    async fn every_search_gets_one_answer() {
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let events = Ui::new(tx);
        drop(Answer::new(&events, 3));
        assert_eq!(failed(rx.next().await), Some((3, None)));
        Answer::new(&events, 4).fail(Some("offline".into()));
        assert_eq!(failed(rx.next().await), Some((4, Some("offline".into()))));
        Answer::new(&events, 5).send(Ev::Indexing { request: 5 });
        drop(Answer::new(&events, 6));
        assert!(matches!(
            rx.next().await,
            Some(Event::Search(Ev::Indexing { request: 5 }))
        ));
        assert_eq!(failed(rx.next().await), Some((6, None)));
    }

    /// A search for an account that is not the active one never reaches the network:
    /// with no session for it, it is answered as failed at once.
    #[tokio::test]
    async fn a_search_without_its_account_is_answered_as_failed() {
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let store = Accounts::new(Arc::new(MemoryStore::default()), None, None);
        let mut backend = Backend::with_store(Ui::new(tx), store);
        handle(
            &mut backend,
            Cmd::Messages {
                account: AccountKey::new("https://a.example", "1").unwrap(),
                request: 9,
                search: Box::new(p::MessageSearch::in_channel("2").text("hi")),
            },
        );
        assert_eq!(failed(rx.next().await), Some((9, None)));
    }
}
