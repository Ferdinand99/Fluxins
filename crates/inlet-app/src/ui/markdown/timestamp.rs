// SPDX-License-Identifier: AGPL-3.0-or-later
//! Timestamps (`<t:1700000000:R>`), formatted as Fluxer's `DateFormatter` does: the
//! nine styles in the system locale's date order and clock, and relative times
//! ("in 5 minutes", "yesterday") like `Intl.RelativeTimeFormat` with `numeric: auto`.
//!
//! A relative timestamp is a [`Live`](super::Live) piece: its label is worked out every
//! time it is drawn, and [`refresh_due`] tells the message list when a label on screen
//! is about to change, so it can draw again without parsing anything.
//!
//! Deviation: month and weekday names are English, like the rest of the UI until it is
//! translated; Fluxer uses the browser locale's names.

use std::cell::Cell;

use chrono::{DateTime, Datelike, Local, NaiveTime, Timelike};
use fluxer_markdown_parser::ast::TimestampStyle;

use super::inline::{push_live, push_marked};
use super::{Inline, LiveKind, Style};
use crate::ui::chat::time::{self, LocaleFormat};

thread_local! {
    /// The soonest moment (Unix seconds) a relative timestamp drawn on screen changes.
    static NEXT_CHANGE: Cell<Option<i64>> = const { Cell::new(None) };
}

fn now_s() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Parsing: appends a timestamp. One that is not a valid date shows its number.
pub(super) fn push(timestamp: u64, style: TimestampStyle, inline: &mut Inline) {
    match i64::try_from(timestamp)
        .ok()
        .filter(|t| DateTime::from_timestamp(*t, 0).is_some())
    {
        Some(unix) => push_live(
            inline,
            &label(unix, style, now_s()),
            LiveKind::Timestamp { unix, style },
        ),
        None => push_marked(inline, &timestamp.to_string(), Style::Timestamp),
    }
}

fn month_name(month: u32) -> String {
    match month {
        1 => t!("markdown-month-1"),
        2 => t!("markdown-month-2"),
        3 => t!("markdown-month-3"),
        4 => t!("markdown-month-4"),
        5 => t!("markdown-month-5"),
        6 => t!("markdown-month-6"),
        7 => t!("markdown-month-7"),
        8 => t!("markdown-month-8"),
        9 => t!("markdown-month-9"),
        10 => t!("markdown-month-10"),
        11 => t!("markdown-month-11"),
        _ => t!("markdown-month-12"),
    }
}

/// `weekday` counts from Monday = 1.
fn weekday_name(weekday: u32) -> String {
    match weekday {
        1 => t!("markdown-weekday-1"),
        2 => t!("markdown-weekday-2"),
        3 => t!("markdown-weekday-3"),
        4 => t!("markdown-weekday-4"),
        5 => t!("markdown-weekday-5"),
        6 => t!("markdown-weekday-6"),
        _ => t!("markdown-weekday-7"),
    }
}

fn long_date(f: &LocaleFormat, t: &DateTime<Local>) -> String {
    let (month, day, year) = (month_name(t.month()), t.day(), t.year());
    match f.order {
        time::DateOrder::MonthDayYear => {
            t!(
                "markdown-date-long-mdy",
                month = month,
                day = day,
                year = year
            )
        }
        _ => t!(
            "markdown-date-long-dmy",
            month = month,
            day = day,
            year = year
        ),
    }
}

fn time_with_seconds(f: &LocaleFormat, t: NaiveTime) -> String {
    let (h, m, s) = (t.hour(), t.minute(), t.second());
    if f.hour12 {
        let h12 = if h % 12 == 0 { 12 } else { h % 12 };
        let clock = format!("{h12}:{m:02}:{s:02}");
        if h < 12 {
            t!("time-am", time = clock)
        } else {
            t!("time-pm", time = clock)
        }
    } else {
        format!("{h:02}:{m:02}:{s:02}")
    }
}

/// The label of a timestamp at `unix` (seconds) in `style`, seen at `now_s`.
pub(super) fn label(unix: i64, style: TimestampStyle, now_s: i64) -> String {
    let Some(utc) = DateTime::from_timestamp(unix, 0) else {
        return unix.to_string();
    };
    let t = utc.with_timezone(&Local);
    let f = time::system();
    let date_and_time = |time: String| {
        t!(
            "markdown-date-at-time",
            date = long_date(&f, &t),
            time = time
        )
    };
    match style {
        TimestampStyle::ShortTime => f.time(t.time()),
        TimestampStyle::LongTime => time_with_seconds(&f, t.time()),
        TimestampStyle::ShortDate => f.date(t.date_naive()),
        TimestampStyle::LongDate => long_date(&f, &t),
        TimestampStyle::ShortDateTime => date_and_time(f.time(t.time())),
        TimestampStyle::LongDateTime => t!(
            "markdown-weekday-and-date",
            weekday = weekday_name(t.weekday().number_from_monday()),
            date = date_and_time(f.time(t.time()))
        ),
        TimestampStyle::ShortDateShortTime => t!(
            "time-date-and-time",
            date = f.date(t.date_naive()),
            time = f.time(t.time())
        ),
        TimestampStyle::ShortDateMediumTime => t!(
            "time-date-and-time",
            date = f.date(t.date_naive()),
            time = time_with_seconds(&f, t.time())
        ),
        TimestampStyle::RelativeTime => relative(unix, now_s),
    }
}

const MINUTE: i64 = 60;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;
const WEEK: i64 = 7 * DAY;
const MONTH: i64 = 30 * DAY;
const YEAR: i64 = 365 * DAY;

/// The unit Fluxer picks for a distance of `abs` seconds, in seconds.
fn unit_of(abs: i64) -> i64 {
    let days = abs / DAY;
    if days >= 365 {
        YEAR
    } else if days >= 30 {
        MONTH
    } else if days >= 7 {
        WEEK
    } else if days > 0 {
        DAY
    } else if abs >= HOUR {
        HOUR
    } else if abs >= MINUTE {
        MINUTE
    } else {
        1
    }
}

/// "in 3 days", "5 minutes ago", "yesterday", "now".
pub(super) fn relative(unix: i64, now_s: i64) -> String {
    let diff = unix.saturating_sub(now_s);
    let future = diff >= 0;
    let abs = diff.unsigned_abs().min(i64::MAX as u64) as i64;
    let unit = unit_of(abs);
    let count = abs / unit;
    match (unit, future, count == 1) {
        (1, _, _) if count == 0 => t!("markdown-time-now"),
        (1, true, true) => t!("markdown-time-seconds-future-one"),
        (1, true, false) => t!("markdown-time-seconds-future", count = count),
        (1, false, true) => t!("markdown-time-seconds-past-one"),
        (1, false, false) => t!("markdown-time-seconds-past", count = count),
        (MINUTE, true, true) => t!("markdown-time-minutes-future-one"),
        (MINUTE, true, false) => t!("markdown-time-minutes-future", count = count),
        (MINUTE, false, true) => t!("markdown-time-minutes-past-one"),
        (MINUTE, false, false) => t!("markdown-time-minutes-past", count = count),
        (HOUR, true, true) => t!("markdown-time-hours-future-one"),
        (HOUR, true, false) => t!("markdown-time-hours-future", count = count),
        (HOUR, false, true) => t!("markdown-time-hours-past-one"),
        (HOUR, false, false) => t!("markdown-time-hours-past", count = count),
        (DAY, true, true) => t!("markdown-time-days-future-one"),
        (DAY, true, false) => t!("markdown-time-days-future", count = count),
        (DAY, false, true) => t!("markdown-time-days-past-one"),
        (DAY, false, false) => t!("markdown-time-days-past", count = count),
        (WEEK, true, true) => t!("markdown-time-weeks-future-one"),
        (WEEK, true, false) => t!("markdown-time-weeks-future", count = count),
        (WEEK, false, true) => t!("markdown-time-weeks-past-one"),
        (WEEK, false, false) => t!("markdown-time-weeks-past", count = count),
        (MONTH, true, true) => t!("markdown-time-months-future-one"),
        (MONTH, true, false) => t!("markdown-time-months-future", count = count),
        (MONTH, false, true) => t!("markdown-time-months-past-one"),
        (MONTH, false, false) => t!("markdown-time-months-past", count = count),
        (YEAR, true, true) => t!("markdown-time-years-future-one"),
        (YEAR, true, false) => t!("markdown-time-years-future", count = count),
        (YEAR, false, true) => t!("markdown-time-years-past-one"),
        (YEAR, false, false) => t!("markdown-time-years-past", count = count),
        _ => t!("markdown-time-now"),
    }
}

/// Seconds from `now_s` until the relative label of `unix` changes.
fn seconds_to_change(unix: i64, now_s: i64) -> i64 {
    let diff = unix.saturating_sub(now_s);
    let abs = diff.saturating_abs();
    let unit = unit_of(abs);
    let step = if diff >= 0 {
        // Counting down: the label changes when the distance drops below this count.
        abs % unit + 1
    } else {
        unit - abs % unit
    };
    step.clamp(1, DAY)
}

/// A relative timestamp at `unix` was drawn at `now_s`.
pub(super) fn note_drawn(unix: i64, now_s: i64) {
    let at = now_s.saturating_add(seconds_to_change(unix, now_s));
    NEXT_CHANGE.with(|next| {
        if next.get().is_none_or(|n| at < n) {
            next.set(Some(at));
        }
    });
}

/// Whether a relative timestamp on screen shows a new label at `now_ms`. The message
/// list asks about once a second and draws again when it does; the next drawing notes
/// the following change.
pub fn refresh_due(now_ms: i64) -> bool {
    NEXT_CHANGE.with(|next| match next.get() {
        Some(at) if now_ms.div_euclid(1000) >= at => {
            next.set(None);
            true
        }
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::super::inline::compose;
    use super::super::{parse, Block, MentionNames};
    use super::*;

    #[test]
    fn timestamp_relative_updates_without_reparsing() {
        let now = now_s();
        let at = now - 30;
        let blocks = parse(&format!("posted <t:{at}:R>"));
        let Block::Text(inline) = &blocks[0] else {
            panic!("{blocks:?}");
        };
        let names = MentionNames::default();
        // The same parsed inline, drawn at three moments.
        assert_eq!(compose(inline, &names, now).text, "posted 30 seconds ago");
        assert_eq!(
            compose(inline, &names, now + 90).text,
            "posted 2 minutes ago"
        );
        assert_eq!(
            compose(inline, &names, now + 2 * DAY).text,
            "posted 2 days ago"
        );
        assert_eq!(compose(inline, &names, at).text, "posted now");
        assert_eq!(
            compose(inline, &names, at - DAY - 5).text,
            "posted tomorrow"
        );
        // The list is told when the label on screen changes, and only then.
        NEXT_CHANGE.with(|n| n.set(None));
        note_drawn(at, now);
        assert!(!refresh_due(now * 1000));
        assert!(refresh_due((now + 1) * 1000));
        assert!(!refresh_due((now + 2) * 1000));
    }

    #[test]
    fn relative_times_follow_intl_numeric_auto() {
        let n = 1_000_000_000;
        assert_eq!(relative(n, n), "now");
        assert_eq!(relative(n + 1, n), "in 1 second");
        assert_eq!(relative(n - 59, n), "59 seconds ago");
        assert_eq!(relative(n - 60, n), "1 minute ago");
        assert_eq!(relative(n + 2 * HOUR, n), "in 2 hours");
        assert_eq!(relative(n - DAY, n), "yesterday");
        assert_eq!(relative(n + 3 * DAY, n), "in 3 days");
        assert_eq!(relative(n - 7 * DAY, n), "last week");
        assert_eq!(relative(n + 14 * DAY, n), "in 2 weeks");
        assert_eq!(relative(n + 30 * DAY, n), "next month");
        assert_eq!(relative(n - 400 * DAY, n), "last year");
        assert_eq!(relative(n - 800 * DAY, n), "2 years ago");
    }

    #[test]
    fn changes_are_noted_at_unit_boundaries() {
        let n = 1_000_000_000;
        assert_eq!(seconds_to_change(n - 30, n), 1);
        assert_eq!(seconds_to_change(n - 90, n), 30);
        assert_eq!(seconds_to_change(n + 3 * HOUR + 10, n), 11);
        assert!(seconds_to_change(n - 400 * DAY, n) <= DAY);
    }

    #[test]
    fn invalid_timestamps_show_their_number() {
        // The parser drops timestamps past year 275 760; this one is out of i64.
        let mut inline = Inline::default();
        push(u64::MAX, TimestampStyle::RelativeTime, &mut inline);
        assert!(inline.live.is_empty(), "{inline:?}");
        assert_eq!(inline.text, u64::MAX.to_string());
        assert_eq!(inline.highlights[0].1, Style::Timestamp);
    }

    #[test]
    fn every_style_formats() {
        let n = 1_700_000_000;
        for style in [
            TimestampStyle::ShortTime,
            TimestampStyle::LongTime,
            TimestampStyle::ShortDate,
            TimestampStyle::LongDate,
            TimestampStyle::ShortDateTime,
            TimestampStyle::LongDateTime,
            TimestampStyle::ShortDateShortTime,
            TimestampStyle::ShortDateMediumTime,
            TimestampStyle::RelativeTime,
        ] {
            let s = label(n, style, n + 10);
            assert!(!s.is_empty() && !s.contains("markdown-"), "{style:?}: {s}");
        }
        assert!(label(n, TimestampStyle::LongDate, n).contains("2023"));
    }
}
