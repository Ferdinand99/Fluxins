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
  AGPL-3.0-or-later` in Fluxer's sources). Copyright the Fluxer authors.
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
- The SVG files are not changed. The pack holds one zstd frame per file (shared dictionary,
  level 19); decompressing a frame gives the original bytes.
- Rebuild with `cargo run --release -p inlet-emoji --example pack_twemoji -- <svg dir>
  assets/twemoji.pack`. The pack holds the drawings of the glyphs the list knows (3970) and
  nothing else, 10,035,564 bytes of SVG in 2,542,184 bytes. The same input gives the same
  bytes.
- Pack SHA-256: `b44cac23d6bba745ea11f16a05f1bf63f4f7decafd66d6e5e65e8494761aefb3`
