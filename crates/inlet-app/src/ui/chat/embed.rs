// SPDX-License-Identifier: AGPL-3.0-or-later
//! Draws an embed. Rich embeds (bots post these), link and article previews, video
//! pages and Bluesky posts are cards: a 4 px colour bar, provider, author, title,
//! description, fields in a 12-column grid, a thumbnail on the right, a large image or
//! video, and the footer with its time. Image and GIF links show as the media alone.
//!
//! Sizes from Fluxer: 516 px wide without media, 432 px with media, media at most
//! 400 x 300 (400 x 450 when tall), thumbnail at most 80 x 80 with 16 px to its left.
//!
//! An embed of a link the text hides in a spoiler is covered as a whole (image, title
//! and description) until the message's spoilers are shown; nothing of it loads.

use gpui::{div, prelude::*, px, rgb, AnyElement, Context, FontWeight, Hsla, SharedString};

use crate::app::Inlet;
use crate::model::{self, EmbedField, EmbedKind};
use crate::theme;
use crate::ui::markdown::{self, RenderCtx};
use crate::ui::media::embed::embed_origin;
use crate::ui::media::requests;

/// Card width without media (`EMBED_TEXT_OUTER_WIDTH`).
pub const EMBED_MAX_WIDTH: f32 = 516.;
/// Card width with an image or video (`EMBED_MEDIA_MAX_WIDTH`).
pub const EMBED_MEDIA_WIDTH: f32 = 432.;

/// The colour of an embed's left bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmbedBar {
    /// The embed's own colour, `0xRRGGBB`.
    Colour(u32),
    /// No colour: `--brand-primary-foreground`, as Fluxer draws it.
    BrandForeground,
}

impl EmbedBar {
    pub fn of(color: Option<u32>) -> EmbedBar {
        match color {
            Some(c) => EmbedBar::Colour(c & 0xFF_FF_FF),
            None => EmbedBar::BrandForeground,
        }
    }

    pub fn hsla(self) -> Hsla {
        match self {
            EmbedBar::Colour(c) => rgb(c).into(),
            EmbedBar::BrandForeground => theme::brand_foreground(),
        }
    }
}

/// Fields side by side: up to three inline fields share a row (two next to a
/// thumbnail); a field that is not inline takes the whole row.
fn field_rows(fields: &[EmbedField], with_thumbnail: bool) -> Vec<Vec<&EmbedField>> {
    let per_row = if with_thumbnail { 2 } else { 3 };
    let mut rows: Vec<Vec<&EmbedField>> = Vec::new();
    for f in fields {
        match rows.last_mut() {
            Some(row) if f.inline && row.len() < per_row && row.iter().all(|r| r.inline) => {
                row.push(f)
            }
            _ => rows.push(vec![f]),
        }
    }
    rows
}

/// The "GIF" mark in the corner of a GIF page shown as a still.
fn gif_badge() -> AnyElement {
    div()
        .absolute()
        .bottom(px(6.))
        .left(px(6.))
        .px(px(6.))
        .rounded(px(4.))
        .bg(theme::media_overlay())
        .text_size(px(12.))
        .font_weight(FontWeight::BOLD)
        .text_color(theme::text_strong())
        .child(t!("media-gif-badge"))
        .into_any_element()
}

/// Whether an embed belongs to a link hidden in a spoiler: its page or one of its media
/// is one of `hidden`.
pub(super) fn embed_spoilered(e: &model::Embed, hidden: &[String]) -> bool {
    if hidden.is_empty() {
        return false;
    }
    let same = |u: &str| {
        let u = u.trim_end_matches('/');
        !u.is_empty() && hidden.iter().any(|h| h.trim_end_matches('/') == u)
    };
    e.url.as_deref().is_some_and(same)
        || [&e.image, &e.thumbnail, &e.video]
            .into_iter()
            .flatten()
            .any(|m| same(&m.url))
}

/// The cover over a whole embed of a spoilered link; a click shows the message's
/// spoilers (text and embeds).
fn spoiler_card(this: &Inlet, id: &str, e: &model::Embed, ctx: &RenderCtx) -> AnyElement {
    // About the card's size: its large media, if any (a thumbnail sits beside text).
    let media = e.image.as_ref().or(e.video.as_ref());
    let width = if media.is_some() {
        EMBED_MEDIA_WIDTH
    } else {
        EMBED_MAX_WIDTH
    };
    let height = media.map_or(80., |m| requests::embed_media_box(m).1 + 24.);
    let size = this.fit_column((width, height), 0.);
    let reveal = ctx.on_reveal.clone();
    div()
        .mt_1()
        .child(
            crate::ui::media::cover_box(format!("{id}-spoiler"), t!("media-spoiler"), size)
                .on_click(move |_, window, cx| reveal(window, cx)),
        )
        .into_any_element()
}

/// Draws an embed. `hidden` are the URLs the text hides in spoilers (empty once the
/// message's spoilers are shown).
pub(super) fn render_embed(
    this: &Inlet,
    id: &str,
    e: &model::Embed,
    hidden: &[String],
    ctx: &RenderCtx,
    cx: &mut Context<Inlet>,
) -> AnyElement {
    if embed_spoilered(e, hidden) {
        return spoiler_card(this, id, e, ctx);
    }
    let origin = embed_origin(e);
    let has_text = e.title.is_some()
        || e.author.is_some()
        || !e.description.is_empty()
        || !e.fields.is_empty();
    // An image or GIF link is the media alone, like an attachment.
    if matches!(e.kind, EmbedKind::Image | EmbedKind::Gifv) && !has_text {
        if let Some(m) = e
            .image
            .as_ref()
            .or(e.thumbnail.as_ref())
            .or(e.video.as_ref())
        {
            let picture = this.embed_image(
                id,
                m,
                requests::PREVIEW_MAX,
                0.,
                e.nsfw,
                t!("embed-image"),
                &origin,
                cx,
            );
            // A GIF page that only has its video shows the first frame: the badge says
            // it is a GIF, which plays on the site (no video player here).
            let badge = (e.kind == EmbedKind::Gifv && m.video).then(gif_badge);
            return div()
                .pt_1()
                .child(div().relative().child(picture).children(badge))
                .into_any_element();
        }
    }

    // Articles show their thumbnail as the large image; video pages show it as the
    // still of the video.
    let video = (e.kind == EmbedKind::Video)
        .then_some(())
        .and_then(|_| e.video.as_ref().or(e.thumbnail.as_ref()));
    let (image, thumbnail) = match (e.kind, &e.image, &e.thumbnail) {
        (EmbedKind::Article, None, Some(t)) => (Some(t), None),
        (EmbedKind::Video, i, _) => (i.as_ref().filter(|_| video.is_none()), None),
        (_, i, t) => (i.as_ref(), t.as_ref()),
    };
    let has_media = image.is_some() || video.is_some();

    let mut text = div().flex().flex_col().gap_2().flex_1().min_w_0();
    if let Some(provider) = &e.provider {
        text = text.child(
            div()
                .text_size(px(12.))
                .font_weight(FontWeight::MEDIUM)
                .line_height(px(16.))
                .text_color(theme::text_tertiary())
                .child(provider.clone()),
        );
    }
    if let Some(author) = &e.author {
        let icon = this.embed_icon(e.author_icon.as_deref(), 24., cx);
        let mut name = div()
            .id(SharedString::from(format!("{id}-author")))
            .text_size(px(14.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(theme::text_strong())
            .child(author.clone());
        if let Some(url) = e
            .author_url
            .clone()
            .filter(|u| markdown::openable(u).is_some())
        {
            name = name
                .cursor_pointer()
                .hover(|s| s.underline())
                .on_click(cx.listener(move |this, _, _, cx| this.open_link(&url, true, cx)));
        }
        text = text.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .children(icon)
                .child(name),
        );
    }
    if let Some(title) = &e.title {
        let link = e.url.clone().filter(|u| markdown::openable(u).is_some());
        let mut t = div()
            .id(SharedString::from(format!("{id}-title")))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(if link.is_some() {
                theme::link()
            } else {
                theme::text_strong()
            })
            .child(title.clone());
        if let Some(url) = link {
            t = t
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| this.open_link(&url, true, cx)));
        }
        text = text.child(t);
    }
    if !e.description.is_empty() {
        text = text.child(
            div()
                .text_size(px(14.))
                .line_height(px(18.))
                .child(markdown::render(&format!("{id}-d"), &e.description, ctx)),
        );
    }
    let rows = field_rows(&e.fields, thumbnail.is_some());
    if !rows.is_empty() {
        let mut grid = div().flex().flex_col().gap_2();
        let mut n = 0;
        for row in rows {
            let mut line = div().flex().flex_row().gap_2();
            for f in row {
                line = line.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .text_size(px(14.))
                        .line_height(px(18.))
                        .child(
                            div()
                                .mb(px(2.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme::text_strong())
                                .child(f.name.clone()),
                        )
                        .child(markdown::render(&format!("{id}-f{n}"), &f.value, ctx)),
                );
                n += 1;
            }
            grid = grid.child(line);
        }
        text = text.child(grid);
    }

    // Text on the left, the thumbnail on the right.
    let mut top = div().flex().flex_row().items_start().child(text);
    if let Some(t) = thumbnail {
        top = top.child(div().ml_4().flex_none().child(this.embed_image(
            &format!("{id}-thumb"),
            t,
            requests::EMBED_THUMBNAIL_MAX,
            0.,
            e.nsfw,
            e.title.clone().unwrap_or_else(|| t!("embed-thumbnail")),
            &origin,
            cx,
        )));
    }
    let mut content = div()
        .flex()
        .flex_col()
        .gap_2()
        .pt_3()
        .px_3()
        .pb(px(14.))
        .flex_1()
        .min_w_0()
        .child(top);
    if let Some(v) = video {
        let still = e.thumbnail.as_ref().filter(|_| e.video.is_some());
        content = content.child(this.embed_video(
            id,
            v,
            still,
            e.url.clone().filter(|u| markdown::openable(u).is_some()),
            e.nsfw,
            &origin,
            cx,
        ));
    } else if let Some(i) = image {
        content = content.child(this.embed_image(
            &format!("{id}-image"),
            i,
            requests::embed_media_box(i),
            crate::ui::media::EMBED_CHROME,
            e.nsfw,
            e.title.clone().unwrap_or_else(|| t!("embed-image")),
            &origin,
            cx,
        ));
    }
    if e.footer.is_some() || e.timestamp_ms.is_some() {
        let icon = this.embed_icon(e.footer_icon.as_deref(), 20., cx);
        let when = e.timestamp_ms.map(crate::ui::chat::time::full);
        let line = match (&e.footer, when) {
            (Some(f), Some(w)) => t!("embed-footer-time", footer = f.clone(), time = w),
            (Some(f), None) => f.clone(),
            (None, Some(w)) => w,
            (None, None) => String::new(),
        };
        content = content.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .children(icon)
                .child(
                    div()
                        .text_size(px(12.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .line_height(px(16.))
                        .text_color(theme::text())
                        .child(line),
                ),
        );
    }
    div()
        .mt_1()
        .max_w(px(if has_media {
            EMBED_MEDIA_WIDTH
        } else {
            EMBED_MAX_WIDTH
        }))
        .flex()
        .flex_row()
        .rounded(px(8.))
        .overflow_hidden()
        .border_1()
        .border_color(theme::accent_border())
        .bg(theme::embed_bg())
        .child(div().w(px(4.)).flex_none().bg(EmbedBar::of(e.color).hsla()))
        .child(content)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::messages::parse::to_embed;
    use crate::model::Names;
    use serde_json::json;

    #[test]
    fn embeds_of_spoilered_links_are_covered() {
        let blocks = markdown::parse("look ||https://x.example/a.png|| and https://open.example/");
        let hidden = markdown::spoilered_links(&blocks);
        assert_eq!(hidden, vec!["https://x.example/a.png".to_string()]);
        let image = model::Embed {
            kind: EmbedKind::Image,
            url: Some("https://x.example/a.png".into()),
            image: Some(model::EmbedMedia {
                url: "https://x.example/a.png".into(),
                proxy_url: Some("https://media.example.org/external/x/a.png".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(embed_spoilered(&image, &hidden));
        // The open link's embed shows; nothing is hidden once the spoilers are shown.
        let open = model::Embed {
            url: Some("https://open.example".into()),
            ..Default::default()
        };
        assert!(!embed_spoilered(&open, &hidden));
        assert!(!embed_spoilered(&image, &[]));
        // Inside a quote too.
        let quoted = markdown::parse("> ||https://x.example/a.png||");
        assert_eq!(markdown::spoilered_links(&quoted), hidden);
        // A spoiler around whole blocks hides every link in it.
        let block = markdown::parse(
            "||line one
https://x.example/a.png||",
        );
        assert!(markdown::spoilered_links(&block).contains(&hidden[0]));
    }

    #[test]
    fn explicit_video_embeds_are_covered() {
        use crate::ui::media::video_explicit;
        let plain = model::EmbedMedia::default();
        let marked = model::EmbedMedia {
            explicit: true,
            ..Default::default()
        };
        assert!(!video_explicit(false, &plain, Some(&plain)));
        assert!(video_explicit(true, &plain, None));
        assert!(video_explicit(false, &marked, None));
        assert!(video_explicit(false, &plain, Some(&marked)));
    }

    #[test]
    fn embed_without_colour_uses_brand_foreground() {
        let names = Names::default();
        let plain = to_embed(&json!({"title": "No colour"}), &names).unwrap();
        assert_eq!(plain.color, None);
        assert_eq!(EmbedBar::of(plain.color), EmbedBar::BrandForeground);
        let null = to_embed(&json!({"title": "Null", "color": null}), &names).unwrap();
        assert_eq!(EmbedBar::of(null.color), EmbedBar::BrandForeground);
        // A colour of its own is used as it is, black included.
        let gold = to_embed(&json!({"title": "Gold", "color": 0xF1C40F}), &names).unwrap();
        assert_eq!(EmbedBar::of(gold.color), EmbedBar::Colour(0xF1C40F));
        assert_eq!(EmbedBar::of(Some(0)), EmbedBar::Colour(0));
        assert_eq!(EmbedBar::Colour(0xF1C40F).hsla(), Hsla::from(rgb(0xF1C40F)));
        // The default is Fluxer's brand foreground, #413CDD.
        assert_eq!(theme::BRAND_FOREGROUND_DEFAULT, 0x413CDD);
    }

    #[test]
    fn inline_fields_share_rows() {
        let f = |inline| EmbedField {
            name: String::new(),
            value: Vec::new(),
            inline,
        };
        let fields = [f(true), f(true), f(true), f(true), f(false), f(true)];
        let sizes: Vec<usize> = field_rows(&fields, false).iter().map(Vec::len).collect();
        assert_eq!(sizes, [3, 1, 1, 1]);
        let sizes: Vec<usize> = field_rows(&fields, true).iter().map(Vec::len).collect();
        assert_eq!(sizes, [2, 2, 1, 1]);
    }

    #[test]
    fn every_embed_kind_is_read() {
        let names = Names::default();
        for (kind, want) in [
            ("rich", EmbedKind::Rich),
            ("link", EmbedKind::Link),
            ("article", EmbedKind::Article),
            ("image", EmbedKind::Image),
            ("gifv", EmbedKind::Gifv),
            ("video", EmbedKind::Video),
            ("bluesky", EmbedKind::Bluesky),
            ("something-new", EmbedKind::Rich),
        ] {
            let e = to_embed(
                &json!({"type": kind, "url": "https://example.com/x",
                        "thumbnail": {"url": "https://example.com/t.png",
                                      "proxy_url": "https://media.example.org/external/s/t.png",
                                      "width": 80, "height": 60}}),
                &names,
            )
            .unwrap();
            assert_eq!(e.kind, want, "{kind}");
            let t = e.thumbnail.unwrap();
            assert_eq!((t.width, t.height), (Some(80), Some(60)));
        }
    }
}
