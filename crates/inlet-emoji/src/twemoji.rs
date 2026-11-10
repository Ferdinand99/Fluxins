// SPDX-License-Identifier: AGPL-3.0-or-later
//! Twemoji drawings (CC-BY-4.0, see `LICENSE-TWEMOJI`), kept in `assets/twemoji.pack`.
//!
//! The pack is made by `examples/pack_twemoji.rs` from the unmodified SVG files of
//! jdecked/twemoji. Each drawing is compressed on its own with zstd and a dictionary
//! trained on all drawings, so one drawing can be unpacked without the rest. Layout, all
//! numbers little endian `u32`:
//!
//! ```text
//! magic "ITWP", count, dict_off, dict_len, keys_off, data_off, 0   (7 header words)
//! count + 1 index entries: key_off, data_off, raw_len   (the last entry is the end marker)
//! keys:  one length byte and the file stem ("1f468-200d-1f469"), sorted, back to back
//! dict:  the zstd dictionary
//! data:  the compressed drawings, back to back
//! ```
//!
//! `Pack::parse` checks all of this before a pack is used, so a damaged or tampered pack
//! makes lookups return `None`; it never panics and never allocates more than
//! `MAX_SVG_BYTES` for one drawing.

use std::borrow::Cow;
use std::sync::OnceLock;

use zstd::bulk::Decompressor;
use zstd::dict::DecoderDictionary;

use crate::data::{qualified, strip_fe0f};

static PACK: &[u8] = include_bytes!("../assets/twemoji.pack");

const MAGIC: &[u8; 4] = b"ITWP";
/// Magic number of a zstd dictionary made by the dictionary trainer.
const DICT_MAGIC: [u8; 4] = [0x37, 0xA4, 0x30, 0xEC];
const HEADER_BYTES: usize = 7 * 4;
const ENTRY_BYTES: usize = 12;
/// More drawings than any emoji set will ever have; keeps a bad `count` from looping long.
const MAX_ENTRIES: usize = 1 << 16;
/// No Twemoji drawing is anywhere near this large (the largest is 52 KB); the limit is
/// also the most memory one lookup can ask for.
const MAX_SVG_BYTES: usize = 1 << 20;

fn word(bytes: &[u8], at: usize) -> Option<usize> {
    let raw: [u8; 4] = bytes.get(at..at.checked_add(4)?)?.try_into().ok()?;
    Some(u32::from_le_bytes(raw) as usize)
}

/// Index entry `index` as `(key_off, data_off, raw_len)`.
fn entry(bytes: &[u8], index: usize) -> Option<(usize, usize, usize)> {
    let at = index.checked_mul(ENTRY_BYTES)?.checked_add(HEADER_BYTES)?;
    Some((word(bytes, at)?, word(bytes, at + 4)?, word(bytes, at + 8)?))
}

fn is_stem(key: &[u8]) -> bool {
    !key.is_empty()
        && key
            .iter()
            .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f' | b'-'))
}

/// A checked view of a pack.
pub(crate) struct Pack<'a> {
    bytes: &'a [u8],
    count: usize,
    keys: &'a [u8],
    data: &'a [u8],
    dict: DecoderDictionary<'static>,
}

impl<'a> Pack<'a> {
    /// Checks the header, the index, the key table and every range, and returns `None` if
    /// anything is wrong: wrong magic or counts, offsets out of the file, ranges that are
    /// reversed, overlapping or leave gaps, keys that are not sorted file stems, a length
    /// above `MAX_SVG_BYTES`, or a dictionary that is not a zstd dictionary.
    pub(crate) fn parse(bytes: &'a [u8]) -> Option<Self> {
        if bytes.get(..4)? != MAGIC {
            return None;
        }
        let count = word(bytes, 4)?;
        let dict_off = word(bytes, 8)?;
        let dict_len = word(bytes, 12)?;
        let keys_off = word(bytes, 16)?;
        let data_off = word(bytes, 20)?;
        if count == 0 || count > MAX_ENTRIES {
            return None;
        }
        let index_len = count.checked_add(1)?.checked_mul(ENTRY_BYTES)?;
        if keys_off != HEADER_BYTES.checked_add(index_len)? {
            return None;
        }
        if dict_off < keys_off || dict_off.checked_add(dict_len)? != data_off {
            return None;
        }
        let keys = bytes.get(keys_off..dict_off)?;
        let dict = bytes.get(dict_off..data_off)?;
        let data = bytes.get(data_off..)?;
        if dict.get(..4)? != DICT_MAGIC {
            return None;
        }

        // Keys follow each other, ascending; frames follow each other without gaps.
        let mut next_key = 0usize;
        let mut next_data = 0usize;
        let mut previous: Option<&[u8]> = None;
        for index in 0..count {
            let (key_off, frame_off, raw_len) = entry(bytes, index)?;
            if key_off != next_key || frame_off != next_data {
                return None;
            }
            if raw_len == 0 || raw_len > MAX_SVG_BYTES {
                return None;
            }
            let key_len = usize::from(*keys.get(key_off)?);
            let key_start = key_off.checked_add(1)?;
            let key = keys.get(key_start..key_start.checked_add(key_len)?)?;
            if !is_stem(key) || previous.is_some_and(|before| before >= key) {
                return None;
            }
            let (_, frame_end, _) = entry(bytes, index + 1)?;
            if frame_end <= frame_off {
                return None;
            }
            next_key = key_start + key_len;
            next_data = frame_end;
            previous = Some(key);
        }
        // The end marker closes both tables exactly.
        let (end_key, end_data, end_raw) = entry(bytes, count)?;
        if (end_key, end_raw) != (0, 0) || end_data != data.len() || next_key != keys.len() {
            return None;
        }

        Some(Pack {
            bytes,
            count,
            keys,
            data,
            dict: DecoderDictionary::copy(dict),
        })
    }

    fn key(&self, index: usize) -> Option<&'a [u8]> {
        if index >= self.count {
            return None;
        }
        let (key_off, _, _) = entry(self.bytes, index)?;
        let len = usize::from(*self.keys.get(key_off)?);
        self.keys.get(key_off + 1..key_off + 1 + len)
    }

    fn find(&self, code: &str) -> Option<usize> {
        let (mut low, mut high) = (0, self.count);
        while low < high {
            let mid = low + (high - low) / 2;
            match self.key(mid)?.cmp(code.as_bytes()) {
                std::cmp::Ordering::Less => low = mid + 1,
                std::cmp::Ordering::Greater => high = mid,
                std::cmp::Ordering::Equal => return Some(mid),
            }
        }
        None
    }

    fn unpack(&self, index: usize) -> Option<Vec<u8>> {
        if index >= self.count {
            return None;
        }
        let (_, start, raw_len) = entry(self.bytes, index)?;
        let (_, end, _) = entry(self.bytes, index + 1)?;
        if raw_len > MAX_SVG_BYTES {
            return None;
        }
        let compressed = self.data.get(start..end)?;
        let mut decompressor = Decompressor::with_prepared_dictionary(&self.dict).ok()?;
        let svg = decompressor.decompress(compressed, raw_len).ok()?;
        (svg.len() == raw_len).then_some(svg)
    }
}

/// The built-in pack, parsed and checked once. `None` if the embedded bytes are bad,
/// which no build should ever ship (a test parses it).
fn pack() -> Option<&'static Pack<'static>> {
    static LOADED: OnceLock<Option<Pack<'static>>> = OnceLock::new();
    LOADED.get_or_init(|| Pack::parse(PACK)).as_ref()
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
    let pack = pack()?;
    let index = match pack.find(&twemoji_code(glyph)) {
        Some(index) => index,
        // Typed text may lack U+FE0F where Twemoji keeps it; use the list's spelling.
        None => pack.find(&twemoji_code(qualified(glyph)?))?,
    };
    pack.unpack(index).map(Cow::Owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    /// SHA-256 of `assets/twemoji.pack`, also in `SOURCES.md`.
    const PACK_SHA256: &str = "b44cac23d6bba745ea11f16a05f1bf63f4f7decafd66d6e5e65e8494761aefb3";
    /// SHA-256 over every drawing, unpacked, in key order: for each, the key, a zero byte,
    /// the length as 8 little endian bytes and the SVG bytes. A script over the unmodified
    /// upstream files gives the same value (see `SOURCES.md`), which is what makes
    /// "the drawings are unchanged" checkable.
    const SVG_DIGEST: &str = "4117b5007770bdabbed271cbdab5ed1faa2e73c0d0b34676dfab285f461a2280";

    fn hex(bytes: impl AsRef<[u8]>) -> String {
        bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
    }

    fn header(name: &str) -> usize {
        match name {
            "count" => 4,
            "dict_off" => 8,
            "dict_len" => 12,
            "keys_off" => 16,
            "data_off" => 20,
            _ => unreachable!(),
        }
    }

    fn put(buf: &mut [u8], at: usize, value: u32) {
        buf[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn get(buf: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(buf[at..at + 4].try_into().unwrap())
    }

    fn entry_at(index: usize) -> usize {
        HEADER_BYTES + index * ENTRY_BYTES
    }

    fn rejected(name: &str, edit: impl FnOnce(&mut Vec<u8>)) {
        let mut copy = PACK.to_vec();
        edit(&mut copy);
        assert!(Pack::parse(&copy).is_none(), "{name} was accepted");
    }

    #[test]
    fn the_embedded_pack_is_valid_and_complete() {
        let pack = Pack::parse(PACK).expect("embedded pack parses");
        assert_eq!(pack.count, 3970);
        assert!(super::pack().is_some());
    }

    #[test]
    fn the_embedded_pack_is_the_pinned_one() {
        assert_eq!(hex(Sha256::digest(PACK)), PACK_SHA256);

        let pack = Pack::parse(PACK).unwrap();
        let mut digest = Sha256::new();
        for index in 0..pack.count {
            let svg = pack.unpack(index).expect("every drawing unpacks");
            digest.update(pack.key(index).unwrap());
            digest.update([0u8]);
            digest.update((svg.len() as u64).to_le_bytes());
            digest.update(&svg);
        }
        assert_eq!(hex(digest.finalize()), SVG_DIGEST);
    }

    #[test]
    fn truncated_packs_are_rejected() {
        let mut lengths: Vec<usize> = (0..200).collect();
        lengths.extend((200..PACK.len()).step_by(1009));
        lengths.push(PACK.len() - 1);
        for len in lengths {
            assert!(Pack::parse(&PACK[..len]).is_none(), "{len} bytes accepted");
        }
        // Extra bytes after the last drawing are not allowed either.
        rejected("trailing byte", |p| p.push(0));
    }

    #[test]
    fn tampered_headers_are_rejected() {
        rejected("magic", |p| p[0] = b'X');
        rejected("empty", |p| p.clear());
        for name in ["count", "dict_off", "dict_len", "keys_off", "data_off"] {
            for delta in [1i64, -1] {
                rejected(&format!("{name} {delta:+}"), |p| {
                    let at = header(name);
                    let value = i64::from(get(p, at)) + delta;
                    put(p, at, u32::try_from(value).unwrap());
                });
            }
            for value in [0, u32::MAX, u32::MAX - 3, 1 << 30] {
                rejected(&format!("{name} = {value}"), |p| {
                    put(p, header(name), value)
                });
            }
        }
        rejected("count past the limit", |p| put(p, header("count"), 1 << 17));
        rejected("dictionary is not zstd", |p| {
            let dict_off = get(p, header("dict_off")) as usize;
            p[dict_off] ^= 0xFF;
        });
    }

    #[test]
    fn tampered_indexes_are_rejected() {
        let n = 3970;
        // Ranges that overlap, run backwards or are empty. (Moving the boundary between two
        // neighbours is a valid layout for parse; the lookup then fails, see below.)
        rejected("overlapping frames", |p| {
            let before = get(p, entry_at(9) + 4);
            put(p, entry_at(10) + 4, before - 1);
        });
        rejected("frames going backwards", |p| {
            let early = get(p, entry_at(3) + 4);
            put(p, entry_at(10) + 4, early);
        });
        rejected("empty frame", |p| {
            let same = get(p, entry_at(10) + 4);
            put(p, entry_at(11) + 4, same);
        });
        rejected("first frame not at zero", |p| put(p, entry_at(0) + 4, 1));
        // Lengths.
        rejected("zero length", |p| put(p, entry_at(5) + 8, 0));
        rejected("length over the limit", |p| {
            put(p, entry_at(5) + 8, MAX_SVG_BYTES as u32 + 1);
        });
        rejected("oversize declared length", |p| {
            put(p, entry_at(5) + 8, u32::MAX);
        });
        rejected("huge declared length", |p| put(p, entry_at(5) + 8, 1 << 31));
        // Key offsets.
        rejected("key offset moved", |p| {
            let at = entry_at(5);
            let value = get(p, at) + 1;
            put(p, at, value);
        });
        rejected("key offset past the table", |p| {
            put(p, entry_at(5), u32::MAX);
        });
        rejected("keys overlap", |p| {
            let before = get(p, entry_at(4));
            put(p, entry_at(5), before);
        });
        // The end marker.
        rejected("end marker data", |p| {
            let at = entry_at(n) + 4;
            let value = get(p, at) - 1;
            put(p, at, value);
        });
        rejected("end marker key", |p| put(p, entry_at(n), 1));
        rejected("end marker length", |p| put(p, entry_at(n) + 8, 1));
    }

    #[test]
    fn tampered_key_tables_are_rejected() {
        let first_key = |p: &[u8]| get(p, header("keys_off")) as usize;
        rejected("length byte too large", |p| {
            let at = first_key(p);
            p[at] = 255;
        });
        rejected("length byte zero", |p| {
            let at = first_key(p);
            p[at] = 0;
        });
        rejected("length byte one short", |p| {
            let at = first_key(p);
            p[at] -= 1;
        });
        rejected("not a file stem", |p| {
            let at = first_key(p) + 1;
            p[at] = b'Z';
        });
        rejected("keys out of order", |p| {
            // Make the first key sort after the second one.
            let at = first_key(p) + 1;
            p[at] = b'f';
        });
    }

    #[test]
    fn bad_sizes_in_a_valid_index_make_a_lookup_fail_cleanly() {
        let valid = Pack::parse(PACK).unwrap();
        let real = u32::try_from(valid.unpack(0).unwrap().len()).unwrap();
        for wrong in [real - 1, real + 1, 1, MAX_SVG_BYTES as u32] {
            let mut copy = PACK.to_vec();
            put(&mut copy, entry_at(0) + 8, wrong);
            let pack = Pack::parse(&copy).expect("a wrong size is not visible to parse");
            assert!(pack.unpack(0).is_none(), "size {wrong} unpacked");
            assert!(pack.unpack(1).is_some());
        }
        // Damaged frame bytes give an error or other bytes, never a panic.
        let mut copy = PACK.to_vec();
        let data_off = get(&copy, header("data_off")) as usize;
        for byte in &mut copy[data_off..data_off + 64] {
            *byte ^= 0xFF;
        }
        let pack = Pack::parse(&copy).unwrap();
        let _ = pack.unpack(0);
        assert!(pack.unpack(pack.count).is_none());
        assert!(pack.find("zz").is_none());
    }

    #[test]
    fn odd_input_gives_none() {
        for glyph in ["", "\u{FE0F}", "\0", "a", "\u{200D}", "\u{1F600}\u{1F600}"] {
            assert!(twemoji_svg(glyph).is_none(), "{glyph:?}");
        }
    }
}
