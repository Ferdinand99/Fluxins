// SPDX-License-Identifier: AGPL-3.0-or-later
//! The GIF tab: the favourite GIFs, trending and the instance's featured search terms,
//! and search. Shown only when the instance has a GIF service.
//!
//! Every preview is a media proxy URL on the session's checked media endpoint
//! (`Gif::preview`, `FavoriteGifEntry::preview`, `GifCategory::preview`); a GIF with
//! none shows a placeholder. The provider's own URLs are never loaded. The search waits
//! for the typing to stop ([`SEARCH_DEBOUNCE`]) and only the newest text is asked for;
//! answers to an older search are dropped.

use std::ops::Range;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    div, prelude::*, px, uniform_list, AnyElement, ClickEvent, Context, MouseButton,
    MouseDownEvent, ObjectFit, SharedString, Task,
};
use inlet_media::{Fit, MediaKind, MediaRequest};
use inlet_protocol as p;
use inlet_protocol::api::gifs::GifCategory;
use inlet_protocol::proto::preferences::FavoriteGifEntry;
use inlet_protocol::Transform;
use url::Url;

use super::emoji::empty;
use super::{Insert, Target, PICKER_WIDTH};
use crate::app::Inlet;
use crate::backend::composer::gif::{favourite_entry, locale, Cmd, PrefEdit};
use crate::backend::composer::Cmd as ComposerCmd;
use crate::theme;
use crate::ui::media::animation_budget::Motion;
use crate::ui::media::image::picture_of;

/// How long the search waits after the last key.
pub const SEARCH_DEBOUNCE: Duration = Duration::from_millis(350);
/// Tiles per row.
const COLUMNS: usize = 2;
/// A tile, in logical pixels.
const TILE_W: f32 = (PICKER_WIDTH - 24. - 8.) / 2.;
const TILE_H: f32 = 120.;
/// A row: a tile and the gap under it.
const ROW_H: f32 = TILE_H + 8.;

/// Which GIFs the tab shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GifView {
    /// The favourites and trending tiles and the featured search terms.
    Home,
    Trending,
    Favourites,
    /// The results of the search text.
    Search,
}

/// Waits for the typing to stop before a search goes out, and never asks for the same
/// text twice in a row.
#[derive(Default)]
pub struct SearchDebounce {
    typed: Option<(String, Instant)>,
    asked: Option<String>,
}

impl SearchDebounce {
    /// The search text changed at `now`. Empty text cancels the search.
    pub fn typed(&mut self, query: &str, now: Instant) {
        let q: String = query.trim().chars().take(256).collect();
        if q.is_empty() {
            self.typed = None;
            self.asked = None;
        } else {
            self.typed = Some((q, now));
        }
    }

    /// The text to search for at `now`: once the typing stopped for
    /// [`SEARCH_DEBOUNCE`], and only if it was not the last text asked for.
    pub fn due(&mut self, now: Instant) -> Option<String> {
        let (q, at) = self.typed.as_ref()?;
        if now.saturating_duration_since(*at) < SEARCH_DEBOUNCE {
            return None;
        }
        let q = q.clone();
        self.typed = None;
        if self.asked.as_deref() == Some(q.as_str()) {
            return None;
        }
        self.asked = Some(q.clone());
        Some(q)
    }

    /// A search to send now (a featured term was clicked); `None` when it was the last
    /// text asked for.
    pub fn immediate(&mut self, query: &str) -> Option<String> {
        let q: String = query.trim().chars().take(256).collect();
        self.typed = None;
        if q.is_empty() || self.asked.as_deref() == Some(q.as_str()) {
            return None;
        }
        self.asked = Some(q.clone());
        Some(q)
    }
}

/// The GIF answers of this account.
#[derive(Default)]
pub struct GifState {
    featured: Option<Result<Vec<GifCategory>, String>>,
    /// The instance's featured GIF: the trending tile's picture until trending is open.
    featured_gif: Option<Box<p::Gif>>,
    featured_waiting: Option<u64>,
    trending: Option<Result<Vec<p::Gif>, String>>,
    trending_waiting: Option<u64>,
    /// The newest search's text and answer.
    search: Option<(String, Result<Vec<p::Gif>, String>)>,
    search_waiting: Option<(u64, String)>,
    pub debounce: SearchDebounce,
    timer: Option<Task<()>>,
}

impl GifState {
    /// A nonce no earlier request of this process had (see [`super::next_nonce`]).
    fn nonce(&mut self) -> u64 {
        super::next_nonce()
    }

    /// The search text changed at `now`. Returns whether a search may follow (then the
    /// caller waits [`SEARCH_DEBOUNCE`] and calls [`take_due`](Self::take_due)); empty
    /// text cancels the search on its way.
    pub fn query_changed(&mut self, text: &str, now: Instant) -> bool {
        self.debounce.typed(text, now);
        if text.trim().is_empty() {
            self.cancel_search();
            return false;
        }
        true
    }

    /// The search to send at `now`, if the typing has stopped: its nonce and text. The
    /// answer with that nonce is the one shown.
    pub fn take_due(&mut self, now: Instant) -> Option<(u64, String)> {
        let query = self.debounce.due(now)?;
        Some(self.start_search(query))
    }

    /// A search goes out now: its nonce, and its text kept to match the answer.
    fn start_search(&mut self, query: String) -> (u64, String) {
        let nonce = self.nonce();
        self.search_waiting = Some((nonce, query.clone()));
        (nonce, query)
    }

    /// The picker closed or the text was cleared: a search on its way is not wanted.
    pub fn cancel_search(&mut self) {
        self.timer = None;
        self.search_waiting = None;
        self.debounce = SearchDebounce::default();
    }

    pub fn answered_gifs(&mut self, nonce: u64, result: Result<Vec<p::Gif>, String>) {
        if self.trending_waiting == Some(nonce) {
            self.trending_waiting = None;
            self.trending = Some(result);
        } else if let Some((n, q)) = &self.search_waiting {
            if *n == nonce {
                self.search = Some((q.clone(), result));
                self.search_waiting = None;
            }
        }
    }

    pub fn answered_categories(
        &mut self,
        nonce: u64,
        result: Result<(Vec<GifCategory>, Option<Box<p::Gif>>), String>,
    ) {
        if self.featured_waiting == Some(nonce) {
            self.featured_waiting = None;
            self.featured = Some(result.map(|(categories, gif)| {
                self.featured_gif = gif;
                categories
            }));
        }
    }
}

/// A tile of the GIF tab.
#[derive(Clone, Debug)]
enum Tile {
    /// Opens the favourites, with the newest one as its picture.
    Favourites(Option<Box<FavoriteGifEntry>>),
    /// Opens trending, with the first trending GIF as its picture.
    Trending(Option<Box<p::Gif>>),
    /// A featured search term.
    Category(Box<GifCategory>),
    Gif(Box<p::Gif>),
    Favourite(Box<FavoriteGifEntry>),
}

/// The link to put into a message: an `http(s)` URL without spaces or control
/// characters, as the parser wrote it (`None` for anything else).
fn sendable(url: &str) -> Option<String> {
    if url.is_empty() || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    let parsed = Url::parse(url).ok()?;
    matches!(parsed.scheme(), "http" | "https").then(|| parsed.as_str().to_owned())
}

/// Whether a stored favourite is a video: Fluxer stores most favourites with a video
/// preview (`content_type` `video/*`, or a video file when the type is missing).
fn is_video(entry: &FavoriteGifEntry) -> bool {
    if entry.content_type.starts_with("video/") {
        return true;
    }
    let path = Url::parse(&entry.proxy_url)
        .map(|u| u.path().to_ascii_lowercase())
        .unwrap_or_default();
    entry.content_type.is_empty() && (path.ends_with(".mp4") || path.ends_with(".webm"))
}

/// A favourite's preview through the media proxy. Inlet plays no video, so a video
/// favourite is never asked for as an animation: the proxy gives its first frame as a
/// still image (WebP), as for video attachments.
fn favourite_preview(entry: &FavoriteGifEntry, media: &p::Media, t: Transform) -> Option<String> {
    if !is_video(entry) {
        return entry.preview(media, t);
    }
    if t.animated {
        return None;
    }
    let still = Transform {
        format: Some(inlet_protocol::ImageFormat::Webp),
        ..t
    };
    // Formats an older client stored, images first, then the stored preview.
    crate::backend::composer::gif::IMAGE_KEYS
        .iter()
        .filter_map(|k| entry.media.get(*k))
        .find_map(|f| media.external(&f.proxy_src, still))
        .or_else(|| media.external(&entry.proxy_url, still))
}

/// The requests for a GIF preview of `w` x `h` logical pixels from a preview URL
/// builder (always the media proxy): the animation at its logical size, and the still
/// at the device pixels shown.
fn requests(
    preview: impl Fn(Transform) -> Option<String>,
    scale: f32,
) -> (Option<MediaRequest>, Option<MediaRequest>) {
    let request = |animated: bool, scale: f32| {
        let w = (TILE_W * scale.max(1.)).ceil() as u32;
        let h = (TILE_H * scale.max(1.)).ceil() as u32;
        let t = Transform {
            width: Some(w),
            height: Some(h),
            format: None,
            animated,
        };
        let url = Url::parse(&preview(t)?).ok()?;
        Some(
            MediaRequest::new(
                url,
                MediaKind::Attachment,
                Fit::Contain {
                    max_width: w,
                    max_height: h,
                },
            )
            .with_proxy_size(w.max(h)),
        )
    };
    (request(true, 1.), request(false, scale))
}

impl Inlet {
    /// The GIF home's featured terms, asked for once (again after a failure).
    pub(super) fn gif_home_needed(&mut self) {
        let gifs = &mut self.composer.pickers.gifs;
        // Asked again after a failure (the backend allows one request per 10 s).
        if matches!(gifs.featured, Some(Ok(_))) || gifs.featured_waiting.is_some() {
            return;
        }
        let nonce = gifs.nonce();
        gifs.featured_waiting = Some(nonce);
        self.backend.send(ComposerCmd::Pickers(Cmd::FeaturedGifs {
            locale: locale(),
            nonce,
        }));
    }

    fn gif_trending_needed(&mut self) {
        let gifs = &mut self.composer.pickers.gifs;
        if matches!(gifs.trending, Some(Ok(_))) || gifs.trending_waiting.is_some() {
            return;
        }
        let nonce = gifs.nonce();
        gifs.trending_waiting = Some(nonce);
        self.backend.send(ComposerCmd::Pickers(Cmd::TrendingGifs {
            locale: locale(),
            nonce,
        }));
    }

    /// The search text changed: the search goes out once the typing stops.
    pub(super) fn gif_query_changed(&mut self, text: String, cx: &mut Context<Self>) {
        let empty = text.trim().is_empty();
        if let Some(open) = &mut self.composer.pickers.open {
            open.gif_view = if empty {
                GifView::Home
            } else {
                GifView::Search
            };
        }
        let gifs = &mut self.composer.pickers.gifs;
        if !gifs.query_changed(&text, Instant::now()) {
            return;
        }
        gifs.timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SEARCH_DEBOUNCE).await;
            let _ = this.update(cx, |this, cx| this.gif_search_due(cx));
        }));
    }

    fn gif_search_due(&mut self, cx: &mut Context<Self>) {
        let Some((nonce, query)) = self.composer.pickers.gifs.take_due(Instant::now()) else {
            return;
        };
        self.send_gif_search(nonce, query, cx);
    }

    fn gif_search(&mut self, query: String, cx: &mut Context<Self>) {
        let (nonce, query) = self.composer.pickers.gifs.start_search(query);
        self.send_gif_search(nonce, query, cx);
    }

    fn send_gif_search(&mut self, nonce: u64, query: String, cx: &mut Context<Self>) {
        self.backend.send(ComposerCmd::Pickers(Cmd::SearchGifs {
            query,
            locale: locale(),
            nonce,
        }));
        cx.notify();
    }

    fn gif_tiles(&self) -> Result<Vec<Tile>, String> {
        let pickers = &self.composer.pickers;
        let view = pickers.open.as_ref().map_or(GifView::Home, |o| o.gif_view);
        let favourites = pickers.prefs.favourite_gifs();
        Ok(match view {
            GifView::Home => {
                let mut tiles = Vec::new();
                if !favourites.is_empty() {
                    tiles.push(Tile::Favourites(favourites.last().cloned().map(Box::new)));
                }
                // Fluxer's trending tile shows the featured GIF.
                let first = match &pickers.gifs.trending {
                    Some(Ok(g)) => g.first().cloned().map(Box::new),
                    _ => None,
                }
                .or_else(|| pickers.gifs.featured_gif.clone());
                tiles.push(Tile::Trending(first));
                match &pickers.gifs.featured {
                    Some(Ok(cats)) => tiles.extend(
                        cats.iter()
                            .filter(|c| !c.name.trim().is_empty())
                            .cloned()
                            .map(|c| Tile::Category(Box::new(c))),
                    ),
                    Some(Err(e)) => return Err(e.clone()),
                    None => {}
                }
                tiles
            }
            GifView::Favourites => favourites
                .iter()
                .rev()
                .cloned()
                .map(|e| Tile::Favourite(Box::new(e)))
                .collect(),
            GifView::Trending => match &pickers.gifs.trending {
                Some(Ok(g)) => g.iter().cloned().map(|g| Tile::Gif(Box::new(g))).collect(),
                Some(Err(e)) => return Err(e.clone()),
                None => Vec::new(),
            },
            GifView::Search => match &pickers.gifs.search {
                Some((q, r)) if pickers.open.as_ref().is_some_and(|o| o.query.trim() == q) => {
                    match r {
                        Ok(g) => g.iter().cloned().map(|g| Tile::Gif(Box::new(g))).collect(),
                        Err(e) => return Err(e.clone()),
                    }
                }
                _ => Vec::new(),
            },
        })
    }

    pub(super) fn render_gif_tab(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(open) = self.composer.pickers.open.as_ref() else {
            return div().into_any_element();
        };
        let (view, handle) = (open.gif_view, open.list.clone());
        let tiles = match self.gif_tiles() {
            Ok(t) => t,
            Err(e) => return empty(t!("picker-gif-failed", error = e)),
        };
        let back = (view != GifView::Home).then(|| {
            div()
                .id("picker-gif-back")
                .mx(px(12.))
                .mb(px(6.))
                .text_size(px(13.))
                .text_color(theme::link())
                .cursor_pointer()
                .child(t!("picker-gif-back"))
                .on_click(cx.listener(|this, _, _, cx| {
                    if let Some(open) = &mut this.composer.pickers.open {
                        open.gif_view = GifView::Home;
                        open.search.update(cx, |s, cx| s.clear(cx));
                        open.query.clear();
                    }
                    this.composer.pickers.gifs.cancel_search();
                    cx.notify();
                }))
        });
        if tiles.is_empty() {
            let text = match view {
                GifView::Search if self.composer.pickers.gifs.search_waiting.is_none() => {
                    t!("picker-gif-none")
                }
                _ => t!("picker-loading"),
            };
            return div()
                .flex_1()
                .flex()
                .flex_col()
                .children(back)
                .child(empty(text))
                .into_any_element();
        }
        let rows: Rc<Vec<Vec<Tile>>> = Rc::new(tiles.chunks(COLUMNS).map(|c| c.to_vec()).collect());
        let count = rows.len();
        let list = uniform_list(
            "picker-gif-grid",
            count,
            cx.processor(move |this: &mut Inlet, range: Range<usize>, _, cx| {
                this.picker_images(true);
                let out = range
                    .filter_map(|ix| rows.get(ix).map(|r| (ix, r)))
                    .map(|(ix, row)| {
                        let mut el = div().h(px(ROW_H)).flex().flex_row().gap(px(8.));
                        for (col, tile) in row.iter().enumerate() {
                            el = el.child(this.render_gif_tile(ix, col, tile, cx));
                        }
                        el.into_any_element()
                    })
                    .collect::<Vec<_>>();
                this.picker_images(false);
                out
            }),
        )
        .track_scroll(&handle)
        .flex_1()
        .w_full();
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .children(back)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .px(px(12.))
                    .child(list),
            )
            .into_any_element()
    }

    /// A preview through the media proxy; it moves while the picker is the top layer.
    fn gif_picture(
        &self,
        preview: impl Fn(Transform) -> Option<String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (moving, still) = requests(preview, self.media.scale);
        // Marked as the picker's by the budget when the picker is on top and drawing.
        let shown = self.moving(moving.as_ref(), still.as_ref(), cx);
        let anim = self.media.images.animator(Motion::Gif);
        picture_of(shown, (TILE_W, TILE_H), 6., ObjectFit::Cover, &anim)
    }

    fn render_gif_tile(
        &mut self,
        row: usize,
        col: usize,
        tile: &Tile,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let proxy = self.media.setup.as_ref().map(|s| s.proxy.clone());
        let picture = match (tile, &proxy) {
            (Tile::Favourites(Some(e)) | Tile::Favourite(e), Some(m)) => {
                let e = e.clone();
                let m = m.clone();
                self.gif_picture(move |t| favourite_preview(&e, &m, t), cx)
            }
            (Tile::Trending(Some(g)) | Tile::Gif(g), Some(m)) => {
                let g = g.clone();
                let m = m.clone();
                self.gif_picture(move |t| g.preview(&m, t), cx)
            }
            (Tile::Category(c), Some(m)) => {
                let c = c.clone();
                let m = m.clone();
                self.gif_picture(move |t| c.preview(&m, t), cx)
            }
            _ => crate::ui::media::image::placeholder((TILE_W, TILE_H), 6., false),
        };
        let label = match tile {
            Tile::Favourites(_) => Some(t!("picker-gif-favourites")),
            Tile::Trending(_) => Some(t!("picker-gif-trending")),
            Tile::Category(c) => Some(crate::model::clean_name(&c.name)),
            Tile::Gif(_) | Tile::Favourite(_) => None,
        };
        let clicked = tile.clone();
        let fav = tile.clone();
        div()
            .id(SharedString::from(format!("picker-gif-{row}-{col}")))
            .w(px(TILE_W))
            .h(px(TILE_H))
            .relative()
            .rounded(px(6.))
            .overflow_hidden()
            .cursor_pointer()
            .child(picture)
            .children(label.map(|l| {
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(theme::media_overlay())
                    .hover(|s| s.bg(theme::media_overlay_hover()))
                    .text_size(px(15.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme::text_strong())
                    .child(l)
            }))
            .on_click(cx.listener(move |this, e: &ClickEvent, window, cx| {
                this.click_gif_tile(clicked.clone(), e.modifiers().shift, window, cx)
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                    let entry = match &fav {
                        Tile::Gif(g) => favourite_entry(g),
                        Tile::Favourite(e) => (**e).clone(),
                        _ => return,
                    };
                    let on = !this.composer.pickers.prefs.is_favourite_gif(&entry.url);
                    this.edit_prefs(PrefEdit::FavouriteGif {
                        entry: Box::new(entry),
                        on,
                    });
                    cx.notify();
                }),
            )
            .into_any_element()
    }

    fn click_gif_tile(
        &mut self,
        tile: Tile,
        keep_open: bool,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        let Some(open) = self.composer.pickers.open.as_mut() else {
            return;
        };
        let target = open.target;
        if matches!(target, Target::Reaction { .. }) {
            return;
        }
        match tile {
            Tile::Favourites(_) => {
                open.gif_view = GifView::Favourites;
                open.list = gpui::UniformListScrollHandle::new();
            }
            Tile::Trending(_) => {
                open.gif_view = GifView::Trending;
                open.list = gpui::UniformListScrollHandle::new();
                self.gif_trending_needed();
            }
            Tile::Category(c) => {
                // The term goes into the search field and is searched for at once.
                let name: String = c.name.trim().chars().take(100).collect();
                open.search.update(cx, |s, cx| s.set_text(name.clone(), cx));
                open.query.clone_from(&name);
                open.gif_view = GifView::Search;
                open.list = gpui::UniformListScrollHandle::new();
                if let Some(query) = self.composer.pickers.gifs.debounce.immediate(&name) {
                    self.gif_search(query, cx);
                }
            }
            Tile::Gif(g) => {
                let Some(link) = sendable(&g.url) else {
                    return;
                };
                let query = (open.gif_view == GifView::Search).then(|| open.query.clone());
                // Only a GIF that went in is shared with the instance.
                if !self.insert_into_composer(target, Insert::Text(format!("{link} ")), window, cx)
                {
                    cx.notify();
                    return;
                }
                self.backend.send(ComposerCmd::Pickers(Cmd::SharedGif {
                    gif: g,
                    query,
                    locale: locale(),
                }));
                self.picked(keep_open, cx);
            }
            Tile::Favourite(e) => {
                let Some(link) = sendable(&e.url) else {
                    return;
                };
                if self.insert_into_composer(target, Insert::Text(format!("{link} ")), window, cx) {
                    self.picked(keep_open, cx);
                }
            }
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previews_are_built_from_the_media_proxy_only() {
        let media = p::Media::new("https://media.example.org").unwrap();
        let gif: p::Gif = serde_json::from_value(serde_json::json!({
            "id": "1", "url": "https://klipy.com/gifs/cat",
            "proxy_src": "https://media.example.org/external/s/https/static.klipy.com/cat.gif",
            "media": {
                "tinygif": {"src": "https://static.klipy.com/tiny.gif", "proxy_src": "https://media.example.org/external/s/https/static.klipy.com/tiny.gif"}
            }
        }))
        .unwrap();
        let (moving, still) = requests(|t| gif.preview(&media, t), 2.);
        for req in [moving.unwrap(), still.unwrap()] {
            assert_eq!(req.url.host_str(), Some("media.example.org"));
            assert!(req.url.path().starts_with("/external/"));
        }
        // A GIF whose proxy URLs point elsewhere has no preview at all.
        let elsewhere: p::Gif = serde_json::from_value(serde_json::json!({
            "id": "2", "url": "https://klipy.com/gifs/dog",
            "proxy_src": "https://static.klipy.com/dog.gif",
            "media": {"tinygif": {"proxy_src": "https://evil.example/tiny.gif"}}
        }))
        .unwrap();
        let (moving, still) = requests(|t| elsewhere.preview(&media, t), 2.);
        assert!(moving.is_none() && still.is_none());
        // Only web links are ever put into a message, as the parser wrote them, and none
        // that would bring more lines or spaces with it.
        assert_eq!(
            sendable("https://klipy.com/gifs/cat").as_deref(),
            Some("https://klipy.com/gifs/cat")
        );
        assert_eq!(
            sendable("HTTPS://Klipy.com/gifs/a%20b").as_deref(),
            Some("https://klipy.com/gifs/a%20b")
        );
        for bad in [
            "javascript:alert(1)",
            "file:///etc/passwd",
            "https://klipy.com/gifs/cat
@everyone look",
            "https://klipy.com/gifs/ca	t",
            "https://klipy.com/gifs/cat <@1>",
            "",
        ] {
            assert_eq!(sendable(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_favourite_stored_by_fluxer_shows_a_still_of_its_video() {
        let media = p::Media::new("https://media.example.org").unwrap();
        // What Fluxer stores (`slimFavoriteGifEntry`): the video preview, no formats.
        let fluxer = FavoriteGifEntry {
            url: "https://klipy.com/gifs/cat".into(),
            proxy_url: "https://media.example.org/external/s/https/static.klipy.com/tiny.mp4"
                .into(),
            width: 220,
            height: 124,
            content_type: "video/mp4".into(),
            ..Default::default()
        };
        assert!(is_video(&fluxer));
        let (moving, still) = requests(|t| favourite_preview(&fluxer, &media, t), 2.);
        assert!(
            moving.is_none(),
            "a video is never asked for as an animation"
        );
        let still = still.unwrap();
        assert!(still.url.path().starts_with("/external/"));
        assert!(still.url.query().unwrap().contains("format=webp"));
        // Without a content type, a video file is a video too.
        let untyped = FavoriteGifEntry {
            content_type: String::new(),
            ..fluxer.clone()
        };
        assert!(is_video(&untyped));
        // What Inlet stores (an image format) moves.
        let gif: p::Gif = serde_json::from_value(serde_json::json!({
            "id": "1", "url": "https://klipy.com/gifs/cat",
            "media": {
                "tinygif": {"proxy_src": "https://media.example.org/external/s/https/static.klipy.com/tiny.gif", "width": 220, "height": 124},
                "tinymp4": {"proxy_src": "https://media.example.org/external/s/https/static.klipy.com/tiny.mp4", "width": 220, "height": 124}
            }
        }))
        .unwrap();
        let ours = favourite_entry(&gif);
        assert!(!is_video(&ours));
        let (moving, _) = requests(|t| favourite_preview(&ours, &media, t), 2.);
        assert!(moving.is_some());
        // A provider URL in `proxy_url` gives nothing.
        let elsewhere = FavoriteGifEntry {
            proxy_url: "https://static.klipy.com/tiny.mp4".into(),
            ..fluxer
        };
        let (moving, still) = requests(|t| favourite_preview(&elsewhere, &media, t), 2.);
        assert!(moving.is_none() && still.is_none());
    }

    #[test]
    fn typing_in_the_gif_search_sends_one_search_for_the_newest_text() {
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let mut gifs = GifState::default();
        // What `gif_query_changed` and its timer do: every key may start a timer; only
        // the timer after the last key finds a search due.
        assert!(gifs.query_changed("c", ms(0)));
        assert!(gifs.query_changed("ca", ms(100)));
        assert!(gifs.query_changed("cat", ms(200)));
        assert_eq!(gifs.take_due(ms(350)), None, "the first key's timer");
        assert_eq!(gifs.take_due(ms(450)), None, "the second key's timer");
        let (nonce, query) = gifs.take_due(ms(550)).unwrap();
        assert_eq!(query, "cat");
        assert_eq!(gifs.take_due(ms(900)), None, "asked once");
        // The answer with that nonce is shown; an older one is not.
        gifs.answered_gifs(nonce.wrapping_sub(1), Ok(Vec::new()));
        assert!(gifs.search.is_none());
        gifs.answered_gifs(nonce, Ok(Vec::new()));
        assert_eq!(gifs.search, Some(("cat".into(), Ok(Vec::new()))));
        // Clearing the field cancels what was on its way.
        assert!(gifs.query_changed("dog", ms(1_000)));
        assert!(!gifs.query_changed("", ms(1_100)));
        assert_eq!(gifs.take_due(ms(2_000)), None);
        // Nonces are never reused, not even by a state made after a sign-out.
        let mut again = GifState::default();
        assert!(again.query_changed("cat", ms(0)));
        assert!(again.take_due(ms(400)).unwrap().0 > nonce);
    }

    #[test]
    fn the_trending_tile_shows_the_featured_gif() {
        let mut gifs = GifState {
            featured_waiting: Some(7),
            ..GifState::default()
        };
        let gif: p::Gif = serde_json::from_value(
            serde_json::json!({"id": "9", "url": "https://klipy.com/gifs/x"}),
        )
        .unwrap();
        gifs.answered_categories(7, Ok((Vec::new(), Some(Box::new(gif)))));
        assert_eq!(gifs.featured_gif.as_ref().map(|g| g.id.as_str()), Some("9"));
        assert_eq!(gifs.featured, Some(Ok(Vec::new())));
    }

    #[test]
    fn only_the_newest_search_is_shown() {
        let mut gifs = GifState {
            search_waiting: Some((2, "cats".into())),
            ..GifState::default()
        };
        gifs.answered_gifs(1, Ok(Vec::new()));
        assert!(gifs.search.is_none(), "an older answer is dropped");
        gifs.answered_gifs(2, Err("offline".into()));
        assert_eq!(gifs.search, Some(("cats".into(), Err("offline".into()))));
    }
}
