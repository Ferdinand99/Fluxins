// SPDX-License-Identifier: AGPL-3.0-or-later
//! Emoji pictures: the Twemoji drawings of Unicode emoji, and custom (guild) emoji from
//! the media proxy, for message text and reactions. Both are painted the same way
//! ([`paint_emoji`]), are bounded, and give their textures back to gpui when let go.
//!
//! - **Custom emoji** go the way of every other image: through the session's image
//!   client (`inlet-media`, limits checked before decoding, a byte-bounded cache) and
//!   the views' per-frame hold ([`Inlet::image`]). An animated one also asks for its
//!   animation, which then follows the one animation budget: it moves only while the
//!   window has the focus, at most 32 move at once, and with animated emoji switched
//!   off only the still is fetched.
//! - **Twemoji** are drawn from the bundled SVG files ([`EmojiImages`]): at most 500
//!   drawings are kept (and at most 200 different ones per text), and the ones let go
//!   are released from gpui at the next frame ([`release_twemoji`]).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    fill, prelude::*, px, AnyElement, App, Bounds, Context, Corners, Image, ImageFormat, ObjectFit,
    Pixels, RenderImage, Window,
};
use inlet_media::{snap_proxy_size, Fit, MediaKind, MediaRequest};
use inlet_protocol as p;
use url::Url;

use super::animation_budget::{paint_animated, AnimationBudget, Motion, MotionPrefs};
use super::image::Shown;
use crate::app::Inlet;
use crate::theme;

/// The size custom emoji are fetched for, in logical pixels: the largest they are
/// drawn (jumbo emoji, 48 px). Inline emoji (22 px) use the same picture.
pub const EMOJI_FETCH_SIZE: f32 = 48.;
/// Different custom emoji drawn as images in one message; the others stay blank.
pub const MAX_CUSTOM_PER_MESSAGE: usize = MAX_IMAGES_PER_TEXT;

// ---- Custom emoji ---------------------------------------------------------------------

/// The requests for a custom emoji: the animation (only for an animated emoji while
/// animated emoji are on; asked for at its logical size, as every animation) and the
/// still (at the device pixels shown).
pub fn custom_emoji_requests(
    proxy: &p::Media,
    id: &str,
    animated: bool,
    prefs: &MotionPrefs,
    scale: f32,
) -> (Option<MediaRequest>, Option<MediaRequest>) {
    let request = |animate: bool, scale: f32| {
        let px = snap_proxy_size((EMOJI_FETCH_SIZE * scale.max(1.)).ceil() as u32);
        let url = Url::parse(&proxy.emoji(id, animate, px)?).ok()?;
        Some(MediaRequest::new(url, MediaKind::Emoji, Fit::square(px)).with_proxy_size(px))
    };
    let moving = (animated && prefs.allows(Motion::Emoji))
        .then(|| request(true, 1.))
        .flatten();
    (moving, request(false, scale))
}

/// What a custom emoji shows this frame.
#[derive(Clone)]
pub enum Picture {
    Ready(Arc<RenderImage>),
    /// Still loading: its place stays blank.
    Loading,
    /// No picture (no image client, or it failed): a faint box in its place.
    Missing,
}

impl From<Shown> for Picture {
    fn from(shown: Shown) -> Self {
        match shown {
            Shown::Ready(image) => Picture::Ready(image),
            Shown::Loading => Picture::Loading,
            Shown::Failed(_) | Shown::Unavailable => Picture::Missing,
        }
    }
}

/// The custom emoji pictures of one message, for drawing its text.
pub struct EmojiPictures {
    pictures: HashMap<String, Picture>,
    pub(crate) budget: Rc<RefCell<AnimationBudget>>,
}

impl EmojiPictures {
    /// The picture of the custom emoji `id`.
    pub fn get(&self, id: &str) -> Picture {
        self.pictures.get(id).cloned().unwrap_or(Picture::Missing)
    }
}

impl Inlet {
    /// The picture of one custom emoji this frame; starts loading it the first time.
    pub(crate) fn custom_emoji_picture(
        &self,
        id: &str,
        animated: bool,
        cx: &mut Context<Self>,
    ) -> Picture {
        let Some(setup) = &self.media.setup else {
            return Picture::Missing;
        };
        let prefs = self.media.images.animations().borrow().prefs;
        let (moving, still) =
            custom_emoji_requests(&setup.proxy, id, animated, &prefs, self.media.scale);
        self.moving(moving.as_ref(), still.as_ref(), cx).into()
    }

    /// The pictures of the custom emoji `(id, animated)` of one message (at most
    /// [`MAX_CUSTOM_PER_MESSAGE`] different ones); `None` when there are none.
    pub(crate) fn emoji_pictures<'a>(
        &self,
        emoji: impl IntoIterator<Item = (&'a str, bool)>,
        cx: &mut Context<Self>,
    ) -> Option<Rc<EmojiPictures>> {
        let mut pictures = HashMap::new();
        for (id, animated) in emoji {
            if pictures.len() >= MAX_CUSTOM_PER_MESSAGE {
                break;
            }
            if !pictures.contains_key(id) {
                pictures.insert(id.to_string(), self.custom_emoji_picture(id, animated, cx));
            }
        }
        (!pictures.is_empty()).then(|| {
            Rc::new(EmojiPictures {
                pictures,
                budget: self.media.images.animations().clone(),
            })
        })
    }

    /// A custom emoji on its own, `size` logical pixels square (for reactions).
    #[allow(dead_code)] // for the reaction bubbles
    pub(crate) fn custom_emoji(
        &self,
        id: &str,
        animated: bool,
        size: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let picture = self.custom_emoji_picture(id, animated, cx);
        let budget = self.media.images.animations().clone();
        gpui::canvas(
            |_, _, _| (),
            move |bounds, (), window, cx| paint_emoji(&picture, bounds, &budget, window, cx),
        )
        .size(px(size))
        .flex_none()
        .into_any_element()
    }
}

/// Paints a custom emoji's picture into `bounds`.
pub(crate) fn paint_emoji(
    picture: &Picture,
    bounds: Bounds<Pixels>,
    budget: &Rc<RefCell<AnimationBudget>>,
    window: &mut Window,
    cx: &mut App,
) {
    match picture {
        Picture::Ready(image) => paint_animated(
            image,
            bounds,
            bounds,
            0.,
            ObjectFit::Contain,
            Motion::Emoji,
            budget,
            window,
            cx,
        ),
        Picture::Loading => {}
        Picture::Missing => window.paint_quad(
            fill(bounds, theme::media_placeholder()).corner_radii(Corners::all(px(4.))),
        ),
    }
}

// ---- Twemoji ----------------------------------------------------------------------------

// Deviation: the plan has Twemoji drawn by `inlet-media` (resvg), through its bounded
// image cache. `inlet-media` refuses SVG on purpose (it only ever sees untrusted
// network images), so the bundled drawings keep their own bounded cache here and go to
// gpui's `Image::from_bytes`. Only drawings from `inlet-emoji` reach gpui; no SVG from
// the network is ever drawn. Custom emoji use the shared image path; both are painted
// by the same code and released at the start of a frame.

/// Twemoji drawings are rendered this large, so they stay sharp at 48 px on a 1.5x
/// screen and at the inline size on a 2x one.
pub(crate) const RASTER: u32 = 64;
/// Drawings kept as images. Each is RASTER x RASTER x 4 bytes = 16 KiB decoded, and as
/// much in the GPU atlas: 500 images are 8 MB of each.
pub const MAX_IMAGES: usize = 500;
/// Different emoji drawn as images in one text; the others are painted as text.
pub const MAX_IMAGES_PER_TEXT: usize = 200;
/// An image used this recently may still be on screen, so it is never evicted to make
/// room; the new emoji is painted as text instead.
const IN_USE: Duration = Duration::from_millis(500);

/// One cached drawing.
struct Entry {
    image: Arc<Image>,
    /// What gpui decoded it to, once drawn: dropped from the atlas on eviction.
    render: Option<Arc<RenderImage>>,
    used: Instant,
}

/// A bounded, least-recently-used cache of Twemoji images. Evicted images are queued
/// and released from gpui (its asset cache and the GPU atlas) at the next frame.
#[derive(Default)]
pub struct EmojiImages {
    entries: HashMap<String, Entry>,
    /// Glyphs with no drawing (looked up once).
    missing: HashSet<String>,
    released: Vec<(Arc<Image>, Option<Arc<RenderImage>>)>,
}

impl EmojiImages {
    /// The image for `glyph`, or `None` when it has no drawing or every cached image
    /// is in use (it is painted as text then).
    fn reserve(&mut self, glyph: &str, now: Instant) -> Option<Arc<Image>> {
        if let Some(entry) = self.entries.get_mut(glyph) {
            entry.used = now;
            return Some(entry.image.clone());
        }
        if self.missing.contains(glyph) {
            return None;
        }
        if self.entries.len() >= MAX_IMAGES {
            let (oldest, used) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.used)
                .map(|(k, e)| (k.clone(), e.used))?;
            if now.saturating_duration_since(used) < IN_USE {
                return None;
            }
            if let Some(entry) = self.entries.remove(&oldest) {
                self.released.push((entry.image, entry.render));
            }
        }
        let Some(svg) = inlet_emoji::twemoji_svg(glyph) else {
            self.missing.insert(glyph.to_string());
            return None;
        };
        let image = Arc::new(Image::from_bytes(ImageFormat::Svg, sized_svg(&svg)));
        self.entries.insert(
            glyph.to_string(),
            Entry {
                image: image.clone(),
                render: None,
                used: now,
            },
        );
        Some(image)
    }

    /// Notes what gpui decoded `glyph` to, so eviction can drop it from the atlas.
    fn rendered(&mut self, glyph: &str, render: &Arc<RenderImage>) {
        if let Some(entry) = self.entries.get_mut(glyph) {
            entry.render.get_or_insert_with(|| render.clone());
        }
    }

    /// For each emoji of one text: its image, or `None` to paint it as text. At most
    /// [`MAX_IMAGES_PER_TEXT`] different emoji get an image.
    fn plan(&mut self, glyphs: &[&str], now: Instant) -> Vec<Option<Arc<Image>>> {
        let mut seen: HashMap<&str, Option<Arc<Image>>> = HashMap::new();
        glyphs
            .iter()
            .map(|glyph| {
                if let Some(found) = seen.get(glyph) {
                    return found.clone();
                }
                let image = if seen.len() < MAX_IMAGES_PER_TEXT {
                    self.reserve(glyph, now)
                } else {
                    None
                };
                seen.insert(glyph, image.clone());
                image
            })
            .collect()
    }

    fn take_released(&mut self) -> Vec<(Arc<Image>, Option<Arc<RenderImage>>)> {
        std::mem::take(&mut self.released)
    }
}

thread_local! {
    static IMAGES: RefCell<EmojiImages> = RefCell::new(EmojiImages::default());
}

/// For each Twemoji of one text: its drawing, or `None` to paint it as text.
pub(crate) fn twemoji_plan(glyphs: &[&str]) -> Vec<Option<Arc<Image>>> {
    IMAGES.with(|c| c.borrow_mut().plan(glyphs, Instant::now()))
}

/// Notes what gpui decoded `glyph` to (released from the atlas when evicted).
pub(crate) fn twemoji_rendered(glyph: &str, render: &Arc<RenderImage>) {
    IMAGES.with(|c| c.borrow_mut().rendered(glyph, render));
}

/// Releases the Twemoji drawings the cache let go of: gpui's copy and the atlas entry.
/// Called at the start of every frame, with the other media releases.
pub(crate) fn release_twemoji(window: &mut Window, cx: &mut App) {
    for (image, render) in IMAGES.with(|c| c.borrow_mut().take_released()) {
        image.remove_asset(cx);
        if let Some(render) = render {
            cx.drop_image(render, Some(window));
        }
    }
}

/// The bundled Twemoji SVG for `glyph`, sized to [`RASTER`] pixels. Only drawings from
/// `inlet-emoji` reach this; no SVG from the network is ever drawn.
fn sized_svg(svg: &[u8]) -> Vec<u8> {
    // Twemoji drawings have a 36 x 36 view box and no size: give the root a size so
    // gpui renders them at RASTER pixels instead of 36.
    let tag = svg
        .windows(4)
        .position(|w| w[0] == b'<' && &w[1..] == b"svg")
        .map(|p| p + 4);
    let Some(at) = tag else {
        return svg.to_vec();
    };
    let head = &svg[..svg.iter().position(|b| *b == b'>').unwrap_or(svg.len())];
    if head.windows(5).any(|w| w == b"width") {
        return svg.to_vec();
    }
    let attrs = [("width", RASTER), ("height", RASTER)]
        .iter()
        .map(|(name, value)| format!("{name}=\"{value}\""))
        .collect::<Vec<_>>()
        .join(" ");
    let mut out = Vec::with_capacity(svg.len() + attrs.len() + 2);
    out.extend_from_slice(&svg[..at]);
    out.push(b' ');
    out.extend_from_slice(attrs.as_bytes());
    out.extend_from_slice(&svg[at..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn svg_gets_a_size_once() {
        let svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 36 36\"><path/></svg>";
        let sized = String::from_utf8(sized_svg(svg)).unwrap();
        assert!(
            sized.starts_with("<svg width=\"64\" height=\"64\" xmlns"),
            "{sized}"
        );
        assert_eq!(sized_svg(sized.as_bytes()), sized.as_bytes());
    }

    #[test]
    fn every_drawable_glyph_has_an_image() {
        let mut cache = EmojiImages::default();
        let now = Instant::now();
        assert!(cache.reserve("\u{1F600}", now).is_some());
        assert!(cache.reserve("\u{1F44B}\u{1F3FD}", now).is_some());
        assert!(cache.reserve("not an emoji", now).is_none());
        assert!(cache.missing.contains("not an emoji"));
    }

    /// Decoded bytes of the images in `cache` (what gpui keeps for each, and as much
    /// again in the GPU atlas).
    fn decoded_bytes(cache: &EmojiImages) -> usize {
        cache.entries.len() * (RASTER * RASTER * 4) as usize
    }

    #[test]
    fn emoji_images_are_bounded_and_released() {
        let distinct: Vec<&str> = inlet_emoji::glyphs()
            .filter(|g| inlet_emoji::twemoji_svg(g).is_some())
            .take(2_000)
            .collect();
        assert_eq!(distinct.len(), 2_000);
        let mut cache = EmojiImages::default();
        let start = Instant::now();

        // One message with 2 000 different emoji: 200 images, the rest as text.
        let plan = cache.plan(&distinct, start);
        let images = plan.iter().filter(|i| i.is_some()).count();
        assert_eq!(images, MAX_IMAGES_PER_TEXT);
        assert_eq!(cache.entries.len(), MAX_IMAGES_PER_TEXT);
        assert_eq!(decoded_bytes(&cache), 200 * 16 * 1024);
        let svg_bytes: usize = cache.entries.values().map(|e| e.image.bytes().len()).sum();
        eprintln!(
            "2000 distinct emoji: {images} images, {} KiB decoded (and in the atlas), {} KiB SVG",
            decoded_bytes(&cache) / 1024,
            svg_bytes / 1024
        );
        // The same emoji again reuses the images.
        let again = cache.plan(&distinct, start);
        assert_eq!(again.iter().filter(|i| i.is_some()).count(), images);
        assert_eq!(cache.entries.len(), MAX_IMAGES_PER_TEXT);

        // More texts on the same screen: the cache stops at its cap, nothing in use is
        // evicted, and the rest is painted as text.
        for chunk in distinct[200..1_000].chunks(200) {
            cache.plan(chunk, start);
        }
        assert_eq!(cache.entries.len(), MAX_IMAGES);
        assert!(cache.released.is_empty());
        assert!(decoded_bytes(&cache) <= 8 * 1024 * 1024);

        // Later, other emoji: the oldest images are evicted and queued for release.
        let later = start + Duration::from_secs(5);
        let plan = cache.plan(&distinct[1_000..1_100], later);
        assert!(plan.iter().all(Option::is_some));
        assert_eq!(cache.entries.len(), MAX_IMAGES);
        assert_eq!(cache.take_released().len(), 100);
        assert!(cache.take_released().is_empty());
    }

    #[test]
    fn animated_emoji_setting_off_fetches_only_the_still() {
        let proxy = p::Media::new("https://media.example.org").unwrap();
        let on = MotionPrefs::default();
        let off = MotionPrefs {
            animate_emoji: false,
            ..MotionPrefs::default()
        };
        // On: the animation at its logical size, the still at the pixels shown.
        let (moving, still) = custom_emoji_requests(&proxy, "123456789012345678", true, &on, 2.);
        let moving = moving.expect("an animation");
        assert!(moving.url.query().unwrap().contains("animated=true"));
        assert_eq!(moving.proxy_size, Some(48));
        assert_eq!(moving.kind, MediaKind::Emoji);
        let still = still.expect("a still");
        assert!(!still.url.query().unwrap().contains("animated"));
        assert_eq!(still.proxy_size, Some(96));
        assert!(still
            .url
            .as_str()
            .starts_with("https://media.example.org/emojis/123456789012345678.webp"));
        // Off: only the still, whose decoded image has one frame.
        let (moving, still) = custom_emoji_requests(&proxy, "123456789012345678", true, &off, 2.);
        assert!(moving.is_none());
        assert!(still.is_some());
        // A still emoji never asks for an animation; a bad ID asks for nothing.
        assert!(custom_emoji_requests(&proxy, "1", false, &on, 1.)
            .0
            .is_none());
        assert_eq!(
            custom_emoji_requests(&proxy, "../x", true, &on, 1.),
            (None, None)
        );
    }
}
