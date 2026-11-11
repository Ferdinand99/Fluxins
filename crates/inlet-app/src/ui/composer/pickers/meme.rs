// SPDX-License-Identifier: AGPL-3.0-or-later
//! The memes tab: the account's saved media (Fluxer's "memes"), with search by name and
//! tags. A click sends the meme at once as a message of its own (`favorite_meme_id`), so
//! the instance attaches its stored copy. Previews are stills through the media proxy.

use std::ops::Range;
use std::rc::Rc;

use gpui::{div, prelude::*, px, uniform_list, AnyElement, Context, ObjectFit, SharedString};
use inlet_media::{Fit, MediaKind, MediaRequest};
use inlet_protocol as p;
use inlet_protocol::{ImageFormat, Transform};
use url::Url;

use super::emoji::empty;
use super::PICKER_WIDTH;
use crate::app::Inlet;
use crate::backend::composer::gif::Cmd;
use crate::theme;
use crate::ui::media::animation_budget::Motion;
use crate::ui::media::image::picture_of;

/// Memes per row.
const COLUMNS: usize = 3;
const TILE_W: f32 = (PICKER_WIDTH - 24. - 16.) / COLUMNS as f32;
const TILE_H: f32 = 100.;
const ROW_H: f32 = TILE_H + 8.;

/// The memes matching `query` (name or tag), newest first.
pub fn matching<'a>(memes: &'a [p::Meme], query: &str) -> Vec<&'a p::Meme> {
    let q = query.trim().to_lowercase();
    memes
        .iter()
        .rev()
        .filter(|m| {
            q.is_empty()
                || m.name.to_lowercase().contains(&q)
                || m.tags.iter().any(|t| t.to_lowercase().contains(&q))
        })
        .collect()
}

/// A still preview of a meme through the media proxy, or `None`.
fn preview(proxy: &p::Media, meme: &p::Meme, scale: f32) -> Option<MediaRequest> {
    let w = (TILE_W * scale.max(1.)).ceil() as u32;
    let h = (TILE_H * scale.max(1.)).ceil() as u32;
    let t = Transform {
        width: Some(w),
        height: Some(h),
        format: Some(ImageFormat::Webp),
        animated: false,
    };
    let url = Url::parse(&meme.proxied(proxy, t)?).ok()?;
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
}

impl Inlet {
    pub(super) fn render_meme_tab(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(open) = self.composer.pickers.open.as_ref() else {
            return div().into_any_element();
        };
        let (query, handle) = (open.query.clone(), open.list.clone());
        let Some(memes) = self.composer.pickers.memes.as_ref() else {
            return empty(t!("picker-loading"));
        };
        let found: Vec<p::Meme> = matching(memes, &query).into_iter().cloned().collect();
        if found.is_empty() {
            return empty(if memes.is_empty() {
                t!("picker-memes-empty")
            } else {
                t!("picker-memes-none")
            });
        }
        let rows: Rc<Vec<Vec<p::Meme>>> =
            Rc::new(found.chunks(COLUMNS).map(|c| c.to_vec()).collect());
        let count = rows.len();
        let list = uniform_list(
            "picker-meme-grid",
            count,
            cx.processor(move |this: &mut Inlet, range: Range<usize>, _, cx| {
                range
                    .filter_map(|ix| rows.get(ix).map(|r| (ix, r)))
                    .map(|(ix, row)| {
                        let mut el = div().h(px(ROW_H)).flex().flex_row().gap(px(8.));
                        for (col, meme) in row.iter().enumerate() {
                            el = el.child(this.render_meme_tile(ix, col, meme, cx));
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
            .px(px(12.))
            .child(list)
            .into_any_element()
    }

    fn render_meme_tile(
        &mut self,
        row: usize,
        col: usize,
        meme: &p::Meme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let req = self
            .media
            .setup
            .as_ref()
            .and_then(|s| preview(&s.proxy, meme, self.media.scale));
        let shown = self.moving(None, req.as_ref(), cx);
        let anim = self.media.images.animator(Motion::Gif);
        let picture = picture_of(shown, (TILE_W, TILE_H), 6., ObjectFit::Cover, &anim);
        let id = meme.id.clone();
        div()
            .id(SharedString::from(format!("picker-meme-{row}-{col}")))
            .w(px(TILE_W))
            .h(px(TILE_H))
            .rounded(px(6.))
            .overflow_hidden()
            .cursor_pointer()
            .bg(theme::media_placeholder())
            .child(picture)
            .on_click(cx.listener(move |this, _, _, cx| {
                let meme_id = id.clone();
                this.send_to_channel(|channel_id, nonce| Cmd::SendMeme {
                    channel_id,
                    meme_id,
                    nonce,
                });
                this.picked(false, cx);
            }))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meme(id: &str, name: &str, tags: &[&str]) -> p::Meme {
        serde_json::from_value(serde_json::json!({
            "id": id, "name": name, "tags": tags,
            "url": format!("https://media.example.org/attachments/1/{id}/m.png")
        }))
        .unwrap()
    }

    #[test]
    fn memes_are_found_by_name_or_tag_newest_first() {
        let memes = [meme("1", "Cat", &["funny"]), meme("2", "Dog", &["cute"])];
        let ids = |q: &str| -> Vec<String> {
            matching(&memes, q)
                .into_iter()
                .map(|m| m.id.clone())
                .collect()
        };
        assert_eq!(ids(""), ["2", "1"]);
        assert_eq!(ids("cat"), ["1"]);
        assert_eq!(ids("CUTE"), ["2"]);
        assert!(ids("bird").is_empty());
        // The preview is the proxy's still.
        let proxy = p::Media::new("https://media.example.org").unwrap();
        let req = preview(&proxy, &memes[0], 1.).unwrap();
        assert_eq!(req.url.host_str(), Some("media.example.org"));
        let elsewhere = meme("3", "x", &[]);
        let mut elsewhere = elsewhere;
        elsewhere.url = "https://evil.example/m.png".into();
        assert!(preview(&proxy, &elsewhere, 1.).is_none());
    }
}
