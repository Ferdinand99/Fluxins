// SPDX-License-Identifier: AGPL-3.0-or-later
//! Builds `assets/twemoji.pack` from a directory of unmodified Twemoji SVG files
//! (`assets/svg` of jdecked/twemoji). Only the drawings of glyphs in the emoji list are
//! packed. The layout is described in `src/twemoji.rs`.
//!
//! ```text
//! cargo run --release -p inlet-emoji --example pack_twemoji -- <svg dir> <out file>
//! ```
//!
//! The output is reproducible: the same files give the same bytes.

use std::collections::BTreeMap;
use std::error::Error;
use std::{env, fs, path::PathBuf};

const LEVEL: i32 = 19;
const DICT_SIZES: [usize; 3] = [64 << 10, 112 << 10, 160 << 10];

fn push_word(out: &mut Vec<u8>, value: usize) -> Result<(), Box<dyn Error>> {
    out.extend_from_slice(&u32::try_from(value)?.to_le_bytes());
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = env::args().skip(1);
    let (Some(svg_dir), Some(out_path)) = (args.next(), args.next()) else {
        return Err("usage: pack_twemoji <svg dir> <out file>".into());
    };
    let svg_dir = PathBuf::from(svg_dir);

    // File stem -> SVG bytes, sorted by stem.
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for glyph in inlet_emoji::glyphs() {
        let code = inlet_emoji::twemoji_code(glyph);
        if files.contains_key(&code) {
            continue;
        }
        let path = svg_dir.join(format!("{code}.svg"));
        let bytes = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        files.insert(code, bytes);
    }
    let raw_total: usize = files.values().map(Vec::len).sum();
    println!("{} drawings, {raw_total} bytes raw", files.len());

    let samples: Vec<&[u8]> = files.values().map(Vec::as_slice).collect();
    let mut best: Option<(usize, Vec<u8>, Vec<Vec<u8>>)> = None;
    for dict_size in DICT_SIZES {
        let dict = zstd::dict::from_samples(&samples, dict_size)?;
        let mut compressor = zstd::bulk::Compressor::with_dictionary(LEVEL, &dict)?;
        // Frames carry no dictionary id, size or checksum: the pack has exactly one
        // dictionary and stores the sizes in its index.
        compressor.include_dictid(false)?;
        compressor.include_checksum(false)?;
        compressor.include_contentsize(false)?;
        let frames = samples
            .iter()
            .map(|sample| compressor.compress(sample))
            .collect::<Result<Vec<_>, _>>()?;
        let total = dict.len() + frames.iter().map(Vec::len).sum::<usize>();
        println!("dictionary {dict_size}: {total} bytes");
        if best.as_ref().is_none_or(|(size, _, _)| total < *size) {
            best = Some((total, dict, frames));
        }
    }
    let (_, dict, frames) = best.ok_or("no dictionary size given")?;

    let count = files.len();
    let header_words = 7;
    let index_bytes = (count + 1) * 12;
    let mut keys = Vec::new();
    let mut key_offsets = Vec::new();
    for key in files.keys() {
        key_offsets.push(keys.len());
        keys.push(u8::try_from(key.len())?);
        keys.extend_from_slice(key.as_bytes());
    }
    let keys_off = header_words * 4 + index_bytes;
    let dict_off = keys_off + keys.len();
    let data_off = dict_off + dict.len();

    let mut out = Vec::new();
    out.extend_from_slice(b"ITWP");
    for word in [count, dict_off, dict.len(), keys_off, data_off, 0] {
        push_word(&mut out, word)?;
    }
    // The seventh header word is reserved (zero); the lookup code ignores it.
    let mut offset = 0;
    for (index, raw) in files.values().enumerate() {
        push_word(&mut out, key_offsets[index])?;
        push_word(&mut out, offset)?;
        push_word(&mut out, raw.len())?;
        offset += frames[index].len();
    }
    push_word(&mut out, 0)?;
    push_word(&mut out, offset)?;
    push_word(&mut out, 0)?;
    out.extend_from_slice(&keys);
    out.extend_from_slice(&dict);
    for frame in &frames {
        out.extend_from_slice(frame);
    }
    fs::write(&out_path, &out)?;
    println!("wrote {out_path}: {} bytes", out.len());
    Ok(())
}
