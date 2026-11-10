# Sources of the emoji data

Everything in `data/` and `assets/` comes from the places below, at the exact versions
named. Nothing is fetched at build time.

## `data/emojis.json`: emoji list, shortcodes, categories, keywords, skin tones

- From Fluxer, https://github.com/fluxerapp/fluxer, file
  `fluxer_app/src/media/data/emojis.json` at commit
  `f1cba64e64bda5f57ad3cf827c1d5a98ecdd80df`.
- SHA-256 `547d4ffa7671588016843f0dcf0ee3cc47cec6be376000578d8bfb4e2a0da9a5`, 429,401 bytes,
  copied unchanged.
- Licence: AGPL-3.0-or-later, the same as Inlet (`SPDX-License-Identifier:
  AGPL-3.0-or-later` in Fluxer's sources). Copyright Fluxer Platform AB and contributors.
- Fluxer generates it from Unicode `emoji-test.txt` 17.0.0 and the CLDR 48 emoji
  annotations (Unicode License v3) and its own shortcode names.
- `build.rs` turns it into the tables in `src/data.rs`.
- To update: replace the file, run the tests, and change the commit and hash above.

## `assets/twemoji.pack`: Twemoji drawings

- Twemoji 17.0.2 from https://github.com/jdecked/twemoji, tag `v17.0.2`, commit
  `388f518b756404152f128a3539f5e7f9a6f1ebd6`, directory `assets/svg`. This is the version
  Fluxer ships (`@twemoji/api` 17.0.2).
- Source archive SHA-256
  `097e9b665da40c8bda2730bf1fe6b1efa71316adc3b6bffceabe7d24eaa49d03`
  (`https://github.com/jdecked/twemoji/archive/388f518b756404152f128a3539f5e7f9a6f1ebd6.tar.gz`).
- Licence of the graphics: CC-BY-4.0, text in `LICENSE-TWEMOJI` (their `LICENSE-GRAPHICS`,
  SHA-256 `8ae9438818c26e4873b91d8c6ad620526c011e27e125677f13031eda903f007c`).
  Copyright 2014-2021 Twitter, Inc and other contributors; copyright 2022-present Jason
  Sofonia, Justine De Caires and other contributors.
- The SVG files are not changed. The pack holds one zstd frame per file (shared
  dictionary); decompressing a frame gives the original bytes.
- What decides the pack's bytes (the same inputs and the same tools give the same file):
  - tools: Rust 1.99.1 (`rust-toolchain.toml`), crates `zstd` 0.13.4, `zstd-safe` 7.4.0 and
    `zstd-sys` 2.1.2+zstd.1.5.7 (libzstd 1.5.7, built from the bundled source), as pinned
    in `Cargo.lock`;
  - compression level 19, frames without dictionary id, size field or checksum;
  - dictionary from `zstd::dict::from_samples` over all drawings in key order, tried at
    64, 112 and 160 KiB; the smallest total wins (160 KiB: 163,840 bytes);
  - drawings in byte order of their file stems; layout described in `src/twemoji.rs`;
  - input: the 3,970 files of the pinned Twemoji archive named by the list's glyphs
    (the archive has 4,009; the rest are not in the list).
- Rebuild with `cargo run --release --locked -p inlet-emoji --example pack_twemoji --
  <svg dir> assets/twemoji.pack`. The workflow `.github/workflows/emoji-pack.yml` does
  this on every change to the crate from the pinned, hash-checked archive and compares the
  result with the committed file.
- Pack: 2,542,184 bytes, SHA-256
  `b44cac23d6bba745ea11f16a05f1bf63f4f7decafd66d6e5e65e8494761aefb3`.
- Drawings: 10,035,564 bytes of SVG. SHA-256 over all drawings in key order, each as the
  key, a zero byte, the length as 8 little endian bytes and the SVG bytes:
  `4117b5007770bdabbed271cbdab5ed1faa2e73c0d0b34676dfab285f461a2280`. This value was
  computed by a separate script from the unmodified files of the pinned archive, and the
  unit tests in `src/twemoji.rs` check the pack file and the unpacked drawings against
  both values.
