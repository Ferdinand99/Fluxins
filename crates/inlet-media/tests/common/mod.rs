//! Fixtures, generated in code so that no binary files are committed.
#![allow(dead_code)]

use std::io::Cursor;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::future::BoxFuture;
use image::{Delay, Frame, ImageFormat, Rgba, RgbaImage};
use inlet_media::{
    DecodeControl, FetchError, FetchResponse, Fetcher, Fit, KindLimits, MediaConfig, MediaKind,
};
use tokio::sync::watch;
use url::Url;

pub const RED: [u8; 4] = [255, 0, 0, 255];
pub const GREEN: [u8; 4] = [0, 255, 0, 255];
pub const BLUE: [u8; 4] = [0, 0, 255, 255];

pub fn solid(width: u32, height: u32, color: [u8; 4]) -> RgbaImage {
    RgbaImage::from_pixel(width, height, Rgba(color))
}

/// Left half red, right half blue.
pub fn split(width: u32, height: u32) -> RgbaImage {
    RgbaImage::from_fn(width, height, |x, _| {
        Rgba(if x < width / 2 { RED } else { BLUE })
    })
}

pub fn png(img: &RgbaImage) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, ImageFormat::Png).unwrap();
    out.into_inner()
}

pub fn jpeg(img: &RgbaImage) -> Vec<u8> {
    let rgb = image::DynamicImage::ImageRgba8(img.clone()).to_rgb8();
    let mut out = Cursor::new(Vec::new());
    rgb.write_to(&mut out, ImageFormat::Jpeg).unwrap();
    out.into_inner()
}

pub fn webp(img: &RgbaImage) -> Vec<u8> {
    let mut out = Vec::new();
    image_webp::WebPEncoder::new(&mut out)
        .encode(
            img.as_raw(),
            img.width(),
            img.height(),
            image_webp::ColorType::Rgba8,
        )
        .unwrap();
    out
}

pub fn gif(frames: &[(RgbaImage, u32)]) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut encoder = image::codecs::gif::GifEncoder::new_with_speed(&mut out, 30);
        encoder
            .set_repeat(image::codecs::gif::Repeat::Infinite)
            .unwrap();
        for (img, delay_ms) in frames {
            let delay = Delay::from_numer_denom_ms(*delay_ms, 1);
            encoder
                .encode_frame(Frame::from_parts(img.clone(), 0, 0, delay))
                .unwrap();
        }
    }
    out
}

pub fn apng(frames: &[(RgbaImage, u16)]) -> Vec<u8> {
    let (w, h) = frames[0].0.dimensions();
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, w, h);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_animated(frames.len() as u32, 0).unwrap();
        let mut writer = encoder.write_header().unwrap();
        for (img, delay_ms) in frames {
            writer.set_frame_delay(*delay_ms, 1000).unwrap();
            writer.write_image_data(img.as_raw()).unwrap();
        }
        writer.finish().unwrap();
    }
    out
}

fn chunk(out: &mut Vec<u8>, fourcc: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(fourcc);
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(data);
    if data.len() % 2 == 1 {
        out.push(0);
    }
}

/// The `VP8L` payload of a lossless still WebP.
fn vp8l_payload(still: &[u8]) -> Vec<u8> {
    let mut pos = 12;
    while pos + 8 <= still.len() {
        let size = u32::from_le_bytes(still[pos + 4..pos + 8].try_into().unwrap()) as usize;
        if &still[pos..pos + 4] == b"VP8L" {
            return still[pos + 8..pos + 8 + size].to_vec();
        }
        pos += 8 + size + (size % 2);
    }
    panic!("no VP8L chunk");
}

/// An animated WebP (RIFF container with `ANIM` and `ANMF`) of lossless frames.
pub fn animated_webp(frames: &[(RgbaImage, u32)]) -> Vec<u8> {
    let (w, h) = frames[0].0.dimensions();
    let mut body = Vec::new();
    body.extend_from_slice(b"WEBP");
    let mut vp8x = vec![0x12u8, 0, 0, 0]; // animation + alpha flags
    vp8x.extend_from_slice(&(w - 1).to_le_bytes()[..3]);
    vp8x.extend_from_slice(&(h - 1).to_le_bytes()[..3]);
    chunk(&mut body, b"VP8X", &vp8x);
    chunk(&mut body, b"ANIM", &[0, 0, 0, 0, 0, 0]);
    for (img, delay_ms) in frames {
        let mut anmf = Vec::new();
        anmf.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // x, y
        anmf.extend_from_slice(&(img.width() - 1).to_le_bytes()[..3]);
        anmf.extend_from_slice(&(img.height() - 1).to_le_bytes()[..3]);
        anmf.extend_from_slice(&delay_ms.to_le_bytes()[..3]);
        anmf.push(0b10); // do not blend, do not dispose
        chunk(&mut anmf, b"VP8L", &vp8l_payload(&webp(img)));
        chunk(&mut body, b"ANMF", &anmf);
    }
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

pub fn png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let mut body = kind.to_vec();
    body.extend_from_slice(data);
    out.extend_from_slice(&body);
    out.extend_from_slice(&crc32(&body).to_be_bytes());
}

pub fn zlib(data: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    enc.write_all(data).unwrap();
    enc.finish().unwrap()
}

/// A PNG that declares `width` x `height` (RGBA, 8 bit) but carries `idat` as its pixel data.
pub fn png_with(width: u32, height: u32, idat: &[u8]) -> Vec<u8> {
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    png_chunk(&mut out, b"IHDR", &ihdr);
    png_chunk(&mut out, b"IDAT", idat);
    png_chunk(&mut out, b"IEND", &[]);
    out
}

/// A valid 1x1 GIF whose logical screen is changed to `width` x `height`.
pub fn gif_claiming(width: u16, height: u16) -> Vec<u8> {
    let mut bytes = gif(&[(solid(1, 1, RED), 100)]);
    bytes[6..8].copy_from_slice(&width.to_le_bytes());
    bytes[8..10].copy_from_slice(&height.to_le_bytes());
    bytes
}

/// A GIF with this canvas and these frames (left, top, width, height). The pixel
/// data is a stub, so it only gets past the structural scan.
pub fn raw_gif(canvas: (u16, u16), frames: &[(u16, u16, u16, u16)]) -> Vec<u8> {
    let mut out = b"GIF89a".to_vec();
    out.extend_from_slice(&canvas.0.to_le_bytes());
    out.extend_from_slice(&canvas.1.to_le_bytes());
    out.extend_from_slice(&[0, 0, 0]);
    for &(left, top, w, h) in frames {
        out.push(0x2C);
        for v in [left, top, w, h] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.push(0);
        out.extend_from_slice(&[0x02, 0x02, 0x4C, 0x01, 0x00]);
    }
    out.push(0x3B);
    out
}

/// A WebP whose EXIF chunk claims `claimed` bytes and carries four.
pub fn webp_claiming_exif(claimed: u32) -> Vec<u8> {
    let mut body = b"WEBP".to_vec();
    let mut vp8x = vec![0x08u8, 0, 0, 0];
    vp8x.extend_from_slice(&9u32.to_le_bytes()[..3]);
    vp8x.extend_from_slice(&9u32.to_le_bytes()[..3]);
    body.extend_from_slice(b"VP8X");
    body.extend_from_slice(&10u32.to_le_bytes());
    body.extend_from_slice(&vp8x);
    body.extend_from_slice(b"EXIF");
    body.extend_from_slice(&claimed.to_le_bytes());
    body.extend_from_slice(&[0, 0, 0, 0]);
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

/// The RGBA value of a pixel. Decoded images are BGRA.
pub fn rgba_at(img: &inlet_media::DecodedImage, frame: usize, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * img.width + x) * 4) as usize;
    let p = &img.frames[frame].bgra[i..i + 4];
    [p[2], p[1], p[0], p[3]]
}

pub fn control() -> DecodeControl {
    DecodeControl::with_budget(Duration::from_secs(30))
}

pub fn avatar_limits() -> KindLimits {
    KindLimits::for_kind(MediaKind::Avatar)
}

pub const NO_RESIZE: Fit = Fit::Contain {
    max_width: 4096,
    max_height: 4096,
};

// ---- a fetcher for the client tests ------------------------------------------------

pub fn url(path: &str) -> Url {
    Url::parse(&format!("https://cdn.example/{path}")).unwrap()
}

/// A config that allows the host of `url()`.
pub fn config() -> MediaConfig {
    MediaConfig {
        allowed_hosts: vec!["cdn.example".into()],
        ..MediaConfig::default()
    }
}

/// Serves fixed bodies as streams. Counts calls and bytes sent. Can wait at a
/// gate. Notices when a call or a stream is dropped before it finished.
pub struct MockFetcher {
    pub bodies: std::sync::Mutex<std::collections::HashMap<String, Bytes>>,
    pub endless: std::sync::Mutex<std::collections::HashSet<String>>,
    pub statuses: std::sync::Mutex<std::collections::HashMap<String, u16>>,
    pub calls: AtomicUsize,
    pub sent: Arc<AtomicUsize>,
    pub dropped: Arc<AtomicBool>,
    pub stream_dropped: Arc<AtomicBool>,
    pub gate: watch::Sender<bool>,
}

impl MockFetcher {
    pub fn new() -> Arc<Self> {
        Self::with_gate(true)
    }

    /// Like `new`, but every call waits until `open_gate` is called.
    pub fn gated() -> Arc<Self> {
        Self::with_gate(false)
    }

    fn with_gate(open: bool) -> Arc<Self> {
        let (gate, _) = watch::channel(open);
        Arc::new(Self {
            bodies: Default::default(),
            endless: Default::default(),
            statuses: Default::default(),
            calls: AtomicUsize::new(0),
            sent: Arc::new(AtomicUsize::new(0)),
            dropped: Arc::new(AtomicBool::new(false)),
            stream_dropped: Arc::new(AtomicBool::new(false)),
            gate,
        })
    }

    pub fn serve(&self, path: &str, body: Vec<u8>) {
        self.bodies
            .lock()
            .unwrap()
            .insert(url(path).to_string(), Bytes::from(body));
    }

    /// A body that never ends and has no announced length.
    pub fn serve_endless(&self, path: &str) {
        self.endless.lock().unwrap().insert(url(path).to_string());
    }

    /// Answers with this HTTP status instead of a body.
    pub fn serve_status(&self, path: &str, status: u16) {
        self.statuses
            .lock()
            .unwrap()
            .insert(url(path).to_string(), status);
    }

    pub fn open_gate(&self) {
        self.gate.send_replace(true);
    }

    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

struct DropFlag(Arc<AtomicBool>, bool);

impl DropFlag {
    /// Marks that the owner finished normally, so dropping it is not a cancel.
    fn finish(&mut self) {
        self.1 = true;
    }
}

impl Drop for DropFlag {
    fn drop(&mut self) {
        if !self.1 {
            self.0.store(true, Ordering::SeqCst);
        }
    }
}

const CHUNK: usize = 4096;

impl Fetcher for MockFetcher {
    fn fetch<'a>(&'a self, url: &'a Url) -> BoxFuture<'a, Result<FetchResponse, FetchError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut flag = DropFlag(self.dropped.clone(), false);
            let mut rx = self.gate.subscribe();
            let _ = rx.wait_for(|open| *open).await;
            flag.finish(); // finished normally, not dropped
            if let Some(status) = self.statuses.lock().unwrap().get(url.as_str()) {
                return Err(FetchError::Status(*status));
            }
            let sent = self.sent.clone();
            let stream_flag = DropFlag(self.stream_dropped.clone(), false);
            if self.endless.lock().unwrap().contains(url.as_str()) {
                let body =
                    futures_util::stream::unfold((sent, stream_flag), |(sent, flag)| async move {
                        sent.fetch_add(CHUNK, Ordering::SeqCst);
                        Some((Ok(Bytes::from(vec![0u8; CHUNK])), (sent, flag)))
                    });
                return Ok(FetchResponse::new(None, Box::pin(body)));
            }
            let body = self.bodies.lock().unwrap().get(url.as_str()).cloned();
            let Some(body) = body else {
                return Err(FetchError::Status(404));
            };
            let length = body.len() as u64;
            let chunks: Vec<Bytes> = (0..body.len())
                .step_by(CHUNK)
                .map(|at| body.slice(at..(at + CHUNK).min(body.len())))
                .collect();
            let stream = futures_util::stream::unfold(
                (chunks.into_iter(), sent, stream_flag),
                |(mut chunks, sent, mut flag)| async move {
                    match chunks.next() {
                        Some(chunk) => {
                            sent.fetch_add(chunk.len(), Ordering::SeqCst);
                            Some((Ok(chunk), (chunks, sent, flag)))
                        }
                        None => {
                            flag.finish(); // read to the end
                            None
                        }
                    }
                },
            );
            Ok(FetchResponse::new(Some(length), Box::pin(stream)))
        })
    }
}
