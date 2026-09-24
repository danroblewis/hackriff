//! UTC civil-date helpers (no calendar crate): `YYYY-MM-DD` days and ISO-8601 `Z` instants to
//! [`Timestamp`]. Proleptic Gregorian, no leap seconds (Unix time), as feeds publish.

use hk_model::{TimeRange, Timestamp};

const NS_PER_S: i64 = 1_000_000_000;
const NS_PER_DAY: i64 = 86_400 * NS_PER_S;

/// Days from 1970-01-01 to `y-m-d` (H. Hinnant's `days_from_civil`).
pub(crate) fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = i64::from((m + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `y-m-d` from days since 1970-01-01 (H. Hinnant's `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (
        if m <= 2 {
            yoe + era * 400 + 1
        } else {
            yoe + era * 400
        },
        m,
        d,
    )
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        _ => 28,
    }
}

fn parse_date(s: &str) -> Option<(i64, u32, u32)> {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let digits = |r: std::ops::Range<usize>| -> Option<i64> {
        let part = &s[r];
        part.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| part.parse().ok())?
    };
    let (y, m, d) = (digits(0..4)?, digits(5..7)? as u32, digits(8..10)? as u32);
    ((1..=12).contains(&m) && d >= 1 && d <= days_in_month(y, m)).then_some((y, m, d))
}

/// Start of the UTC day `YYYY-MM-DD`.
pub fn day_start(date: &str) -> Option<Timestamp> {
    let (y, m, d) = parse_date(date)?;
    Some(Timestamp::from_unix_nanos(
        days_from_civil(y, m, d) * NS_PER_DAY,
    ))
}

/// The closed range covering UTC day `YYYY-MM-DD`: `[00:00:00, next midnight − 1 ns]`.
pub fn day_range(date: &str) -> Option<TimeRange> {
    let start = day_start(date)?;
    Some(TimeRange::new(
        start,
        start.saturating_add_nanos(NS_PER_DAY - 1),
    ))
}

/// `YYYY-MM-DD` of the UTC day containing `t`.
pub fn date_of(t: Timestamp) -> String {
    let (y, m, d) = civil_from_days(t.as_unix_nanos().div_euclid(NS_PER_DAY));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Parses `YYYY-MM-DDTHH:MM:SS[.fraction]Z` (UTC only; offsets are refused).
pub fn parse_utc(s: &str) -> Option<Timestamp> {
    let (date, rest) = s.split_once('T')?;
    let day = day_start(date)?.as_unix_nanos();
    let time = rest.strip_suffix('Z')?;
    let (hms, frac) = match time.split_once('.') {
        Some((hms, frac)) => (hms, frac),
        None => (time, ""),
    };
    let b = hms.as_bytes();
    if b.len() != 8 || b[2] != b':' || b[5] != b':' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let part = &hms[r];
        part.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| part.parse().ok())?
    };
    let (h, mi, sec) = (num(0..2)?, num(3..5)?, num(6..8)?);
    if h > 23 || mi > 59 || sec > 59 || frac.len() > 9 || !frac.bytes().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let frac_ns = if frac.is_empty() {
        0
    } else {
        frac.parse::<i64>().ok()? * 10i64.pow(9 - frac.len() as u32)
    };
    Some(Timestamp::from_unix_nanos(
        day + (h * 3600 + mi * 60 + sec) * NS_PER_S + frac_ns,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_round_trip_and_known_values() {
        assert_eq!(day_start("1970-01-01"), Some(Timestamp::UNIX_EPOCH));
        // 2026-09-13T00:00:00Z = 1_789_257_600 s.
        assert_eq!(
            day_start("2026-09-13").unwrap().as_unix_nanos(),
            1_789_257_600 * NS_PER_S
        );
        // Timestamp spans ±292 years (±106 751 days).
        for days in [-100_000i64, -1, 0, 59, 11_016, 20_709, 100_000] {
            let t = Timestamp::from_unix_nanos(days * NS_PER_DAY + 5);
            assert_eq!(
                day_start(&date_of(t)).unwrap().as_unix_nanos(),
                days * NS_PER_DAY
            );
        }
        assert_eq!(date_of(Timestamp::from_unix_nanos(-1)), "1969-12-31");
    }

    #[test]
    fn rejects_malformed() {
        for bad in [
            "2026-02-30",
            "2026-13-01",
            "2026-9-13",
            "20260913",
            "2026-09-13x",
        ] {
            assert_eq!(day_start(bad), None, "{bad}");
        }
        assert!(day_start("2024-02-29").is_some());
        assert!(day_start("2100-02-29").is_none());
        assert_eq!(parse_utc("2026-09-13T12:00:00+01:00"), None);
        assert_eq!(parse_utc("2026-09-13T24:00:00Z"), None);
    }

    #[test]
    fn instants_and_day_ranges() {
        let noon = parse_utc("2026-09-13T12:00:00Z").unwrap();
        assert_eq!(
            noon.as_unix_nanos() - day_start("2026-09-13").unwrap().as_unix_nanos(),
            12 * 3600 * NS_PER_S
        );
        assert_eq!(
            parse_utc("2026-09-13T12:00:00.25Z")
                .unwrap()
                .as_unix_nanos()
                - noon.as_unix_nanos(),
            250_000_000
        );
        let r = day_range("2026-09-13").unwrap();
        assert!(r.overlaps(&TimeRange::instant(noon)));
        assert!(!r.overlaps(&TimeRange::instant(day_start("2026-09-14").unwrap())));
        assert_eq!(r.duration_ns(), NS_PER_DAY - 1);
    }
}
