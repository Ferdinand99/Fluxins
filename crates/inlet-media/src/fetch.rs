//! The fetch hook. The app implements it on top of the protocol session, so that
//! certificate pinning, the shared rate limiter and any proxy settings apply to
//! image downloads as well. This crate never opens a connection itself.

use bytes::Bytes;
use futures_util::future::BoxFuture;
use url::Url;

use crate::FetchError;

/// Downloads one URL.
///
/// Implementations must:
///
/// - stop reading and fail with [`FetchError::TooLarge`] as soon as the body is
///   larger than `max_bytes`, instead of buffering it first;
/// - fail with [`FetchError::Status`] on a non-success status;
/// - never follow a redirect to a host the session would not talk to.
///
/// Dropping the returned future must cancel the download.
///
/// ```
/// use bytes::Bytes;
/// use futures_util::future::BoxFuture;
/// use inlet_media::{FetchError, Fetcher};
/// use url::Url;
///
/// struct Nothing;
/// impl Fetcher for Nothing {
///     fn fetch<'a>(&'a self, _url: &'a Url, _max_bytes: usize) -> BoxFuture<'a, Result<Bytes, FetchError>> {
///         Box::pin(async { Err(FetchError::Status(404)) })
///     }
/// }
/// ```
pub trait Fetcher: Send + Sync + 'static {
    /// Fetches `url`, reading at most `max_bytes` bytes of body.
    fn fetch<'a>(
        &'a self,
        url: &'a Url,
        max_bytes: usize,
    ) -> BoxFuture<'a, Result<Bytes, FetchError>>;
}
