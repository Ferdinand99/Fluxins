// SPDX-License-Identifier: AGPL-3.0-or-later
//! Images in the window: avatars, guild icons and banners, attachments, embed media, and
//! the image viewer. Everything goes through the session's image client (`inlet-media`)
//! and the media proxy; see `backend::media`.
//!
//! **Memory.** Decoded images live in the client's cache, bounded in bytes. The views
//! hold an image only while they draw it: every image a frame does not draw is let go at
//! the next frame (its load is cancelled if it was still loading), and a texture is freed
//! once the cache has let go of it and nothing draws it any more.

mod attachment;
mod audio_file;
mod avatar;
pub(crate) mod embed;
mod image;
pub mod requests;
mod video_thumb;
mod viewer;

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use futures::StreamExt;
use gpui::{Context, Pixels, RenderImage, Size, Task, Window};

use crate::app::Inlet;
use crate::backend::media::{Ev, Setup};

pub use attachment::Refresh;
pub(crate) use image::Images;
pub use viewer::Viewer;

/// Spoilers and explicit media shown on request, remembered for this many items.
const MAX_REVEALED: usize = 1_000;
/// Width around the content of a cozy message row: 16 | 40 avatar | 12 | … | 16.
const COZY_GUTTER: f32 = 84.;
/// The same for compact rows: 16 | 64 time | 8 | name | 8 | … | 16, with room for a
/// name (the content starts after it).
const COMPACT_GUTTER: f32 = 220.;
/// Media never shrink below this width.
const MIN_MEDIA_WIDTH: f32 = 120.;
/// Width an embed card takes around its media: border, colour bar and padding.
pub const EMBED_CHROME: f32 = 30.;

/// `(w, h)` scaled down, keeping its shape, to at most `max_w` wide.
fn fit_width((w, h): (f32, f32), max_w: f32) -> (f32, f32) {
    if w <= max_w || w <= 0. {
        return (w, h);
    }
    let max_w = max_w.floor();
    (max_w, (h * max_w / w).round().max(1.))
}

/// The media view state of the signed-in session.
pub struct State {
    /// The image client of the session; `None` before sign-in or without a media endpoint.
    pub setup: Option<Setup>,
    pub images: Images,
    /// Textures the cache let go of, freed once nothing draws them.
    pending_drop: Vec<Arc<RenderImage>>,
    /// Frees evicted textures as soon as the cache lets go of them, between frames too.
    evict_task: Option<Task<()>>,
    /// The display scale, for asking the proxy for device pixels.
    pub scale: f32,
    /// The window size, for the viewer.
    pub viewport: Size<Pixels>,
    /// Width of the message list at the last layout, read once per frame before the
    /// list lays out (the list's state is borrowed while it draws its rows).
    column_width: f32,
    /// The message under the pointer: its author's avatar plays if animated.
    pub hovered_message: Option<String>,
    /// Spoilers and explicit media the person chose to show.
    revealed: HashSet<String>,
    revealed_order: VecDeque<String>,
    /// Cover media marked explicit until clicked. On by default, as in Fluxer; the
    /// settings task makes it a choice.
    pub cover_explicit: bool,
    /// Fresh signed URLs of expired attachments.
    pub refresh: Refresh,
    /// The open image viewer.
    pub viewer: Option<Viewer>,
    /// Demo screenshots: the viewer was opened once for the `image-viewer` scene.
    #[cfg(feature = "demo")]
    pub demo_viewer_done: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            setup: None,
            images: Images::default(),
            pending_drop: Vec::new(),
            evict_task: None,
            scale: 1.,
            viewport: Size::default(),
            column_width: 0.,
            hovered_message: None,
            revealed: HashSet::new(),
            revealed_order: VecDeque::new(),
            cover_explicit: true,
            refresh: Refresh::default(),
            viewer: None,
            #[cfg(feature = "demo")]
            demo_viewer_done: false,
        }
    }
}

impl State {
    /// Whether the spoiler or explicit item `key` was revealed.
    pub fn is_revealed(&self, key: &str) -> bool {
        self.revealed.contains(key)
    }

    pub fn reveal(&mut self, key: String) {
        if self.revealed.insert(key.clone()) {
            self.revealed_order.push_back(key);
            while self.revealed_order.len() > MAX_REVEALED {
                if let Some(old) = self.revealed_order.pop_front() {
                    self.revealed.remove(&old);
                }
            }
        }
    }

    /// Ends the session's media: every image is let go, the textures are freed at the
    /// next frame.
    pub fn reset(&mut self) {
        self.viewer = None;
        self.images.clear();
        if let Some(setup) = self.setup.take() {
            setup.client.clear_memory();
            self.pending_drop.extend(setup.evicted.drain());
        }
        self.evict_task = None;
        self.hovered_message = None;
        self.revealed.clear();
        self.revealed_order.clear();
        self.refresh = Refresh::default();
    }
}

impl Inlet {
    /// Once per frame, before anything is drawn: lets go of the images the last frame
    /// did not draw and frees textures nothing draws any more.
    pub(crate) fn media_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.media.scale = window.scale_factor();
        self.media.viewport = window.viewport_size();
        // Here, not while rows are drawn: the list holds its state borrowed then.
        self.media.column_width = self.chat.message_list.viewport_bounds().size.width.as_f32();
        self.media.images.next_frame();
        self.release_evicted(Some(window), cx);
        #[cfg(feature = "demo")]
        self.demo_open_viewer(cx);
    }

    /// Takes the images the cache let go of and frees the texture of every one that
    /// nothing draws any more; the others wait for a later frame.
    fn release_evicted(&mut self, mut window: Option<&mut Window>, cx: &mut Context<Self>) {
        if let Some(setup) = &self.media.setup {
            self.media.pending_drop.extend(setup.evicted.drain());
        }
        let mut keep = Vec::new();
        for image in self.media.pending_drop.drain(..) {
            // Only this list holds it: nothing draws it, free the texture.
            if Arc::strong_count(&image) > 1 {
                keep.push(image);
            } else {
                cx.drop_image(image, window.as_deref_mut());
            }
        }
        self.media.pending_drop = keep;
    }

    /// The shown size of media of `size`, shrunk to fit the message column (less
    /// `chrome`, the width of a card around it) so a narrow window never clips it. The
    /// request keeps its own size, so resizing the window loads nothing again.
    pub(crate) fn fit_column(&self, size: (f32, f32), chrome: f32) -> (f32, f32) {
        let list = self.media.column_width;
        if list <= 0. {
            return size;
        }
        let gutter = if self.chat.view.compact {
            COMPACT_GUTTER
        } else {
            COZY_GUTTER
        };
        fit_width(size, (list - gutter - chrome).max(MIN_MEDIA_WIDTH))
    }

    /// Events from the backend's media domain.
    pub(crate) fn on_media_event(
        &mut self,
        event: Ev,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            Ev::Ready(setup) => {
                self.media.reset();
                let mut setup = *setup;
                // Woken whenever the cache lets images go, so they are freed even while
                // no frame is drawn (a hidden window).
                self.media.evict_task = setup.evicted.wake.take().map(|mut wake| {
                    cx.spawn(async move |this, cx| {
                        while wake.next().await.is_some() {
                            let freed = this.update(cx, |this, cx| this.release_evicted(None, cx));
                            if freed.is_err() {
                                break;
                            }
                        }
                    })
                });
                self.media.setup = Some(setup);
            }
            Ev::Unavailable => self.media.reset(),
            Ev::Refreshed {
                message_id,
                attachments,
            } => self.media.refresh.refreshed(&message_id, attachments),
            Ev::RefreshFailed { message_id } => self.media.refresh.failed(&message_id),
        }
        cx.notify();
    }
}

/// Phosphor icons (MIT) of the media views.
#[derive(Clone, Copy)]
pub(crate) enum MediaIcon {
    File,
    FileAudio,
    Play,
    Download,
    OpenOriginal,
    Copy,
    ZoomIn,
    ZoomOut,
    Fit,
    Close,
    Broken,
}

impl MediaIcon {
    fn data(self) -> &'static [u8] {
        match self {
            MediaIcon::File => include_bytes!("../../../assets/icons/file.svg"),
            MediaIcon::FileAudio => include_bytes!("../../../assets/icons/file-audio.svg"),
            MediaIcon::Play => include_bytes!("../../../assets/icons/play.svg"),
            MediaIcon::Download => include_bytes!("../../../assets/icons/download-simple.svg"),
            MediaIcon::OpenOriginal => {
                include_bytes!("../../../assets/icons/arrow-square-out.svg")
            }
            MediaIcon::Copy => include_bytes!("../../../assets/icons/copy.svg"),
            MediaIcon::ZoomIn => {
                include_bytes!("../../../assets/icons/magnifying-glass-plus.svg")
            }
            MediaIcon::ZoomOut => {
                include_bytes!("../../../assets/icons/magnifying-glass-minus.svg")
            }
            MediaIcon::Fit => include_bytes!("../../../assets/icons/arrows-in.svg"),
            MediaIcon::Close => include_bytes!("../../../assets/icons/x.svg"),
            MediaIcon::Broken => include_bytes!("../../../assets/icons/image-broken.svg"),
        }
    }

    pub fn el(self, size: Pixels, color: gpui::Hsla) -> gpui::Svg {
        use gpui::Styled;
        gpui::svg()
            .data(self.data())
            .size(size)
            .flex_none()
            .text_color(color)
    }
}

/// "1.5 MB" for a file size, in the units Fluxer uses (powers of 1024).
pub fn file_size(bytes: u64) -> String {
    if bytes < 1024 {
        return t!("media-size-bytes", size = bytes.to_string());
    }
    let mut v = bytes as f64 / 1024.;
    let mut unit = 0;
    while v >= 1024. && unit < 3 {
        v /= 1024.;
        unit += 1;
    }
    let size = if v >= 100. {
        format!("{v:.0}")
    } else {
        format!("{v:.1}")
    };
    match unit {
        0 => t!("media-size-kb", size = size),
        1 => t!("media-size-mb", size = size),
        2 => t!("media-size-gb", size = size),
        _ => t!("media-size-tb", size = size),
    }
}

/// "3:07" for a length in seconds.
pub fn duration(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60)
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_and_durations_read_like_fluxer() {
        assert_eq!(file_size(1536), "1.5 KB");
        assert_eq!(file_size(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(file_size(300 * 1024 * 1024), "300 MB");
        assert_eq!(duration(187), "3:07");
        assert_eq!(duration(3725), "1:02:05");
    }

    #[test]
    fn media_shrink_to_a_narrow_column_keeping_their_shape() {
        // Fits: unchanged.
        assert_eq!(fit_width((480., 350.), 600.), (480., 350.));
        // Too wide: scaled down, same shape.
        assert_eq!(fit_width((480., 350.), 440.), (440., 321.));
        assert_eq!(fit_width((550., 100.), 275.5), (275., 50.));
        // Degenerate sizes stay as they are.
        assert_eq!(fit_width((0., 10.), 100.), (0., 10.));
    }

    #[test]
    fn revealed_items_are_bounded() {
        let mut s = State::default();
        for i in 0..(MAX_REVEALED + 10) {
            s.reveal(format!("k{i}"));
        }
        assert!(!s.is_revealed("k0"));
        assert!(s.is_revealed(&format!("k{}", MAX_REVEALED + 9)));
        assert_eq!(s.revealed.len(), MAX_REVEALED);
    }
}
