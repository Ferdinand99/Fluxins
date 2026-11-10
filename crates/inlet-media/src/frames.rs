//! The decoded result, and a pausable clock for animations.

use std::time::{Duration, Instant};

/// Delays of 10 ms or less are replaced by this, like browsers do. Many GIFs
/// carry a delay of zero and are meant to play at about 10 frames per second.
pub const DEFAULT_FRAME_DELAY: Duration = Duration::from_millis(100);

/// One frame: tightly packed RGBA8, straight (not premultiplied) alpha.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageFrame {
    /// `width * height * 4` bytes, in R, G, B, A order.
    pub rgba: Vec<u8>,
    /// How long this frame stays on screen. Always above 10 ms.
    pub delay: Duration,
}

/// A decoded and downscaled image. A still image has one frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedImage {
    /// Width of every frame in pixels.
    pub width: u32,
    /// Height of every frame in pixels.
    pub height: u32,
    /// At least one frame.
    pub frames: Vec<ImageFrame>,
}

impl DecodedImage {
    /// True when there is more than one frame.
    pub fn is_animated(&self) -> bool {
        self.frames.len() > 1
    }

    /// Memory the pixel data takes. This is what the cache counts.
    pub fn byte_size(&self) -> u64 {
        self.frames.iter().map(|f| f.rgba.len() as u64).sum()
    }

    /// The delay of every frame.
    pub fn delays(&self) -> Vec<Duration> {
        self.frames.iter().map(|f| f.delay).collect()
    }

    /// Frames in the form gpui wants, for `RenderImage::new(image.to_bgra_frames())`.
    ///
    /// gpui's `RenderImage` holds `image::Frame` values whose buffer is in **B, G,
    /// R, A** order with straight alpha (the order its own GIF and WebP decoders
    /// produce), so the red and blue bytes are swapped here. gpui plays the frames
    /// by itself and holds them back while the window is inactive or the system
    /// asks for reduced motion. Use [`Playback`] when you drive the frames yourself.
    ///
    /// This copies the pixels. Build the `RenderImage` once and keep it.
    pub fn to_bgra_frames(&self) -> Vec<image::Frame> {
        self.frames
            .iter()
            .map(|frame| {
                let mut bgra = frame.rgba.clone();
                for px in bgra.as_chunks_mut::<4>().0 {
                    px.swap(0, 2);
                }
                let buffer = image::RgbaImage::from_raw(self.width, self.height, bgra)
                    .expect("frame length matches the image size");
                image::Frame::from_parts(
                    buffer,
                    0,
                    0,
                    image::Delay::from_saturating_duration(frame.delay),
                )
            })
            .collect()
    }
}

/// Decides which frame of an animation to show. It is pure: the caller passes
/// the time, so pausing (window unfocused) and reduced motion are exact and testable.
///
/// ```
/// use std::time::{Duration, Instant};
/// use inlet_media::Playback;
///
/// let t0 = Instant::now();
/// let mut p = Playback::new(vec![Duration::from_millis(100); 3], t0);
/// assert_eq!(p.frame_at(t0 + Duration::from_millis(250)), 2);
/// p.pause(t0 + Duration::from_millis(250));
/// assert_eq!(p.frame_at(t0 + Duration::from_secs(60)), 2);
/// ```
#[derive(Debug, Clone)]
pub struct Playback {
    delays: Vec<Duration>,
    total: Duration,
    current: usize,
    into_frame: Duration,
    last: Option<Instant>,
    paused: bool,
    reduced_motion: bool,
}

impl Playback {
    /// Starts playing at `now`. A still image (one delay or none) never moves.
    pub fn new(delays: Vec<Duration>, now: Instant) -> Self {
        let delays: Vec<Duration> = delays
            .into_iter()
            .map(|d| d.max(Duration::from_millis(1)))
            .collect();
        let total = delays.iter().sum();
        Self {
            delays,
            total,
            current: 0,
            into_frame: Duration::ZERO,
            last: Some(now),
            paused: false,
            reduced_motion: false,
        }
    }

    /// Playback for a decoded image.
    pub fn for_image(image: &DecodedImage, now: Instant) -> Self {
        Self::new(image.delays(), now)
    }

    fn running(&self) -> bool {
        !self.paused && !self.reduced_motion && self.delays.len() > 1
    }

    fn advance(&mut self, now: Instant) {
        let Some(last) = self.last else {
            return;
        };
        self.last = Some(now.max(last));
        if !self.running() {
            return;
        }
        // Whole cycles carry no information. Dropping them keeps the loop below
        // short after a long sleep of the machine.
        let mut elapsed = self.into_frame + now.saturating_duration_since(last);
        if elapsed >= self.total {
            let nanos = elapsed.as_nanos() % self.total.as_nanos();
            elapsed = Duration::new(
                (nanos / 1_000_000_000) as u64,
                (nanos % 1_000_000_000) as u32,
            );
        }
        while elapsed >= self.delays[self.current] {
            elapsed -= self.delays[self.current];
            self.current = (self.current + 1) % self.delays.len();
        }
        self.into_frame = elapsed;
    }

    /// The frame to show at `now`.
    pub fn frame_at(&mut self, now: Instant) -> usize {
        self.advance(now);
        self.current
    }

    /// Stops the animation on the current frame. Call it when the window loses focus.
    pub fn pause(&mut self, now: Instant) {
        self.advance(now);
        self.paused = true;
    }

    /// Continues from where it stopped. The time spent paused does not count.
    pub fn resume(&mut self, now: Instant) {
        if self.paused {
            self.paused = false;
            self.last = Some(now);
        }
    }

    /// Reduced motion shows the first frame and keeps it, until switched off.
    pub fn set_reduced_motion(&mut self, reduced: bool, now: Instant) {
        if reduced == self.reduced_motion {
            return;
        }
        self.advance(now);
        self.reduced_motion = reduced;
        if reduced {
            self.current = 0;
            self.into_frame = Duration::ZERO;
        } else {
            self.last = Some(now);
        }
    }

    /// True while the frame changes with time.
    pub fn is_playing(&self) -> bool {
        self.running()
    }

    /// How long until the frame changes, or `None` when it never will (paused,
    /// reduced motion, or a still image). Use it to schedule the next repaint.
    pub fn time_to_next_frame(&self, now: Instant) -> Option<Duration> {
        if !self.running() {
            return None;
        }
        let since = now.saturating_duration_since(self.last.unwrap_or(now));
        Some(
            self.delays[self.current]
                .saturating_sub(self.into_frame)
                .saturating_sub(since),
        )
    }
}
