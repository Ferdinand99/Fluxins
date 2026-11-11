// SPDX-License-Identifier: AGPL-3.0-or-later
//! Loading images for the views and drawing them, with a placeholder of the same size
//! while they load or when they cannot be shown, so a row never changes height.
//!
//! A view asks for an image while it draws ([`Inlet::image`]). The first time, the
//! image comes from the client's memory cache or starts loading in a task; the task is
//! dropped (and the load cancelled) when a frame passes without the image being drawn.
//!
//! **Animations** keep every frame decoded, so they are counted. Each is capped by
//! the client (`ANIMATION_MAX_BYTES`, see `backend::media`), and the animations one
//! frame draws share [`ANIMATION_BUDGET_BYTES`]. One that is too large, failed, or
//! does not fit the budget shows its still (the first frame) instead ([`Inlet::moving`]).
//! Only animations on screen are drawn, so only they play.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::Arc;

use gpui::{div, img, prelude::*, px, AnyElement, Context, ObjectFit, RenderImage, Task};
use inlet_media::{MediaError, MediaRequest};

use super::MediaIcon;
use crate::app::Inlet;
use crate::theme;

/// Decoded bytes the animations drawn in one frame may hold together (the GPU copies
/// of the frames they have shown come on top).
pub const ANIMATION_BUDGET_BYTES: u64 = 32 * 1024 * 1024;
/// Animations whose decoded size is remembered, so one over the budget is not loaded
/// again every frame.
const MAX_KNOWN_ANIMATIONS: usize = 512;

/// The decoded bytes of a gpui image (all its frames).
pub(crate) fn decoded_bytes(image: &RenderImage) -> u64 {
    (0..image.frame_count())
        .map(|i| image.as_bytes(i).map_or(0, |b| b.len() as u64))
        .sum()
}

/// The animations of one frame against [`ANIMATION_BUDGET_BYTES`].
#[derive(Default)]
pub struct AnimationBudget {
    budget: u64,
    used: u64,
    /// Decoded sizes of animations seen, by request.
    known: HashMap<MediaRequest, u64>,
}

impl AnimationBudget {
    pub fn new(budget: u64) -> Self {
        Self {
            budget,
            ..Self::default()
        }
    }

    /// A new frame: nothing plays yet.
    pub fn next_frame(&mut self) {
        self.used = 0;
    }

    /// Whether an animation of `bytes` still fits this frame.
    pub fn fits(&self, bytes: u64) -> bool {
        self.used.saturating_add(bytes) <= self.budget
    }

    /// Counts a playing animation of `bytes` if it fits; `false` when it does not.
    pub fn admit(&mut self, bytes: u64) -> bool {
        if !self.fits(bytes) {
            return false;
        }
        self.used = self.used.saturating_add(bytes);
        true
    }

    /// Bytes of the animations playing this frame.
    #[cfg(test)]
    pub fn used(&self) -> u64 {
        self.used
    }

    fn known(&self, req: &MediaRequest) -> Option<u64> {
        self.known.get(req).copied()
    }

    fn remember(&mut self, req: &MediaRequest, bytes: u64) {
        if self.known.len() >= MAX_KNOWN_ANIMATIONS && !self.known.contains_key(req) {
            self.known.clear();
        }
        self.known.insert(req.clone(), bytes);
    }
}

/// What a view can draw for an image this frame.
pub(crate) enum Shown {
    Ready(Arc<RenderImage>),
    Loading,
    Failed(MediaError),
    /// No image client (no session or no media endpoint) or no URL.
    Unavailable,
}

enum Slot {
    Loading(#[allow(dead_code)] Task<()>),
    Ready(Arc<RenderImage>),
    Failed(MediaError),
}

struct Entry {
    slot: Slot,
    /// The frame that last drew it.
    used: u64,
}

/// The images the views hold, by request. Only the ones drawn in the last frame stay.
pub struct Images {
    entries: RefCell<HashMap<MediaRequest, Entry>>,
    frame: Cell<u64>,
    animations: RefCell<AnimationBudget>,
}

impl Default for Images {
    fn default() -> Self {
        Self {
            entries: RefCell::default(),
            frame: Cell::default(),
            animations: RefCell::new(AnimationBudget::new(ANIMATION_BUDGET_BYTES)),
        }
    }
}

impl Images {
    /// Starts a frame: lets go of every image the previous frame did not draw.
    pub fn next_frame(&self) {
        let frame = self.frame.get() + 1;
        self.frame.set(frame);
        self.animations.borrow_mut().next_frame();
        self.entries
            .borrow_mut()
            .retain(|_, e| e.used.saturating_add(1) >= frame);
    }

    pub fn clear(&self) {
        self.entries.borrow_mut().clear();
    }

    /// Lets go of one image now.
    fn remove(&self, req: &MediaRequest) {
        self.entries.borrow_mut().remove(req);
    }

    /// Images held (loaded, loading or failed).
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.borrow().len()
    }

    /// A load finished. Returns whether a view still wants it.
    fn finish(&self, req: &MediaRequest, result: Result<Arc<RenderImage>, MediaError>) -> bool {
        let mut entries = self.entries.borrow_mut();
        let Some(entry) = entries.get_mut(req) else {
            return false;
        };
        entry.slot = match result {
            Ok(image) => Slot::Ready(image),
            Err(err) => Slot::Failed(err),
        };
        true
    }

    #[cfg(test)]
    fn insert_ready(&self, req: MediaRequest, image: Arc<RenderImage>) {
        let used = self.frame.get();
        self.entries.borrow_mut().insert(
            req,
            Entry {
                slot: Slot::Ready(image),
                used,
            },
        );
    }

    #[cfg(test)]
    fn touch(&self, req: &MediaRequest) {
        if let Some(e) = self.entries.borrow_mut().get_mut(req) {
            e.used = self.frame.get();
        }
    }
}

impl Inlet {
    /// The image for `req` this frame. Starts loading it the first time.
    pub(crate) fn image(&self, req: &MediaRequest, cx: &mut Context<Self>) -> Shown {
        let Some(setup) = &self.media.setup else {
            return Shown::Unavailable;
        };
        let images = &self.media.images;
        let frame = images.frame.get();
        let mut entries = images.entries.borrow_mut();
        if let Some(entry) = entries.get_mut(req) {
            entry.used = frame;
            return match &entry.slot {
                Slot::Loading(_) => Shown::Loading,
                Slot::Ready(image) => Shown::Ready(image.clone()),
                Slot::Failed(err) => Shown::Failed(err.clone()),
            };
        }
        if let Some(image) = setup.client.cached(req) {
            entries.insert(
                req.clone(),
                Entry {
                    slot: Slot::Ready(image.clone()),
                    used: frame,
                },
            );
            return Shown::Ready(image);
        }
        let client = setup.client.clone();
        let key = req.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = client.load(key.clone()).await;
            let _ = this.update(cx, |this, cx| {
                if this.media.images.finish(&key, result) {
                    cx.notify();
                }
            });
        });
        entries.insert(
            req.clone(),
            Entry {
                slot: Slot::Loading(task),
                used: frame,
            },
        );
        Shown::Loading
    }

    /// An image that may be animated: `moving` (the animation, if there is one) while
    /// it fits the frame's animation budget and loads, otherwise `still`. An animation
    /// that is too large, failed, or does not fit shows the still, never a broken
    /// image; while it loads, the still shows if it is at hand.
    pub(crate) fn moving(
        &self,
        moving: Option<&MediaRequest>,
        still: Option<&MediaRequest>,
        cx: &mut Context<Self>,
    ) -> Shown {
        if let Some(anim) = moving {
            let images = &self.media.images;
            let known = images.animations.borrow().known(anim);
            if known.is_none_or(|bytes| images.animations.borrow().fits(bytes)) {
                match self.image(anim, cx) {
                    Shown::Ready(image) => {
                        let bytes = decoded_bytes(&image);
                        let mut budget = images.animations.borrow_mut();
                        budget.remember(anim, bytes);
                        if budget.admit(bytes) {
                            return Shown::Ready(image);
                        }
                        drop(budget);
                        // Over the budget: let it go now, the still shows.
                        images.remove(anim);
                    }
                    Shown::Loading => {
                        return match still.map(|s| self.cached_only(s)) {
                            Some(Some(image)) => Shown::Ready(image),
                            _ => Shown::Loading,
                        };
                    }
                    // Too large, too many frames, or any other failure: the still.
                    Shown::Failed(_) | Shown::Unavailable => {}
                }
            }
        }
        match still {
            Some(req) => self.image(req, cx),
            None => Shown::Unavailable,
        }
    }

    /// An image the views or the cache already hold, without starting a load.
    fn cached_only(&self, req: &MediaRequest) -> Option<Arc<RenderImage>> {
        let images = &self.media.images;
        if let Some(entry) = images.entries.borrow_mut().get_mut(req) {
            if let Slot::Ready(image) = &entry.slot {
                entry.used = images.frame.get();
                return Some(image.clone());
            }
        }
        self.media.setup.as_ref()?.client.cached(req)
    }
}

/// Draws what [`Inlet::image`] gave.
pub(crate) fn picture_of(
    shown: Shown,
    (w, h): (f32, f32),
    radius: f32,
    fit: ObjectFit,
) -> AnyElement {
    match shown {
        Shown::Ready(image) => img(image)
            .w(px(w))
            .h(px(h))
            .flex_none()
            .rounded(px(radius))
            .object_fit(fit)
            .into_any_element(),
        Shown::Loading => placeholder((w, h), radius, false),
        Shown::Failed(_) | Shown::Unavailable => placeholder((w, h), radius, true),
    }
}

/// The box an image takes while loading; with a broken-image icon when it failed.
pub(crate) fn placeholder((w, h): (f32, f32), radius: f32, broken: bool) -> AnyElement {
    div()
        .w(px(w))
        .h(px(h))
        .flex_none()
        .rounded(px(radius))
        .bg(theme::media_placeholder())
        .flex()
        .items_center()
        .justify_center()
        .when(broken && w >= 24. && h >= 24., |d| {
            d.child(MediaIcon::Broken.el(px((w.min(h) / 3.).min(32.)), theme::text_muted()))
        })
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use inlet_media::{Fit, MediaKind};

    fn req(n: u32) -> MediaRequest {
        let url =
            url::Url::parse(&format!("https://media.example.org/avatars/1/{n}.webp")).unwrap();
        MediaRequest::new(url, MediaKind::Avatar, Fit::square(40))
    }

    #[test]
    fn animations_share_one_budget_per_frame() {
        const MIB: u64 = 1024 * 1024;
        let mut budget = AnimationBudget::new(ANIMATION_BUDGET_BYTES);
        // A capped animation (24 MiB) plays; a second one that does not fit shows its
        // still; small ones fill what is left.
        assert!(budget.admit(24 * MIB));
        assert!(!budget.admit(24 * MIB));
        assert!(budget.admit(6 * MIB));
        assert!(budget.admit(2 * MIB));
        assert!(!budget.admit(1));
        assert!(budget.used() <= ANIMATION_BUDGET_BYTES);
        // Many on screen: never more than the budget plays.
        let mut budget = AnimationBudget::new(ANIMATION_BUDGET_BYTES);
        let playing = (0..100).filter(|_| budget.admit(3 * MIB)).count();
        assert_eq!(playing, 10);
        assert!(budget.used() <= ANIMATION_BUDGET_BYTES);
        // A new frame starts empty; sizes seen stay known, bounded.
        budget.next_frame();
        assert_eq!(budget.used(), 0);
        for n in 0..(MAX_KNOWN_ANIMATIONS as u32 + 10) {
            budget.remember(&req(n), 1);
        }
        assert!(budget.known.len() <= MAX_KNOWN_ANIMATIONS);
        assert_eq!(budget.known(&req(MAX_KNOWN_ANIMATIONS as u32 + 9)), Some(1));
    }

    #[test]
    fn images_not_drawn_last_frame_are_let_go() {
        let images = Images::default();
        let limits = inlet_media::KindLimits::for_kind(MediaKind::Avatar);
        let decoded = inlet_media::decode(
            &crate::backend::media::png::solid(2, 2, [1, 2, 3, 255]),
            Fit::square(2),
            &limits,
            &inlet_media::DecodeControl::with_budget(std::time::Duration::from_secs(5)),
        )
        .unwrap();
        let image = Arc::new(RenderImage::new(decoded.into_gpui_frames()));
        images.next_frame();
        images.insert_ready(req(1), image.clone());
        images.insert_ready(req(2), image.clone());
        assert_eq!(Arc::strong_count(&image), 3);
        // Frame 2 draws only the first; both survive into it.
        images.next_frame();
        images.touch(&req(1));
        assert_eq!(images.len(), 2);
        // Frame 3: the second was not drawn in frame 2 and goes.
        images.next_frame();
        assert_eq!(images.len(), 1);
        assert_eq!(Arc::strong_count(&image), 2);
        // Nothing drawn for a frame: everything goes.
        images.next_frame();
        assert_eq!(images.len(), 0);
        assert_eq!(Arc::strong_count(&image), 1);
    }
}
