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
use crate::ui::media::image::{picture_of, Shown};

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
    next_nonce: u64,
    featured: Option<Result<Vec<GifCategory>, String>>,
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
    fn nonce(&mut self) -> u64 {
        self.next_nonce += 1;
        self.next_nonce
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

    pub fn answered_categories(&mut self, nonce: u64, result: Result<Vec<GifCategory>, String>) {
        if self.featured_waiting == Some(nonce) {
            self.featured_waiting = None;
            self.featured = Some(result);
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

/// Only `http(s)` links are put into a message.
fn sendable(url: &str) -> bool {
    Url::parse(url).is_ok_and(|u| matches!(u.scheme(), "http" | "https"))
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
        gifs.debounce.typed(&text, Instant::now());
        if empty {
            gifs.cancel_search();
            return;
        }
        gifs.timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SEARCH_DEBOUNCE).await;
            let _ = this.update(cx, |this, cx| this.gif_search_due(cx));
        }));
    }

    fn gif_search_due(&mut self, cx: &mut Context<Self>) {
        let Some(query) = self.composer.pickers.gifs.debounce.due(Instant::now()) else {
            return;
        };
        self.gif_search(query, cx);
    }

    fn gif_search(&mut self, query: String, cx: &mut Context<Self>) {
        let gifs = &mut self.composer.pickers.gifs;
        let nonce = gifs.nonce();
        gifs.search_waiting = Some((nonce, query.clone()));
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
                let first = match &pickers.gifs.trending {
                    Some(Ok(g)) => g.first().cloned().map(Box::new),
                    _ => None,
                };
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
                range
                    .filter_map(|ix| rows.get(ix).map(|r| (ix, r)))
                    .map(|(ix, row)| {
                        let mut el = div().h(px(ROW_H)).flex().flex_row().gap(px(8.));
                        for (col, tile) in row.iter().enumerate() {
                            el = el.child(this.render_gif_tile(ix, col, tile, cx));
                        }
                        el.into_any_element()
                    })
                    .collect::<Vec<_>>()
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
        let shown = self.moving(moving.as_ref(), still.as_ref(), cx);
        if let Shown::Ready(image) = &shown {
            self.media
                .images
                .animations()
                .borrow_mut()
                .in_top_layer(image.id);
        }
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
                self.gif_picture(move |t| e.preview(&m, t), cx)
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
        if open.target != Target::Composer {
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
                if !sendable(&g.url) {
                    return;
                }
                let query = (open.gif_view == GifView::Search).then(|| open.query.clone());
                self.insert_into_composer(Insert::Text(format!("{} ", g.url)), window, cx);
                self.backend.send(ComposerCmd::Pickers(Cmd::SharedGif {
                    gif: g,
                    query,
                    locale: locale(),
                }));
                self.picked(keep_open, cx);
            }
            Tile::Favourite(e) => {
                if !sendable(&e.url) {
                    return;
                }
                self.insert_into_composer(Insert::Text(format!("{} ", e.url)), window, cx);
                self.picked(keep_open, cx);
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
        // Only web links are ever put into a message.
        assert!(sendable("https://klipy.com/gifs/cat"));
        assert!(!sendable("javascript:alert(1)"));
        assert!(!sendable("file:///etc/passwd"));
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
