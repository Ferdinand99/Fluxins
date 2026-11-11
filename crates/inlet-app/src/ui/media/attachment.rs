// SPDX-License-Identifier: AGPL-3.0-or-later
//! Attachments under a message: images (at most 550 x 350), videos as a first frame with
//! a play button, audio and other files as a card with size and a save button. Spoilers
//! and explicit media are covered until clicked.
//!
//! Signed attachment URLs expire. When an attachment's time has passed, or its image
//! fails the way an expired URL does, the view asks the server for the message again,
//! once per message, and draws the fresh URLs ([`Refresh`]).
//!
//! Saving asks for a place with the system's save dialog and writes the file there;
//! nothing is opened afterwards.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use gpui::{div, prelude::*, px, AnyElement, Context, FontWeight, ObjectFit, SharedString};
use inlet_media::{FetchError, MediaError};
use url::Url;

use super::image::{picture_of, placeholder, Shown};
use super::{file_size, requests, MediaIcon};
use crate::app::Inlet;
use crate::backend::media::{self as backend_media, Cmd};
use crate::model::{Attachment, AttachmentKind};
use crate::theme;

/// Messages and attachments the refresh remembers.
const MAX_TRACKED: usize = 500;
/// At most this many refreshes start in [`REFRESH_WINDOW`]; the rest wait.
const REFRESHES_PER_WINDOW: usize = 4;
const REFRESH_WINDOW: Duration = Duration::from_secs(10);
/// Width of a file card.
pub const FILE_CARD_WIDTH: f32 = 432.;
/// Height of a file card (32 px icon, 12 px padding, borders).
const FILE_CARD_HEIGHT: f32 = 66.;

/// Fresh signed URLs for expired attachments, asked for at most once per message.
#[derive(Default)]
pub struct Refresh {
    inner: RefCell<RefreshInner>,
}

#[derive(Default)]
struct RefreshInner {
    asked: HashSet<String>,
    asked_order: VecDeque<String>,
    fresh: HashMap<String, Attachment>,
    fresh_order: VecDeque<String>,
    recent: VecDeque<Instant>,
}

/// Whether a failed load looks like an expired signed URL.
fn looks_expired(err: &MediaError) -> bool {
    matches!(
        err,
        MediaError::Fetch(FetchError::Status(401 | 403 | 404 | 410))
    )
}

fn remember<T>(set: &mut VecDeque<T>, item: T) -> Option<T> {
    set.push_back(item);
    if set.len() > MAX_TRACKED {
        set.pop_front()
    } else {
        None
    }
}

impl Refresh {
    /// The attachment with fresh URLs, if it was refreshed.
    pub fn current(&self, a: &Attachment) -> Attachment {
        self.inner
            .borrow()
            .fresh
            .get(&a.id)
            .cloned()
            .unwrap_or_else(|| a.clone())
    }

    /// Whether to ask for fresh URLs of message `message_id` now: `a` (as drawn) has
    /// expired by its time, or failed with `failure` the way an expired URL does. Only
    /// once per message, never for an attachment that was already refreshed, and at most
    /// a few per ten seconds (a later frame asks again).
    pub fn want(
        &self,
        message_id: &str,
        a: &Attachment,
        failure: Option<&MediaError>,
        now_ms: i64,
        now: Instant,
    ) -> bool {
        let mut s = self.inner.borrow_mut();
        if s.fresh.contains_key(&a.id) || s.asked.contains(message_id) {
            return false;
        }
        let expired = a.expired
            || a.expires_at_ms.is_some_and(|t| t <= now_ms)
            || failure.is_some_and(looks_expired);
        if !expired {
            return false;
        }
        while s
            .recent
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) >= REFRESH_WINDOW)
        {
            s.recent.pop_front();
        }
        if s.recent.len() >= REFRESHES_PER_WINDOW {
            return false;
        }
        s.recent.push_back(now);
        s.asked.insert(message_id.to_string());
        if let Some(old) = remember(&mut s.asked_order, message_id.to_string()) {
            s.asked.remove(&old);
        }
        true
    }

    /// Whether the message was asked about.
    #[cfg(test)]
    pub fn asked(&self, message_id: &str) -> bool {
        self.inner.borrow().asked.contains(message_id)
    }

    /// The server's answer: the attachments with fresh URLs.
    pub fn refreshed(&self, _message_id: &str, attachments: Vec<Attachment>) {
        let mut s = self.inner.borrow_mut();
        for a in attachments {
            let id = a.id.clone();
            if s.fresh.insert(id.clone(), a).is_none() {
                if let Some(old) = remember(&mut s.fresh_order, id) {
                    s.fresh.remove(&old);
                }
            }
        }
    }

    /// The refresh failed. The message stays asked: there is no second try.
    pub fn failed(&self, _message_id: &str) {}
}

impl Inlet {
    /// Asks for fresh URLs of a message's attachments when [`Refresh::want`] says so.
    fn maybe_refresh(
        &self,
        channel_id: &str,
        message_id: &str,
        a: &Attachment,
        failure: Option<&MediaError>,
    ) -> bool {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let ask = self
            .media
            .refresh
            .want(message_id, a, failure, now_ms, Instant::now());
        if ask {
            self.backend.send(Cmd::Refresh {
                channel_id: channel_id.to_string(),
                message_id: message_id.to_string(),
            });
        }
        ask
    }

    /// Images and files under a message's text.
    pub(crate) fn render_attachments(
        &self,
        channel_id: &str,
        message_id: &str,
        attachments: &[Attachment],
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if attachments.is_empty() {
            return None;
        }
        // Images and videos side by side while they fit, files below them.
        let mut media = div().flex().flex_row().flex_wrap().gap_1();
        let mut files = div().flex().flex_col().gap_1();
        let (mut any_media, mut any_file) = (false, false);
        for a in attachments {
            let a = self.media.refresh.current(a);
            let key = format!("{message_id}-{}", a.id);
            match a.kind {
                AttachmentKind::Image => {
                    any_media = true;
                    media =
                        media.child(self.image_attachment(channel_id, message_id, &key, &a, cx));
                }
                AttachmentKind::Video => {
                    any_media = true;
                    media =
                        media.child(self.video_attachment(channel_id, message_id, &key, &a, cx));
                }
                AttachmentKind::Audio => {
                    any_file = true;
                    files = files.child(self.audio_file(&key, &a, cx));
                }
                AttachmentKind::File => {
                    any_file = true;
                    files = files.child(self.file_card(&key, &a, cx));
                }
            }
        }
        Some(
            div()
                .flex()
                .flex_col()
                .items_start()
                .gap_1()
                .pt_1()
                .when(any_media, |d| d.child(media))
                .when(any_file, |d| d.child(files))
                .into_any_element(),
        )
    }

    /// The preview of an attachment that is an image or a video frame, with the
    /// refresh of expired URLs. `None` while covered.
    pub(super) fn attachment_frame(
        &self,
        channel_id: &str,
        message_id: &str,
        a: &Attachment,
        size: (f32, f32),
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(setup) = &self.media.setup else {
            return placeholder(size, 4., true);
        };
        // Expired by its time: ask for fresh URLs before loading the old one.
        if self.maybe_refresh(channel_id, message_id, a, None) {
            return placeholder(size, 4., false);
        }
        let scale = self.media.scale;
        let still = requests::attachment_preview(&setup.proxy, a, scale, false);
        let moving = requests::attachment_preview(&setup.proxy, a, scale, true);
        let shown = self.moving(moving.as_ref(), still.as_ref(), cx);
        if let Shown::Failed(err) = &shown {
            if self.maybe_refresh(channel_id, message_id, a, Some(err)) {
                return placeholder(size, 4., false);
            }
        }
        picture_of(shown, size, 4., ObjectFit::Contain)
    }

    fn image_attachment(
        &self,
        channel_id: &str,
        message_id: &str,
        key: &str,
        a: &Attachment,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let size = self.fit_column(
            requests::fit_inside(a.width, a.height, requests::PREVIEW_MAX),
            0.,
        );
        if let Some(cover) = self.cover(key, a.spoiler, a.explicit, size, cx) {
            return cover;
        }
        let frame = self.attachment_frame(channel_id, message_id, a, size, cx);
        let viewer = super::Viewer::for_attachment(self, a);
        let group = SharedString::from(format!("att-{key}"));
        let save = self.save_button(&group, key, a, cx);
        div()
            .id(SharedString::from(format!("att-img-{key}")))
            .group(group)
            .relative()
            .flex_none()
            .cursor_pointer()
            .child(frame)
            .children(save)
            .on_click(cx.listener(move |this, _, _, cx| {
                if let Some(v) = viewer.clone() {
                    this.open_viewer(v, cx);
                }
            }))
            .into_any_element()
    }

    /// The cover over a spoiler or explicit image, `None` when it shows (revealed, or
    /// neither). A click reveals it.
    // Deviation: Fluxer blurs the image under the cover; gpui has no blur filter, so the
    // cover is solid (nothing shows through, and nothing loads until revealed).
    pub(super) fn cover(
        &self,
        key: &str,
        spoiler: bool,
        explicit: bool,
        size: (f32, f32),
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let covered =
            (spoiler || (explicit && self.media.cover_explicit)) && !self.media.is_revealed(key);
        if !covered {
            return None;
        }
        let label = if spoiler {
            t!("media-spoiler")
        } else {
            t!("media-explicit")
        };
        let key = key.to_string();
        Some(
            cover_box(format!("cover-{key}"), label, size)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.media.reveal(key.clone());
                    cx.notify();
                }))
                .into_any_element(),
        )
    }

    /// The save button shown over an image or video while the pointer is on it.
    pub(super) fn save_button(
        &self,
        group: &SharedString,
        key: &str,
        a: &Attachment,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let setup = self.media.setup.as_ref()?;
        let url = requests::attachment_original(&setup.proxy, a)?;
        let (name, size) = (a.filename.clone(), a.size);
        Some(
            div()
                .absolute()
                .top(px(6.))
                .right(px(6.))
                .opacity(0.)
                .group_hover(group.clone(), |s| s.opacity(1.))
                .child(
                    icon_button(format!("save-{key}"), MediaIcon::Download).on_click(cx.listener(
                        move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.save_media(url.clone(), &name, size, cx);
                        },
                    )),
                )
                .into_any_element(),
        )
    }

    /// A file that is not an image, video or audio: icon, name, size and save.
    fn file_card(&self, key: &str, a: &Attachment, cx: &mut Context<Self>) -> AnyElement {
        if let Some(cover) = self.file_cover(key, a, cx) {
            return cover;
        }
        self.file_row(key, a, MediaIcon::File, file_size(a.size), None, cx)
    }

    /// The cover over a spoiler file or audio card, the card's size: the name, size and
    /// length stay hidden until it is clicked.
    pub(super) fn file_cover(
        &self,
        key: &str,
        a: &Attachment,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let width = self.fit_column((FILE_CARD_WIDTH, FILE_CARD_HEIGHT), 0.).0;
        self.cover(key, a.spoiler, false, (width, FILE_CARD_HEIGHT), cx)
    }

    /// The card of a file: icon, name (click saves), a second line, an optional play
    /// button and the save button.
    pub(super) fn file_row(
        &self,
        key: &str,
        a: &Attachment,
        icon: MediaIcon,
        detail: String,
        play: Option<AnyElement>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let url = self
            .media
            .setup
            .as_ref()
            .and_then(|s| requests::attachment_original(&s.proxy, a));
        let save = url.clone().map(|url| {
            let (name, size) = (a.filename.clone(), a.size);
            icon_button(format!("file-save-{key}"), MediaIcon::Download).on_click(
                cx.listener(move |this, _, _, cx| this.save_media(url.clone(), &name, size, cx)),
            )
        });
        let name = {
            let mut d = div()
                .id(SharedString::from(format!("file-name-{key}")))
                .min_w_0()
                .whitespace_nowrap()
                .overflow_hidden()
                .text_ellipsis()
                .text_size(px(16.))
                .line_height(px(20.))
                .text_color(theme::link())
                .child(a.filename.clone());
            if let Some(url) = url {
                let (name, size) = (a.filename.clone(), a.size);
                d =
                    d.cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.save_media(url.clone(), &name, size, cx)
                        }));
            }
            d
        };
        div()
            .w(px(FILE_CARD_WIDTH))
            .max_w_full()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .p_3()
            .rounded(px(8.))
            .border_1()
            .border_color(theme::accent_border())
            .bg(theme::embed_bg())
            .child(icon.el(px(32.), theme::text_muted()))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(name)
                    .child(
                        div()
                            .text_size(px(12.))
                            .line_height(px(16.))
                            .text_color(theme::text_muted())
                            .child(detail),
                    ),
            )
            .children(play)
            .children(save)
            .into_any_element()
    }

    /// Saves a file from the media endpoint where the person picks in the save dialog.
    /// The file is written there and never opened.
    pub(crate) fn save_media(&mut self, url: Url, name: &str, size: u64, cx: &mut Context<Self>) {
        let Some(setup) = &self.media.setup else {
            return;
        };
        let fetcher = setup.fetcher.clone();
        let runtime = setup.runtime.clone();
        let dir = dirs::download_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_default();
        let suggested = backend_media::suggested_file_name(name);
        let chosen = cx.prompt_for_new_path(&dir, Some(&suggested));
        let limit = backend_media::save_limit(size);
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(path))) = chosen.await else {
                return;
            };
            let shown_name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let result = runtime
                .spawn(backend_media::save_to(fetcher, url, path, limit))
                .await;
            let _ = this.update(cx, |this, cx| {
                this.chat.notice = Some(match result {
                    Ok(Ok(_)) => t!("media-saved", name = shown_name),
                    Ok(Err(err)) => t!("media-save-failed", reason = err.to_string()),
                    Err(_) => t!("media-save-failed", reason = t!("media-save-stopped")),
                });
                cx.notify();
            });
        })
        .detach();
    }
}

/// A solid box of `size` with `label` in the middle: what covers spoilers and explicit
/// media. The caller adds what a click does.
pub(crate) fn cover_box(id: String, label: String, size: (f32, f32)) -> gpui::Stateful<gpui::Div> {
    div()
        .id(SharedString::from(id))
        .w(px(size.0))
        .h(px(size.1))
        .flex_none()
        .rounded(px(4.))
        .bg(theme::media_cover())
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .child(
            div()
                .px_2()
                .py_1()
                .rounded(px(12.))
                .bg(theme::media_overlay())
                .text_size(px(13.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme::text_strong())
                .child(label),
        )
}

/// A 32 px round button with an icon, for actions over media.
pub(super) fn icon_button(id: String, icon: MediaIcon) -> gpui::Stateful<gpui::Div> {
    div()
        .id(SharedString::from(id))
        .size(px(32.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.))
        .bg(theme::media_overlay())
        .hover(|s| s.bg(theme::media_overlay_hover()))
        .cursor_pointer()
        .child(icon.el(px(20.), theme::text_strong()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn att(id: &str, expires_at_ms: Option<i64>) -> Attachment {
        Attachment {
            id: id.into(),
            url: Some(format!(
                "https://media.example.org/attachments/1/{id}/a.png?ex=1"
            )),
            kind: AttachmentKind::Image,
            expires_at_ms,
            ..Default::default()
        }
    }

    #[test]
    fn expired_attachment_url_is_refreshed_once() {
        let r = Refresh::default();
        let now = Instant::now();
        let now_ms = 1_000_000;
        let old = att("a1", Some(now_ms - 1));

        // Expired by its time: ask once for the message.
        assert!(r.want("m1", &old, None, now_ms, now));
        assert!(!r.want("m1", &old, None, now_ms, now));
        // A second attachment of the same message does not ask again.
        assert!(!r.want("m1", &att("a2", Some(now_ms - 1)), None, now_ms, now));
        assert!(r.asked("m1"));

        // The answer replaces the URL that is drawn.
        let mut fresh = att("a1", Some(now_ms + 86_400_000));
        fresh.url = Some("https://media.example.org/attachments/1/a1/a.png?ex=2".into());
        r.refreshed("m1", vec![fresh.clone()]);
        assert_eq!(r.current(&old), fresh);

        // The fresh URL failing too does not ask again, not even as a new message.
        let denied = MediaError::Fetch(FetchError::Status(403));
        assert!(!r.want("m1", &fresh, Some(&denied), now_ms, now));
        assert!(!r.want("m-other", &fresh, Some(&denied), now_ms, now));

        // A failure that is not about the URL never asks.
        let valid = att("b1", Some(now_ms + 1_000));
        let corrupt = MediaError::Corrupt("x".into());
        assert!(!r.want("m2", &valid, Some(&corrupt), now_ms, now));
        // A 404 on a URL that has not expired by its time does.
        let gone = MediaError::Fetch(FetchError::Status(404));
        assert!(r.want("m2", &valid, Some(&gone), now_ms, now));
        // A failed refresh is not retried.
        r.failed("m2");
        assert!(!r.want("m2", &valid, Some(&gone), now_ms, now));

        // A channel full of expired attachments: a few per ten seconds.
        let started = (0..20)
            .filter(|i| {
                r.want(
                    &format!("x{i}"),
                    &att(&format!("x{i}"), Some(0)),
                    None,
                    now_ms,
                    now,
                )
            })
            .count();
        assert_eq!(started, REFRESHES_PER_WINDOW - 2);
        assert!(r.want(
            "x19",
            &att("x19", Some(0)),
            None,
            now_ms,
            now + REFRESH_WINDOW
        ));
    }

    #[test]
    fn refresh_memory_is_bounded() {
        let r = Refresh::default();
        let now = Instant::now();
        for i in 0..(MAX_TRACKED + 50) {
            let t = now + REFRESH_WINDOW * i as u32;
            assert!(r.want(
                &format!("m{i}"),
                &att(&format!("a{i}"), Some(0)),
                None,
                1,
                t
            ));
            r.refreshed(&format!("m{i}"), vec![att(&format!("a{i}"), None)]);
        }
        let s = r.inner.borrow();
        assert_eq!(s.asked.len(), MAX_TRACKED);
        assert_eq!(s.fresh.len(), MAX_TRACKED);
    }
}
