//! A small ISO 8601 parser for the timestamps the API sends.

/// Parses an ISO 8601 / RFC 3339 timestamp such as `2026-11-10T08:00:00.000Z`
/// or `2026-11-10T09:00:00+01:00` into Unix milliseconds. Returns `None` for
/// anything else. Fractions beyond milliseconds are dropped.
pub fn parse_timestamp_ms(s: &str) -> Option<i64> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || !(b[10] == b'T' || b[10] == b' ') {
        return None;
    }
    if b[13] != b':' || b[16] != b':' {
        return None;
    }
    let num = |from: usize, to: usize| -> Option<i64> {
        let part = s.get(from..to)?;
        if part.bytes().all(|c| c.is_ascii_digit()) {
            part.parse().ok()
        } else {
            None
        }
    };
    let year = num(0, 4)?;
    let month = num(5, 7)?;
    let day = num(8, 10)?;
    let hour = num(11, 13)?;
    let minute = num(14, 16)?;
    let second = num(17, 19)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return None;
    }
    if second > 60 {
        return None;
    }
    let mut rest = &s[19..];
    let mut millis = 0i64;
    if let Some(frac) = rest.strip_prefix('.') {
        let digits = frac.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        let mut ms_text: String = frac[..digits.min(3)].to_owned();
        while ms_text.len() < 3 {
            ms_text.push('0');
        }
        millis = ms_text.parse().ok()?;
        rest = &frac[digits..];
    }
    let offset_secs = match rest {
        "Z" | "z" | "" => 0,
        tz if tz.len() == 6 && (tz.starts_with('+') || tz.starts_with('-')) => {
            let sign = if tz.starts_with('-') { -1 } else { 1 };
            if tz.as_bytes()[3] != b':' {
                return None;
            }
            let h: i64 = tz.get(1..3)?.parse().ok()?;
            let m: i64 = tz.get(4..6)?.parse().ok()?;
            if !(0..=23).contains(&h) || !(0..=59).contains(&m) {
                return None;
            }
            sign * (h * 3600 + m * 60)
        }
        _ => return None,
    };
    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3600 + minute * 60 + second - offset_secs;
    Some(secs * 1000 + millis)
}

/// Days since 1970-01-01 (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The current time in Unix milliseconds.
pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_forms() {
        assert_eq!(parse_timestamp_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_timestamp_ms("2026-11-10T08:00:00.000Z"),
            Some(1_794_297_600_000)
        );
        assert_eq!(
            parse_timestamp_ms("2026-11-10T09:00:00.5+01:00"),
            Some(1_794_297_600_500)
        );
        assert_eq!(
            parse_timestamp_ms("2026-11-10T08:00:00.123456+00:00"),
            Some(1_794_297_600_123)
        );
        assert_eq!(
            parse_timestamp_ms("2000-02-29T00:00:00Z"),
            Some(951_782_400_000)
        );
        assert_eq!(parse_timestamp_ms("not a time"), None);
        assert_eq!(parse_timestamp_ms("2026-13-10T08:00:00Z"), None);
        assert_eq!(parse_timestamp_ms("2026-11-10T08:00:00X"), None);
        assert_eq!(parse_timestamp_ms("2026-11-10T08:00:00.Z"), None);
    }
}
