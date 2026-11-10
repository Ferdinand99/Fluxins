//! Images for Inlet: fetch, decode, downscale, cache and animate, with hard limits,
//! because every image comes from another user.
//!
//! The app gives the crate finished URLs and a [`Fetcher`]. The fetcher is the
//! app's protocol session, so certificate pinning and the shared rate limiter
//! apply to image downloads too. This crate never opens a connection.
//!
//! # Flow of one request
//!
//! 1. [`MediaClient::load`] checks the URL (https, host on the allow list), then
//!    looks in memory (an LRU counted in bytes), then at remembered failures, then
//!    joins a request already running for the same URL, kind and size.
//! 2. The bytes come from the disk cache or from the [`Fetcher`], which returns a
//!    stream that is read up to the encoded limit of the [`MediaKind`] and dropped
//!    when it passes it.
//! 3. [`inspect`] checks the format, strips dangerous PNG chunks, validates WebP
//!    and GIF structure against the real file length, checks the declared size and
//!    estimates the memory a decode needs. The decode then waits for a slot and for
//!    that much memory in a budget shared by all decodes.
//! 4. [`decode()`] runs on the blocking pool. It checks frame count, output bytes,
//!    total work and time, catches decoder panics, and downscales once to the
//!    requested [`Fit`], straight into BGRA.
//! 5. The [`ImageSink`] of the app turns the pixels into the UI's own object once,
//!    and is told when the cache lets go of it.
//!
//! Formats: PNG, APNG, JPEG, GIF (animated) and WebP (animated), all decoded
//! with pure-Rust crates. SVG and everything else is refused.
//!
//! # For the gpui app
//!
//! At the pinned Zed revision, `gpui::RenderImage::new` takes `image::Frame` values
//! with **BGRA** pixels. [`DecodedImage::into_gpui_frames`] moves the decoded pixels
//! into them without a copy. Implement [`ImageSink`] with `Handle = Arc<RenderImage>`
//! and call `cx.drop_image` from its `evicted`. gpui's `img` element plays the frames
//! itself and holds them while the window is inactive or reduced motion is on. For
//! code that drives frames by itself there is [`Playback`].
//!
//! The crate relies on unwinding to survive a decoder bug. A build with
//! `panic = "abort"` still enforces every limit, but cannot catch a panic. An
//! allocation failure aborts the process in any case, which is why the limits keep
//! the allocations of every decoder bounded before they happen.

#![forbid(unsafe_code)]

mod client;
mod clock;
mod decode;
mod disk;
mod error;
mod fetch;
mod frames;
mod limits;
mod memory;
mod scan;
mod sink;

pub use client::{MediaClient, MediaConfig, MediaRequest, MediaStats};
pub use clock::{Clock, SystemClock};
pub use decode::{decode, inspect, DecodeControl, Inspected};
pub use disk::DiskCacheConfig;
pub use error::{FetchError, MediaError};
pub use fetch::{ByteStream, FetchResponse, Fetcher};
pub use frames::{DecodedImage, ImageFrame, Playback, DEFAULT_FRAME_DELAY};
pub use limits::{snap_proxy_size, Fit, KindLimits, Limits, MediaKind, PROXY_SIZE_LADDER};
pub use sink::{ImageSink, RawImages};
