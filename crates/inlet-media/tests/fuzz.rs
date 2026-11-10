//! Fuzz-style tests: random and damaged bytes must produce an error or an image,
//! never a panic, a hang or a huge allocation.

mod common;

use std::sync::OnceLock;
use std::time::Duration;

use common::*;
use inlet_media::{decode, DecodeControl, Fit, KindLimits, MediaError, MediaKind};
use proptest::prelude::*;

/// Small limits, so that damaged headers that claim big sizes are refused fast.
fn small_limits() -> KindLimits {
    let mut limits = KindLimits::for_kind(MediaKind::Avatar);
    limits.max_width = 256;
    limits.max_height = 256;
    limits.max_pixels = 256 * 256;
    limits.max_frames = 16;
    limits.max_animation_bytes = 4 * 1024 * 1024;
    limits.max_animation_work_pixels = 4_000_000;
    limits
}

fn fixtures() -> &'static Vec<Vec<u8>> {
    static FIXTURES: OnceLock<Vec<Vec<u8>>> = OnceLock::new();
    FIXTURES.get_or_init(|| {
        vec![
            png(&split(30, 20)),
            jpeg(&split(30, 20)),
            webp(&split(30, 20)),
            gif(&[
                (solid(16, 16, RED), 50),
                (solid(16, 16, GREEN), 50),
                (solid(16, 16, BLUE), 50),
            ]),
            apng(&[(solid(16, 16, RED), 50), (solid(16, 16, GREEN), 50)]),
            animated_webp(&[(solid(16, 16, RED), 50), (solid(16, 16, GREEN), 50)]),
        ]
    })
}

const MAGIC: [&[u8]; 6] = [
    b"\x89PNG\r\n\x1a\n",
    b"\xff\xd8\xff\xe0",
    b"RIFF\x20\x00\x00\x00WEBPVP8X",
    b"GIF89a",
    b"GIF87a",
    b"RIFF\x00\x00\x00\x00WEBPVP8L",
];

fn run(bytes: &[u8]) {
    let control = DecodeControl::with_budget(Duration::from_secs(5));
    // The result does not matter, except that a decoder must not panic: `decode`
    // would catch it and report DecoderPanic, which is the safety net, not the goal.
    let result = decode(bytes, Fit::square(32), &small_limits(), &control);
    assert_ne!(result.err(), Some(MediaError::DecoderPanic));
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 400, failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn random_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..4096)) {
        run(&bytes);
    }

    #[test]
    fn random_bytes_behind_a_valid_signature_never_panic(
        magic in 0..MAGIC.len(),
        tail in proptest::collection::vec(any::<u8>(), 0..2048),
    ) {
        let mut bytes = MAGIC[magic].to_vec();
        bytes.extend_from_slice(&tail);
        run(&bytes);
    }

    #[test]
    fn damaged_real_files_never_panic(
        which in 0..6usize,
        edits in proptest::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 0..12),
        cut in proptest::option::of(any::<prop::sample::Index>()),
    ) {
        let mut bytes = fixtures()[which].clone();
        for (at, value) in edits {
            let i = at.index(bytes.len());
            bytes[i] = value;
        }
        if let Some(cut) = cut {
            bytes.truncate(cut.index(bytes.len() + 1));
        }
        run(&bytes);
    }
}
