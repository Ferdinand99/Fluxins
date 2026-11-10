//! Decoding with hard limits. Everything here is synchronous and CPU-bound; the
//! client runs it on a blocking thread. It never panics: decoder panics are
//! caught and reported as [`MediaError::DecoderPanic`].
//!
//! Order of checks, cheapest first (steps 1 to 4 are [`inspect`], which costs
//! microseconds to milliseconds and allocates at most the encoded size):
//!
//! 1. encoded size against the limit for the kind;
//! 2. format by magic bytes (PNG, JPEG, GIF, WebP only);
//! 3. a structural scan that checks every size the file declares against its real
//!    length: PNG chunks (`iCCP`, `zTXt`, `iTXt` and large ancillary chunks are
//!    stripped), WebP RIFF chunks, GIF frames (count, area, inside the canvas);
//! 4. dimensions from the header, before a pixel buffer exists;
//! 5. the decoder's own allocation limit;
//! 6. while decoding an animation, after every frame: cancel flag, time budget,
//!    frame count, output bytes and total work.

use std::io::Cursor;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use image::codecs::gif::GifDecoder;
use image::codecs::jpeg::JpegDecoder;
use image::codecs::png::PngDecoder;
use image::codecs::webp::WebPDecoder;
use image::imageops::{self, FilterType};
use image::{
    AnimationDecoder, DynamicImage, Frames, ImageDecoder, ImageError, ImageFormat, Limits, Rgba,
    RgbaImage,
};

use crate::frames::{DecodedImage, ImageFrame, DEFAULT_FRAME_DELAY};
use crate::scan::{scan_gif, scan_png, scan_webp};
use crate::{Fit, KindLimits, MediaError};

/// Time budget and cancellation for one decode.
#[derive(Debug, Clone)]
pub struct DecodeControl {
    /// Decoding stops with [`MediaError::DecodeTimeout`] after this moment.
    /// A single still image cannot be interrupted, so it is only checked between
    /// the steps and between animation frames. The size limits bound a still image.
    pub deadline: Instant,
    /// Set to true to stop decoding with [`MediaError::Cancelled`].
    pub cancel: Arc<AtomicBool>,
}

impl DecodeControl {
    /// A budget of `budget` from now, and a fresh cancel flag.
    pub fn with_budget(budget: Duration) -> Self {
        Self {
            deadline: Instant::now() + budget,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    fn check(&self) -> Result<(), MediaError> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(MediaError::Cancelled);
        }
        if Instant::now() >= self.deadline {
            return Err(MediaError::DecodeTimeout);
        }
        Ok(())
    }
}

/// Image data that passed every check that does not need a pixel buffer.
#[derive(Debug, Clone)]
pub struct Inspected {
    bytes: Bytes,
    format: ImageFormat,
    width: u32,
    height: u32,
    animated: bool,
    peak: u64,
}

impl Inspected {
    /// Declared width of the source.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Declared height of the source.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// True for GIF, animated WebP and APNG with more than one frame announced.
    pub fn is_animated(&self) -> bool {
        self.animated
    }

    /// A rough upper bound of the memory a decode needs at its peak, in bytes. The
    /// client uses it to keep the sum over all running decodes under a budget.
    pub fn estimated_peak_bytes(&self) -> u64 {
        self.peak
    }

    /// Decodes and downscales. Pass the same limits that [`inspect`] got.
    pub fn decode(
        self,
        fit: Fit,
        limits: &KindLimits,
        control: &DecodeControl,
    ) -> Result<DecodedImage, MediaError> {
        control.check()?;
        let fit = fit.sanitized();
        catch_panic(|| decode_inspected(&self, fit, limits, control))
    }
}

/// Checks `bytes` against the limits without allocating pixel buffers.
pub fn inspect(bytes: Bytes, limits: &KindLimits) -> Result<Inspected, MediaError> {
    if bytes.len() > limits.max_encoded_bytes {
        return Err(MediaError::TooLargeEncoded {
            size: bytes.len(),
            max: limits.max_encoded_bytes,
        });
    }
    catch_panic(|| inspect_checked(bytes, limits))
}

/// Decodes `bytes`, enforces `limits` and downscales to `fit`.
pub fn decode(
    bytes: &[u8],
    fit: Fit,
    limits: &KindLimits,
    control: &DecodeControl,
) -> Result<DecodedImage, MediaError> {
    if bytes.len() > limits.max_encoded_bytes {
        return Err(MediaError::TooLargeEncoded {
            size: bytes.len(),
            max: limits.max_encoded_bytes,
        });
    }
    control.check()?;
    inspect(Bytes::copy_from_slice(bytes), limits)?.decode(fit, limits, control)
}

/// Turns a panic inside `f` into [`MediaError::DecoderPanic`].
fn catch_panic<T>(f: impl FnOnce() -> Result<T, MediaError>) -> Result<T, MediaError> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(_) => Err(MediaError::DecoderPanic),
    }
}

const MIB: u64 = 1024 * 1024;

/// Memory for the pixel work of one decode: the decoder's buffer
/// (`bytes_per_pixel`), an RGBA8 copy and a crop or working copy. Animations add
/// room for the finished frames.
fn estimate_peak(
    width: u32,
    height: u32,
    bytes_per_pixel: u64,
    animated: bool,
    encoded: usize,
) -> u64 {
    estimate_peak_pixels(
        u64::from(width) * u64::from(height),
        bytes_per_pixel,
        animated,
        encoded,
    )
}

fn estimate_peak_pixels(pixels: u64, bytes_per_pixel: u64, animated: bool, encoded: usize) -> u64 {
    // Still images: the decoder's buffer, an RGBA8 copy and a working copy. An
    // animation holds three canvases (the canvas, the previous one and the frame) and
    // the raw frame next to the RGBA copy, so at least 4 bytes per pixel and 12 more.
    let per_pixel = if animated {
        bytes_per_pixel.max(4) + 12
    } else {
        bytes_per_pixel + 8
    };
    let mut peak = pixels * per_pixel + encoded as u64 + 8 * MIB;
    if animated {
        // The finished frames, up to the animation cap of 64 MiB.
        peak += 64 * MIB;
    }
    peak
}

fn image_limits(limits: &KindLimits) -> Limits {
    let mut image_limits = Limits::default();
    image_limits.max_image_width = Some(limits.max_width);
    image_limits.max_image_height = Some(limits.max_height);
    // Room for what an animation holds at the pixel limit: the canvas, the canvas of
    // the previous frame, the frame, and the decoder's own buffers.
    image_limits.max_alloc = Some(limits.max_pixels.saturating_mul(4 * 6) + 16 * MIB);
    image_limits
}

fn inspect_checked(bytes: Bytes, limits: &KindLimits) -> Result<Inspected, MediaError> {
    let format = image::guess_format(&bytes).map_err(|_| MediaError::Unsupported)?;
    let encoded = bytes.len();
    let mut gif_peak = None;
    let (bytes, width, height, animated, bpp) = match format {
        ImageFormat::Png => {
            let bytes = scan_png(&bytes)?.map_or(bytes, Bytes::from);
            // Before the decoder is built: it refuses sizes over its limits with an
            // error that says nothing about the size.
            if let Some((w, h)) = png_dimensions(&bytes) {
                check_dimensions(w, h, limits)?;
            }
            let decoder = PngDecoder::with_limits(Cursor::new(&bytes[..]), image_limits(limits))
                .map_err(map_image_error)?;
            let (w, h) = decoder.dimensions();
            let bpp = u64::from(decoder.color_type().bytes_per_pixel());
            let animated = decoder.is_apng().map_err(map_image_error)?;
            drop(decoder);
            (bytes, w, h, animated, bpp)
        }
        ImageFormat::Gif => {
            let scan = scan_gif(&bytes, limits)?;
            // A frame can be larger than the canvas; the bigger of the two is the
            // size of the working buffers.
            let pixels =
                (u64::from(scan.width) * u64::from(scan.height)).max(scan.largest_frame_pixels);
            let peak = estimate_peak_pixels(pixels, 4, scan.frames > 1, encoded);
            gif_peak = Some(peak);
            (bytes, scan.width, scan.height, scan.frames > 1, 4)
        }
        ImageFormat::WebP => {
            scan_webp(&bytes, limits)?;
            let decoder = WebPDecoder::new(Cursor::new(&bytes[..])).map_err(map_image_error)?;
            let (w, h) = decoder.dimensions();
            let animated = decoder.has_animation();
            drop(decoder);
            (bytes, w, h, animated, 4)
        }
        ImageFormat::Jpeg => {
            let decoder = JpegDecoder::new(Cursor::new(&bytes[..])).map_err(map_image_error)?;
            let (w, h) = decoder.dimensions();
            drop(decoder);
            // Progressive files keep their coefficients: about 8 bytes per pixel.
            (bytes, w, h, false, 8)
        }
        _ => return Err(MediaError::Unsupported),
    };
    check_dimensions(width, height, limits)?;
    Ok(Inspected {
        peak: gif_peak.unwrap_or_else(|| estimate_peak(width, height, bpp, animated, encoded)),
        bytes,
        format,
        width,
        height,
        animated,
    })
}

/// Width and height from the IHDR chunk, which has to come first.
fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let ihdr = bytes.get(8..24)?;
    if &ihdr[4..8] != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes(ihdr[8..12].try_into().ok()?);
    let height = u32::from_be_bytes(ihdr[12..16].try_into().ok()?);
    Some((width, height))
}

fn decode_inspected(
    inspected: &Inspected,
    fit: Fit,
    limits: &KindLimits,
    control: &DecodeControl,
) -> Result<DecodedImage, MediaError> {
    let cursor = Cursor::new(&inspected.bytes[..]);
    match inspected.format {
        ImageFormat::Png => {
            let mut decoder =
                PngDecoder::with_limits(cursor, image_limits(limits)).map_err(map_image_error)?;
            prepare(&mut decoder, limits)?;
            if decoder.is_apng().map_err(map_image_error)? {
                let apng = decoder.apng().map_err(map_image_error)?;
                decode_animation(apng.into_frames(), fit, limits, control)
            } else {
                decode_still(decoder, fit, control)
            }
        }
        ImageFormat::Gif => {
            let mut decoder = GifDecoder::new(cursor).map_err(map_image_error)?;
            prepare(&mut decoder, limits)?;
            decode_animation(decoder.into_frames(), fit, limits, control)
        }
        ImageFormat::WebP => {
            let mut decoder = WebPDecoder::new(cursor).map_err(map_image_error)?;
            prepare(&mut decoder, limits)?;
            if decoder.has_animation() {
                // Transparent canvas, like gpui's own WebP decoder.
                decoder
                    .set_background_color(Rgba([0, 0, 0, 0]))
                    .map_err(map_image_error)?;
                decode_animation(decoder.into_frames(), fit, limits, control)
            } else {
                decode_still(decoder, fit, control)
            }
        }
        ImageFormat::Jpeg => {
            let mut decoder = JpegDecoder::new(cursor).map_err(map_image_error)?;
            prepare(&mut decoder, limits)?;
            decode_still(decoder, fit, control)
        }
        _ => Err(MediaError::Unsupported),
    }
}

/// Checks the declared size and gives the decoder its own allocation limit.
fn prepare<D: ImageDecoder>(decoder: &mut D, limits: &KindLimits) -> Result<(), MediaError> {
    let (width, height) = decoder.dimensions();
    check_dimensions(width, height, limits)?;
    decoder
        .set_limits(image_limits(limits))
        .map_err(map_image_error)
}

pub(crate) fn check_dimensions(
    width: u32,
    height: u32,
    limits: &KindLimits,
) -> Result<(), MediaError> {
    let pixels = u64::from(width) * u64::from(height);
    if width == 0
        || height == 0
        || width > limits.max_width
        || height > limits.max_height
        || pixels > limits.max_pixels
    {
        return Err(MediaError::BadDimensions { width, height });
    }
    Ok(())
}

fn map_image_error(err: ImageError) -> MediaError {
    match err {
        ImageError::Limits(_) => MediaError::AnimationTooLarge,
        ImageError::Unsupported(_) => MediaError::Unsupported,
        other => MediaError::Corrupt(other.to_string()),
    }
}

fn decode_still<D: ImageDecoder>(
    mut decoder: D,
    fit: Fit,
    control: &DecodeControl,
) -> Result<DecodedImage, MediaError> {
    control.check()?;
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(decoder).map_err(map_image_error)?;
    img.apply_orientation(orientation);
    control.check()?;
    let rgba = fit_to(img.into_rgba8(), fit, FilterType::Lanczos3);
    control.check()?;
    Ok(single(rgba))
}

/// The pixels as B, G, R, A, the order gpui wants. The swap is done in place.
fn into_bgra(img: RgbaImage) -> (u32, u32, Vec<u8>) {
    let (width, height) = img.dimensions();
    let mut raw = img.into_raw();
    for px in raw.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }
    (width, height, raw)
}

fn single(img: RgbaImage) -> DecodedImage {
    let (width, height, bgra) = into_bgra(img);
    DecodedImage {
        width,
        height,
        frames: vec![ImageFrame {
            bgra,
            delay: DEFAULT_FRAME_DELAY,
        }],
    }
}

fn decode_animation(
    frames: Frames<'_>,
    fit: Fit,
    limits: &KindLimits,
    control: &DecodeControl,
) -> Result<DecodedImage, MediaError> {
    let mut out: Vec<ImageFrame> = Vec::new();
    let mut size: Option<(u32, u32)> = None;
    let mut out_bytes: u64 = 0;
    let mut work: u64 = 0;

    for frame in frames {
        control.check()?;
        let frame = match frame {
            Ok(frame) => frame,
            // Limit errors are errors. A cut-off file is shown as far as it goes.
            Err(ImageError::Limits(_)) => return Err(MediaError::AnimationTooLarge),
            Err(err) if out.is_empty() => return Err(map_image_error(err)),
            Err(err) => {
                tracing::debug!("animation ends early: {err}");
                break;
            }
        };
        if out.len() >= limits.max_frames {
            return Err(MediaError::TooManyFrames {
                max: limits.max_frames,
            });
        }
        let delay = frame_delay(&frame);
        let buffer = frame.into_buffer();
        check_dimensions(buffer.width(), buffer.height(), limits)?;
        work = work.saturating_add(u64::from(buffer.width()) * u64::from(buffer.height()));
        if work > limits.max_animation_work_pixels {
            return Err(MediaError::AnimationTooLarge);
        }
        let scaled = fit_to(buffer, fit, FilterType::Triangle);
        match size {
            None => size = Some(scaled.dimensions()),
            Some(first) if first != scaled.dimensions() => {
                return Err(MediaError::Corrupt("frame size changes".into()));
            }
            Some(_) => {}
        }
        out_bytes = out_bytes.saturating_add(scaled.as_raw().len() as u64);
        if out_bytes > limits.max_animation_bytes {
            return Err(MediaError::AnimationTooLarge);
        }
        let (_, _, bgra) = into_bgra(scaled);
        out.push(ImageFrame { bgra, delay });
    }

    let Some((width, height)) = size else {
        return Err(MediaError::Corrupt("no frames".into()));
    };
    Ok(DecodedImage {
        width,
        height,
        frames: out,
    })
}

fn frame_delay(frame: &image::Frame) -> Duration {
    let delay = Duration::from(frame.delay());
    if delay <= Duration::from_millis(10) {
        DEFAULT_FRAME_DELAY
    } else {
        delay.min(Duration::from_secs(60))
    }
}

/// Where the pixels of a source image go: an optional centre crop, then a scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Plan {
    pub crop: (u32, u32, u32, u32),
    pub out: (u32, u32),
}

/// Never upscales, never produces a zero side.
pub(crate) fn plan(width: u32, height: u32, fit: Fit) -> Plan {
    match fit {
        Fit::Contain {
            max_width,
            max_height,
        } => {
            let out = if width <= max_width && height <= max_height {
                (width, height)
            } else {
                let scale = (f64::from(max_width) / f64::from(width))
                    .min(f64::from(max_height) / f64::from(height));
                let scaled =
                    |side: u32, max: u32| ((f64::from(side) * scale).round() as u32).clamp(1, max);
                (scaled(width, max_width), scaled(height, max_height))
            };
            Plan {
                crop: (0, 0, width, height),
                out,
            }
        }
        Fit::CoverSquare { size } => {
            let side = width.min(height);
            let out = size.min(side);
            Plan {
                crop: ((width - side) / 2, (height - side) / 2, side, side),
                out: (out, out),
            }
        }
    }
}

fn fit_to(img: RgbaImage, fit: Fit, filter: FilterType) -> RgbaImage {
    let plan = plan(img.width(), img.height(), fit);
    let (x, y, cw, ch) = plan.crop;
    let needs_crop = (x, y, cw, ch) != (0, 0, img.width(), img.height());
    let needs_scale = plan.out != (cw, ch);
    if !needs_crop && !needs_scale {
        return img;
    }
    let mut work = if needs_crop {
        imageops::crop_imm(&img, x, y, cw, ch).to_image()
    } else {
        img
    };
    if !needs_scale {
        return work;
    }
    // The resampler expects premultiplied alpha. Without it, the colour of fully
    // transparent pixels bleeds into the edges of the picture.
    premultiply(&mut work);
    // A big step with a wide filter is slow (Lanczos looks at six source pixels per
    // output pixel and axis) and gains nothing: average down to twice the target
    // with a box filter first, then filter the last step.
    let (ow, oh) = plan.out;
    if cw > ow.saturating_mul(2) || ch > oh.saturating_mul(2) {
        work = imageops::thumbnail(&work, cw.min(ow * 2), ch.min(oh * 2));
    }
    let mut scaled = if work.dimensions() == (ow, oh) {
        work
    } else {
        imageops::resize(&work, ow, oh, filter)
    };
    unpremultiply(&mut scaled);
    scaled
}

fn premultiply(img: &mut RgbaImage) {
    for px in img.as_chunks_mut::<4>().0 {
        let a = u32::from(px[3]);
        if a != 255 {
            for c in &mut px[..3] {
                *c = ((u32::from(*c) * a + 127) / 255) as u8;
            }
        }
    }
}

fn unpremultiply(img: &mut RgbaImage) {
    for px in img.as_chunks_mut::<4>().0 {
        let a = u32::from(px[3]);
        if a != 255 && a != 0 {
            for c in &mut px[..3] {
                *c = ((u32::from(*c) * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panicking_decoder_becomes_an_error() {
        let result: Result<(), _> = catch_panic(|| panic!("decoder bug"));
        assert_eq!(result, Err(MediaError::DecoderPanic));
        assert_eq!(catch_panic(|| Ok(1)), Ok(1));
    }

    #[test]
    fn contain_never_upscales() {
        let p = plan(
            100,
            50,
            Fit::Contain {
                max_width: 550,
                max_height: 350,
            },
        );
        assert_eq!(p.out, (100, 50));
    }

    #[test]
    fn contain_keeps_the_aspect_ratio() {
        let p = plan(2000, 1000, Fit::PREVIEW);
        assert_eq!(p.out, (550, 275));
        let p = plan(1000, 2000, Fit::PREVIEW);
        assert_eq!(p.out, (175, 350));
    }

    #[test]
    fn contain_never_produces_a_zero_side() {
        let p = plan(
            4096,
            1,
            Fit::Contain {
                max_width: 10,
                max_height: 10,
            },
        );
        assert_eq!(p.out, (10, 1));
    }

    #[test]
    fn cover_square_crops_the_centre() {
        let p = plan(300, 100, Fit::square(40));
        assert_eq!(p.crop, (100, 0, 100, 100));
        assert_eq!(p.out, (40, 40));
        let p = plan(30, 20, Fit::square(40));
        assert_eq!(p.out, (20, 20));
    }

    #[test]
    fn scaling_does_not_bleed_transparent_colour() {
        // Left half opaque red, right half fully transparent with a green colour.
        let mut img = RgbaImage::new(8, 8);
        for (x, _, px) in img.enumerate_pixels_mut() {
            *px = if x < 4 {
                Rgba([255, 0, 0, 255])
            } else {
                Rgba([0, 255, 0, 0])
            };
        }
        let out = fit_to(
            img,
            Fit::Contain {
                max_width: 2,
                max_height: 2,
            },
            FilterType::Triangle,
        );
        for px in out.pixels() {
            if px.0[3] > 0 {
                assert_eq!(px.0[1], 0, "green leaked into {px:?}");
            }
        }
    }
}
