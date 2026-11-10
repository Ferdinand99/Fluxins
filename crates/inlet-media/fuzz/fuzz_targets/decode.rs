//! Fuzzes the whole decode entry point with the real limits of every kind.
//!
//! The first byte picks the kind and the size the image is shrunk to; the rest is
//! the file. The property: no decoder panics (`decode` would catch a panic and
//! report `DecoderPanic`, which is a bug here, not a result) and memory stays
//! bounded, which libFuzzer checks with `-rss_limit_mb`.
//!
//! Seeds: `cargo run -p inlet-media --example fuzz_seeds -- corpus/decode`.
//! Run: `cargo +nightly fuzz run decode -- -rss_limit_mb=2048 -max_len=4194304`.
#![no_main]

use std::time::Duration;

use bytes::Bytes;
use inlet_media::{inspect, DecodeControl, Fit, KindLimits, MediaError, MediaKind};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&selector, file)) = data.split_first() else {
        return;
    };
    let kind = match selector % 3 {
        0 => MediaKind::Emoji,
        1 => MediaKind::Avatar,
        _ => MediaKind::Attachment,
    };
    let fit = match (selector / 3) % 4 {
        0 => Fit::square(32),
        1 => Fit::square(96),
        2 => Fit::PREVIEW,
        _ => Fit::Contain {
            max_width: 40,
            max_height: 40,
        },
    };
    let limits = KindLimits::for_kind(kind);
    // A short budget keeps the fuzzer fast; a timeout is a valid answer.
    let control = DecodeControl::with_budget(Duration::from_millis(500));
    let result = inspect(Bytes::copy_from_slice(file), &limits)
        .and_then(|inspected| inspected.decode(fit, &limits, &control));
    assert_ne!(result.err(), Some(MediaError::DecoderPanic), "a decoder panicked");
});
