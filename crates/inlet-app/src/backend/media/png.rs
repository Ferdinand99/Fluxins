// SPDX-License-Identifier: AGPL-3.0-or-later
//! A tiny PNG and APNG writer (stored deflate, no compression) for the demo pictures and
//! the tests. Nothing in a release build uses it.

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in data.chunks(5552) {
        for &x in chunk {
            a += u32::from(x);
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    (b << 16) | a
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend((data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend(kind);
    out.extend(data);
    let crc = crc32(&out[start..]);
    out.extend(crc.to_be_bytes());
}

/// zlib with stored (uncompressed) deflate blocks.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let mut blocks = data.chunks(65_535).peekable();
    if blocks.peek().is_none() {
        out.extend([1, 0, 0, 0xFF, 0xFF]);
    }
    while let Some(block) = blocks.next() {
        out.push(u8::from(blocks.peek().is_none()));
        let len = block.len() as u16;
        out.extend(len.to_le_bytes());
        out.extend((!len).to_le_bytes());
        out.extend(block);
    }
    out.extend(adler32(data).to_be_bytes());
    out
}

/// RGBA rows with a filter byte (none) in front of each.
fn scanlines(w: u32, rgba: &[u8]) -> Vec<u8> {
    let row = w as usize * 4;
    let mut raw = Vec::with_capacity(rgba.len() + rgba.len() / row.max(1) + 1);
    for line in rgba.chunks(row.max(1)) {
        raw.push(0);
        raw.extend(line);
    }
    raw
}

fn ihdr(out: &mut Vec<u8>, w: u32, h: u32) {
    let mut data = Vec::with_capacity(13);
    data.extend(w.to_be_bytes());
    data.extend(h.to_be_bytes());
    data.extend([8, 6, 0, 0, 0]);
    chunk(out, b"IHDR", &data);
}

const SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

/// An RGBA image (`rgba.len() == w * h * 4`) as a PNG.
pub fn encode(w: u32, h: u32, rgba: &[u8]) -> Vec<u8> {
    let mut out = SIGNATURE.to_vec();
    ihdr(&mut out, w, h);
    chunk(&mut out, b"IDAT", &zlib_stored(&scanlines(w, rgba)));
    chunk(&mut out, b"IEND", &[]);
    out
}

/// Frames of the same size as an animated PNG that loops, `delay_ms` per frame.
#[cfg(feature = "demo")]
pub fn encode_apng(w: u32, h: u32, frames: &[Vec<u8>], delay_ms: u16) -> Vec<u8> {
    let mut out = SIGNATURE.to_vec();
    ihdr(&mut out, w, h);
    let mut actl = Vec::new();
    actl.extend((frames.len() as u32).to_be_bytes());
    actl.extend(0u32.to_be_bytes());
    chunk(&mut out, b"acTL", &actl);
    let mut seq = 0u32;
    for (i, rgba) in frames.iter().enumerate() {
        let mut fctl = Vec::new();
        fctl.extend(seq.to_be_bytes());
        seq += 1;
        fctl.extend(w.to_be_bytes());
        fctl.extend(h.to_be_bytes());
        fctl.extend(0u32.to_be_bytes());
        fctl.extend(0u32.to_be_bytes());
        fctl.extend(delay_ms.to_be_bytes());
        fctl.extend(1000u16.to_be_bytes());
        fctl.extend([0, 0]);
        chunk(&mut out, b"fcTL", &fctl);
        let data = zlib_stored(&scanlines(w, rgba));
        if i == 0 {
            chunk(&mut out, b"IDAT", &data);
        } else {
            let mut fdat = seq.to_be_bytes().to_vec();
            seq += 1;
            fdat.extend(data);
            chunk(&mut out, b"fdAT", &fdat);
        }
    }
    chunk(&mut out, b"IEND", &[]);
    out
}

/// One colour, `w` x `h`.
#[cfg(test)]
pub fn solid(w: u32, h: u32, rgba: [u8; 4]) -> Vec<u8> {
    let pixels: Vec<u8> = rgba
        .iter()
        .copied()
        .cycle()
        .take(w as usize * h as usize * 4)
        .collect();
    encode(w, h, &pixels)
}

/// A well-formed PNG whose header claims `w` x `h` pixels, with one row of data.
#[cfg(any(test, feature = "demo"))]
pub fn claiming(w: u32, h: u32) -> Vec<u8> {
    let mut out = SIGNATURE.to_vec();
    ihdr(&mut out, w, h);
    chunk(&mut out, b"IDAT", &zlib_stored(&[0, 0, 0, 0, 0]));
    chunk(&mut out, b"IEND", &[]);
    out
}

/// A GIF of `frames` frames on a `w` x `h` canvas: the first frame fills it, the others
/// change one pixel. Small to send, large once every frame is decoded at full size.
/// LZW without compression: 7-bit codes in bytes, a clear code every 64 pixels so the
/// code width never grows.
#[cfg(test)]
pub fn long_gif(w: u16, h: u16, frames: usize) -> Vec<u8> {
    fn image_data(out: &mut Vec<u8>, pixels: impl Iterator<Item = u8>) {
        const CLEAR: u8 = 128;
        const END: u8 = 129;
        let mut codes = Vec::new();
        let mut run = 0;
        codes.push(CLEAR);
        for p in pixels {
            if run == 64 {
                codes.push(CLEAR);
                run = 0;
            }
            codes.push(p & 0x7f);
            run += 1;
        }
        codes.push(END);
        out.push(7); // LZW minimum code size: 128 colours.
        for block in codes.chunks(255) {
            out.push(block.len() as u8);
            out.extend(block);
        }
        out.push(0);
    }
    fn descriptor(out: &mut Vec<u8>, w: u16, h: u16) {
        // Graphic control: 100 ms, no transparency.
        out.extend([0x21, 0xF9, 4, 0, 10, 0, 0, 0]);
        out.push(0x2C);
        out.extend(0u16.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(w.to_le_bytes());
        out.extend(h.to_le_bytes());
        out.push(0);
    }
    let mut out = b"GIF89a".to_vec();
    out.extend(w.to_le_bytes());
    out.extend(h.to_le_bytes());
    // Global colour table of 128 entries, 8-bit colour resolution.
    out.extend([0xF6, 0, 0]);
    for i in 0..128u8 {
        out.extend([i * 2, 255 - i * 2, 90]);
    }
    descriptor(&mut out, w, h);
    image_data(
        &mut out,
        (0..usize::from(w) * usize::from(h)).map(|i| (i / 97) as u8),
    );
    for i in 1..frames {
        descriptor(&mut out, 1, 1);
        image_data(&mut out, std::iter::once((i % 128) as u8));
    }
    out.push(0x3B);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn written_pngs_decode() {
        let bytes = solid(3, 2, [10, 20, 30, 255]);
        let limits = inlet_media::KindLimits::for_kind(inlet_media::MediaKind::Attachment);
        let inspected = inlet_media::inspect(bytes::Bytes::from(bytes), &limits).unwrap();
        assert_eq!((inspected.width(), inspected.height()), (3, 2));
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn written_gifs_decode_with_every_frame() {
        let fit = inlet_media::Fit::Contain {
            max_width: 40,
            max_height: 30,
        };
        let limits = inlet_media::KindLimits::for_kind(inlet_media::MediaKind::Attachment);
        let decoded = inlet_media::decode(
            &long_gif(40, 30, 5),
            fit,
            &limits,
            &inlet_media::DecodeControl::with_budget(std::time::Duration::from_secs(5)),
        )
        .unwrap();
        assert_eq!(decoded.frames.len(), 5);
    }
}
