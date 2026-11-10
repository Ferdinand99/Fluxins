// SPDX-License-Identifier: AGPL-3.0-or-later
//! Twemoji drawings (CC-BY-4.0, see `LICENSE-TWEMOJI`), kept in `assets/twemoji.pack`.
//!
//! The pack is made by `examples/pack_twemoji.rs` from the unmodified SVG files of
//! jdecked/twemoji. Each drawing is compressed on its own with zstd and a dictionary
//! trained on all drawings, so one drawing can be unpacked without the rest. Layout, all
//! numbers little endian `u32`:
//!
//! ```text
//! magic "ITWP", count, dict_off, dict_len, keys_off, data_off, (7 header words in all)
//! count + 1 index entries: key_off, data_off, raw_len   (the last entry is the end marker)
//! keys:  one length byte and the file stem ("1f468-200d-1f469"), sorted
//! dict:  the zstd dictionary
//! data:  the compressed drawings, back to back
//! ```

use std::borrow::Cow;
use std::sync::OnceLock;

use zstd::bulk::Decompressor;
use zstd::dict::DecoderDictionary;

use crate::data::{qualified, strip_fe0f};

static PACK: &[u8] = include_bytes!("../assets/twemoji.pack");

const MAGIC: &[u8; 4] = b"ITWP";
const HEADER_WORDS: usize = 7;
const ENTRY_BYTES: usize = 12;
/// No Twemoji drawing is anywhere near this large; the limit guards against a damaged pack.
const MAX_SVG_BYTES: usize = 1 << 20;

fn word(bytes: &[u8], index: usize) -> usize {
    let at = index * 4;
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]) as usize
}

struct Pack {
    count: usize,
    keys_off: usize,
    data_off: usize,
    dict: DecoderDictionary<'static>,
}

fn pack() -> &'static Pack {
    static LOADED: OnceLock<Pack> = OnceLock::new();
    LOADED.get_or_init(|| {
        assert_eq!(&PACK[..4], MAGIC, "twemoji.pack has the wrong magic");
        let dict_off = word(PACK, 2);
        let dict_len = word(PACK, 3);
        Pack {
            count: word(PACK, 1),
            keys_off: word(PACK, 4),
            data_off: word(PACK, 5),
            dict: DecoderDictionary::copy(&PACK[dict_off..dict_off + dict_len]),
        }
    })
}

impl Pack {
    fn entry(&self, index: usize) -> (usize, usize, usize) {
        let base = HEADER_WORDS * 4 + index * ENTRY_BYTES;
        let at = &PACK[base..base + ENTRY_BYTES];
        (word(at, 0), word(at, 1), word(at, 2))
    }

    fn key(&self, index: usize) -> &'static [u8] {
        let (key_off, _, _) = self.entry(index);
        let start = self.keys_off + key_off;
        let len = usize::from(PACK[start]);
        &PACK[start + 1..start + 1 + len]
    }

    fn find(&self, code: &str) -> Option<usize> {
        let (mut low, mut high) = (0, self.count);
        while low < high {
            let mid = (low + high) / 2;
            match self.key(mid).cmp(code.as_bytes()) {
                std::cmp::Ordering::Less => low = mid + 1,
                std::cmp::Ordering::Greater => high = mid,
                std::cmp::Ordering::Equal => return Some(mid),
            }
        }
        None
    }

    fn unpack(&self, index: usize) -> Option<Vec<u8>> {
        let (_, start, raw_len) = self.entry(index);
        let (_, end, _) = self.entry(index + 1);
        if raw_len > MAX_SVG_BYTES {
            return None;
        }
        let compressed = PACK.get(self.data_off + start..self.data_off + end)?;
        let mut decompressor = Decompressor::with_prepared_dictionary(&self.dict).ok()?;
        let svg = decompressor.decompress(compressed, raw_len).ok()?;
        (svg.len() == raw_len).then_some(svg)
    }
}

/// The file stem Twemoji uses for a glyph: lower case hex code points joined by `-`.
/// `U+FE0F` is dropped, except in sequences joined by `U+200D`, where Twemoji keeps it
/// (the eye in speech bubble excepted). Fluxer builds the same names.
#[must_use]
pub fn twemoji_code(glyph: &str) -> String {
    const EYE_IN_SPEECH_BUBBLE: &str = "\u{1F441}\u{200D}\u{1F5E8}";
    let stripped = strip_fe0f(glyph);
    let keep = glyph.contains('\u{200D}') && stripped != EYE_IN_SPEECH_BUBBLE;
    let text = if keep { glyph } else { stripped.as_ref() };
    let mut code = String::with_capacity(text.len() * 2);
    for (index, c) in text.chars().enumerate() {
        if index > 0 {
            code.push('-');
        }
        code.push_str(&format!("{:x}", u32::from(c)));
    }
    code
}

/// The Twemoji drawing of a glyph as SVG, or `None` for text that is not a known emoji.
/// A glyph without its `U+FE0F` is found too.
///
/// Draw it with the SVG code in `inlet-media`; it is trusted data from this crate and the
/// only SVG Inlet draws.
#[must_use]
pub fn twemoji_svg(glyph: &str) -> Option<Cow<'static, [u8]>> {
    let pack = pack();
    let direct = pack.find(&twemoji_code(glyph));
    let index = match direct {
        Some(index) => index,
        // Typed text may lack U+FE0F where Twemoji keeps it; use the list's spelling.
        None => pack.find(&twemoji_code(qualified(glyph)?))?,
    };
    pack.unpack(index).map(Cow::Owned)
}
