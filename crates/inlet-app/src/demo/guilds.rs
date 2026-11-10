// SPDX-License-Identifier: AGPL-3.0-or-later
//! Demo guilds and their channels.

use std::collections::HashMap;

use super::channel;
use crate::model::{Channel, ChannelKind, Guild};

pub fn guilds() -> Vec<Guild> {
    vec![
        Guild {
            id: "g1".into(),
            name: "Inlet Dev".into(),
        },
        Guild {
            id: "g2".into(),
            name: "Rust Norway".into(),
        },
        Guild {
            id: "g3".into(),
            name: "Self Hosting".into(),
        },
    ]
}

/// Adds the channel lists of the demo guilds.
pub fn channels(channels: &mut HashMap<Option<String>, Vec<Channel>>) {
    channels.insert(
        Some("g1".to_string()),
        vec![
            channel("c-info", "Information", ChannelKind::Category, None, 0),
            channel("c-welcome", "welcome", ChannelKind::Text, Some("c-info"), 0),
            channel("c-text", "Text channels", ChannelKind::Category, None, 1),
            channel("c-general", "general", ChannelKind::Text, Some("c-text"), 0),
            channel("c-dev", "development", ChannelKind::Text, Some("c-text"), 1),
            channel("c-voice", "Voice", ChannelKind::Category, None, 2),
            channel("c-lounge", "Lounge", ChannelKind::Voice, Some("c-voice"), 0),
            channel(
                "c-music",
                "\u{1F50A} Music",
                ChannelKind::Voice,
                Some("c-voice"),
                1,
            ),
            // Names like real servers have: emoji, very long names, non-Latin text.
            channel(
                "c-community",
                "Community \u{1F30D}",
                ChannelKind::Category,
                None,
                3,
            ),
            channel(
                "c-ann",
                "\u{1F389}-announcements",
                ChannelKind::Announcement,
                Some("c-community"),
                0,
            ),
            channel(
                "c-rules",
                "\u{1F4DC}-rules-and-information-for-everyone-who-joins-the-server-please-read",
                ChannelKind::Text,
                Some("c-community"),
                1,
            ),
            channel(
                "c-ja",
                "\u{65E5}\u{672C}\u{8A9E}-\u{30C1}\u{30E3}\u{30C3}\u{30C8}",
                ChannelKind::Text,
                Some("c-community"),
                2,
            ),
            channel(
                "c-ru",
                "\u{0440}\u{0443}\u{0441}\u{0441}\u{043A}\u{0438}\u{0439}-\u{0447}\u{0430}\u{0442}",
                ChannelKind::Text,
                Some("c-community"),
                3,
            ),
            channel(
                "c-ar",
                "\u{0627}\u{0644}\u{0639}\u{0631}\u{0628}\u{064A}\u{0629}",
                ChannelKind::Text,
                Some("c-community"),
                4,
            ),
            channel(
                "c-memes",
                "memes \u{1F602}",
                ChannelKind::Text,
                Some("c-community"),
                5,
            ),
            channel(
                "c-help",
                "help-desk",
                ChannelKind::Text,
                Some("c-community"),
                6,
            ),
            channel(
                "c-offtopic",
                "off-topic",
                ChannelKind::Text,
                Some("c-community"),
                7,
            ),
            channel(
                "c-bots",
                "bot-commands \u{2699}\u{FE0F}",
                ChannelKind::Text,
                Some("c-community"),
                8,
            ),
            channel("c-sylo", "Sylo", ChannelKind::Category, None, 4),
            channel(
                "c-joins",
                "New Members",
                ChannelKind::Text,
                Some("c-sylo"),
                0,
            ),
            channel("c-logs", "sylo-logs", ChannelKind::Text, Some("c-sylo"), 1),
            channel(
                "c-mod",
                "moderation-log",
                ChannelKind::Text,
                Some("c-sylo"),
                2,
            ),
            channel(
                "c-tickets",
                "\u{1F3AB}-tickets",
                ChannelKind::Text,
                Some("c-sylo"),
                3,
            ),
        ],
    );
    channels.insert(
        Some("g2".to_string()),
        vec![channel("r-general", "general", ChannelKind::Text, None, 0)],
    );
    channels.insert(
        Some("g3".to_string()),
        vec![channel("s-general", "general", ChannelKind::Text, None, 0)],
    );
}
