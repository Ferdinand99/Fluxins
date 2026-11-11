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

use super::failed::{Origin, Trouble};
use super::image::{placeholder, Shown};
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
/// Longest time a preview keeps its placeholder while the refresh of its message is on
/// its way. The answer (fresh URLs, or a failure) ends the wait sooner; past this the
/// refresh counts as failed and the old URL decides what shows.
const REFRESH_WAIT: Duration = Duration::from_secs(15);
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
    /// Messages whose refresh is on its way, since when.
    pending: HashMap<String, Instant>,
    fresh: HashMap<String, Attachment>,
    fresh_order: VecDeque<String>,
    recent: VecDeque<Instant>,
    /// When the one redraw for refreshes held back by the rate limit is due.
    wake: Option<Instant>,
}

/// What a preview does about its URLs this frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshState {
    /// Nothing to refresh (or no second try): load what the attachment has.
    NotNeeded,
    /// Ask for fresh URLs now, and wait.
    Ask,
    /// The refresh of the message is on its way: keep the placeholder.
    Waiting,
    /// The URL has expired, but the rate limit holds the refresh back until then: keep
    /// the placeholder and draw again at that time.
    Throttled(Instant),
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

    /// What to do about the URLs of `a` (as drawn) of message `message_id`: ask for
    /// fresh ones when it has expired by its time, or failed with `failure` the way an
    /// expired URL does. Only once per message, never for an attachment that was already
    /// refreshed, and at most a few per ten seconds. While the answer is on its way (up
    /// to [`REFRESH_WAIT`]) or the rate limit holds the refresh back, the preview waits.
    pub fn check(
        &self,
        message_id: &str,
        a: &Attachment,
        failure: Option<&MediaError>,
        now_ms: i64,
        now: Instant,
    ) -> RefreshState {
        let mut s = self.inner.borrow_mut();
        if s.fresh.contains_key(&a.id) {
            return RefreshState::NotNeeded;
        }
        if let Some(since) = s.pending.get(message_id) {
            if now.saturating_duration_since(*since) < REFRESH_WAIT {
                return RefreshState::Waiting;
            }
            // No answer in time: as good as failed.
            s.pending.remove(message_id);
        }
        if s.asked.contains(message_id) {
            return RefreshState::NotNeeded;
        }
        let expired = a.expired
            || a.expires_at_ms.is_some_and(|t| t <= now_ms)
            || failure.is_some_and(looks_expired);
        if !expired {
            return RefreshState::NotNeeded;
        }
        while s
            .recent
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) >= REFRESH_WINDOW)
        {
            s.recent.pop_front();
        }
        if s.recent.len() >= REFRESHES_PER_WINDOW {
            let free = s.recent.front().map_or(now, |t| *t + REFRESH_WINDOW);
            return RefreshState::Throttled(free);
        }
        s.recent.push_back(now);
        s.asked.insert(message_id.to_string());
        s.pending.insert(message_id.to_string(), now);
        if let Some(old) = remember(&mut s.asked_order, message_id.to_string()) {
            s.asked.remove(&old);
            s.pending.remove(&old);
        }
        RefreshState::Ask
    }

    /// Whether to ask for fresh URLs now ([`Refresh::check`] says [`RefreshState::Ask`]).
    #[cfg(test)]
    pub fn want(
        &self,
        message_id: &str,
        a: &Attachment,
        failure: Option<&MediaError>,
        now_ms: i64,
        now: Instant,
    ) -> bool {
        self.check(message_id, a, failure, now_ms, now) == RefreshState::Ask
    }

    /// Takes the one redraw for refreshes the rate limit holds back: `true` when none
    /// is due by `at` yet (the caller then schedules it), `false` when one already is.
    pub fn take_wake(&self, at: Instant, now: Instant) -> bool {
        let mut s = self.inner.borrow_mut();
        match s.wake {
            Some(due) if due > now && due <= at => false,
            _ => {
                s.wake = Some(at);
                true
            }
        }
    }

    /// Whether the message was asked about.
    #[cfg(test)]
    pub fn asked(&self, message_id: &str) -> bool {
        self.inner.borrow().asked.contains(message_id)
    }

    /// The server's answer: the attachments with fresh URLs.
    pub fn refreshed(&self, message_id: &str, attachments: Vec<Attachment>) {
        let mut s = self.inner.borrow_mut();
        s.pending.remove(message_id);
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
    pub fn failed(&self, message_id: &str) {
        self.inner.borrow_mut().pending.remove(message_id);
    }
}

impl Inlet {
    /// Asks for fresh URLs of a message's attachments when [`Refresh::check`] says so,
    /// and makes sure a waiting preview is drawn again: once when the answer can be
    /// given up on, and once (shared by all rows) when the rate limit lets go. Returns
    /// whether the preview waits.
    fn maybe_refresh(
        &self,
        channel_id: &str,
        message_id: &str,
        a: &Attachment,
        failure: Option<&MediaError>,
        cx: &mut Context<Self>,
    ) -> bool {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let now = Instant::now();
        let refresh = &self.media.refresh;
        match refresh.check(message_id, a, failure, now_ms, now) {
            RefreshState::NotNeeded => false,
            RefreshState::Waiting => true,
            RefreshState::Ask => {
                self.backend.send(Cmd::Refresh {
                    channel_id: channel_id.to_string(),
                    message_id: message_id.to_string(),
                });
                self.redraw_after(REFRESH_WAIT, cx);
                true
            }
            RefreshState::Throttled(at) => {
                if refresh.take_wake(at, now) {
                    self.redraw_after(at.saturating_duration_since(now), cx);
                }
                true
            }
        }
    }

    /// Draws again after `wait`, so a placeholder that waits for an answer never waits
    /// longer.
    fn redraw_after(&self, wait: Duration, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(wait).await;
            let _ = this.update(cx, |_, cx| cx.notify());
        })
        .detach();
    }

    /// The name and link of an attachment's tile: the file, opened from the media proxy
    /// (or, when only a link elsewhere is known, through the link check).
    fn attachment_origin(&self, a: &Attachment) -> Origin {
        let proxied = self
            .media
            .setup
            .as_ref()
            .and_then(|s| requests::attachment_original(&s.proxy, a))
            .map(|u| (String::from(u), false));
        let elsewhere = || {
            a.url
                .clone()
                .filter(|u| crate::ui::markdown::openable(u).is_some())
                .map(|u| (u, true))
        };
        Origin {
            name: a.filename.clone(),
            link: proxied.or_else(elsewhere),
        }
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
    /// refresh of expired URLs. It ends as the picture or as a tile that says why not
    /// (with retry and open in browser), never as an empty box. The flag is set for
    /// the tile, which takes clicks of its own.
    pub(super) fn attachment_frame(
        &self,
        channel_id: &str,
        message_id: &str,
        key: &str,
        a: &Attachment,
        size: (f32, f32),
        cx: &mut Context<Self>,
    ) -> (AnyElement, bool) {
        let origin = self.attachment_origin(a);
        let tile = |trouble, cx: &mut Context<Self>| {
            (
                self.failed_tile(key, size, 4., trouble, &origin, Vec::new(), cx),
                true,
            )
        };
        let Some(setup) = &self.media.setup else {
            return tile(Trouble::Unavailable, cx);
        };
        // Expired by its time, or its refresh is on its way: wait for fresh URLs
        // before loading the old one.
        if self.maybe_refresh(channel_id, message_id, a, None, cx) {
            return (placeholder(size, 4., false), false);
        }
        let scale = self.media.scale;
        let still = requests::attachment_preview(&setup.proxy, a, scale, false);
        let moving = requests::attachment_preview(&setup.proxy, a, scale, true);
        let shown = self.moving_preview(moving.as_ref(), still.as_ref(), cx);
        if let Shown::Failed(err) = &shown {
            if self.maybe_refresh(channel_id, message_id, a, Some(err), cx) {
                return (placeholder(size, 4., false), false);
            }
        }
        // The server sends no link for a file whose time is up.
        let no_link = a.url.is_none() && a.proxy_url.is_none();
        if matches!(shown, Shown::Unavailable) && (a.expired || no_link) {
            return tile(Trouble::Expired, cx);
        }
        let failed = matches!(shown, Shown::Failed(_) | Shown::Unavailable);
        let retry = still.into_iter().chain(moving).collect();
        let el = self.media_or_tile(key, shown, size, 4., ObjectFit::Contain, &origin, retry, cx);
        (el, failed)
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
        let (frame, failed) = self.attachment_frame(channel_id, message_id, key, a, size, cx);
        let viewer = super::Viewer::for_attachment(self, a);
        let group = SharedString::from(format!("att-{key}"));
        // No save button over a tile: there is nothing to save, and it would cover the
        // tile's own buttons.
        let save = if failed {
            None
        } else {
            self.save_button(&group, key, a, cx)
        };
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
    fn an_expired_link_waits_for_its_refresh_then_shows_the_image() {
        let r = Refresh::default();
        let now = Instant::now();
        let now_ms = 1_000_000;
        let old = att("a1", Some(now_ms - 1));
        let gone = MediaError::Fetch(FetchError::Status(404));

        // Expired by its time: ask, then wait in every frame until the answer, even
        // when the old URL has failed meanwhile (no "no longer available" tile).
        assert_eq!(r.check("m1", &old, None, now_ms, now), RefreshState::Ask);
        for later in [Duration::ZERO, Duration::from_secs(1), REFRESH_WAIT / 2] {
            assert_eq!(
                r.check("m1", &old, Some(&gone), now_ms, now + later),
                RefreshState::Waiting
            );
        }
        // The answer: the fresh attachment loads, nothing waits any more.
        let mut fresh = att("a1", Some(now_ms + 86_400_000));
        fresh.url = Some("https://media.example.org/attachments/1/a1/a.png?ex=2".into());
        r.refreshed("m1", vec![fresh.clone()]);
        let shown = r.current(&old);
        assert_eq!(shown, fresh);
        assert_eq!(
            r.check("m1", &shown, None, now_ms, now),
            RefreshState::NotNeeded
        );

        // A failed refresh ends the wait: then the old URL decides (the tile).
        let other = att("b1", Some(now_ms - 1));
        assert_eq!(r.check("m2", &other, None, now_ms, now), RefreshState::Ask);
        r.failed("m2");
        assert_eq!(
            r.check("m2", &other, Some(&gone), now_ms, now),
            RefreshState::NotNeeded
        );
        // No answer at all: the wait ends after REFRESH_WAIT.
        let third = att("c1", Some(now_ms - 1));
        assert_eq!(r.check("m3", &third, None, now_ms, now), RefreshState::Ask);
        assert_eq!(
            r.check("m3", &third, Some(&gone), now_ms, now + REFRESH_WAIT),
            RefreshState::NotNeeded
        );

        // Rate limited: the fifth message waits too, and all rows share one redraw.
        assert_eq!(
            r.check("m4", &att("d1", Some(0)), None, now_ms, now),
            RefreshState::Ask
        );
        let held = r.check("m5", &att("e1", Some(0)), Some(&gone), now_ms, now);
        let RefreshState::Throttled(at) = held else {
            panic!("{held:?}");
        };
        assert_eq!(at, now + REFRESH_WINDOW);
        assert!(r.take_wake(at, now));
        assert!(!r.take_wake(at, now));
        assert_eq!(
            r.check("m6", &att("f1", Some(0)), None, now_ms, now),
            RefreshState::Throttled(at)
        );
        // When the window has passed, it asks.
        assert_eq!(
            r.check("m5", &att("e1", Some(0)), Some(&gone), now_ms, at),
            RefreshState::Ask
        );
        assert!(r.take_wake(at + REFRESH_WINDOW, at));
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
