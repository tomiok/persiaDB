//! `datetime` values: microseconds since the Unix epoch, UTC, and their RFC 3339 form (SPEC §6.4.1).

const MICROS_PER_SECOND: i64 = 1_000_000;
const SECONDS_PER_DAY: i64 = 86_400;
const MICROS_PER_DAY: i64 = SECONDS_PER_DAY * MICROS_PER_SECOND;

/// The earliest `datetime`, `0000-01-01T00:00:00Z`, in microseconds since the epoch.
pub const MIN_DATETIME: i64 = days_from_civil(0, 1, 1) * MICROS_PER_DAY;
/// The latest `datetime`, `9999-12-31T23:59:59.999999Z`, in microseconds since the epoch.
pub const MAX_DATETIME: i64 = (days_from_civil(9999, 12, 31) + 1) * MICROS_PER_DAY - 1;

/// The range SPEC §6.4.1 allows, for error messages.
pub(super) const RANGE: &str = "0000-01-01T00:00:00Z..=9999-12-31T23:59:59.999999Z";

pub(super) const fn in_range(micros: i64) -> bool {
    MIN_DATETIME <= micros && micros <= MAX_DATETIME
}

/// Days from 1970-01-01 to the given proleptic Gregorian date (H. Hinnant's `days_from_civil`).
const fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The date `days` after 1970-01-01, as `(year, month, day)`.
const fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

const fn is_leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

const fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if is_leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// `YYYY-MM-DDTHH:MM:SS[.f{1,6}](Z|±hh:mm)` (RFC 3339 §5.6; `t` and `z` accepted too). The result may be outside
/// the `datetime` range: validation reports that, as for the integer form.
pub(super) fn parse(text: &str) -> Option<i64> {
    let mut p = Parser(text.as_bytes());
    let year = p.digits(4)?;
    p.byte(b'-')?;
    let month = p.digits(2)?;
    p.byte(b'-')?;
    let day = p.digits(2)?;
    p.one_of(b"Tt")?;
    let hour = p.digits(2)?;
    p.byte(b':')?;
    let minute = p.digits(2)?;
    p.byte(b':')?;
    let second = p.digits(2)?;
    let mut micros = 0;
    if p.byte(b'.').is_some() {
        let mut scale = 100_000;
        let mut count = 0;
        while let Some(d) = p.digit() {
            count += 1;
            if count > 6 {
                return None;
            }
            micros += d * scale;
            scale /= 10;
        }
        if count == 0 {
            return None;
        }
    }
    let offset_minutes = if p.one_of(b"Zz").is_some() {
        0
    } else {
        let sign = if p.byte(b'+').is_some() {
            1
        } else {
            p.byte(b'-')?;
            -1
        };
        let h = p.digits(2)?;
        p.byte(b':')?;
        let m = p.digits(2)?;
        if h > 23 || m > 59 {
            return None;
        }
        sign * (h * 60 + m)
    };
    let valid = p.0.is_empty()
        && (1..=12).contains(&month)
        && (1..=days_in_month(year, month)).contains(&day)
        && hour <= 23
        && minute <= 59
        && second <= 59;
    if !valid {
        return None;
    }
    let seconds =
        days_from_civil(year, month, day) * SECONDS_PER_DAY + hour * 3600 + minute * 60 + second
            - offset_minutes * 60;
    let micros = seconds * MICROS_PER_SECOND + micros;
    Some(micros)
}

/// The RFC 3339 UTC form SPEC §6.4.1 writes, or `None` outside the range.
pub(super) fn format(micros: i64) -> Option<String> {
    if !in_range(micros) {
        return None;
    }
    let days = micros.div_euclid(MICROS_PER_DAY);
    let in_day = micros.rem_euclid(MICROS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    let second_of_day = in_day / MICROS_PER_SECOND;
    let fraction = in_day % MICROS_PER_SECOND;
    let base = format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}",
        second_of_day / 3600,
        second_of_day / 60 % 60,
        second_of_day % 60
    );
    Some(if fraction == 0 {
        base + "Z"
    } else {
        format!("{base}.{fraction:06}Z")
    })
}

struct Parser<'a>(&'a [u8]);

impl Parser<'_> {
    fn digit(&mut self) -> Option<i64> {
        let (&b, rest) = self.0.split_first()?;
        if !b.is_ascii_digit() {
            return None;
        }
        self.0 = rest;
        Some(i64::from(b - b'0'))
    }

    fn digits(&mut self, count: usize) -> Option<i64> {
        let mut value = 0;
        for _ in 0..count {
            value = value * 10 + self.digit()?;
        }
        Some(value)
    }

    fn byte(&mut self, expected: u8) -> Option<()> {
        self.one_of(&[expected])
    }

    fn one_of(&mut self, allowed: &[u8]) -> Option<()> {
        let (b, rest) = self.0.split_first()?;
        allowed.contains(b).then(|| self.0 = rest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_ends() {
        assert_eq!(
            format(MIN_DATETIME).as_deref(),
            Some("0000-01-01T00:00:00Z")
        );
        assert_eq!(
            format(MAX_DATETIME).as_deref(),
            Some("9999-12-31T23:59:59.999999Z")
        );
        assert_eq!(format(MIN_DATETIME - 1), None);
        assert_eq!(format(MAX_DATETIME + 1), None);
        assert_eq!(parse("0000-01-01T00:00:00Z"), Some(MIN_DATETIME));
        assert_eq!(parse("9999-12-31T23:59:59.999999Z"), Some(MAX_DATETIME));
        assert_eq!(
            parse("0000-01-01T00:00:00+00:01"),
            Some(MIN_DATETIME - 60_000_000)
        );
        assert_eq!(
            parse("9999-12-31T23:59:59-00:01"),
            Some(MAX_DATETIME + 1 + 59_000_000)
        );
    }

    #[test]
    fn known_values() {
        assert_eq!(parse("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse("1970-01-01T00:00:00.000001z"), Some(1));
        assert_eq!(parse("1969-12-31t23:59:59.9Z"), Some(-100_000));
        assert_eq!(
            parse("2024-02-29T12:00:00+02:30"),
            parse("2024-02-29T09:30:00Z")
        );
        assert_eq!(
            parse("2001-09-09T01:46:40Z"),
            Some(1_000_000_000 * MICROS_PER_SECOND)
        );
        assert_eq!(format(-1).as_deref(), Some("1969-12-31T23:59:59.999999Z"));
        assert_eq!(
            format(1_000_000_000 * MICROS_PER_SECOND).as_deref(),
            Some("2001-09-09T01:46:40Z")
        );
        assert_eq!(
            format(1_500).as_deref(),
            Some("1970-01-01T00:00:00.001500Z")
        );
    }

    #[test]
    fn rejects_what_rfc_3339_or_the_spec_does_not_allow() {
        for bad in [
            "",
            "2024-01-01",
            "2024-01-01T00:00:00",
            "2024-01-01 00:00:00Z",
            "2024-1-01T00:00:00Z",
            "2023-02-29T00:00:00Z",
            "1900-02-29T00:00:00Z",
            "2024-04-31T00:00:00Z",
            "2024-13-01T00:00:00Z",
            "2024-00-01T00:00:00Z",
            "2024-01-00T00:00:00Z",
            "2024-01-01T24:00:00Z",
            "2024-01-01T00:60:00Z",
            "2024-12-31T23:59:60Z",
            "2024-01-01T00:00:00.Z",
            "2024-01-01T00:00:00.1234567Z",
            "2024-01-01T00:00:00+24:00",
            "2024-01-01T00:00:00+00:60",
            "2024-01-01T00:00:00+0000",
            "2024-01-01T00:00:00Z ",
            "+2024-01-01T00:00:00Z",
            "２０２４-01-01T00:00:00Z",
        ] {
            assert_eq!(parse(bad), None, "{bad:?}");
        }
        assert!(parse("2000-02-29T00:00:00Z").is_some());
    }

    #[test]
    fn format_parses_back_across_the_range() {
        let step = (MAX_DATETIME - MIN_DATETIME) / 100_003;
        for i in 0..=100_003 {
            let v = (MIN_DATETIME + i * step + i % 1_000_000).min(MAX_DATETIME);
            assert_eq!(format(v).and_then(|s| parse(&s)), Some(v), "{v}");
        }
    }
}
