//! Writes small, valid files of every format into a directory, as seeds for the
//! fuzz target: `cargo run -p inlet-media --example fuzz_seeds -- fuzz/corpus/decode`.
//! Each file starts with the selector byte of the target (kind and size).

#[path = "../tests/common/mod.rs"]
mod common;

use common::*;

fn main() {
    let dir = std::env::args()
        .nth(1)
        .expect("usage: fuzz_seeds <directory>");
    std::fs::create_dir_all(&dir).unwrap();
    let frames = |n: u32| -> Vec<_> {
        (0..n)
            .map(|i| (solid(24, 24, [(i * 60) as u8, 120, 200, 255]), 50))
            .collect()
    };
    let files: Vec<(&str, Vec<u8>)> = vec![
        ("png", png(&split(32, 24))),
        ("jpeg", jpeg(&split(32, 24))),
        ("webp", webp(&split(32, 24))),
        ("gif", gif(&frames(4))),
        (
            "apng",
            apng(
                &frames(3)
                    .into_iter()
                    .map(|(i, d)| (i, d as u16))
                    .collect::<Vec<_>>(),
            ),
        ),
        ("awebp", animated_webp(&frames(3))),
        ("rawgif", raw_gif((8, 8), &[(0, 0, 8, 8), (2, 2, 4, 4)])),
        ("webp-exif", webp_claiming_exif(0xFFFF_FFFE)),
    ];
    for (selector, kind) in [
        (0u8, "emoji"),
        (1, "avatar"),
        (2, "attachment"),
        (8, "preview"),
    ] {
        for (name, bytes) in &files {
            let mut seed = vec![selector];
            seed.extend_from_slice(bytes);
            std::fs::write(format!("{dir}/{name}-{kind}"), seed).unwrap();
        }
    }
    println!("wrote {} seeds to {dir}", files.len() * 4);
}
