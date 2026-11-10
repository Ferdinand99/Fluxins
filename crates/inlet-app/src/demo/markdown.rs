// SPDX-License-Identifier: AGPL-3.0-or-later
//! Demo messages for `#development` that show every kind of node Fluxer's markdown
//! parser makes. Three scenes split them so each fits on one screen:
//!
//! - `markdown-inline`: styles, links, mention pills (user, role, channel, everyone,
//!   here, unknown), timestamps, Unicode and custom emoji, jumbo emoji, spoilers.
//! - `markdown-blocks`: headings, subtext, lists, quotes, alerts.
//! - `markdown-code`: a table, code blocks with highlighting, maths, and a spoiler
//!   around blocks.

use inlet_protocol as p;

use super::{channel_ref, msg, user_id, ROLE_MODERATORS};

const CHANNEL: &str = "c-dev";

/// The demo channel the markdown scenes fill.
pub const SCENE_CHANNEL: &str = CHANNEL;

pub fn inline() -> Vec<p::Message> {
    let now = chrono::Utc::now().timestamp();
    vec![
        msg(
            1,
            CHANNEL,
            "ada",
            "10:00",
            "**bold** *italic* __underline__ ~~strike~~ ***bold italic*** `inline code` and a [masked link](https://fluxer.app/download) next to https://fluxer.app",
        ),
        msg(
            2,
            CHANNEL,
            "linus",
            "10:01",
            &format!(
                "Pills: <@{}> <@&{}> {} @everyone @here </deploy run:1> and someone gone: <@1999999999999999999>",
                user_id("grace"),
                ROLE_MODERATORS,
                channel_ref("c-general"),
            ),
        ),
        msg(
            3,
            CHANNEL,
            "grace",
            "10:02",
            &format!(
                "Release <t:{}:R> (<t:{}:f>), on call <t:{}:t>, deadline <t:{}:D>",
                now - 3 * 3600,
                now - 3 * 3600,
                now + 600,
                now + 9 * 86400
            ),
        ),
        msg(
            4,
            CHANNEL,
            "ada",
            "10:03",
            "Emoji: \u{1F44B} :rocket: :thumbsup::skin-tone-4: \u{1F1F3}\u{1F1F4} 1\u{FE0F}\u{20E3} and a custom one <:inlet:1234567890123456789>",
        ),
        msg(5, CHANNEL, "linus", "10:04", "\u{1F389}\u{1F680}\u{1F525}"),
        msg(
            6,
            CHANNEL,
            "grace",
            "10:05",
            "Spoiler: ||the cake is a lie|| and the rest is plain.",
        ),
    ]
}

pub fn blocks() -> Vec<p::Message> {
    vec![
        msg(
            11,
            CHANNEL,
            "ada",
            "11:00",
            "# Release notes\n## Fixes\n### Small things\n-# Subtext under the headings\n1. First\n2. Second\n   - nested\n     - deeper\n- loose item",
        ),
        msg(
            12,
            CHANNEL,
            "linus",
            "11:01",
            "> A quote\n> over two lines\n\n> [!NOTE]\n> Useful information.\n\n> [!WARNING]\n> Careful here.",
        ),
    ]
}

pub fn code() -> Vec<p::Message> {
    vec![
        msg(
            13,
            CHANNEL,
            "grace",
            "11:02",
            "| Name | Role | Since |\n| :--- | :---: | ---: |\n| Ada | Moderator | 2024 |\n| Linus | Member | 2025 |",
        ),
        msg(
            14,
            CHANNEL,
            "ada",
            "11:03",
            "```rust\n// Highlighted with syntect\nfn main() {\n    let answer = 42;\n    println!(\"{answer}\");\n}\n```\n```tex\nE = mc^2\n```",
        ),
        msg(
            15,
            CHANNEL,
            "linus",
            "11:04",
            "||A spoiler around\nmore than one line||",
        ),
    ]
}
