//! Tests that measure memory. A counting global allocator records the peak of live
//! bytes during a decode, which shows that a bomb is refused *before* it allocates,
//! not just that it is refused. This is its own test binary because the allocator
//! is global. Tests run one at a time (see `measure`) so that they do not see each
//! other's allocations.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use bytes::Bytes;
use common::*;
use inlet_media::{decode, inspect, Fit, KindLimits, MediaError, MediaKind};

struct Counting;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grow(by: usize) {
    let now = CURRENT.fetch_add(by, Ordering::SeqCst) + by;
    PEAK.fetch_max(now, Ordering::SeqCst);
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc(layout);
        if !ptr.is_null() {
            grow(layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc_zeroed(layout);
        if !ptr.is_null() {
            grow(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
        CURRENT.fetch_sub(layout.size(), Ordering::SeqCst);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new = System.realloc(ptr, layout, new_size);
        if !new.is_null() {
            CURRENT.fetch_sub(layout.size(), Ordering::SeqCst);
            grow(new_size);
        }
        new
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

/// Runs `f` and returns its result and the most memory it held beyond what was
/// live when it started.
fn measure<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let _guard = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let base = CURRENT.load(Ordering::SeqCst);
    PEAK.store(base, Ordering::SeqCst);
    let result = f();
    let peak = PEAK.load(Ordering::SeqCst).saturating_sub(base);
    (result, peak)
}

const MIB: usize = 1024 * 1024;

fn run(
    bytes: &[u8],
    fit: Fit,
    limits: &KindLimits,
) -> Result<inlet_media::DecodedImage, MediaError> {
    decode(bytes, fit, limits, &control())
}

/// A 1x1 PNG whose iCCP chunk inflates to `inflated` bytes.
fn icc_bomb(inflated: usize) -> Vec<u8> {
    let plain = png(&solid(1, 1, RED));
    let (head, tail) = plain.split_at(8 + 25);
    let mut profile = b"bomb\0\0".to_vec();
    profile.extend_from_slice(&zlib(&vec![0u8; inflated]));
    let mut chunks = Vec::new();
    png_chunk(&mut chunks, b"iCCP", &profile);
    let mut bytes = head.to_vec();
    bytes.extend_from_slice(&chunks);
    bytes.extend_from_slice(tail);
    bytes
}

#[test]
fn an_iccp_bomb_never_allocates_what_it_claims() {
    // 40 MiB of zeros in a few dozen KiB. Before the chunk was stripped, the png
    // crate inflated it without a limit.
    let bomb = icc_bomb(40 * MIB);
    assert!(
        bomb.len() < 100 * 1024,
        "the fixture must be small: {}",
        bomb.len()
    );
    for kind in [MediaKind::Emoji, MediaKind::Avatar, MediaKind::Attachment] {
        let limits = KindLimits::for_kind(kind);
        let (result, peak) = measure(|| run(&bomb, NO_RESIZE, &limits));
        let img = result.unwrap_or_else(|e| panic!("{kind:?}: {e:?}"));
        assert_eq!((img.width, img.height), (1, 1));
        assert!(
            peak < 8 * MIB,
            "{kind:?}: peaked at {} MiB for a 1x1 image",
            peak / MIB
        );
    }
}

#[test]
fn text_chunks_are_bombs_too() {
    let plain = png(&solid(1, 1, RED));
    let (head, tail) = plain.split_at(8 + 25);
    let mut chunks = Vec::new();
    let mut ztxt = b"k\0\0".to_vec();
    ztxt.extend_from_slice(&zlib(&vec![0u8; 40 * MIB]));
    png_chunk(&mut chunks, b"zTXt", &ztxt);
    let mut itxt = b"k\0\x01\0\0\0".to_vec();
    itxt.extend_from_slice(&zlib(&vec![0u8; 40 * MIB]));
    png_chunk(&mut chunks, b"iTXt", &itxt);
    let mut bytes = head.to_vec();
    bytes.extend_from_slice(&chunks);
    bytes.extend_from_slice(tail);
    let (result, peak) = measure(|| run(&bytes, NO_RESIZE, &avatar_limits()));
    assert!(result.is_ok(), "{result:?}");
    assert!(peak < 8 * MIB, "peaked at {} MiB", peak / MIB);
}

#[test]
fn a_webp_that_claims_four_gigabytes_of_exif_is_refused_before_it_allocates() {
    let bytes = webp_claiming_exif(0xFFFF_FFFE);
    for kind in [MediaKind::Emoji, MediaKind::Avatar, MediaKind::Attachment] {
        let (result, peak) = measure(|| run(&bytes, NO_RESIZE, &KindLimits::for_kind(kind)));
        assert!(matches!(result, Err(MediaError::Corrupt(_))), "{result:?}");
        assert!(peak < MIB, "{kind:?}: peaked at {peak} bytes");
    }
    // The same through inspect, which the client calls before it waits for a permit.
    let (result, peak) = measure(|| inspect(Bytes::from(bytes.clone()), &avatar_limits()));
    assert!(result.is_err());
    assert!(peak < MIB);
}

#[test]
fn a_gif_with_a_tiny_canvas_and_huge_frames_is_refused_before_it_allocates() {
    let frames = vec![(0u16, 0u16, 3535u16, 3535u16); 512];
    let bytes = raw_gif((1, 1), &frames);
    let (result, peak) = measure(|| run(&bytes, NO_RESIZE, &avatar_limits()));
    assert!(matches!(result, Err(MediaError::Corrupt(_))), "{result:?}");
    assert!(peak < MIB, "peaked at {peak} bytes");
}

#[test]
fn an_apng_that_announces_two_billion_frames_stays_small() {
    // IHDR 10x10 RGBA, acTL with 0x7FFFFFFF frames, one real frame, no more.
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&10u32.to_be_bytes());
    ihdr.extend_from_slice(&10u32.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    png_chunk(&mut out, b"IHDR", &ihdr);
    let mut actl = Vec::new();
    actl.extend_from_slice(&0x7FFF_FFFFu32.to_be_bytes());
    actl.extend_from_slice(&0u32.to_be_bytes());
    png_chunk(&mut out, b"acTL", &actl);
    let mut fctl = Vec::new();
    fctl.extend_from_slice(&0u32.to_be_bytes());
    fctl.extend_from_slice(&10u32.to_be_bytes());
    fctl.extend_from_slice(&10u32.to_be_bytes());
    fctl.extend_from_slice(&0u32.to_be_bytes());
    fctl.extend_from_slice(&0u32.to_be_bytes());
    fctl.extend_from_slice(&1u16.to_be_bytes());
    fctl.extend_from_slice(&10u16.to_be_bytes());
    fctl.extend_from_slice(&[0, 0]);
    png_chunk(&mut out, b"fcTL", &fctl);
    png_chunk(&mut out, b"IDAT", &zlib(&vec![0u8; 10 * (1 + 40)]));
    png_chunk(&mut out, b"IEND", &[]);

    let start = std::time::Instant::now();
    let (result, peak) = measure(|| run(&out, NO_RESIZE, &avatar_limits()));
    assert!(start.elapsed() < std::time::Duration::from_secs(5));
    assert!(peak < 8 * MIB, "peaked at {} MiB", peak / MIB);
    // Either the one frame that is there, or an error. Never a hang or a blow-up.
    if let Ok(img) = result {
        assert!(img.frames.len() <= 2);
    }
}

#[test]
fn a_rejected_decompression_bomb_within_the_header_limits_peaks_at_what_the_estimate_says() {
    // The largest an avatar may be, all zeros: tiny to send, big to decode.
    let limits = avatar_limits();
    let raw = vec![0u8; 2048 * (1 + 2048 * 4)];
    let bytes = png_with(2048, 2048, &zlib(&raw));
    let estimate = inspect(Bytes::from(bytes.clone()), &limits)
        .unwrap()
        .estimated_peak_bytes() as usize;
    let (result, peak) = measure(|| run(&bytes, Fit::square(96), &limits));
    assert!(result.is_ok());
    assert!(
        peak <= estimate,
        "peak {} MiB is above the estimate {} MiB",
        peak / MIB,
        estimate / MIB
    );
}

fn noisy(width: u32, height: u32) -> image::RgbaImage {
    noisy_seeded(width, height, 0)
}

fn noisy_seeded(width: u32, height: u32, seed: u32) -> image::RgbaImage {
    image::RgbaImage::from_fn(width, height, |x, y| {
        let v = x.wrapping_mul(2654435761) ^ y.wrapping_mul(40503) ^ seed.wrapping_mul(97);
        image::Rgba([
            (v >> 8) as u8,
            (v >> 16) as u8,
            (v >> 24) as u8,
            255 - (v as u8 >> 2),
        ])
    })
}

fn png16(width: u32, height: u32) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Sixteen);
        let mut writer = encoder.write_header().unwrap();
        let data: Vec<u8> = (0..width * height * 8).map(|i| (i % 251) as u8).collect();
        writer.write_image_data(&data).unwrap();
        writer.finish().unwrap();
    }
    out
}

#[test]
fn the_estimate_covers_the_real_peak_for_every_format_at_the_size_limits() {
    let attachment = KindLimits::for_kind(MediaKind::Attachment);
    let mut cases: Vec<(&str, Vec<u8>, KindLimits, Fit)> = vec![
        (
            "png8 1500x1500",
            png(&noisy(1500, 1500)),
            attachment.clone(),
            Fit::PREVIEW,
        ),
        (
            "png16 1024x1024",
            png16(1024, 1024),
            KindLimits::for_kind(MediaKind::Emoji),
            Fit::square(96),
        ),
        (
            "png16 2000x1500",
            png16(2000, 1500),
            attachment.clone(),
            Fit::PREVIEW,
        ),
        (
            "jpeg 2000x1500",
            jpeg(&noisy(2000, 1500)),
            attachment.clone(),
            Fit::PREVIEW,
        ),
        (
            "webp 1500x1000",
            webp(&noisy(1500, 1000)),
            attachment.clone(),
            Fit::PREVIEW,
        ),
    ];
    let frames: Vec<_> = (0..6).map(|i| (noisy_seeded(400, 300, i), 40)).collect();
    cases.push((
        "gif 6 frames",
        gif(&frames),
        attachment.clone(),
        Fit::PREVIEW,
    ));
    let frames: Vec<_> = (0..6).map(|i| (noisy_seeded(512, 384, i), 40)).collect();
    cases.push((
        "awebp 6 frames",
        animated_webp(&frames),
        attachment.clone(),
        Fit::PREVIEW,
    ));
    let frames: Vec<_> = (0..6).map(|i| (noisy_seeded(400, 300, i), 40u16)).collect();
    cases.push(("apng 6 frames", apng(&frames), attachment, Fit::PREVIEW));

    for (name, bytes, limits, fit) in cases {
        let estimate = inspect(Bytes::from(bytes.clone()), &limits)
            .unwrap_or_else(|e| panic!("{name}: {e:?}"))
            .estimated_peak_bytes() as usize;
        let (result, peak) = measure(|| run(&bytes, fit, &limits));
        assert!(result.is_ok(), "{name}: {result:?}");
        assert!(
            peak <= estimate,
            "{name}: peak {:.1} MiB is above the estimate {:.1} MiB",
            peak as f64 / MIB as f64,
            estimate as f64 / MIB as f64
        );
    }
}
