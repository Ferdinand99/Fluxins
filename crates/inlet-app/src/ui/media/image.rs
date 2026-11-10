// SPDX-License-Identifier: AGPL-3.0-or-later
//! Loading images for the views and drawing them, with a placeholder of the same size
//! while they load or when they cannot be shown, so a row never changes height.
//!
//! A view asks for an image while it draws ([`Inlet::image`]). The first time, the
//! image comes from the client's memory cache or starts loading in a task; the task is
//! dropped (and the load cancelled) when a frame passes without the image being drawn.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::Arc;

use gpui::{div, img, prelude::*, px, AnyElement, Context, ObjectFit, RenderImage, Task};
use inlet_media::{MediaError, MediaRequest};

use super::MediaIcon;
use crate::app::Inlet;
use crate::theme;

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
#[derive(Default)]
pub struct Images {
    entries: RefCell<HashMap<MediaRequest, Entry>>,
    frame: Cell<u64>,
}

impl Images {
    /// Starts a frame: lets go of every image the previous frame did not draw.
    pub fn next_frame(&self) {
        let frame = self.frame.get() + 1;
        self.frame.set(frame);
        self.entries
            .borrow_mut()
            .retain(|_, e| e.used.saturating_add(1) >= frame);
    }

    pub fn clear(&self) {
        self.entries.borrow_mut().clear();
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

    /// An image of `w` x `h` logical pixels, or a placeholder of the same size while it
    /// loads or when it cannot be shown.
    pub(crate) fn picture(
        &self,
        req: Option<&MediaRequest>,
        (w, h): (f32, f32),
        radius: f32,
        fit: ObjectFit,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let shown = match req {
            Some(req) => self.image(req, cx),
            None => Shown::Unavailable,
        };
        picture_of(shown, (w, h), radius, fit)
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
