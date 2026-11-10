//! Hard limits for untrusted images, and the sizes the UI can ask for.

use std::time::Duration;

/// What an image is used for. Decides the limits that apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaKind {
    /// Custom emoji and stickers.
    Emoji,
    /// User avatars, guild icons and banners.
    Avatar,
    /// Attachment previews and embed images.
    Attachment,
}

/// Limits for one kind of image. All of them are checked before and during decoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KindLimits {
    /// Largest accepted encoded size, checked before any decoding.
    pub max_encoded_bytes: usize,
    /// Largest accepted width, as declared in the header.
    pub max_width: u32,
    /// Largest accepted height, as declared in the header.
    pub max_height: u32,
    /// Largest accepted `width * height`, as declared in the header.
    pub max_pixels: u64,
    /// Most frames of an animation.
    pub max_frames: usize,
    /// Most bytes of decoded (already downscaled) frames of an animation.
    pub max_animation_bytes: u64,
    /// Most source pixels an animation may cost to decode, summed over all frames.
    /// This bounds the work, because every frame is composed at full size first.
    pub max_animation_work_pixels: u64,
    /// Longest time one decode may take. It is checked between steps and between
    /// animation frames; a single still image cannot be interrupted, so the size
    /// limits above are what bound it.
    pub decode_budget: Duration,
}

impl KindLimits {
    /// The defaults for a kind.
    ///
    /// | Kind | Encoded | Side | Pixels | Time |
    /// |---|---|---|---|---|
    /// | Emoji | 512 KiB | 1024 | 1 Mpx | 2 s |
    /// | Avatar | 2 MiB | 2048 | 4.2 Mpx | 3 s |
    /// | Attachment | 20 MiB | 8192 | 24 Mpx | 5 s |
    pub fn for_kind(kind: MediaKind) -> Self {
        const MIB: usize = 1024 * 1024;
        match kind {
            MediaKind::Emoji => Self {
                max_encoded_bytes: MIB / 2,
                max_width: 1024,
                max_height: 1024,
                max_pixels: 1024 * 1024,
                max_frames: 512,
                max_animation_bytes: 64 * MIB as u64,
                max_animation_work_pixels: 64_000_000,
                decode_budget: Duration::from_secs(2),
            },
            MediaKind::Avatar => Self {
                max_encoded_bytes: 2 * MIB,
                max_width: 2048,
                max_height: 2048,
                max_pixels: 2048 * 2048,
                max_frames: 512,
                max_animation_bytes: 64 * MIB as u64,
                max_animation_work_pixels: 128_000_000,
                decode_budget: Duration::from_secs(3),
            },
            MediaKind::Attachment => Self {
                max_encoded_bytes: 20 * MIB,
                max_width: 8192,
                max_height: 8192,
                max_pixels: 24_000_000,
                max_frames: 512,
                max_animation_bytes: 64 * MIB as u64,
                max_animation_work_pixels: 256_000_000,
                decode_budget: Duration::from_secs(5),
            },
        }
    }

    /// The limits for an image that the app asked the media proxy to scale to
    /// `proxy_size` pixels on its longest side.
    ///
    /// The proxy never enlarges, but it snaps the size to a ladder and clamps it
    /// per asset class (an icon is never smaller than 128, a banner never smaller
    /// than 480), so the answer can be larger than asked. The side limit becomes
    /// four times the request, but at least 512, and never more than the kind
    /// allows. A "scaled" image that is far larger is not from the proxy.
    pub fn for_proxy_size(&self, proxy_size: u32) -> Self {
        let side = proxy_size.saturating_mul(4).max(512);
        let mut tight = self.clone();
        tight.max_width = tight.max_width.min(side);
        tight.max_height = tight.max_height.min(side);
        tight.max_pixels = tight.max_pixels.min(u64::from(side) * u64::from(side));
        tight
    }
}

/// The sizes the media proxy snaps `size` to. A value between two rungs gets the
/// next larger one.
pub const PROXY_SIZE_LADDER: [u32; 33] = [
    16, 20, 22, 24, 28, 32, 40, 44, 48, 56, 60, 64, 80, 96, 100, 128, 160, 240, 256, 300, 320, 480,
    512, 600, 640, 1024, 1280, 1536, 2048, 3072, 4096, 8192, 16384,
];

/// The smallest proxy ladder size that is at least `pixels`. Use it for the `size`
/// query parameter, so the cache key of the URL matches what the proxy serves.
pub fn snap_proxy_size(pixels: u32) -> u32 {
    PROXY_SIZE_LADDER
        .iter()
        .copied()
        .find(|&rung| rung >= pixels)
        .unwrap_or(16384)
}

/// Limits for every kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// Limits for [`MediaKind::Emoji`].
    pub emoji: KindLimits,
    /// Limits for [`MediaKind::Avatar`].
    pub avatar: KindLimits,
    /// Limits for [`MediaKind::Attachment`].
    pub attachment: KindLimits,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            emoji: KindLimits::for_kind(MediaKind::Emoji),
            avatar: KindLimits::for_kind(MediaKind::Avatar),
            attachment: KindLimits::for_kind(MediaKind::Attachment),
        }
    }
}

impl Limits {
    /// The limits for `kind`.
    pub fn get(&self, kind: MediaKind) -> &KindLimits {
        match kind {
            MediaKind::Emoji => &self.emoji,
            MediaKind::Avatar => &self.avatar,
            MediaKind::Attachment => &self.attachment,
        }
    }
}

/// The size the UI wants. The image is downscaled once to this size and never upscaled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Fit {
    /// Keep the aspect ratio and fit inside the box. Previews use 550x350.
    Contain {
        /// Box width in pixels.
        max_width: u32,
        /// Box height in pixels.
        max_height: u32,
    },
    /// Crop the centre to a square, then scale to `size` pixels. Avatars and icons
    /// use this: 32, 40, 44 or 96 logical pixels times the display scale.
    CoverSquare {
        /// Side in pixels.
        size: u32,
    },
}

impl Fit {
    /// The longest side of the output, at most.
    pub fn longest_side(self) -> u32 {
        match self.sanitized() {
            Self::Contain {
                max_width,
                max_height,
            } => max_width.max(max_height),
            Self::CoverSquare { size } => size,
        }
    }

    /// Largest side the UI may ask for. Larger requests are clamped.
    pub const MAX_SIDE: u32 = 4096;

    /// Fit inside 550x350 pixels, the size of Fluxer's message previews.
    pub const PREVIEW: Self = Self::Contain {
        max_width: 550,
        max_height: 350,
    };

    /// A square of `size` pixels.
    pub const fn square(size: u32) -> Self {
        Self::CoverSquare { size }
    }

    /// The same fit with zero sizes raised to 1 and large sizes clamped.
    pub(crate) fn sanitized(self) -> Self {
        let clamp = |v: u32| v.clamp(1, Self::MAX_SIDE);
        match self {
            Self::Contain {
                max_width,
                max_height,
            } => Self::Contain {
                max_width: clamp(max_width),
                max_height: clamp(max_height),
            },
            Self::CoverSquare { size } => Self::CoverSquare { size: clamp(size) },
        }
    }
}
