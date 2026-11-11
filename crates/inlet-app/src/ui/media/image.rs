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
//! frame draws share one budget ([`AnimationBudget`]). One that is too large, failed,
//! or does not fit the budget shows its still (the first frame) instead
//! ([`Inlet::moving`]). Only animations on screen are drawn, so only they play; how
//! many move and when they stop is in `animation_budget.rs`.
//!
//! **Every load ends.** A load that has not answered after [`LOAD_TIMEOUT`] fails as
//! timed out, so a placeholder never waits forever; a failure is drawn as an error
//! tile (`failed.rs`) with a retry where one can help.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::future::Future;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::{select, Either};
use gpui::{div, img, prelude::*, px, AnyElement, Context, ObjectFit, RenderImage, Task};
use inlet_media::{MediaError, MediaRequest};

use super::animation_budget::{animated, AnimationBudget, Motion, Stage};
use super::MediaIcon;
use crate::app::Inlet;
use crate::theme;

/// Longest time a view waits for one image: the client's fetch timeout (30 s) plus a
/// wait for a fetch or decode slot and the decode itself. Past it the load is dropped
/// and the image counts as timed out.
pub(crate) const LOAD_TIMEOUT: Duration = Duration::from_secs(45);

/// `load`, unless `timeout` comes first: then the load is dropped (which cancels it
/// when nothing else waits for it) and the answer is [`MediaError::FetchTimeout`].
pub(crate) async fn bounded<T>(
    load: impl Future<Output = Result<T, MediaError>>,
    timeout: impl Future<Output = ()>,
) -> Result<T, MediaError> {
    match select(Box::pin(load), Box::pin(timeout)).await {
        Either::Left((result, _)) => result,
        Either::Right(((), _)) => Err(MediaError::FetchTimeout),
    }
}

/// The decoded bytes of a gpui image (all its frames).
pub(crate) fn decoded_bytes(image: &RenderImage) -> u64 {
    (0..image.frame_count())
        .map(|i| image.as_bytes(i).map_or(0, |b| b.len() as u64))
        .sum()
}

/// How an animated image is drawn: its kind (for the settings) and the shared budget.
#[derive(Clone)]
pub(crate) struct Animator {
    pub motion: Motion,
    pub budget: Rc<RefCell<AnimationBudget>>,
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
    /// The one animation budget, shared with the elements that draw animations.
    animations: Rc<RefCell<AnimationBudget>>,
}

impl Default for Images {
    fn default() -> Self {
        Self {
            entries: RefCell::default(),
            frame: Cell::default(),
            animations: Rc::new(RefCell::new(AnimationBudget::default())),
        }
    }
}

impl Images {
    /// Starts a frame: lets go of every image the previous frame did not draw. `stage`
    /// is what the window is like (see [`Stage`]).
    pub fn next_frame(&self, stage: Stage) {
        let frame = self.frame.get() + 1;
        self.frame.set(frame);
        self.animations
            .borrow_mut()
            .next_frame(stage, Instant::now());
        self.entries
            .borrow_mut()
            .retain(|_, e| e.used.saturating_add(1) >= frame);
    }

    pub fn clear(&self) {
        self.entries.borrow_mut().clear();
    }

    /// How animations of `motion` are drawn this frame.
    pub(crate) fn animator(&self, motion: Motion) -> Animator {
        Animator {
            motion,
            budget: self.animations.clone(),
        }
    }

    /// The shared animation budget (its settings, its counters).
    pub(crate) fn animations(&self) -> &Rc<RefCell<AnimationBudget>> {
        &self.animations
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
        let timeout = cx.background_executor().timer(LOAD_TIMEOUT);
        let task = cx.spawn(async move |this, cx| {
            let result = bounded(client.load(key.clone()), timeout).await;
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
        // While the picker is the layer on top, only its own images may take a place in
        // the budget: the frozen chat under it shows stills and leaves the room to it.
        let may_move = self.media.images.animations.borrow().may_admit();
        if let Some(anim) = moving.filter(|_| may_move) {
            let images = &self.media.images;
            let known = images.animations.borrow().known(anim);
            if known.is_none_or(|bytes| images.animations.borrow().fits(bytes)) {
                match self.image(anim, cx) {
                    Shown::Ready(image) => {
                        let bytes = decoded_bytes(&image);
                        let mut budget = images.animations.borrow_mut();
                        budget.remember(anim, bytes);
                        if budget.admit_image(image.id, bytes) {
                            budget.mark_if_top_layer(image.id);
                            return Shown::Ready(image);
                        }
                        drop(budget);
                        // Over the budget: let it go now, the still shows.
                        images.remove(anim);
                    }
                    Shown::Loading => {
                        // The still loads alongside: it shows first, and it is what
                        // stays if the animation fails or times out.
                        return match still.map(|s| self.image(s, cx)) {
                            Some(Shown::Ready(image)) => Shown::Ready(image),
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

    /// Loads `requests` again from the start: what the views hold of them goes, and
    /// the client forgets their failures (it would answer with them for a while).
    pub(crate) fn retry_media(&self, requests: &[MediaRequest], cx: &mut Context<Self>) {
        for req in requests {
            self.media.images.remove(req);
            if let Some(setup) = &self.media.setup {
                setup.client.forget_failure(req);
            }
        }
        cx.notify();
    }
}

/// Draws what [`Inlet::image`] gave. An animation moves under the rules of `anim`'s
/// budget (gpui's own `img` would only ever show its first frame here).
pub(crate) fn picture_of(
    shown: Shown,
    (w, h): (f32, f32),
    radius: f32,
    fit: ObjectFit,
    anim: &Animator,
) -> AnyElement {
    match shown {
        Shown::Ready(image) if image.frame_count() > 1 => {
            animated(image, (w, h), radius, fit, anim.motion, anim.budget.clone())
        }
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
    fn a_load_that_never_answers_ends_as_timed_out() {
        use futures::executor::block_on;
        use futures::future::{pending, ready};
        // No answer before the timer: a timeout, never an endless wait.
        let never = pending::<Result<u32, MediaError>>();
        assert_eq!(
            block_on(bounded(never, ready(()))),
            Err(MediaError::FetchTimeout)
        );
        // An answer in time is passed on, a success or a failure.
        assert_eq!(block_on(bounded(ready(Ok(7)), pending())), Ok(7));
        let refused = MediaError::HostNotAllowed("x.example".into());
        assert_eq!(
            block_on(bounded(ready(Err::<u32, _>(refused.clone())), pending())),
            Err(refused)
        );
        // The bound covers the client's own fetch timeout and a wait for a slot.
        assert!(LOAD_TIMEOUT > inlet_media::MediaConfig::default().fetch_timeout);
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
        images.next_frame(Stage::LIVE);
        images.insert_ready(req(1), image.clone());
        images.insert_ready(req(2), image.clone());
        assert_eq!(Arc::strong_count(&image), 3);
        // Frame 2 draws only the first; both survive into it.
        images.next_frame(Stage::LIVE);
        images.touch(&req(1));
        assert_eq!(images.len(), 2);
        // Frame 3: the second was not drawn in frame 2 and goes.
        images.next_frame(Stage::LIVE);
        assert_eq!(images.len(), 1);
        assert_eq!(Arc::strong_count(&image), 2);
        // Nothing drawn for a frame: everything goes.
        images.next_frame(Stage::LIVE);
        assert_eq!(images.len(), 0);
        assert_eq!(Arc::strong_count(&image), 1);
    }
}
