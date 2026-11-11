// SPDX-License-Identifier: AGPL-3.0-or-later
//! Removing photo metadata (EXIF and XMP: where and when a photo was taken, the camera,
//! often a GPS position) from JPEG, PNG and WebP files before they are uploaded. On by
//! default; the person can turn it off in the attachment tray.
//!
//! Only metadata blocks are taken out; every other byte stays as it was, so the image
//! itself does not change. The one value that changes how a photo looks, its rotation
//! (EXIF orientation), is kept in a minimal EXIF block of its own. A file that cannot be
//! read as one of the three formats is left alone (`None`); the server strips metadata
//! again on its side.

use bytes::Bytes;

const EXIF_HEADER: &[u8] = b"Exif\0\0";
const XMP_HEADER: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
/// Extended XMP: the part of a large XMP block that did not fit in one segment.
const XMP_EXTENSION_HEADER: &[u8] = b"http://ns.adobe.com/xmp/extension/\0";
/// EXIF's orientation tag.
const ORIENTATION: u16 = 0x0112;

/// The file without its photo metadata, or `None` when there is nothing to take out or
/// the file is not a JPEG, PNG or WebP this code can read.
pub fn strip(data: &[u8]) -> Option<Bytes> {
    if data.starts_with(&[0xFF, 0xD8]) {
        strip_jpeg(data)
    } else if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        strip_png(data)
    } else if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        strip_webp(data)
    } else {
        None
    }
}

/// Whether a file of this media type may carry photo metadata this module removes.
pub fn applies_to(content_type: &str) -> bool {
    matches!(content_type, "image/jpeg" | "image/png" | "image/webp")
}

fn u16_at(d: &[u8], at: usize, big: bool) -> Option<u16> {
    let b: [u8; 2] = d.get(at..at + 2)?.try_into().ok()?;
    Some(if big {
        u16::from_be_bytes(b)
    } else {
        u16::from_le_bytes(b)
    })
}

fn u32_at(d: &[u8], at: usize, big: bool) -> Option<u32> {
    let b: [u8; 4] = d.get(at..at + 4)?.try_into().ok()?;
    Some(if big {
        u32::from_be_bytes(b)
    } else {
        u32::from_le_bytes(b)
    })
}

/// The orientation (2 to 8) in a TIFF structure (the body of an EXIF block), or `None`
/// for the default (1) or anything unreadable.
fn orientation(tiff: &[u8]) -> Option<u16> {
    let big = match tiff.get(..4)? {
        b"MM\0*" => true,
        b"II*\0" => false,
        _ => return None,
    };
    let ifd = u32_at(tiff, 4, big)? as usize;
    let count = u16_at(tiff, ifd, big)? as usize;
    (0..count.min(512)).find_map(|i| {
        let entry = ifd + 2 + i * 12;
        if u16_at(tiff, entry, big)? != ORIENTATION || u16_at(tiff, entry + 2, big)? != 3 {
            return None;
        }
        let value = u16_at(tiff, entry + 8, big)?;
        (2..=8).contains(&value).then_some(value)
    })
}

/// A TIFF structure with only the orientation in it.
fn orientation_only(value: u16) -> Vec<u8> {
    let mut t = Vec::with_capacity(26);
    t.extend_from_slice(b"MM\0*");
    t.extend_from_slice(&8u32.to_be_bytes());
    t.extend_from_slice(&1u16.to_be_bytes());
    t.extend_from_slice(&ORIENTATION.to_be_bytes());
    t.extend_from_slice(&3u16.to_be_bytes());
    t.extend_from_slice(&1u32.to_be_bytes());
    t.extend_from_slice(&value.to_be_bytes());
    t.extend_from_slice(&[0, 0]);
    t.extend_from_slice(&0u32.to_be_bytes());
    t
}

/// The end of the entropy-coded data that starts at `from`: the next marker that is
/// not a stuffed `FF 00` or a restart marker.
fn scan_end(d: &[u8], from: usize) -> usize {
    let mut i = from;
    while i + 1 < d.len() {
        if d[i] == 0xFF && d[i + 1] != 0x00 && !(0xD0..=0xD7).contains(&d[i + 1]) {
            return i;
        }
        i += 1;
    }
    d.len()
}

fn strip_jpeg(d: &[u8]) -> Option<Bytes> {
    let mut out = Vec::with_capacity(d.len());
    out.extend_from_slice(&d[..2]);
    let mut at = 2;
    let mut removed = false;
    let mut kept_orientation = false;
    loop {
        // Markers may be padded with extra 0xFF bytes.
        let mut m = at;
        while d.get(m) == Some(&0xFF) && d.get(m + 1) == Some(&0xFF) {
            m += 1;
        }
        if d.get(m) != Some(&0xFF) {
            return None;
        }
        let marker = *d.get(m + 1)?;
        if marker == 0xD9 {
            // End of image. What follows (the other pictures of a multi-picture file,
            // which carry their own metadata) is not sent.
            out.extend_from_slice(&d[at..m + 2]);
            removed |= m + 2 < d.len();
            break;
        }
        // Markers without a length.
        if (0xD0..=0xD7).contains(&marker) || marker == 0x01 {
            out.extend_from_slice(&d[at..m + 2]);
            at = m + 2;
            continue;
        }
        let len = u16_at(d, m + 2, true)? as usize;
        if len < 2 {
            return None;
        }
        let end = m + 2 + len;
        let body = d.get(m + 4..end)?;
        if marker == 0xDA {
            // A scan: its header, then the image data up to the next marker.
            let data_end = scan_end(d, end);
            out.extend_from_slice(&d[at..data_end]);
            at = data_end;
            if at >= d.len() {
                break;
            }
            continue;
        }
        let exif = marker == 0xE1 && body.starts_with(EXIF_HEADER);
        let xmp = marker == 0xE1
            && (body.starts_with(XMP_HEADER) || body.starts_with(XMP_EXTENSION_HEADER));
        // The index of the other pictures, which are not sent.
        let mpf = marker == 0xE2 && body.starts_with(b"MPF\0");
        if exif || xmp || mpf {
            removed = true;
            if exif && !kept_orientation {
                if let Some(value) = orientation(&body[EXIF_HEADER.len()..]) {
                    let tiff = orientation_only(value);
                    let seg_len = (2 + EXIF_HEADER.len() + tiff.len()) as u16;
                    out.extend_from_slice(&[0xFF, 0xE1]);
                    out.extend_from_slice(&seg_len.to_be_bytes());
                    out.extend_from_slice(EXIF_HEADER);
                    out.extend_from_slice(&tiff);
                    kept_orientation = true;
                }
            }
        } else {
            out.extend_from_slice(&d[at..end]);
        }
        at = end;
    }
    removed.then(|| Bytes::from(out))
}

/// CRC-32 (ISO-HDLC), for the PNG chunk this module writes.
fn crc32(parts: &[&[u8]]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for part in parts {
        for &byte in *part {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
    }
    !crc
}

fn strip_png(d: &[u8]) -> Option<Bytes> {
    let mut out = Vec::with_capacity(d.len());
    out.extend_from_slice(&d[..8]);
    let mut at = 8;
    let mut removed = false;
    while at < d.len() {
        let len = u32_at(d, at, true)? as usize;
        let kind = d.get(at + 4..at + 8)?;
        let end = at.checked_add(12)?.checked_add(len)?;
        let data = d.get(at + 8..at + 8 + len)?;
        if end > d.len() {
            return None;
        }
        let xmp = kind == b"iTXt" && data.starts_with(b"XML:com.adobe.xmp\0");
        if kind == b"eXIf" || xmp {
            removed = true;
            if kind == b"eXIf" {
                if let Some(value) = orientation(data) {
                    let tiff = orientation_only(value);
                    out.extend_from_slice(&(tiff.len() as u32).to_be_bytes());
                    out.extend_from_slice(b"eXIf");
                    out.extend_from_slice(&tiff);
                    out.extend_from_slice(&crc32(&[b"eXIf", &tiff]).to_be_bytes());
                }
            }
        } else {
            out.extend_from_slice(&d[at..end]);
        }
        at = end;
        if kind == b"IEND" {
            out.extend_from_slice(&d[at..]);
            break;
        }
    }
    removed.then(|| Bytes::from(out))
}

const VP8X_EXIF: u8 = 0x08;
const VP8X_XMP: u8 = 0x04;

fn strip_webp(d: &[u8]) -> Option<Bytes> {
    let riff_end = (u32_at(d, 4, false)? as usize).checked_add(8)?.min(d.len());
    let mut chunks: Vec<Vec<u8>> = Vec::new();
    let mut at = 12;
    let mut removed = false;
    let mut kept: Option<Vec<u8>> = None;
    while at + 8 <= riff_end {
        let kind = &d[at..at + 4];
        let len = u32_at(d, at + 4, false)? as usize;
        let padded = len + (len & 1);
        let end = (at + 8).checked_add(padded)?.min(riff_end);
        let data = d.get(at + 8..(at + 8 + len).min(riff_end))?;
        match kind {
            b"EXIF" => {
                removed = true;
                let tiff = data.strip_prefix(EXIF_HEADER).unwrap_or(data);
                if let Some(value) = orientation(tiff) {
                    let tiff = orientation_only(value);
                    let mut chunk = b"EXIF".to_vec();
                    chunk.extend_from_slice(&(tiff.len() as u32).to_le_bytes());
                    chunk.extend_from_slice(&tiff);
                    kept = Some(chunk);
                }
            }
            b"XMP " => removed = true,
            _ => chunks.push(d[at..end].to_vec()),
        }
        at = end;
    }
    if !removed {
        return None;
    }
    // The extended header says which metadata chunks follow.
    for chunk in &mut chunks {
        if chunk.starts_with(b"VP8X") && chunk.len() > 8 {
            chunk[8] &= !(VP8X_EXIF | VP8X_XMP);
            if kept.is_some() {
                chunk[8] |= VP8X_EXIF;
            }
        }
    }
    // EXIF goes after the image data.
    chunks.extend(kept);
    let body: usize = chunks.iter().map(Vec::len).sum();
    let mut out = Vec::with_capacity(12 + body);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&((4 + body) as u32).to_le_bytes());
    out.extend_from_slice(b"WEBP");
    for chunk in chunks {
        out.extend_from_slice(&chunk);
    }
    Some(Bytes::from(out))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// An EXIF body (TIFF, little-endian) with a GPS pointer and an orientation.
    fn tiff_with_gps(orientation: u16) -> Vec<u8> {
        let mut t = b"II*\0".to_vec();
        t.extend_from_slice(&8u32.to_le_bytes());
        t.extend_from_slice(&2u16.to_le_bytes());
        // Orientation.
        t.extend_from_slice(&0x0112u16.to_le_bytes());
        t.extend_from_slice(&3u16.to_le_bytes());
        t.extend_from_slice(&1u32.to_le_bytes());
        t.extend_from_slice(&orientation.to_le_bytes());
        t.extend_from_slice(&[0, 0]);
        // GPS IFD pointer, and a marker that must not survive.
        t.extend_from_slice(&0x8825u16.to_le_bytes());
        t.extend_from_slice(&4u16.to_le_bytes());
        t.extend_from_slice(&1u32.to_le_bytes());
        t.extend_from_slice(&38u32.to_le_bytes());
        t.extend_from_slice(&0u32.to_le_bytes());
        t.extend_from_slice(b"GPS 59.91N 10.75E");
        t
    }

    /// A small JPEG: SOI, an EXIF block, an XMP block, a quantisation table and the
    /// scan data with its end.
    pub(crate) fn jpeg_with_exif(orientation: u16) -> Vec<u8> {
        let mut d = vec![0xFF, 0xD8];
        let mut app1 = EXIF_HEADER.to_vec();
        app1.extend_from_slice(&tiff_with_gps(orientation));
        d.extend_from_slice(&[0xFF, 0xE1]);
        d.extend_from_slice(&((app1.len() + 2) as u16).to_be_bytes());
        d.extend_from_slice(&app1);
        let mut xmp = XMP_HEADER.to_vec();
        xmp.extend_from_slice(b"<x:xmpmeta>GPS</x:xmpmeta>");
        d.extend_from_slice(&[0xFF, 0xE1]);
        d.extend_from_slice(&((xmp.len() + 2) as u16).to_be_bytes());
        d.extend_from_slice(&xmp);
        d.extend_from_slice(&[0xFF, 0xDB, 0x00, 0x05, 1, 2, 3]);
        d.extend_from_slice(&[
            0xFF, 0xDA, 0x00, 0x04, 9, 9, 0x12, 0x34, 0xFF, 0x00, 0xFF, 0xD9,
        ]);
        d
    }

    fn contains(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn exif_is_removed_and_the_image_bytes_stay() {
        // A rotated photo: the position goes, the rotation stays.
        let input = jpeg_with_exif(6);
        let out = strip(&input).expect("stripped");
        assert!(!contains(&out, b"GPS"), "no position left");
        assert!(!contains(&out, XMP_HEADER));
        assert!(out.starts_with(&[0xFF, 0xD8, 0xFF, 0xE1]));
        assert_eq!(orientation(&out[12..]), Some(6), "the rotation is kept");
        let tail = &input[input.len() - 19..];
        assert!(out.ends_with(tail), "the tables and the scan are untouched");
        // An upright photo keeps no EXIF at all.
        let out = strip(&jpeg_with_exif(1)).unwrap();
        assert!(!contains(&out, EXIF_HEADER));
        assert_eq!(&out[..], &[&[0xFF, 0xD8][..], tail].concat()[..]);
        // Extended XMP goes too, and so do the other pictures of a multi-picture file
        // (with their own metadata) after the end of the first.
        let mut multi = jpeg_with_exif(1);
        let mut ext = XMP_EXTENSION_HEADER.to_vec();
        ext.extend_from_slice(b"GPS extended");
        let ext_seg = [
            &[0xFF, 0xE1][..],
            &((ext.len() + 2) as u16).to_be_bytes(),
            &ext,
        ]
        .concat();
        multi.splice(2..2, ext_seg);
        multi.extend_from_slice(&jpeg_with_exif(6));
        let out = strip(&multi).unwrap();
        assert!(!contains(&out, b"GPS"));
        assert_eq!(
            &out[..],
            &[&[0xFF, 0xD8][..], tail].concat()[..],
            "the first picture only"
        );
        // Nothing to remove: `None` (the file is sent as it is).
        assert!(strip(&out).is_none());
        assert!(strip(b"not an image").is_none());
    }

    #[test]
    fn png_and_webp_lose_their_metadata_chunks_only() {
        let png = crate::backend::media::png::solid(2, 2, [1, 2, 3, 255]);
        // Insert an eXIf chunk before IDAT.
        let tiff = tiff_with_gps(1);
        let mut chunk = (tiff.len() as u32).to_be_bytes().to_vec();
        chunk.extend_from_slice(b"eXIf");
        chunk.extend_from_slice(&tiff);
        chunk.extend_from_slice(&crc32(&[b"eXIf", &tiff]).to_be_bytes());
        let ihdr_end = 8 + 12 + 13;
        let mut with = png[..ihdr_end].to_vec();
        with.extend_from_slice(&chunk);
        with.extend_from_slice(&png[ihdr_end..]);
        assert_eq!(
            strip(&with).as_deref(),
            Some(&png[..]),
            "byte for byte the original"
        );

        // WebP: a VP8L image with EXIF and XMP chunks and the flags set.
        let mut body = b"WEBP".to_vec();
        body.extend_from_slice(b"VP8X");
        body.extend_from_slice(&10u32.to_le_bytes());
        body.extend_from_slice(&[VP8X_EXIF | VP8X_XMP | 0x10, 0, 0, 0, 1, 0, 0, 1, 0, 0]);
        body.extend_from_slice(b"VP8L");
        body.extend_from_slice(&3u32.to_le_bytes());
        body.extend_from_slice(&[7, 7, 7, 0]);
        body.extend_from_slice(b"EXIF");
        body.extend_from_slice(&(tiff.len() as u32).to_le_bytes());
        body.extend_from_slice(&tiff);
        if tiff.len() % 2 == 1 {
            body.push(0);
        }
        body.extend_from_slice(b"XMP ");
        body.extend_from_slice(&4u32.to_le_bytes());
        body.extend_from_slice(b"GPS!");
        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&(body.len() as u32).to_le_bytes());
        webp.extend_from_slice(&body);
        let out = strip(&webp).unwrap();
        assert!(!contains(&out, b"GPS"));
        assert_eq!(out[20] & (VP8X_EXIF | VP8X_XMP), 0, "flags cleared");
        assert_eq!(out[20] & 0x10, 0x10, "other flags kept");
        assert_eq!(u32_at(&out, 4, false), Some(out.len() as u32 - 8));
        assert!(contains(
            &out,
            &[b"VP8L".as_slice(), &3u32.to_le_bytes(), &[7, 7, 7, 0]].concat()
        ));
    }
}
