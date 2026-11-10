//! REST calls around messages beyond the basics in [`crate::rest`]:
//!
//! - [`threads`]: threads and forums (start, forum posts with tags, join,
//!   leave, archive, archived lists, thread search, post data).
//! - [`search`]: message search with filters.
//! - [`saved`]: saved messages (bookmarks).
//! - [`inbox`]: recent mentions.
//! - [`messages`]: messages around or after an ID (jump to a message),
//!   forwarding and hiding embeds.
//!
//! Every call is a method on [`crate::Client`] and goes through the
//! account's shared rate limiter like the rest. On top of that, message and
//! thread searches of one account start at least
//! [`crate::coalesce::SEARCH_INTERVAL`] apart ([`crate::coalesce`]).
//!
//! Thread, forum and media channel routes need the `channel_threads` client
//! capability; the crate sends it on every request
//! ([`crate::rest::FLUXER_FEATURES`]) and sets the matching Identify flag
//! ([`crate::gateway::FLAG_CHANNEL_THREADS`]) by default.

pub mod inbox;
pub mod messages;
pub mod saved;
pub mod search;
pub mod threads;

use std::time::Duration;

/// The answer to a search: results, or "the index is being built, try
/// again later". The crate never retries on its own.
#[derive(Debug, Clone, PartialEq)]
pub enum SearchOutcome<T> {
    /// The search finished.
    Ready(T),
    /// The instance is still indexing; search again after `retry_after`
    /// (when it said how long).
    Indexing {
        /// The server's suggested wait, at most 5 minutes.
        retry_after: Option<Duration>,
    },
}

impl<T> SearchOutcome<T> {
    /// The results, if the search finished.
    pub fn ready(self) -> Option<T> {
        match self {
            SearchOutcome::Ready(t) => Some(t),
            SearchOutcome::Indexing { .. } => None,
        }
    }
}

impl crate::Client {
    /// Waits for the account's next search slot, or refuses when it is far away.
    pub(crate) async fn search_slot(&self) -> crate::Result<()> {
        let limiter = self.rate_limiter();
        match limiter.coalesce.reserve_search(tokio::time::Instant::now()) {
            Ok(wait) if wait.is_zero() => Ok(()),
            Ok(wait) => {
                tokio::time::sleep(wait).await;
                Ok(())
            }
            Err(retry_after) => Err(crate::Error::RateLimited { retry_after }),
        }
    }
}

/// Checks a list of IDs for a query or body: at most `max`, all snowflakes.
pub(crate) fn id_list(ids: &[String], max: usize, what: &str) -> crate::Result<()> {
    if ids.len() > max {
        return Err(crate::Error::InvalidInput(format!(
            "{what}: {} IDs, at most {max}",
            ids.len()
        )));
    }
    for i in ids {
        crate::rest::id(i)?;
    }
    Ok(())
}

/// Checks text for a query or body: 1 to `max` characters, no control characters.
pub(crate) fn text(value: &str, max: usize, what: &str) -> crate::Result<()> {
    let n = value.chars().count();
    if n == 0 || n > max || value.chars().any(char::is_control) {
        return Err(crate::Error::InvalidInput(format!(
            "{what}: 1-{max} characters without control characters"
        )));
    }
    Ok(())
}
