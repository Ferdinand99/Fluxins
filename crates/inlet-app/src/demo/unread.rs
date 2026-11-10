// SPDX-License-Identifier: AGPL-3.0-or-later
//! Demo unread state: the pills, counts and muted looks the sidebar and the rail can show.

use crate::backend::notifications::{Badge, BadgeUpdate};

fn unread() -> Badge {
    Badge {
        unread: true,
        ..Badge::default()
    }
}

fn mentioned(count: u32) -> Badge {
    Badge {
        unread: true,
        mentions: count,
        ..Badge::default()
    }
}

/// A muted channel with unread messages: faded pill, half-strength row.
fn muted_unread() -> Badge {
    Badge {
        unread: true,
        muted: true,
        dimmed: true,
        ..Badge::default()
    }
}

/// What the demo account has not read yet.
pub fn badges() -> BadgeUpdate {
    BadgeUpdate {
        reset: true,
        channels: vec![
            ("c-dev".into(), unread()),
            ("c-ann".into(), unread()),
            ("c-rules".into(), mentioned(12)),
            ("c-help".into(), mentioned(3)),
            ("c-memes".into(), muted_unread()),
            // A muted category: the name is drawn at half strength.
            (
                "c-voice".into(),
                Badge {
                    muted: true,
                    dimmed: true,
                    ..Badge::default()
                },
            ),
            ("dm-ada".into(), mentioned(1)),
            ("dm-group".into(), unread()),
        ],
        guilds: vec![
            (Some("g1".into()), mentioned(15)),
            (Some("g2".into()), unread()),
            // A muted server: no pill, its mentions still count.
            (
                Some("g3".into()),
                Badge {
                    unread: false,
                    mentions: 2,
                    muted: true,
                    dimmed: false,
                },
            ),
            (None, mentioned(1)),
        ],
    }
}
