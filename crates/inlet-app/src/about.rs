// SPDX-License-Identifier: AGPL-3.0-or-later
//! The About view: what Inlet is, its licence and where the source is (the "Appropriate
//! Legal Notices" of AGPL-3.0 §5(d)), and the licences of everything it is built from.

use std::sync::OnceLock;

use gpui::{div, prelude::*, px, uniform_list, Context, FontWeight, SharedString};

use crate::{button, theme, Inlet};

/// Generated with `cargo about` (see `scripts/third-party-notices.sh`) and shipped next to
/// every binary as `THIRD-PARTY-NOTICES`; also built into the app so it is always there.
const NOTICES: &str = include_str!("../../../THIRD-PARTY-NOTICES");

const SOURCE_URL: &str = "https://github.com/Ferdinand99/Inlet";

/// Width the notices are wrapped to, in characters. The list draws one line per row.
const WRAP: usize = 110;

fn notice_lines() -> &'static [SharedString] {
    static LINES: OnceLock<Vec<SharedString>> = OnceLock::new();
    LINES.get_or_init(|| {
        NOTICES
            .lines()
            .flat_map(|l| wrap(l, WRAP))
            .map(SharedString::from)
            .collect()
    })
}

/// Splits a line at spaces so no piece is longer than `width` characters (a single word
/// longer than that stays whole).
fn wrap(line: &str, width: usize) -> Vec<String> {
    if line.chars().count() <= width {
        return vec![line.to_string()];
    }
    let mut out = Vec::new();
    let mut current = String::new();
    for word in line.split(' ') {
        if !current.is_empty() && current.chars().count() + 1 + word.chars().count() > width {
            out.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// The source of exactly this build: the commit CI built from, or the version tag.
pub fn source_link() -> String {
    match option_env!("INLET_SOURCE_REV") {
        Some(rev) if !rev.is_empty() => format!("{SOURCE_URL}/tree/{rev}"),
        // A local build has no commit to point at: link the repository (default branch).
        _ => SOURCE_URL.to_string(),
    }
}

pub fn render(show_licences: bool, cx: &mut Context<Inlet>) -> impl IntoElement {
    let source = source_link();
    let header = div()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .flex()
                .flex_row()
                .items_baseline()
                .gap_2()
                .child(div().text_size(px(24.)).font_weight(FontWeight::BOLD).text_color(theme::text_strong()).child("Inlet"))
                .child(div().text_color(theme::text_muted()).child(format!("version {}", env!("CARGO_PKG_VERSION")))),
        )
        .child(div().child("A desktop client that works with Fluxer. Inlet is an independent project; it is not made or endorsed by Fluxer Platform AB. \"Fluxer\" is their trademark."))
        .child(div().child("Copyright \u{a9} 2026 the Inlet contributors."))
        .child(div().child(
            "Inlet is free software: you can redistribute it and/or modify it under the terms of the GNU Affero General Public License, version 3 or (at your option) any later version. It comes with ABSOLUTELY NO WARRANTY.",
        ))
        .child(
            div()
                .id("source-link")
                .text_color(theme::link())
                .cursor_pointer()
                .child(format!("Source code of this build: {source}"))
                .on_click(move |_, _, cx| cx.open_url(&source)),
        )
        .child(div().text_color(theme::text_muted()).child(
            "Built with Fluxer's markdown parser (fluxer_markdown_parser, AGPL-3.0-or-later, Fluxer Platform AB, commit f1cba64), gpui from Zed (Apache-2.0, Zed Industries) and the crates listed under third-party licences. The full text of every licence, including the AGPL, is under \"Third-party licences\".",
        ))
        .child(
            div()
                .flex()
                .flex_row()
                .gap_2()
                .child(
                    button("toggle-licences", if show_licences { "Hide third-party licences" } else { "Third-party licences" }, false)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.show_licences = !this.show_licences;
                            cx.notify();
                        })),
                )
                .child(button("close-about", "Close", true).on_click(cx.listener(|this, _, _, cx| {
                    this.show_about = false;
                    this.show_licences = false;
                    cx.notify();
                }))),
        );

    let lines = notice_lines();
    div()
        .size_full()
        .flex()
        .flex_col()
        .gap_4()
        .p_8()
        .bg(theme::chat_bg())
        .child(header)
        .when(show_licences, |d| {
            d.child(
                div()
                    .flex_1()
                    .rounded_md()
                    .bg(theme::code_bg())
                    .p_2()
                    .font_family(crate::markdown::mono_font())
                    .text_size(px(12.))
                    .child(
                        uniform_list("licences", lines.len(), move |range, _window, _cx| {
                            range
                                .map(|ix| {
                                    div()
                                        .whitespace_nowrap()
                                        .overflow_hidden()
                                        .child(lines[ix].clone())
                                })
                                .collect::<Vec<_>>()
                        })
                        .size_full(),
                    ),
            )
        })
}

#[cfg(test)]
mod tests {
    #[test]
    fn wrap_keeps_lines_short() {
        let text = "word ".repeat(100);
        for line in super::wrap(text.trim_end(), 30) {
            assert!(line.chars().count() <= 30);
        }
        assert_eq!(super::wrap("short", 30), vec!["short".to_string()]);
    }
}
