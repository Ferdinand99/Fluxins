// SPDX-License-Identifier: AGPL-3.0-or-later
//! The image viewer: an image at a larger size over the whole window, with zoom, save,
//! copy link and open original. A click on the backdrop or the close button closes it.
//!
//! It asks the proxy for at most 2560 pixels on the longest side. That is larger than
//! the image cache keeps, so the image lives only while the viewer shows it.

use gpui::{div, prelude::*, px, AnyElement, ClipboardItem, Context, FontWeight, ObjectFit};
use inlet_media::MediaRequest;
use url::Url;

use super::attachment::icon_button;
use super::{requests, MediaIcon};
use crate::app::Inlet;
use crate::model::{Attachment, EmbedMedia};
use crate::theme;

/// Zoom steps; `None` fits the window.
const ZOOMS: [f32; 7] = [0.25, 0.5, 0.75, 1., 1.5, 2., 3.];
/// Space around the image when it fits the window.
const MARGIN: f32 = 72.;

/// What the viewer shows.
#[derive(Clone)]
pub struct Viewer {
    request: Option<MediaRequest>,
    /// Natural size in logical pixels (at most the requested size).
    size: (f32, f32),
    name: String,
    alt: Option<String>,
    /// Saving: the original on the media endpoint and its announced size.
    save: Option<(Url, u64)>,
    /// Opened in the browser; `masked` asks first (a third-party link).
    original: Option<(String, bool)>,
    /// `None` fits the window.
    zoom: Option<f32>,
}

impl Viewer {
    /// The viewer for an image attachment.
    pub fn for_attachment(this: &Inlet, a: &Attachment) -> Option<Viewer> {
        let setup = this.media.setup.as_ref()?;
        let max = requests::VIEWER_MAX_PIXELS as f32;
        let size = requests::fit_inside(a.width, a.height, (max, max));
        let original = requests::attachment_original(&setup.proxy, a);
        Some(Viewer {
            request: requests::attachment_full(&setup.proxy, a),
            size,
            name: a.filename.clone(),
            alt: a.alt.clone(),
            save: original.clone().map(|u| (u, a.size)),
            original: original.map(|u| (u.to_string(), false)),
            zoom: None,
        })
    }

    /// The viewer for an embed image.
    pub fn for_embed(this: &Inlet, m: &EmbedMedia, name: String) -> Option<Viewer> {
        let setup = this.media.setup.as_ref()?;
        let max = requests::VIEWER_MAX_PIXELS as f32;
        let size = requests::fit_inside(m.width, m.height, (max, max));
        Some(Viewer {
            request: requests::embed_full(&setup.proxy, m),
            size,
            name,
            alt: None,
            save: requests::embed_original(&setup.proxy, m).map(|u| (u, 0)),
            original: (!m.url.is_empty()).then(|| (m.url.clone(), true)),
            zoom: None,
        })
    }

    /// The zoom that fits `size` into the window.
    fn fit_zoom(&self, viewport: (f32, f32)) -> f32 {
        let (w, h) = self.size;
        let avail = (
            (viewport.0 - 2. * MARGIN).max(64.),
            (viewport.1 - 2. * MARGIN - 48.).max(64.),
        );
        (avail.0 / w).min(avail.1 / h).min(1.)
    }

    fn step(&mut self, viewport: (f32, f32), up: bool) {
        let now = self.zoom.unwrap_or_else(|| self.fit_zoom(viewport));
        let next = if up {
            ZOOMS.iter().copied().find(|z| *z > now + 0.01)
        } else {
            ZOOMS.iter().rev().copied().find(|z| *z < now - 0.01)
        };
        if let Some(z) = next {
            self.zoom = Some(z);
        }
    }
}

impl Inlet {
    /// The viewer over the window, if it is open.
    pub(crate) fn render_image_viewer(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let viewer = self.media.viewer.clone()?;
        let viewport = (
            self.media.viewport.width.as_f32(),
            self.media.viewport.height.as_f32(),
        );
        let zoom = viewer.zoom.unwrap_or_else(|| viewer.fit_zoom(viewport));
        let shown = (viewer.size.0 * zoom, viewer.size.1 * zoom);
        let picture = self.picture(viewer.request.as_ref(), shown, 0., ObjectFit::Contain, cx);
        let image = div()
            .id("viewer-image")
            .max_w(px((viewport.0 - 2. * MARGIN).max(64.)))
            .max_h(px((viewport.1 - 2. * MARGIN - 48.).max(64.)))
            .overflow_scroll()
            .child(picture)
            // A click on the image itself does not close the viewer.
            .on_click(|_, _, cx| cx.stop_propagation());

        let mut tools = div().flex().flex_row().gap_2();
        tools = tools
            .child(
                icon_button("viewer-zoom-out".into(), MediaIcon::ZoomOut).on_click(cx.listener(
                    move |this, _, _, cx| {
                        cx.stop_propagation();
                        if let Some(v) = &mut this.media.viewer {
                            v.step(viewport, false);
                        }
                        cx.notify();
                    },
                )),
            )
            .child(
                icon_button("viewer-zoom-in".into(), MediaIcon::ZoomIn).on_click(cx.listener(
                    move |this, _, _, cx| {
                        cx.stop_propagation();
                        if let Some(v) = &mut this.media.viewer {
                            v.step(viewport, true);
                        }
                        cx.notify();
                    },
                )),
            )
            .child(
                icon_button("viewer-fit".into(), MediaIcon::Fit).on_click(cx.listener(
                    |this, _, _, cx| {
                        cx.stop_propagation();
                        if let Some(v) = &mut this.media.viewer {
                            v.zoom = None;
                        }
                        cx.notify();
                    },
                )),
            );
        if let Some((url, size)) = viewer.save.clone() {
            let name = viewer.name.clone();
            tools = tools.child(
                icon_button("viewer-save".into(), MediaIcon::Download).on_click(cx.listener(
                    move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.save_media(url.clone(), &name, size, cx);
                    },
                )),
            );
        }
        if let Some((link, _)) = viewer.original.clone() {
            tools = tools.child(icon_button("viewer-copy".into(), MediaIcon::Copy).on_click(
                cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    cx.write_to_clipboard(ClipboardItem::new_string(link.clone()));
                    this.chat.notice = Some(t!("media-link-copied"));
                    cx.notify();
                }),
            ));
        }
        if let Some((link, masked)) = viewer.original.clone() {
            tools = tools.child(
                icon_button("viewer-open".into(), MediaIcon::OpenOriginal).on_click(cx.listener(
                    move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.open_link(&link, masked, cx);
                    },
                )),
            );
        }
        tools = tools.child(
            icon_button("viewer-close".into(), MediaIcon::Close).on_click(cx.listener(
                |this, _, _, cx| {
                    cx.stop_propagation();
                    this.media.viewer = None;
                    cx.notify();
                },
            )),
        );

        let caption = div()
            .flex()
            .flex_col()
            .items_center()
            .max_w(px((viewport.0 - 2. * MARGIN).max(64.)))
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::text_strong())
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(viewer.name.clone()),
            )
            .children(viewer.alt.clone().map(|alt| {
                div()
                    .text_size(px(14.))
                    .text_color(theme::text_muted())
                    .child(alt)
            }));

        Some(
            div()
                .id("image-viewer")
                .occlude()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .bg(theme::viewer_backdrop())
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_3()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.media.viewer = None;
                    cx.notify();
                }))
                .child(div().absolute().top(px(16.)).right(px(16.)).child(tools))
                .child(image)
                .child(caption)
                .into_any_element(),
        )
    }

    /// Demo screenshots: opens the viewer on the first image of the open channel.
    #[cfg(feature = "demo")]
    pub(super) fn demo_open_viewer(&mut self, cx: &mut Context<Self>) {
        use crate::ui::chat::messages::Row;
        if self.media.demo_viewer_done
            || crate::demo::scenes::current() != crate::demo::scenes::Scene::ImageViewer
            || self.media.setup.is_none()
        {
            return;
        }
        // The newest image that is neither a spoiler nor explicit.
        let first = self.chat.view.rows.iter().rev().find_map(|row| match row {
            Row::Message(m) => m
                .message
                .attachments
                .iter()
                .find(|a| {
                    a.kind == crate::model::AttachmentKind::Image && !a.spoiler && !a.explicit
                })
                .cloned(),
            _ => None,
        });
        if let Some(a) = first {
            self.media.viewer = Viewer::for_attachment(self, &a);
            self.media.demo_viewer_done = true;
            cx.notify();
        }
    }
}
