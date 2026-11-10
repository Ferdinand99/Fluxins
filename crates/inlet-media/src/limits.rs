//! Hard limits for untrusted images, and the sizes the UI can ask for.

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
}

impl KindLimits {
    /// The defaults for a kind: emoji 512 KiB, avatars 2 MiB, attachments 20 MiB.
    pub fn for_kind(kind: MediaKind) -> Self {
        const MIB: usize = 1024 * 1024;
        match kind {
            MediaKind::Emoji => Self {
                max_encoded_bytes: MIB / 2,
                max_width: 2048,
                max_height: 2048,
                max_pixels: 2048 * 2048,
                max_frames: 512,
                max_animation_bytes: 64 * MIB as u64,
                max_animation_work_pixels: 128_000_000,
            },
            MediaKind::Avatar => Self {
                max_encoded_bytes: 2 * MIB,
                max_width: 4096,
                max_height: 4096,
                max_pixels: 4096 * 4096,
                max_frames: 512,
                max_animation_bytes: 64 * MIB as u64,
                max_animation_work_pixels: 256_000_000,
            },
            MediaKind::Attachment => Self {
                max_encoded_bytes: 20 * MIB,
                max_width: 8192,
                max_height: 8192,
                max_pixels: 40_000_000,
                max_frames: 512,
                max_animation_bytes: 64 * MIB as u64,
                max_animation_work_pixels: 512_000_000,
            },
        }
    }
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
