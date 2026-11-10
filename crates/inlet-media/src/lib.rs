//! Images for Inlet: fetch, decode, downscale, cache and animate, with hard limits,
//! because every image comes from another user.
//!
//! The app gives the crate finished URLs and a [`Fetcher`]. The fetcher is the
//! app's protocol session, so certificate pinning and the shared rate limiter
//! apply to image downloads too. This crate never opens a connection.
//!
//! # Flow of one request
//!
//! 1. [`MediaClient::load`] looks in memory (an LRU counted in bytes), then
//!    joins a request already running for the same URL, kind and size.
//! 2. The bytes come from the disk cache or from the [`Fetcher`], never more than
//!    the encoded limit of the [`MediaKind`].
//! 3. [`decode()`] runs on the blocking pool, at most a few at a time. It checks
//!    format, declared size, frame count, output bytes, total work and time, catches
//!    decoder panics, and downscales once to the requested [`Fit`].
//! 4. The result is a [`DecodedImage`]: RGBA8 frames with delays, ready for the UI.
//!
//! Formats: PNG, APNG, JPEG, GIF (animated) and WebP (animated), all decoded
//! with pure-Rust crates. SVG and everything else is refused.
//!
//! # For the gpui app
//!
//! At the pinned Zed revision, `gpui::RenderImage::new` takes `image::Frame` values
//! with **BGRA** pixels. [`DecodedImage::to_bgra_frames`] produces them. Then use
//! `ImageSource::Render(Arc<RenderImage>)`. gpui's `img` element plays the frames
//! itself and holds them while the window is inactive or reduced motion is on.
//! For code that drives frames by itself there is [`Playback`].
//!
//! The crate relies on unwinding to survive a decoder bug. A build with
//! `panic = "abort"` still enforces every limit, but cannot catch a panic.

mod client;
mod decode;
mod disk;
mod error;
mod fetch;
mod frames;
mod limits;
mod memory;

pub use client::{MediaClient, MediaConfig, MediaRequest, MediaStats};
pub use decode::{decode, DecodeControl};
pub use disk::DiskCacheConfig;
pub use error::{FetchError, MediaError};
pub use fetch::Fetcher;
pub use frames::{DecodedImage, ImageFrame, Playback, DEFAULT_FRAME_DELAY};
pub use limits::{Fit, KindLimits, Limits, MediaKind};
