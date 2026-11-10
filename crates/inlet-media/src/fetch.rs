//! The fetch hook. The app implements it on top of the protocol session, so that
//! certificate pinning, the shared rate limiter and any proxy settings apply to
//! image downloads as well. This crate never opens a connection itself.

use bytes::Bytes;
use futures_util::future::BoxFuture;
use futures_util::stream::BoxStream;
use url::Url;

use crate::FetchError;

/// The body of a response, in chunks as they arrive.
pub type ByteStream = BoxStream<'static, Result<Bytes, FetchError>>;

/// A response whose body has not been read yet. The crate reads it with the
/// size limit of the kind of image and drops it as soon as the limit is passed,
/// so a large body is never buffered.
pub struct FetchResponse {
    /// The announced length, if the server sent one. A length over the limit
    /// is refused without reading anything.
    pub content_length: Option<u64>,
    /// The body.
    pub body: ByteStream,
}

impl FetchResponse {
    /// A response with an optional announced length and a streamed body.
    pub fn new(content_length: Option<u64>, body: ByteStream) -> Self {
        Self {
            content_length,
            body,
        }
    }

    /// A response that is already in memory.
    pub fn from_bytes(bytes: Bytes) -> Self {
        Self {
            content_length: Some(bytes.len() as u64),
            body: Box::pin(futures_util::stream::once(std::future::ready(Ok(bytes)))),
        }
    }
}

/// Downloads one URL.
///
/// The crate has already checked the URL: `https` (or loopback `http`), no
/// credentials, and a host from `MediaConfig::allowed_hosts`. Implementations must:
///
/// - **send no credentials.** No `Authorization` header, no cookies, no session
///   token. Media hosts are public, and some URLs come from other users (embeds).
///   Use a client that is separate from the REST client, or strip the headers;
/// - **not follow a redirect to a host outside the allowed list** (and not from
///   `https` to `http`). Fail with [`FetchError::Network`] instead;
/// - fail with [`FetchError::Status`] on a non-success status;
/// - return the body as a stream and never buffer it. The crate stops reading
///   at the limit and drops the stream;
/// - stop the download when the returned future or the stream is dropped.
///
/// Pinning and the shared rate limiter should apply, as for any other request.
///
/// ```
/// use futures_util::future::BoxFuture;
/// use inlet_media::{FetchError, FetchResponse, Fetcher};
/// use url::Url;
///
/// struct Nothing;
/// impl Fetcher for Nothing {
///     fn fetch<'a>(&'a self, _url: &'a Url) -> BoxFuture<'a, Result<FetchResponse, FetchError>> {
///         Box::pin(async { Err(FetchError::Status(404)) })
///     }
/// }
/// ```
pub trait Fetcher: Send + Sync + 'static {
    /// Starts the download of `url` and returns when the headers are in.
    fn fetch<'a>(&'a self, url: &'a Url) -> BoxFuture<'a, Result<FetchResponse, FetchError>>;
}
