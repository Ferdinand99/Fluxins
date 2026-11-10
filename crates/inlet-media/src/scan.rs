//! Cheap structural scans of the container formats, run before any decoder sees
//! the data. The decoders trust sizes that the file declares about itself; these
//! scans check every one of them against the real length of the file, and refuse
//! or strip what would make a decoder allocate far more than the file holds.
//!
//! Every scan is a single linear pass over the bytes, bounded by the encoded size.

use crate::{KindLimits, MediaError};

fn corrupt(what: &str) -> MediaError {
    MediaError::Corrupt(what.to_owned())
}

// ---- PNG --------------------------------------------------------------------------

/// Ancillary chunks above this size are dropped, whatever they are.
const PNG_MAX_ANCILLARY: usize = 64 * 1024;

/// Chunks that are needed for the picture although their names say "ancillary".
const PNG_KEEP: [&[u8; 4]; 4] = [b"tRNS", b"acTL", b"fcTL", b"fdAT"];

/// Chunks that hold zlib data. The png decoder inflates them with no limit that
/// the caller can set (a 2 MiB file can claim 2 GiB), and gpui ignores colour
/// profiles anyway, so they are always dropped.
const PNG_DROP: [&[u8; 4]; 3] = [b"iCCP", b"zTXt", b"iTXt"];

/// Walks the PNG chunks and returns the file without `iCCP`, `zTXt`, `iTXt` and
/// large ancillary chunks, or `None` when nothing had to go. Chunks that run
/// past the end of the file are an error.
pub(crate) fn scan_png(data: &[u8]) -> Result<Option<Vec<u8>>, MediaError> {
    if data.len() < 8 || &data[..8] != b"\x89PNG\r\n\x1a\n" {
        return Err(corrupt("not a PNG"));
    }
    let mut pos = 8usize;
    let mut dropped: Vec<(usize, usize)> = Vec::new();
    while pos + 8 <= data.len() {
        let length = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
        if length > 0x7FFF_FFFF {
            return Err(corrupt("PNG chunk too long"));
        }
        let length = length as usize;
        let name: [u8; 4] = [data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]];
        // length + type + CRC
        let end = pos
            .checked_add(12)
            .and_then(|p| p.checked_add(length))
            .filter(|&end| end <= data.len())
            .ok_or_else(|| corrupt("PNG chunk runs past the end of the file"))?;
        let ancillary = name[0] & 0x20 != 0;
        let drop = PNG_DROP.contains(&&name)
            || (ancillary && length > PNG_MAX_ANCILLARY && !PNG_KEEP.contains(&&name));
        if drop {
            dropped.push((pos, end));
        }
        pos = end;
        if &name == b"IEND" {
            break;
        }
    }
    if dropped.is_empty() {
        return Ok(None);
    }
    let mut out = Vec::with_capacity(data.len());
    let mut from = 0;
    for (start, end) in dropped {
        out.extend_from_slice(&data[from..start]);
        from = end;
    }
    out.extend_from_slice(&data[from..]);
    Ok(Some(out))
}

// ---- WebP -------------------------------------------------------------------------

/// Checks the RIFF structure: the header, and every chunk size (also inside
/// `ANMF` frames) against the bytes that are really there. image-webp allocates
/// the size a chunk declares, up to 4 GiB, before it reads a byte.
pub(crate) fn scan_webp(data: &[u8], limits: &KindLimits) -> Result<(), MediaError> {
    if data.len() < 12 || &data[..4] != b"RIFF" || &data[8..12] != b"WEBP" {
        return Err(corrupt("not a WebP file"));
    }
    let mut frames = 0usize;
    walk_riff(&data[12..], false, &mut frames, limits)
}

fn walk_riff(
    mut data: &[u8],
    inside_frame: bool,
    frames: &mut usize,
    limits: &KindLimits,
) -> Result<(), MediaError> {
    while data.len() >= 8 {
        let size = u32::from_le_bytes([data[4], data[5], data[6], data[7]]) as usize;
        let body = data.get(8..8usize.saturating_add(size));
        let Some(body) = body else {
            return Err(corrupt("WebP chunk runs past the end of the file"));
        };
        if &data[..4] == b"ANMF" && !inside_frame {
            *frames += 1;
            if *frames > limits.max_frames {
                return Err(MediaError::TooManyFrames {
                    max: limits.max_frames,
                });
            }
            // 16 bytes of frame header, then the frame's own chunks.
            if body.len() < 16 {
                return Err(corrupt("WebP frame is too short"));
            }
            walk_riff(&body[16..], true, frames, limits)?;
        }
        // Chunks are padded to an even size. The last pad byte may be missing.
        let step = 8 + size + (size & 1);
        data = data.get(step..).unwrap_or(&[]);
    }
    Ok(())
}

// ---- GIF --------------------------------------------------------------------------

/// What the GIF walk found.
pub(crate) struct GifScan {
    pub width: u32,
    pub height: u32,
    /// Frames that have pixels (frames of zero size do not count).
    pub frames: usize,
    /// The largest area of one frame, which the decoder allocates next to the canvas.
    pub largest_frame_pixels: u64,
}

fn le16(data: &[u8], at: usize) -> u32 {
    u32::from(u16::from_le_bytes([data[at], data[at + 1]]))
}

/// Walks the GIF blocks. It reads the canvas size, then counts frames and adds up
/// their areas, all before decoding. The decoder allocates for the declared size of
/// every frame, and a 1x1 canvas can carry hundreds of 3500x3500 frames in a few
/// hundred KiB.
///
/// As lenient as a browser where that costs no memory: a frame that overhangs the
/// canvas is clipped by the decoder (its whole area still counts, and it has to be
/// within `max_pixels` like the canvas), a frame of zero size is skipped, and an
/// unknown block or junk after at least one frame ends the file. A file that ends
/// in the middle of a block shows the frames before it.
pub(crate) fn scan_gif(data: &[u8], limits: &KindLimits) -> Result<GifScan, MediaError> {
    if data.len() < 13 || &data[..3] != b"GIF" {
        return Err(corrupt("not a GIF"));
    }
    let width = le16(data, 6);
    let height = le16(data, 8);
    crate::decode::check_dimensions(width, height, limits)?;
    let flags = data[10];
    let mut pos = 13usize;
    if flags & 0x80 != 0 {
        pos += 3usize << ((flags & 7) + 1);
    }

    let mut frames = 0usize;
    let mut area: u64 = 0;
    let mut largest: u64 = 0;
    while let Some(&block) = data.get(pos) {
        match block {
            0x3B => break,
            0x21 => {
                // Extension: label, then data sub-blocks.
                pos = match skip_sub_blocks(data, pos + 2) {
                    Some(next) => next,
                    None => break,
                };
            }
            0x2C => {
                if pos + 10 > data.len() {
                    break;
                }
                let w = le16(data, pos + 5);
                let h = le16(data, pos + 7);
                let local_flags = data[pos + 9];
                let pixels = u64::from(w) * u64::from(h);
                if pixels > 0 {
                    // The decoder allocates the frame itself, whatever the canvas is.
                    if pixels > limits.max_pixels {
                        return Err(MediaError::BadDimensions {
                            width: w,
                            height: h,
                        });
                    }
                    frames += 1;
                    if frames > limits.max_frames {
                        return Err(MediaError::TooManyFrames {
                            max: limits.max_frames,
                        });
                    }
                    area += pixels;
                    if area > limits.max_animation_work_pixels {
                        return Err(MediaError::AnimationTooLarge);
                    }
                    largest = largest.max(pixels);
                }
                pos += 10;
                if local_flags & 0x80 != 0 {
                    pos += 3usize << ((local_flags & 7) + 1);
                }
                // LZW minimum code size, then the data sub-blocks.
                pos = match skip_sub_blocks(data, pos + 1) {
                    Some(next) => next,
                    None => break,
                };
            }
            // Junk, or a block this scan does not know: the file ends here. What
            // came before it is shown. Junk before any frame is not a GIF.
            _ if frames > 0 => break,
            _ => return Err(corrupt("unknown GIF block")),
        }
    }
    Ok(GifScan {
        width,
        height,
        frames,
        largest_frame_pixels: largest,
    })
}

/// Returns the position after the terminating empty sub-block, or `None` when
/// the data ends first.
fn skip_sub_blocks(data: &[u8], mut pos: usize) -> Option<usize> {
    loop {
        let len = usize::from(*data.get(pos)?);
        pos += 1;
        if len == 0 {
            return Some(pos);
        }
        pos += len;
        if pos > data.len() {
            return None;
        }
    }
}
