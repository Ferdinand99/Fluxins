// SPDX-License-Identifier: AGPL-3.0-or-later
//! Message dates and times in the order and clock of the system's locale, the way the
//! web client follows the browser's locale ("01/10/2026, 23:52" in en-GB, "10/01/2026,
//! 11:52 PM" in en-US, "01.10.2026, 23:52" in nb-NO).
//!
//! Deviation: Fluxer formats with the browser's full CLDR data. Inlet reads the system
//! locale and covers the common patterns (day, month or year first; `/`, `.` or `-`;
//! 12- or 24-hour clock) without a locale database. Weekday and month names in date
//! separators stay English, like the rest of the UI until it is translated.

use std::sync::OnceLock;

use chrono::{DateTime, Datelike, Local, NaiveDate, NaiveTime, Timelike};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DateOrder {
    DayMonthYear,
    MonthDayYear,
    YearMonthDay,
}

/// How a locale writes a short date and a time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocaleFormat {
    pub order: DateOrder,
    pub separator: char,
    pub hour12: bool,
}

impl LocaleFormat {
    /// The format of a BCP 47 or POSIX locale name (`en-US`, `nb_NO.UTF-8`). Unknown and
    /// empty names get US English, the language of the UI.
    pub fn for_locale(name: &str) -> LocaleFormat {
        let name = name
            .split(['.', '@'])
            .next()
            .unwrap_or_default()
            .replace('_', "-")
            .to_ascii_lowercase();
        let mut parts = name.split('-');
        let lang = parts.next().unwrap_or_default();
        // The region is the first two-letter or three-digit part after the language.
        let region = parts
            .find(|p| p.len() == 2 || (p.len() == 3 && p.chars().all(|c| c.is_ascii_digit())))
            .unwrap_or_default();
        let lang = match lang {
            "" | "c" | "posix" => "en",
            other => other,
        };
        let region = if lang == "en" && region.is_empty() && name.len() <= 2 {
            "us"
        } else {
            region
        };
        use DateOrder::*;
        let (order, separator) = match (lang, region) {
            ("en", "us" | "ph" | "") | ("es", "us") => (MonthDayYear, '/'),
            ("en", "ca") | ("fr", "ca") | ("sv", _) | ("lt", _) => (YearMonthDay, '-'),
            ("zh" | "ja", _) => (YearMonthDay, '/'),
            ("ko" | "hu", _) => (YearMonthDay, '.'),
            ("nl", _) => (DayMonthYear, '-'),
            (
                "de" | "nb" | "nn" | "no" | "da" | "fi" | "ru" | "uk" | "pl" | "cs" | "sk" | "tr"
                | "ro" | "hr" | "sl" | "sr" | "bg" | "et" | "lv" | "is" | "be" | "kk" | "az" | "mk"
                | "sq",
                _,
            ) => (DayMonthYear, '.'),
            _ => (DayMonthYear, '/'),
        };
        let hour12 = matches!(lang, "ko" | "hi" | "ar" | "bn" | "ur")
            || (lang == "en" && matches!(region, "us" | "ca" | "au" | "nz" | "in" | "ph" | ""))
            || (lang == "es" && matches!(region, "us" | "mx"))
            || (lang == "zh" && region == "tw");
        LocaleFormat {
            order,
            separator,
            hour12,
        }
    }

    pub fn date(&self, d: NaiveDate) -> String {
        let (y, m, day, s) = (d.year(), d.month(), d.day(), self.separator);
        match self.order {
            DateOrder::DayMonthYear => format!("{day:02}{s}{m:02}{s}{y}"),
            DateOrder::MonthDayYear => format!("{m:02}{s}{day:02}{s}{y}"),
            DateOrder::YearMonthDay => format!("{y}{s}{m:02}{s}{day:02}"),
        }
    }

    pub fn time(&self, t: NaiveTime) -> String {
        let (h, m) = (t.hour(), t.minute());
        if self.hour12 {
            let h12 = if h % 12 == 0 { 12 } else { h % 12 };
            let clock = format!("{h12}:{m:02}");
            if h < 12 {
                t!("time-am", time = clock)
            } else {
                t!("time-pm", time = clock)
            }
        } else {
            format!("{h:02}:{m:02}")
        }
    }
}

/// The system's format, read once.
pub fn system() -> LocaleFormat {
    static FORMAT: OnceLock<LocaleFormat> = OnceLock::new();
    *FORMAT.get_or_init(|| LocaleFormat::for_locale(&sys_locale::get_locale().unwrap_or_default()))
}

/// Milliseconds since the Unix epoch in local time; `None` for 0 and out-of-range values.
pub fn local(ms: i64) -> Option<DateTime<Local>> {
    (ms > 0)
        .then(|| DateTime::from_timestamp_millis(ms))
        .flatten()
        .map(|t| t.with_timezone(&Local))
}

/// The local calendar day, for date separators and grouping.
pub fn day(ms: i64) -> Option<NaiveDate> {
    local(ms).map(|t| t.date_naive())
}

/// Date and time, as next to an author's name.
pub fn full(ms: i64) -> String {
    let f = system();
    local(ms)
        .map(|t| {
            t!(
                "time-date-and-time",
                date = f.date(t.date_naive()),
                time = f.time(t.time())
            )
        })
        .unwrap_or_default()
}

/// The time alone, as beside grouped messages and in compact mode.
pub fn clock(ms: i64) -> String {
    local(ms)
        .map(|t| system().time(t.time()))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(locale: &str) -> String {
        let f = LocaleFormat::for_locale(locale);
        let d = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        let t = NaiveTime::from_hms_opt(23, 52, 0).unwrap();
        format!("{}, {}", f.date(d), f.time(t))
    }

    #[test]
    fn dates_and_times_follow_the_locale() {
        assert_eq!(fmt("en-GB"), "01/10/2026, 23:52");
        assert_eq!(fmt("en_US.UTF-8"), "10/01/2026, 11:52 PM");
        assert_eq!(fmt("en"), "10/01/2026, 11:52 PM");
        assert_eq!(fmt("nb-NO"), "01.10.2026, 23:52");
        assert_eq!(fmt("de_DE@euro"), "01.10.2026, 23:52");
        assert_eq!(fmt("sv-SE"), "2026-10-01, 23:52");
        assert_eq!(fmt("ja-JP"), "2026/10/01, 23:52");
        assert_eq!(fmt("fr-FR"), "01/10/2026, 23:52");
        assert_eq!(fmt("C"), "10/01/2026, 11:52 PM");
        assert_eq!(fmt(""), "10/01/2026, 11:52 PM");
        assert_eq!(fmt("zh-Hant-TW"), "2026/10/01, 11:52 PM");
        let f = LocaleFormat::for_locale("en-US");
        assert_eq!(
            f.time(NaiveTime::from_hms_opt(0, 5, 0).unwrap()),
            "12:05 AM"
        );
        assert_eq!(
            f.time(NaiveTime::from_hms_opt(12, 0, 0).unwrap()),
            "12:00 PM"
        );
    }
}
