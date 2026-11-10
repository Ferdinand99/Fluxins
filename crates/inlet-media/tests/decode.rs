mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use common::*;
use inlet_media::{decode, DecodeControl, Fit, MediaError};

fn near(actual: [u8; 4], expected: [u8; 4]) -> bool {
    actual
        .iter()
        .zip(expected)
        .all(|(a, e)| (i32::from(*a) - i32::from(e)).abs() <= 40)
}

// ---- formats ---------------------------------------------------------------------

#[test]
fn decodes_png() {
    let img = decode(
        &png(&split(40, 20)),
        NO_RESIZE,
        &avatar_limits(),
        &control(),
    )
    .unwrap();
    assert_eq!((img.width, img.height), (40, 20));
    assert!(!img.is_animated());
    assert_eq!(rgba_at(&img, 0, 2, 2), RED);
    assert_eq!(rgba_at(&img, 0, 37, 2), BLUE);
}

#[test]
fn decodes_jpeg() {
    let img = decode(
        &jpeg(&solid(32, 32, RED)),
        NO_RESIZE,
        &avatar_limits(),
        &control(),
    )
    .unwrap();
    assert_eq!((img.width, img.height), (32, 32));
    assert!(
        near(rgba_at(&img, 0, 16, 16), RED),
        "{:?}",
        rgba_at(&img, 0, 16, 16)
    );
}

#[test]
fn decodes_still_webp() {
    let img = decode(
        &webp(&split(24, 24)),
        NO_RESIZE,
        &avatar_limits(),
        &control(),
    )
    .unwrap();
    assert!(!img.is_animated());
    assert_eq!(rgba_at(&img, 0, 1, 1), RED);
    assert_eq!(rgba_at(&img, 0, 22, 1), BLUE);
}

#[test]
fn decodes_animated_gif_with_delays() {
    let bytes = gif(&[
        (solid(16, 16, RED), 50),
        (solid(16, 16, GREEN), 120),
        (solid(16, 16, BLUE), 0),
    ]);
    let img = decode(&bytes, NO_RESIZE, &avatar_limits(), &control()).unwrap();
    assert!(img.is_animated());
    assert_eq!(img.frames.len(), 3);
    assert!(near(rgba_at(&img, 0, 8, 8), RED));
    assert!(near(rgba_at(&img, 1, 8, 8), GREEN));
    assert!(near(rgba_at(&img, 2, 8, 8), BLUE));
    assert_eq!(img.frames[0].delay, Duration::from_millis(50));
    assert_eq!(img.frames[1].delay, Duration::from_millis(120));
    // A delay of zero plays at the browser default.
    assert_eq!(img.frames[2].delay, Duration::from_millis(100));
}

#[test]
fn single_frame_gif_is_not_animated() {
    let img = decode(
        &gif(&[(solid(8, 8, RED), 100)]),
        NO_RESIZE,
        &avatar_limits(),
        &control(),
    )
    .unwrap();
    assert!(!img.is_animated());
}

#[test]
fn decodes_animated_webp() {
    let bytes = animated_webp(&[
        (solid(16, 16, RED), 40),
        (solid(16, 16, GREEN), 80),
        (solid(16, 16, BLUE), 160),
    ]);
    let img = decode(&bytes, NO_RESIZE, &avatar_limits(), &control()).unwrap();
    assert_eq!(img.frames.len(), 3);
    assert_eq!(rgba_at(&img, 0, 3, 3), RED);
    assert_eq!(rgba_at(&img, 1, 3, 3), GREEN);
    assert_eq!(rgba_at(&img, 2, 3, 3), BLUE);
    assert_eq!(img.frames[1].delay, Duration::from_millis(80));
}

#[test]
fn decodes_apng() {
    let bytes = apng(&[(solid(16, 16, RED), 30), (solid(16, 16, GREEN), 70)]);
    let img = decode(&bytes, NO_RESIZE, &avatar_limits(), &control()).unwrap();
    assert_eq!(img.frames.len(), 2);
    assert_eq!(rgba_at(&img, 0, 5, 5), RED);
    assert_eq!(rgba_at(&img, 1, 5, 5), GREEN);
    assert_eq!(img.frames[1].delay, Duration::from_millis(70));
}

#[test]
fn plain_png_is_not_animated() {
    let img = decode(
        &png(&solid(8, 8, RED)),
        NO_RESIZE,
        &avatar_limits(),
        &control(),
    )
    .unwrap();
    assert!(!img.is_animated());
}

#[test]
fn refuses_other_formats() {
    for bytes in [
        &b"<svg xmlns='http://www.w3.org/2000/svg'><script>alert(1)</script></svg>"[..],
        &b"MZ\x90\x00 an executable"[..],
        &b"BM\x00\x00\x00\x00"[..],
        &b""[..],
    ] {
        let err = decode(bytes, NO_RESIZE, &avatar_limits(), &control()).unwrap_err();
        assert_eq!(err, MediaError::Unsupported, "{bytes:?}");
    }
}

// ---- scaling ----------------------------------------------------------------------

#[test]
fn downscales_once_to_the_requested_box() {
    let img = decode(
        &png(&solid(400, 200, RED)),
        Fit::PREVIEW,
        &avatar_limits(),
        &control(),
    )
    .unwrap();
    assert_eq!((img.width, img.height), (400, 200)); // smaller than the box: untouched
    let img = decode(
        &png(&solid(1100, 700, RED)),
        Fit::PREVIEW,
        &attachment_limits(),
        &control(),
    )
    .unwrap();
    assert_eq!((img.width, img.height), (550, 350));
}

fn attachment_limits() -> inlet_media::KindLimits {
    inlet_media::KindLimits::for_kind(inlet_media::MediaKind::Attachment)
}

#[test]
fn never_upscales() {
    let img = decode(
        &png(&solid(20, 20, RED)),
        Fit::square(96),
        &avatar_limits(),
        &control(),
    )
    .unwrap();
    assert_eq!((img.width, img.height), (20, 20));
}

#[test]
fn avatar_fit_crops_the_centre_to_a_square() {
    // 200x100: left half red, right half blue. The centre square (x 50..150)
    // is half red, half blue as well.
    let img = decode(
        &png(&split(200, 100)),
        Fit::square(32),
        &avatar_limits(),
        &control(),
    )
    .unwrap();
    assert_eq!((img.width, img.height), (32, 32));
    assert!(near(rgba_at(&img, 0, 3, 16), RED));
    assert!(near(rgba_at(&img, 0, 28, 16), BLUE));
}

#[test]
fn animated_frames_are_downscaled_too() {
    let bytes = apng(&[(solid(128, 64, RED), 30), (solid(128, 64, GREEN), 30)]);
    let img = decode(&bytes, Fit::square(32), &avatar_limits(), &control()).unwrap();
    assert_eq!((img.width, img.height), (32, 32));
    assert!(img.frames.iter().all(|f| f.bgra.len() == 32 * 32 * 4));
}

#[test]
fn gpui_frames_are_bgra_with_delays_and_need_no_copy() {
    let img = decode(
        &png(&solid(2, 2, [10, 20, 30, 255])),
        NO_RESIZE,
        &avatar_limits(),
        &control(),
    )
    .unwrap();
    assert_eq!(&img.frames[0].bgra[..4], &[30, 20, 10, 255]);
    assert_eq!(rgba_at(&img, 0, 0, 0), [10, 20, 30, 255]);
    let address = img.frames[0].bgra.as_ptr();
    let frames = img.into_gpui_frames();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].buffer().get_pixel(0, 0).0, [30, 20, 10, 255]);
    assert_eq!(
        frames[0].buffer().as_raw().as_ptr(),
        address,
        "pixels were copied"
    );
}

#[test]
fn a_big_source_shrinks_correctly_through_the_pre_shrink() {
    // 3000 x 1500, left half red, right half blue, to a 40 pixel avatar.
    let img = decode(
        &png(&split(3000, 1500)),
        Fit::square(40),
        &attachment_limits(),
        &control(),
    )
    .unwrap();
    assert_eq!((img.width, img.height), (40, 40));
    assert!(near(rgba_at(&img, 0, 3, 20), RED));
    assert!(near(rgba_at(&img, 0, 36, 20), BLUE));
}

// ---- limits -------------------------------------------------------------------------

#[test]
fn gif_claiming_a_huge_canvas_is_refused_before_any_allocation() {
    let err = decode(
        &gif_claiming(65535, 65535),
        NO_RESIZE,
        &avatar_limits(),
        &control(),
    )
    .unwrap_err();
    assert_eq!(
        err,
        MediaError::BadDimensions {
            width: 65535,
            height: 65535
        }
    );
}

#[test]
fn png_claiming_huge_dimensions_is_refused() {
    let tiny = zlib(&[0; 8]);
    let err = decode(
        &png_with(100_000, 100_000, &tiny),
        NO_RESIZE,
        &attachment_limits(),
        &control(),
    )
    .unwrap_err();
    assert!(matches!(err, MediaError::BadDimensions { .. }), "{err:?}");
}

#[test]
fn png_within_the_side_limit_but_over_the_pixel_limit_is_refused() {
    // 8192 x 8192 is 67 million pixels; attachments allow 40 million.
    let err = decode(
        &png_with(8192, 8192, &zlib(&[0; 8])),
        NO_RESIZE,
        &attachment_limits(),
        &control(),
    )
    .unwrap_err();
    assert!(matches!(err, MediaError::BadDimensions { .. }), "{err:?}");
}

#[test]
fn zero_sized_images_are_refused() {
    let err = decode(
        &png_with(0, 10, &zlib(&[0; 8])),
        NO_RESIZE,
        &avatar_limits(),
        &control(),
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            MediaError::BadDimensions { .. } | MediaError::Corrupt(_)
        ),
        "{err:?}"
    );
}

#[test]
fn huge_idat_is_refused_by_the_encoded_size_limit() {
    // 3 MiB of IDAT in an avatar (limit 2 MiB), 600 KiB in an emoji (limit 512 KiB).
    let noise: Vec<u8> = (0..3 * 1024 * 1024u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 24) as u8)
        .collect();
    let bytes = png_with(100, 100, &noise);
    let err = decode(&bytes, NO_RESIZE, &avatar_limits(), &control()).unwrap_err();
    assert!(
        matches!(err, MediaError::TooLargeEncoded { max, .. } if max == 2 * 1024 * 1024),
        "{err:?}"
    );
    let emoji = inlet_media::KindLimits::for_kind(inlet_media::MediaKind::Emoji);
    let err = decode(&bytes[..600 * 1024], NO_RESIZE, &emoji, &control()).unwrap_err();
    assert!(
        matches!(err, MediaError::TooLargeEncoded { max, .. } if max == 512 * 1024),
        "{err:?}"
    );
}

#[test]
fn idat_that_inflates_far_beyond_the_header_does_not_blow_up() {
    // Header says 10x10 (410 bytes of pixel data); the zlib stream holds 50 MB.
    let bytes = png_with(10, 10, &zlib(&vec![0u8; 50 * 1024 * 1024]));
    assert!(
        bytes.len() < 100 * 1024,
        "the fixture must be small: {}",
        bytes.len()
    );
    let result = decode(&bytes, NO_RESIZE, &avatar_limits(), &control());
    if let Ok(img) = result {
        assert_eq!((img.width, img.height), (10, 10));
    }
}

#[test]
fn compressed_bomb_within_limits_is_bounded_by_the_pixel_limit() {
    // 2048 x 2048 of zeros compresses to a few KB. It is the largest an avatar may be,
    // and it must decode and shrink without exceeding the limits.
    let raw = vec![0u8; 2048 * (1 + 2048 * 4)];
    let bytes = png_with(2048, 2048, &zlib(&raw));
    assert!(bytes.len() < 100 * 1024);
    let img = decode(&bytes, Fit::square(96), &avatar_limits(), &control()).unwrap();
    assert_eq!((img.width, img.height), (96, 96));
}

#[test]
fn too_many_frames_are_refused() {
    let mut limits = avatar_limits();
    limits.max_frames = 5;
    let frames: Vec<_> = (0..10).map(|_| (solid(8, 8, RED), 50)).collect();
    let err = decode(&gif(&frames), NO_RESIZE, &limits, &control()).unwrap_err();
    assert_eq!(err, MediaError::TooManyFrames { max: 5 });
    // Exactly at the limit is fine.
    limits.max_frames = 10;
    assert_eq!(
        decode(&gif(&frames), NO_RESIZE, &limits, &control())
            .unwrap()
            .frames
            .len(),
        10
    );
}

#[test]
fn animation_output_bytes_are_capped() {
    let mut limits = avatar_limits();
    limits.max_animation_bytes = 8 * 8 * 4 * 3; // room for three frames
    let frames: Vec<_> = (0..6).map(|_| (solid(8, 8, RED), 50)).collect();
    let err = decode(
        &apng(
            &frames
                .iter()
                .map(|(i, d)| (i.clone(), *d as u16))
                .collect::<Vec<_>>(),
        ),
        NO_RESIZE,
        &limits,
        &control(),
    )
    .unwrap_err();
    assert_eq!(err, MediaError::AnimationTooLarge);
}

#[test]
fn animation_work_is_capped() {
    let mut limits = avatar_limits();
    limits.max_animation_work_pixels = 8 * 8 * 3;
    let frames: Vec<_> = (0..6).map(|_| (solid(8, 8, RED), 50)).collect();
    let err = decode(&gif(&frames), NO_RESIZE, &limits, &control()).unwrap_err();
    assert_eq!(err, MediaError::AnimationTooLarge);
}

#[test]
fn an_expired_budget_stops_decoding() {
    let control = DecodeControl {
        deadline: std::time::Instant::now() - Duration::from_secs(1),
        cancel: Default::default(),
    };
    let err = decode(
        &png(&solid(8, 8, RED)),
        NO_RESIZE,
        &avatar_limits(),
        &control,
    )
    .unwrap_err();
    assert_eq!(err, MediaError::DecodeTimeout);
}

#[test]
fn a_cancelled_decode_stops_between_frames() {
    let control = control();
    control.cancel.store(true, Ordering::Relaxed);
    let frames: Vec<_> = (0..4).map(|_| (solid(8, 8, RED), 50)).collect();
    let err = decode(&gif(&frames), NO_RESIZE, &avatar_limits(), &control).unwrap_err();
    assert_eq!(err, MediaError::Cancelled);
}

// ---- damaged input ------------------------------------------------------------------

fn fixtures() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("png", png(&split(30, 30))),
        ("jpeg", jpeg(&split(30, 30))),
        ("webp", webp(&split(30, 30))),
        (
            "gif",
            gif(&[
                (solid(16, 16, RED), 50),
                (solid(16, 16, GREEN), 50),
                (solid(16, 16, BLUE), 50),
            ]),
        ),
        (
            "apng",
            apng(&[(solid(16, 16, RED), 50), (solid(16, 16, GREEN), 50)]),
        ),
        (
            "awebp",
            animated_webp(&[(solid(16, 16, RED), 50), (solid(16, 16, GREEN), 50)]),
        ),
    ]
}

#[test]
fn truncated_files_never_panic() {
    for (name, bytes) in fixtures() {
        for cut in [
            0,
            1,
            4,
            8,
            12,
            16,
            30,
            bytes.len() / 3,
            bytes.len() / 2,
            bytes.len() - 1,
        ] {
            let cut = cut.min(bytes.len());
            // Either a placeholder-worthy error or a shorter result, never a panic.
            let _ = decode(&bytes[..cut], NO_RESIZE, &avatar_limits(), &control());
            let _ = name;
        }
    }
}

#[test]
fn files_cut_off_in_the_header_are_errors() {
    for (name, bytes) in fixtures() {
        for cut in [0, 1, 4] {
            let err = decode(&bytes[..cut], NO_RESIZE, &avatar_limits(), &control());
            assert!(err.is_err(), "{name} cut at {cut} decoded");
        }
    }
}

#[test]
fn a_truncated_still_png_is_an_error() {
    let bytes = png(&split(64, 64));
    let err = decode(
        &bytes[..bytes.len() / 2],
        NO_RESIZE,
        &avatar_limits(),
        &control(),
    )
    .unwrap_err();
    assert!(matches!(err, MediaError::Corrupt(_)), "{err:?}");
}

#[test]
fn an_animation_cut_off_after_its_first_frame_shows_what_there_is() {
    let bytes = gif(&[
        (solid(16, 16, RED), 50),
        (solid(16, 16, GREEN), 50),
        (solid(16, 16, BLUE), 50),
    ]);
    // Drop the last quarter: the first frames are complete, the trailer is missing.
    let cut = &bytes[..bytes.len() * 3 / 4];
    if let Ok(img) = decode(cut, NO_RESIZE, &avatar_limits(), &control()) {
        assert!(!img.frames.is_empty() && img.frames.len() < 3);
    }
}

// ---- structural scans -----------------------------------------------------------------

#[test]
fn a_gif_frame_over_the_pixel_limit_is_refused_even_on_a_tiny_canvas() {
    // The classic: a 1x1 canvas with a 3535x3535 frame (12.5 Mpx, over the 4.2 Mpx
    // of an avatar). The decoder would allocate the frame.
    let err = decode(
        &raw_gif((1, 1), &[(0, 0, 3535, 3535)]),
        NO_RESIZE,
        &avatar_limits(),
        &control(),
    )
    .unwrap_err();
    assert!(matches!(err, MediaError::BadDimensions { .. }), "{err:?}");
}

#[test]
fn many_big_frames_on_a_tiny_canvas_hit_the_work_cap_before_decoding() {
    // Each frame is within the pixel limit, but together they cost 160 Mpx of
    // decoding against a cap of 128 Mpx.
    let frames = vec![(0, 0, 2000, 2000); 40];
    let err = decode(
        &raw_gif((1, 1), &frames),
        NO_RESIZE,
        &avatar_limits(),
        &control(),
    )
    .unwrap_err();
    assert_eq!(err, MediaError::AnimationTooLarge);
}

#[test]
fn a_gif_frame_that_overhangs_the_canvas_is_clipped_like_in_a_browser() {
    // The canvas is 4x4 (the first frame). The second frame is 4x4 at (2, 2), so
    // most of it is outside.
    let bytes = gif_at(&[(solid(4, 4, RED), 0, 0, 50), (solid(4, 4, BLUE), 2, 2, 50)]);
    let img = decode(&bytes, NO_RESIZE, &avatar_limits(), &control()).unwrap();
    assert_eq!((img.width, img.height), (4, 4));
    assert_eq!(img.frames.len(), 2);
    assert!(near(rgba_at(&img, 1, 3, 3), BLUE));
    // The part of the frame that is on the canvas is where the encoder put it, and
    // the first frame is still the first frame.
    assert!(near(rgba_at(&img, 0, 0, 0), RED));
    assert!(near(rgba_at(&img, 0, 3, 3), RED));
}

#[test]
fn gif_frames_of_zero_size_are_skipped() {
    let bytes = raw_gif((8, 8), &[(0, 0, 0, 0), (0, 0, 8, 8), (3, 3, 0, 5)]);
    let inspected = inlet_media::inspect(bytes::Bytes::from(bytes), &avatar_limits()).unwrap();
    assert_eq!((inspected.width(), inspected.height()), (8, 8));
    assert!(!inspected.is_animated(), "one frame has pixels");
}

#[test]
fn junk_and_unknown_blocks_after_the_frames_end_the_file() {
    let frames = [
        (solid(8, 8, RED), 50),
        (solid(8, 8, GREEN), 50),
        (solid(8, 8, BLUE), 50),
    ];
    let good = gif(&frames);
    // Junk after the trailer.
    let mut junk = good.clone();
    junk.extend_from_slice(b"\x00\xFF trailing garbage \x2C\x2C");
    let img = decode(&junk, NO_RESIZE, &avatar_limits(), &control()).unwrap();
    assert_eq!(img.frames.len(), 3);
    // An unknown block where the trailer should be.
    let mut unknown = good.clone();
    let last = unknown.len() - 1;
    assert_eq!(unknown[last], 0x3B);
    unknown.truncate(last);
    unknown.extend_from_slice(&[0x99, 0x02, 0xAA, 0xBB, 0x00]);
    let img = decode(&unknown, NO_RESIZE, &avatar_limits(), &control()).unwrap();
    assert_eq!(img.frames.len(), 3);
    // Junk where the first frame should be is not a GIF.
    let mut head = b"GIF89a".to_vec();
    head.extend_from_slice(&[8, 0, 8, 0, 0, 0, 0, 0x99, 1, 2, 3]);
    let err = decode(&head, NO_RESIZE, &avatar_limits(), &control()).unwrap_err();
    assert!(matches!(err, MediaError::Corrupt(_)), "{err:?}");
}

#[test]
fn gif_frame_count_is_checked_before_decoding() {
    let frames = vec![(0, 0, 100, 100); 600];
    let err = decode(
        &raw_gif((100, 100), &frames),
        NO_RESIZE,
        &avatar_limits(),
        &control(),
    )
    .unwrap_err();
    assert_eq!(err, MediaError::TooManyFrames { max: 512 });
}

#[test]
fn gif_total_frame_area_is_capped_before_decoding() {
    let mut limits = avatar_limits();
    limits.max_animation_work_pixels = 100 * 100 * 3;
    let frames = vec![(0, 0, 100, 100); 5];
    let err = decode(
        &raw_gif((100, 100), &frames),
        NO_RESIZE,
        &limits,
        &control(),
    )
    .unwrap_err();
    assert_eq!(err, MediaError::AnimationTooLarge);
}

#[test]
fn png_colour_profiles_and_text_are_stripped_and_the_picture_still_decodes() {
    let plain = png(&split(20, 20));
    let (head, tail) = plain.split_at(8 + 25); // signature + IHDR
    let mut chunks = Vec::new();
    let mut profile = b"profile\0\0".to_vec();
    profile.extend_from_slice(&zlib(&[7u8; 300]));
    png_chunk(&mut chunks, b"iCCP", &profile);
    png_chunk(&mut chunks, b"zTXt", b"key\0\0abc");
    png_chunk(&mut chunks, b"iTXt", b"key\0\0\0\0\0abc");
    png_chunk(&mut chunks, b"tEXt", &vec![b'a'; 100 * 1024]);
    let mut bytes = head.to_vec();
    bytes.extend_from_slice(&chunks);
    bytes.extend_from_slice(tail);
    assert!(bytes.len() > plain.len() + 100 * 1024);

    let img = decode(&bytes, NO_RESIZE, &avatar_limits(), &control()).unwrap();
    assert_eq!(rgba_at(&img, 0, 1, 1), RED);
    assert_eq!(rgba_at(&img, 0, 18, 1), BLUE);
}

#[test]
fn png_chunks_that_run_past_the_end_are_refused() {
    let mut bytes = png(&solid(4, 4, RED));
    // Make the IHDR chunk claim to be larger than the file.
    bytes[8..12].copy_from_slice(&0x7FFF_0000u32.to_be_bytes());
    let err = decode(&bytes, NO_RESIZE, &avatar_limits(), &control()).unwrap_err();
    assert!(matches!(err, MediaError::Corrupt(_)), "{err:?}");
}

#[test]
fn webp_chunks_that_claim_more_than_the_file_holds_are_refused() {
    let err = decode(
        &webp_claiming_exif(0xFFFF_FFFE),
        NO_RESIZE,
        &avatar_limits(),
        &control(),
    )
    .unwrap_err();
    assert!(matches!(err, MediaError::Corrupt(_)), "{err:?}");
}

#[test]
fn the_estimate_of_decode_memory_grows_with_the_image() {
    use bytes::Bytes;
    let small =
        inlet_media::inspect(Bytes::from(png(&solid(100, 100, RED))), &avatar_limits()).unwrap();
    let big =
        inlet_media::inspect(Bytes::from(png(&solid(1500, 1500, RED))), &avatar_limits()).unwrap();
    assert!(big.estimated_peak_bytes() > small.estimated_peak_bytes() + 1500 * 1500 * 8);
    assert_eq!((big.width(), big.height()), (1500, 1500));
    assert!(!big.is_animated());
}

#[test]
fn the_build_unwinds_so_decoder_panics_can_be_caught() {
    const {
        assert!(cfg!(panic = "unwind"));
    }
}

#[test]
fn proxy_sized_requests_get_tighter_limits() {
    let tight = avatar_limits().for_proxy_size(128);
    assert_eq!(tight.max_width, 512);
    assert_eq!(tight.max_pixels, 512 * 512);
    let err = decode(
        &png(&solid(1000, 1000, RED)),
        Fit::square(64),
        &tight,
        &control(),
    )
    .unwrap_err();
    assert!(matches!(err, MediaError::BadDimensions { .. }), "{err:?}");
    assert!(decode(
        &png(&solid(500, 500, RED)),
        Fit::square(64),
        &tight,
        &control()
    )
    .is_ok());
    // Never looser than the kind.
    assert_eq!(avatar_limits().for_proxy_size(100_000).max_width, 2048);
    assert_eq!(inlet_media::snap_proxy_size(41), 44);
    assert_eq!(inlet_media::snap_proxy_size(641), 1024);
    assert_eq!(inlet_media::snap_proxy_size(99_999), 16384);
}
