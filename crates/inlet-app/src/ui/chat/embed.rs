// SPDX-License-Identifier: AGPL-3.0-or-later
//! Draws an embed (bots post these) as a card.

use gpui::{div, prelude::*, px, AnyElement, Context, FontWeight, SharedString};

use crate::app::Inlet;
use crate::model;
use crate::theme;
use crate::ui::markdown::{self, RenderCtx};

/// Draws an embed: colour bar, author, title, description, fields, footer, media links.
pub(super) fn render_embed(
    id: &str,
    e: &model::Embed,
    ctx: &RenderCtx,
    cx: &mut Context<Inlet>,
) -> AnyElement {
    let bar = e
        .color
        .map(|c| gpui::rgb(c).into())
        .unwrap_or(theme::brand());
    let mut content = div()
        .flex()
        .flex_col()
        .gap_2()
        .pt_3()
        .px_3()
        .pb(px(14.))
        .flex_1()
        .min_w_0();
    if let Some(author) = &e.author {
        content = content.child(
            div()
                .text_size(px(13.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(author.clone()),
        );
    }
    if let Some(title) = &e.title {
        let link = e.url.clone().filter(|u| markdown::openable(u).is_some());
        let mut t = div()
            .id(SharedString::from(format!("{id}-title")))
            .font_weight(FontWeight::BOLD)
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
        content = content.child(t);
    }
    if !e.description.is_empty() {
        content = content.child(div().text_size(px(14.)).child(markdown::render(
            &format!("{id}-d"),
            &e.description,
            ctx,
        )));
    }
    for (i, f) in e.fields.iter().enumerate() {
        content = content.child(
            div()
                .pt_1()
                .flex()
                .flex_col()
                .text_size(px(14.))
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme::text_strong())
                        .child(f.name.clone()),
                )
                .child(markdown::render(&format!("{id}-f{i}"), &f.value, ctx)),
        );
    }
    for (i, a) in e.media.iter().enumerate() {
        let url = a.url.clone();
        content = content.child(
            div()
                .id(SharedString::from(format!("{id}-media{i}")))
                .text_size(px(13.))
                .text_color(theme::link())
                .cursor_pointer()
                .child(format!(
                    "{}: {}",
                    a.filename,
                    markdown::link_target_label(&a.url)
                ))
                .on_click(cx.listener(move |this, _, _, cx| this.open_link(&url, false, cx))),
        );
    }
    if let Some(footer) = &e.footer {
        content = content.child(
            div()
                .pt_1()
                .text_size(px(12.))
                .text_color(theme::text_muted())
                .child(footer.clone()),
        );
    }
    div()
        .mt_1()
        .max_w(px(516.))
        .flex()
        .flex_row()
        .rounded(px(8.))
        .overflow_hidden()
        .border_1()
        .border_color(theme::accent_border())
        .bg(theme::embed_bg())
        .child(div().w(px(4.)).flex_none().bg(bar))
        .child(content)
        .into_any_element()
}
