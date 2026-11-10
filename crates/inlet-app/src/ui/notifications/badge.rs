// SPDX-License-Identifier: AGPL-3.0-or-later
//! The unread pill and the mention badge, as Fluxer draws them, and the rules for what a
//! channel row or a guild rail entry shows for a [`Badge`].

use gpui::{div, prelude::*, px, Div, FontWeight, Hsla};

use crate::theme;

/// What the sidebar and the guild rail show for one channel, category or guild.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Badge {
    /// Unread messages: the white pill and bold name (not for a muted guild).
    pub unread: bool,
    /// Unread mentions: the red count.
    pub mentions: u32,
    /// Muted at some level (the channel, its category or the guild).
    pub muted: bool,
    /// The channel or category is muted itself: its row is drawn dimmed.
    pub dimmed: bool,
}

impl Badge {
    /// Nothing to show: the UI keeps no entry for it.
    pub fn is_default(&self) -> bool {
        *self == Badge::default()
    }
}

/// What a channel or DM row shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RowMarks {
    /// The small white pill to the left of the row.
    pub pill: bool,
    /// The pill is drawn at 40 % (the channel is muted but has unread messages).
    pub pill_faded: bool,
    /// The name is drawn brighter: a mention, or unread in a channel that is not muted.
    pub highlight: bool,
    /// The red count at the right end of the row.
    pub count: bool,
    /// The row is drawn at half strength: the channel is muted itself.
    pub dimmed: bool,
}

impl RowMarks {
    // Deviation: Fluxer lets a person set an "unread badges" level per channel and turn
    // the faded unread pill on muted channels off. Inlet has neither setting: customisation
    // is off and the faded pill is always shown.
    /// Fluxer's `getChannelUnreadState` with the unread-badge customisation off, plus the
    /// rules of `ChannelItem`: no count on the open channel, faded unread on muted ones.
    pub fn of(b: Badge, selected: bool) -> RowMarks {
        RowMarks {
            pill: b.unread,
            pill_faded: b.unread && b.muted,
            highlight: b.mentions > 0 || (b.unread && !b.muted),
            count: !selected && b.mentions > 0,
            dimmed: b.dimmed,
        }
    }
}

/// What a guild rail entry shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RailMarks {
    /// The pill at rest (8 px) when the guild is not the open one.
    pub pill: bool,
    /// The red count on the icon; 0 for none.
    pub mentions: u32,
}

impl RailMarks {
    /// A muted guild shows no pill, but its mentions still count.
    pub fn of(b: Badge) -> RailMarks {
        RailMarks {
            pill: b.unread && !b.muted,
            mentions: b.mentions,
        }
    }
}

/// The count as Fluxer writes it: 1 to 99, then "99+" up to 999, then "1K", "2K" …
pub fn count_text(n: u32) -> String {
    match n {
        0..=99 => n.to_string(),
        100..=999 => "99+".to_string(),
        1_000..=999_499 => t!("count-thousands", count = (n + 500) / 1_000),
        _ => t!(
            "count-millions",
            count = n.saturating_add(500_000) / 1_000_000
        ),
    }
}

/// The red mention badge, 20 px high (`MentionBadge`, size small).
pub fn mention_badge(count: u32) -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .h(px(20.))
        .min_w(px(20.))
        .px(px(6.))
        .rounded(px(6.))
        .bg(theme::danger())
        .text_color(theme::on_danger())
        .text_size(px(11.))
        .line_height(px(14.))
        .font_weight(FontWeight::SEMIBOLD)
        .child(count_text(count))
}

/// The badge on a guild icon: bottom right, 4 px out, with a 3 px ring in the rail's
/// colour cutting it out of the icon (`guildBadge`).
pub fn rail_badge(count: u32, surface: Hsla) -> Div {
    div()
        .absolute()
        .right(px(-7.))
        .bottom(px(-7.))
        .p(px(3.))
        .rounded(px(9.))
        .bg(surface)
        .child(mention_badge(count))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn badge(unread: bool, mentions: u32, muted: bool, dimmed: bool) -> Badge {
        Badge {
            unread,
            mentions,
            muted,
            dimmed,
        }
    }

    #[test]
    fn counts_read_like_fluxers() {
        assert_eq!(count_text(1), "1");
        assert_eq!(count_text(99), "99");
        assert_eq!(count_text(100), "99+");
        assert_eq!(count_text(999), "99+");
        assert_eq!(count_text(1_000), "1K");
        assert_eq!(count_text(1_499), "1K");
        assert_eq!(count_text(1_500), "2K");
        assert_eq!(count_text(u32::MAX), "4294M");
    }

    #[test]
    fn a_muted_guild_has_no_pill_but_keeps_its_count() {
        let muted_guild = badge(false, 3, true, false);
        assert_eq!(
            RailMarks::of(muted_guild),
            RailMarks {
                pill: false,
                mentions: 3
            }
        );
        // Even if a muted guild ever came with `unread`, the pill stays off.
        assert!(!RailMarks::of(badge(true, 0, true, false)).pill);
        assert_eq!(
            RailMarks::of(badge(true, 2, false, false)),
            RailMarks {
                pill: true,
                mentions: 2
            }
        );
        assert_eq!(RailMarks::of(Badge::default()), RailMarks::default());
    }

    #[test]
    fn rows_follow_the_unread_rules() {
        // Nothing to show.
        assert_eq!(RowMarks::of(Badge::default(), false), RowMarks::default());

        // Unread: pill and bright name, no count.
        let unread = RowMarks::of(badge(true, 0, false, false), false);
        assert!(unread.pill && !unread.pill_faded && unread.highlight && !unread.count);

        // A mention adds the count, except on the channel that is open.
        let mention = badge(true, 2, false, false);
        assert!(RowMarks::of(mention, false).count);
        assert!(!RowMarks::of(mention, true).count);

        // Muted with unread messages: a faded pill, no bright name.
        let muted = RowMarks::of(badge(true, 0, true, true), false);
        assert!(muted.pill && muted.pill_faded && !muted.highlight && muted.dimmed);

        // Muted but mentioned: the count and the bright name stay.
        let muted_mention = RowMarks::of(badge(true, 1, true, true), false);
        assert!(muted_mention.count && muted_mention.highlight);

        // A channel muted only through its category or guild is not dimmed itself.
        let inherited = RowMarks::of(badge(true, 0, true, false), false);
        assert!(inherited.pill_faded && !inherited.dimmed);
    }
}
