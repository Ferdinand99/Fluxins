// SPDX-License-Identifier: AGPL-3.0-or-later
//! Removing photo metadata (where and when a photo was taken, the camera, the
//! photographer, often a GPS position) from JPEG, PNG and WebP files before they are
//! uploaded. On by default; the person can turn it off in the attachment tray, or send
//! one file without it.
//!
//! What goes:
//! - JPEG: EXIF, XMP and extended XMP (APP1), Photoshop/IPTC (APP13), comments (COM),
//!   the multi-picture index (APP2 MPF) and everything after the end of the first
//!   image (the other pictures of a multi-picture file, trailers).
//! - PNG: `eXIf`, every text chunk (`tEXt`, `zTXt`, `iTXt`: XMP, "Raw profile" EXIF in
//!   hex, comments), `tIME`, and anything after `IEND`.
//! - WebP: the `EXIF` and `XMP ` chunks, with the extended header's flags fixed.
//!
//! Only metadata blocks are taken out; every byte of the image data stays as it was,
//! so the pixels do not change. The one value that changes how a photo looks, its
//! rotation (EXIF orientation), is kept in a minimal EXIF block of its own.
//!
//! The file type comes from the bytes, not the name ([`sniff`]). It fails closed: a
//! file of these types that cannot be read is an [`Error::Unreadable`], and HEIF/AVIF
//! and TIFF, which this module cannot clean, are [`Error::Unsupported`]. The upload
//! then refuses to send the original unless the person chose to send that file with
//! its metadata.
//!
//! The result is a list of [`Piece`]s: ranges of the input to keep and a few new
//! bytes. A large image is therefore held once (the input), never twice.

use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(test)]
use bytes::{Bytes, BytesMut};

const EXIF_HEADER: &[u8] = b"Exif\0\0";
const XMP_HEADER: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
/// Extended XMP: the part of a large XMP block that did not fit in one segment.
const XMP_EXTENSION_HEADER: &[u8] = b"http://ns.adobe.com/xmp/extension/\0";
/// APP13: Photoshop resources with IPTC (city, location, the photographer's name).
const PHOTOSHOP_HEADER: &[u8] = b"Photoshop 3.0\0";
/// EXIF's orientation tag.
const ORIENTATION: u16 = 0x0112;
/// How many bytes the scan of a JPEG's image data runs between looks at Cancel.
const CANCEL_EVERY: usize = 1 << 20;

/// What a file is, from its first bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Jpeg,
    Png,
    Webp,
    /// HEIC, HEIF and AVIF (ISO base media with an image brand).
    Heif,
    Tiff,
    /// Anything else: no photo metadata this module knows of.
    Other,
}

/// The kind of a file from its first 32 bytes (or fewer).
pub fn sniff(head: &[u8]) -> Kind {
    if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Kind::Jpeg
    } else if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        Kind::Png
    } else if head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WEBP" {
        Kind::Webp
    } else if head.starts_with(b"II*\0") || head.starts_with(b"MM\0*") {
        Kind::Tiff
    } else if head.len() >= 12 && &head[4..8] == b"ftyp" {
        let brands = &head[8..head.len().min(32)];
        let image: [&[u8]; 9] = [
            b"heic", b"heix", b"hevc", b"heim", b"heis", b"mif1", b"msf1", b"avif", b"avis",
        ];
        if brands.chunks(4).any(|b| image.contains(&b)) {
            Kind::Heif
        } else {
            Kind::Other
        }
    } else {
        Kind::Other
    }
}

/// Why metadata could not be removed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The file looks like an image of a supported type but cannot be read.
    Unreadable,
    /// An image type that can carry metadata this module cannot remove.
    Unsupported,
    Cancelled,
}

/// A piece of the cleaned file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Piece {
    /// These bytes of the input.
    Keep(Range<usize>),
    /// New bytes (a header, a minimal EXIF block).
    New(Vec<u8>),
}

/// The cleaned file as pieces, or `None` when there is nothing to remove (send the
/// input as it is). The input is never copied.
pub fn plan(data: &[u8], cancel: &AtomicBool) -> Result<Option<Vec<Piece>>, Error> {
    match sniff(&data[..data.len().min(32)]) {
        Kind::Jpeg => plan_jpeg(data, cancel),
        Kind::Png => plan_png(data, cancel),
        Kind::Webp => plan_webp(data, cancel),
        Kind::Heif | Kind::Tiff => Err(Error::Unsupported),
        Kind::Other => Ok(None),
    }
}

/// The length of the cleaned file.
#[cfg(test)]
pub fn planned_len(pieces: &[Piece]) -> usize {
    pieces
        .iter()
        .map(|p| match p {
            Piece::Keep(r) => r.len(),
            Piece::New(b) => b.len(),
        })
        .sum()
}

/// The cleaned file as one buffer (tests). `None`: nothing to remove.
#[cfg(test)]
pub fn strip(data: &[u8]) -> Result<Option<Bytes>, Error> {
    let Some(pieces) = plan(data, &AtomicBool::new(false))? else {
        return Ok(None);
    };
    let mut out = BytesMut::with_capacity(planned_len(&pieces));
    for p in &pieces {
        match p {
            Piece::Keep(r) => out.extend_from_slice(&data[r.clone()]),
            Piece::New(b) => out.extend_from_slice(b),
        }
    }
    Ok(Some(out.freeze()))
}

/// Pieces, with adjacent kept ranges merged.
#[derive(Default)]
struct Pieces(Vec<Piece>);

impl Pieces {
    fn keep(&mut self, r: Range<usize>) {
        if r.is_empty() {
            return;
        }
        if let Some(Piece::Keep(last)) = self.0.last_mut() {
            if last.end == r.start {
                last.end = r.end;
                return;
            }
        }
        self.0.push(Piece::Keep(r));
    }

    fn add(&mut self, b: Vec<u8>) {
        self.0.push(Piece::New(b));
    }
}

fn check(cancel: &AtomicBool) -> Result<(), Error> {
    if cancel.load(Ordering::Relaxed) {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

fn u16_at(d: &[u8], at: usize, big: bool) -> Option<u16> {
    let b: [u8; 2] = d.get(at..at.checked_add(2)?)?.try_into().ok()?;
    Some(if big {
        u16::from_be_bytes(b)
    } else {
        u16::from_le_bytes(b)
    })
}

fn u32_at(d: &[u8], at: usize, big: bool) -> Option<u32> {
    let b: [u8; 4] = d.get(at..at.checked_add(4)?)?.try_into().ok()?;
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
        let entry = ifd.checked_add(2 + i * 12)?;
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
fn scan_end(d: &[u8], from: usize, cancel: &AtomicBool) -> Result<usize, Error> {
    let mut i = from;
    while i + 1 < d.len() {
        if (i - from).is_multiple_of(CANCEL_EVERY) {
            check(cancel)?;
        }
        if d[i] == 0xFF && d[i + 1] != 0x00 && !(0xD0..=0xD7).contains(&d[i + 1]) {
            return Ok(i);
        }
        i += 1;
    }
    Ok(d.len())
}

fn plan_jpeg(d: &[u8], cancel: &AtomicBool) -> Result<Option<Vec<Piece>>, Error> {
    let bad = Error::Unreadable;
    let mut out = Pieces::default();
    out.keep(0..2);
    let mut at = 2;
    let mut removed = false;
    let mut kept_orientation = false;
    loop {
        check(cancel)?;
        // Markers may be padded with extra 0xFF bytes.
        let mut m = at;
        while d.get(m) == Some(&0xFF) && d.get(m + 1) == Some(&0xFF) {
            m += 1;
        }
        if m >= d.len() {
            // Ends without an end-of-image marker: what is there goes as it is.
            break;
        }
        if d.get(m) != Some(&0xFF) {
            return Err(bad);
        }
        let marker = *d.get(m + 1).ok_or(bad)?;
        if marker == 0xD9 {
            // End of image. What follows (the other pictures of a multi-picture file,
            // which carry their own metadata) is not sent.
            out.keep(at..m + 2);
            removed |= m + 2 < d.len();
            break;
        }
        // Markers without a length.
        if (0xD0..=0xD7).contains(&marker) || marker == 0x01 {
            out.keep(at..m + 2);
            at = m + 2;
            continue;
        }
        let len = u16_at(d, m + 2, true).ok_or(bad)? as usize;
        if len < 2 {
            return Err(bad);
        }
        let end = m + 2 + len;
        let body = d.get(m + 4..end).ok_or(bad)?;
        if marker == 0xDA {
            // A scan: its header, then the image data up to the next marker.
            let data_end = scan_end(d, end, cancel)?;
            out.keep(at..data_end);
            at = data_end;
            if at >= d.len() {
                break;
            }
            continue;
        }
        let exif = marker == 0xE1 && body.starts_with(EXIF_HEADER);
        let metadata = exif
            || (marker == 0xE1
                && (body.starts_with(XMP_HEADER) || body.starts_with(XMP_EXTENSION_HEADER)))
            || (marker == 0xE2 && body.starts_with(b"MPF\0"))
            || (marker == 0xED && body.starts_with(PHOTOSHOP_HEADER))
            || marker == 0xFE;
        if metadata {
            removed = true;
            if exif && !kept_orientation {
                if let Some(value) = orientation(&body[EXIF_HEADER.len()..]) {
                    let tiff = orientation_only(value);
                    let seg_len = (2 + EXIF_HEADER.len() + tiff.len()) as u16;
                    let mut seg = vec![0xFF, 0xE1];
                    seg.extend_from_slice(&seg_len.to_be_bytes());
                    seg.extend_from_slice(EXIF_HEADER);
                    seg.extend_from_slice(&tiff);
                    out.add(seg);
                    kept_orientation = true;
                }
            }
        } else {
            out.keep(at..end);
        }
        at = end;
    }
    Ok(removed.then_some(out.0))
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

fn plan_png(d: &[u8], cancel: &AtomicBool) -> Result<Option<Vec<Piece>>, Error> {
    let bad = Error::Unreadable;
    let mut out = Pieces::default();
    out.keep(0..8);
    let mut at = 8;
    let mut removed = false;
    loop {
        check(cancel)?;
        let len = u32_at(d, at, true).ok_or(bad)? as usize;
        let kind = d.get(at + 4..at + 8).ok_or(bad)?;
        let end = at
            .checked_add(12)
            .and_then(|e| e.checked_add(len))
            .ok_or(bad)?;
        if end > d.len() {
            return Err(bad);
        }
        let data = &d[at + 8..at + 8 + len];
        let metadata = matches!(kind, b"eXIf" | b"tEXt" | b"zTXt" | b"iTXt" | b"tIME");
        if metadata {
            removed = true;
            if kind == b"eXIf" {
                if let Some(value) = orientation(data) {
                    let tiff = orientation_only(value);
                    let mut chunk = (tiff.len() as u32).to_be_bytes().to_vec();
                    chunk.extend_from_slice(b"eXIf");
                    chunk.extend_from_slice(&tiff);
                    chunk.extend_from_slice(&crc32(&[b"eXIf", &tiff]).to_be_bytes());
                    out.add(chunk);
                }
            }
        } else {
            out.keep(at..end);
        }
        at = end;
        if kind == b"IEND" {
            // Anything after the end is not sent.
            removed |= at < d.len();
            break;
        }
    }
    Ok(removed.then_some(out.0))
}

const VP8X_EXIF: u8 = 0x08;
const VP8X_XMP: u8 = 0x04;

fn plan_webp(d: &[u8], cancel: &AtomicBool) -> Result<Option<Vec<Piece>>, Error> {
    let bad = Error::Unreadable;
    let riff_end = (u32_at(d, 4, false).ok_or(bad)? as usize)
        .checked_add(8)
        .ok_or(bad)?;
    if riff_end > d.len() {
        return Err(bad);
    }
    // Chunks as ranges of the input, and the extended header's position.
    let mut chunks: Vec<Range<usize>> = Vec::new();
    let mut vp8x = None;
    let mut at = 12;
    let mut removed = riff_end < d.len();
    let mut kept: Option<Vec<u8>> = None;
    while at + 8 <= riff_end {
        check(cancel)?;
        let kind = &d[at..at + 4];
        let len = u32_at(d, at + 4, false).ok_or(bad)? as usize;
        let data_end = (at + 8).checked_add(len).ok_or(bad)?;
        if data_end > riff_end {
            return Err(bad);
        }
        let end = (data_end + (len & 1)).min(riff_end);
        let data = &d[at + 8..data_end];
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
            _ => {
                if kind == b"VP8X" && len > 0 {
                    vp8x = Some(chunks.len());
                }
                chunks.push(at..end);
            }
        }
        at = end;
    }
    if !removed {
        return Ok(None);
    }
    let body: usize =
        chunks.iter().map(Range::len).sum::<usize>() + kept.as_ref().map_or(0, Vec::len);
    let mut out = Pieces::default();
    let mut header = b"RIFF".to_vec();
    header.extend_from_slice(&((4 + body) as u32).to_le_bytes());
    header.extend_from_slice(b"WEBP");
    out.add(header);
    for (i, r) in chunks.into_iter().enumerate() {
        if Some(i) == vp8x {
            // The extended header says which metadata chunks follow.
            let mut chunk = d[r].to_vec();
            chunk[8] &= !(VP8X_EXIF | VP8X_XMP);
            if kept.is_some() {
                chunk[8] |= VP8X_EXIF;
            }
            out.add(chunk);
        } else {
            out.keep(r);
        }
    }
    // EXIF goes after the image data.
    if let Some(k) = kept {
        out.add(k);
    }
    Ok(Some(out.0))
}

#[cfg(test)]
pub(crate) mod tests {
    use std::time::{Duration, Instant};

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

    fn segment(marker: u8, body: &[u8]) -> Vec<u8> {
        let mut s = vec![0xFF, marker];
        s.extend_from_slice(&((body.len() + 2) as u16).to_be_bytes());
        s.extend_from_slice(body);
        s
    }

    fn app1_exif(orientation: u16) -> Vec<u8> {
        segment(0xE1, &[EXIF_HEADER, &tiff_with_gps(orientation)].concat())
    }

    /// A small JPEG: SOI, an EXIF block, an XMP block, a quantisation table and the
    /// scan data with its end.
    pub(crate) fn jpeg_with_exif(orientation: u16) -> Vec<u8> {
        let mut d = vec![0xFF, 0xD8];
        d.extend_from_slice(&app1_exif(orientation));
        d.extend_from_slice(&segment(
            0xE1,
            &[XMP_HEADER, b"<x:xmpmeta>GPS</x:xmpmeta>"].concat(),
        ));
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
        let out = strip(&input).unwrap().expect("stripped");
        assert!(!contains(&out, b"GPS"), "no position left");
        assert!(!contains(&out, XMP_HEADER));
        assert!(out.starts_with(&[0xFF, 0xD8, 0xFF, 0xE1]));
        assert_eq!(orientation(&out[12..]), Some(6), "the rotation is kept");
        let tail = &input[input.len() - 19..];
        assert!(out.ends_with(tail), "the tables and the scan are untouched");
        // An upright photo keeps no EXIF at all.
        let out = strip(&jpeg_with_exif(1)).unwrap().unwrap();
        assert!(!contains(&out, EXIF_HEADER));
        assert_eq!(&out[..], &[&[0xFF, 0xD8][..], tail].concat()[..]);
        // Extended XMP, IPTC and comments go too, and so do the other pictures of a
        // multi-picture file (with their own metadata) after the end of the first.
        let mut multi = jpeg_with_exif(1);
        let extra = [
            segment(0xE1, &[XMP_EXTENSION_HEADER, b"GPS extended"].concat()),
            segment(0xED, &[PHOTOSHOP_HEADER, b"8BIM GPS Oslo"].concat()),
            segment(0xFE, b"GPS in a comment"),
        ]
        .concat();
        multi.splice(2..2, extra);
        multi.extend_from_slice(&jpeg_with_exif(6));
        let out = strip(&multi).unwrap().unwrap();
        assert!(!contains(&out, b"GPS"));
        assert_eq!(
            &out[..],
            &[&[0xFF, 0xD8][..], tail].concat()[..],
            "the first picture only"
        );
        // Nothing to remove: `None` (the file is sent as it is).
        assert_eq!(strip(&out), Ok(None));
        assert_eq!(strip(b"not an image"), Ok(None));
    }

    #[test]
    fn a_broken_or_unknown_photo_is_never_sent_with_its_metadata() {
        // A JPEG with GPS and then a segment that runs past the end of the file.
        let mut broken = vec![0xFF, 0xD8];
        broken.extend_from_slice(&app1_exif(1));
        broken.extend_from_slice(&[0xFF, 0xDB, 0x40, 0x00, 1, 2]);
        assert_eq!(strip(&broken), Err(Error::Unreadable));
        // Bytes between segments that are no marker.
        let mut junk = vec![0xFF, 0xD8];
        junk.extend_from_slice(&app1_exif(1));
        junk.extend_from_slice(b"junk");
        assert_eq!(strip(&junk), Err(Error::Unreadable));
        // A PNG without its end, and types this module cannot clean.
        let png = crate::backend::media::png::encode(1, 1, &[1, 2, 3, 255]);
        assert_eq!(strip(&png[..png.len() - 12]), Err(Error::Unreadable));
        let mut heic = vec![0, 0, 0, 24];
        heic.extend_from_slice(b"ftypheic\0\0\0\0mif1heic");
        assert_eq!(sniff(&heic), Kind::Heif);
        assert_eq!(strip(&heic), Err(Error::Unsupported));
        assert_eq!(strip(&tiff_with_gps(1)), Err(Error::Unsupported));
        // The type comes from the bytes: a JPEG named anything is still a JPEG.
        assert_eq!(sniff(&jpeg_with_exif(1)), Kind::Jpeg);
        // Cancel stops it.
        assert_eq!(
            plan(&jpeg_with_exif(1), &AtomicBool::new(true)),
            Err(Error::Cancelled)
        );
    }

    fn png_with_chunks(chunks: &[(&[u8; 4], Vec<u8>)]) -> (Vec<u8>, Vec<u8>) {
        let png = crate::backend::media::png::encode(2, 2, &[7u8; 16]);
        let ihdr_end = 8 + 12 + 13;
        let mut with = png[..ihdr_end].to_vec();
        for (kind, data) in chunks {
            with.extend_from_slice(&(data.len() as u32).to_be_bytes());
            with.extend_from_slice(*kind);
            with.extend_from_slice(data);
            with.extend_from_slice(&crc32(&[*kind, data]).to_be_bytes());
        }
        with.extend_from_slice(&png[ihdr_end..]);
        (png, with)
    }

    #[test]
    fn png_and_webp_lose_their_metadata_chunks_only() {
        let tiff = tiff_with_gps(1);
        let raw_profile = b"Raw profile type exif\0\0\0\n exif\n 4547505320353920\n".to_vec();
        let (png, mut with) = png_with_chunks(&[
            (b"eXIf", tiff.clone()),
            (b"tEXt", raw_profile),
            (b"zTXt", b"Comment\0\0xx".to_vec()),
            (b"tIME", vec![7, 234, 11, 11, 9, 0, 0]),
        ]);
        with.extend_from_slice(b"GPS after the end");
        assert_eq!(
            strip(&with).unwrap().as_deref(),
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
        let out = strip(&webp).unwrap().unwrap();
        assert!(!contains(&out, b"GPS"));
        assert_eq!(out[20] & (VP8X_EXIF | VP8X_XMP), 0, "flags cleared");
        assert_eq!(out[20] & 0x10, 0x10, "other flags kept");
        assert_eq!(u32_at(&out, 4, false), Some(out.len() as u32 - 8));
        assert!(contains(
            &out,
            &[b"VP8L".as_slice(), &3u32.to_le_bytes(), &[7, 7, 7, 0]].concat()
        ));
    }

    /// A tiny xorshift, so the test is the same on every run.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n.max(1) as u64) as usize
        }
    }

    fn real_jpeg() -> Vec<u8> {
        let pixels: Vec<u8> = (0..48u32 * 32 * 3).map(|i| (i * 7 % 255) as u8).collect();
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90)
            .encode(&pixels, 48, 32, image::ExtendedColorType::Rgb8)
            .unwrap();
        out
    }

    /// The decoded pixels, or `None` for a file `inlet-media` does not decode.
    fn try_pixels(data: &[u8]) -> Option<Vec<u8>> {
        let limits = inlet_media::KindLimits::for_kind(inlet_media::MediaKind::Attachment);
        let control = inlet_media::DecodeControl::with_budget(Duration::from_secs(5));
        let fit = inlet_media::Fit::Contain {
            max_width: 4096,
            max_height: 4096,
        };
        Some(
            inlet_media::decode(data, fit, &limits, &control)
                .ok()?
                .into_gpui_frames()
                .into_iter()
                .flat_map(|f| f.buffer().as_raw().clone())
                .collect(),
        )
    }

    fn pixels(data: &[u8]) -> Vec<u8> {
        try_pixels(data).expect("decodes")
    }

    /// A lossless WebP with an extended header, EXIF (with GPS) and XMP.
    fn webp_with_metadata() -> Vec<u8> {
        let (w, h) = (6u32, 4u32);
        let plain = crate::backend::media::png::encode_webp(w, h, &[90u8; 6 * 4 * 4]);
        // The VP8L chunk of the simple file.
        let vp8l = &plain[12..];
        let mut body = b"WEBP".to_vec();
        body.extend_from_slice(b"VP8X");
        body.extend_from_slice(&10u32.to_le_bytes());
        body.push(VP8X_EXIF | VP8X_XMP | 0x10);
        body.extend_from_slice(&[0, 0, 0]);
        body.extend_from_slice(&(w - 1).to_le_bytes()[..3]);
        body.extend_from_slice(&(h - 1).to_le_bytes()[..3]);
        body.extend_from_slice(vp8l);
        let tiff = tiff_with_gps(1);
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
        webp
    }

    fn materialize(d: &[u8], pieces: &[Piece]) -> Vec<u8> {
        pieces
            .iter()
            .flat_map(|p| match p {
                Piece::Keep(r) => d[r.clone()].to_vec(),
                Piece::New(b) => b.clone(),
            })
            .collect()
    }

    /// Properties over many broken and valid inputs: no panic, linear time, never
    /// larger than the input plus a minimal EXIF block, and the same pixels.
    #[test]
    fn the_stripper_holds_its_properties_on_mutated_files() {
        let jpeg = real_jpeg();
        let mut jpeg_meta = jpeg.clone();
        jpeg_meta.splice(2..2, app1_exif(1));
        jpeg_meta.splice(2..2, segment(0xFE, b"GPS"));
        let (png, png_meta) = png_with_chunks(&[(b"tEXt", b"GPS\0Oslo".to_vec())]);
        // The pixels stay the same.
        let out = strip(&jpeg_meta).unwrap().unwrap();
        assert_eq!(&out[..], &jpeg[..]);
        assert_eq!(pixels(&out), pixels(&jpeg));
        let out = strip(&png_meta).unwrap().unwrap();
        assert_eq!(pixels(&out), pixels(&png));
        let webp = webp_with_metadata();
        let out = strip(&webp).unwrap().unwrap();
        assert!(!contains(&out, b"GPS"));
        assert_eq!(pixels(&out), pixels(&webp));

        let seeds = [jpeg_meta, png_meta, webp, jpeg_with_exif(6), real_jpeg()];
        let mut compared = 0;
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let started = Instant::now();
        for round in 0..4000 {
            let mut d = seeds[round % seeds.len()].clone();
            for _ in 0..1 + rng.below(4) {
                if d.is_empty() {
                    break;
                }
                match rng.below(4) {
                    0 => {
                        let i = rng.below(d.len());
                        d[i] = rng.next() as u8;
                    }
                    1 => d.truncate(rng.below(d.len())),
                    2 => {
                        let i = rng.below(d.len());
                        d.insert(i, 0xFF);
                    }
                    _ => {
                        let i = rng.below(d.len());
                        let n = rng.below(16).min(d.len() - i);
                        d.drain(i..i + n);
                    }
                }
            }
            let one = Instant::now();
            if let Ok(Some(pieces)) = plan(&d, &AtomicBool::new(false)) {
                assert!(planned_len(&pieces) <= d.len() + 40, "round {round}");
                for p in &pieces {
                    if let Piece::Keep(r) = p {
                        assert!(r.end <= d.len(), "round {round}");
                    }
                }
                // A mutated file that still decodes keeps its pixels once cleaned.
                if let Some(before) = try_pixels(&d) {
                    let after = try_pixels(&materialize(&d, &pieces));
                    assert_eq!(after.as_ref(), Some(&before), "round {round}");
                    compared += 1;
                }
            }
            assert!(one.elapsed() < Duration::from_secs(1), "round {round}");
        }
        assert!(
            compared > 100,
            "pixels compared for {compared} mutated files"
        );
        assert!(started.elapsed() < Duration::from_secs(120));
    }
}
