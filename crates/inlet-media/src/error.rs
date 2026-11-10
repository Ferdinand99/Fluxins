//! Error types. Every error here means "show a placeholder"; none of them is fatal.

use thiserror::Error;

/// Why a fetch failed. Implemented by the app's [`Fetcher`](crate::Fetcher).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum FetchError {
    /// The body is larger than the `max_bytes` the caller allowed.
    #[error("response is larger than {max} bytes")]
    TooLarge {
        /// The limit that was exceeded.
        max: usize,
    },
    /// The server answered with a non-success status.
    #[error("unexpected HTTP status {0}")]
    Status(u16),
    /// Connection, TLS, timeout or any other transport problem.
    #[error("network error: {0}")]
    Network(String),
}

impl FetchError {
    /// Wraps any displayable transport error.
    pub fn network(err: impl std::fmt::Display) -> Self {
        Self::Network(err.to_string())
    }
}

/// Why an image could not be produced.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MediaError {
    /// The URL is not a plain `http` or `https` URL without credentials.
    #[error("invalid image URL: {0}")]
    InvalidUrl(String),
    /// The host is not in `MediaConfig::allowed_hosts`.
    #[error("host {0} is not allowed")]
    HostNotAllowed(String),
    /// The fetch failed.
    #[error("fetch failed: {0}")]
    Fetch(FetchError),
    /// The fetch did not finish in time.
    #[error("fetch timed out")]
    FetchTimeout,
    /// The encoded data is larger than the limit for this kind of image.
    #[error("encoded image is {size} bytes, the limit is {max}")]
    TooLargeEncoded {
        /// Size of the data (or the limit plus one when the size is not known).
        size: usize,
        /// The limit.
        max: usize,
    },
    /// The format is not PNG, JPEG, GIF or WebP.
    #[error("unsupported image format")]
    Unsupported,
    /// The width or height is over the limit, or zero.
    #[error("image dimensions {width}x{height} are not allowed")]
    BadDimensions {
        /// Declared width.
        width: u32,
        /// Declared height.
        height: u32,
    },
    /// The animation has more frames than allowed.
    #[error("animation has more than {max} frames")]
    TooManyFrames {
        /// The limit.
        max: usize,
    },
    /// The animation needs more memory or work than allowed.
    #[error("animation is too large to decode")]
    AnimationTooLarge,
    /// The data is not a valid image, or is cut off before the first frame.
    #[error("corrupt image: {0}")]
    Corrupt(String),
    /// Decoding took longer than the time budget.
    #[error("decoding took too long")]
    DecodeTimeout,
    /// A decoder panicked. The panic was caught.
    #[error("decoder panicked")]
    DecoderPanic,
    /// Nobody wants the image any more.
    #[error("request cancelled")]
    Cancelled,
    /// The task that loads the image died, for example because the app's
    /// [`Fetcher`](crate::Fetcher) or [`ImageSink`](crate::ImageSink) panicked.
    #[error("the loading task failed")]
    TaskFailed,
}

impl MediaError {
    /// True when asking again cannot succeed: the URL or the data is the problem.
    /// Everything else (network, timeouts, load, a 429 or 5xx answer) may work later.
    /// Both kinds are remembered with a growing back-off, see [`MediaConfig`](crate::MediaConfig).
    pub fn is_permanent(&self) -> bool {
        match self {
            Self::InvalidUrl(_)
            | Self::HostNotAllowed(_)
            | Self::TooLargeEncoded { .. }
            | Self::Unsupported
            | Self::BadDimensions { .. }
            | Self::TooManyFrames { .. }
            | Self::AnimationTooLarge
            | Self::Corrupt(_)
            | Self::DecoderPanic => true,
            Self::Fetch(FetchError::Status(code)) => {
                (400..500).contains(code) && !matches!(code, 408 | 429)
            }
            Self::Fetch(_)
            | Self::FetchTimeout
            | Self::DecodeTimeout
            | Self::Cancelled
            | Self::TaskFailed => false,
        }
    }
}

impl From<FetchError> for MediaError {
    fn from(err: FetchError) -> Self {
        match err {
            FetchError::TooLarge { max } => Self::TooLargeEncoded {
                size: max.saturating_add(1),
                max,
            },
            other => Self::Fetch(other),
        }
    }
}
