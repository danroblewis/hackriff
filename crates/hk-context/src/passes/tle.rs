//! Two-line element sets (NORAD TLE, the CelesTrak `FORMAT=tle` text): parsing and checksum.
//!
//! Only the fields SGP4 uses are kept. Every malformed line fails loudly with its line number (a
//! feed that changed format must not silently yield fewer satellites).

use hk_model::Timestamp;

use crate::feeds::ParseError;
use crate::utc::days_from_civil;

const NS_PER_DAY: f64 = 86_400e9;

/// One satellite's mean elements at epoch.
#[derive(Clone, Debug, PartialEq)]
pub struct Tle {
    /// Name line (3-line format), trimmed; the catalogue number as text when absent.
    pub name: String,
    /// NORAD catalogue number.
    pub norad_id: u32,
    /// Element epoch (UTC).
    pub epoch: Timestamp,
    /// B* drag term, 1/earth radii.
    pub bstar: f64,
    /// Inclination, degrees.
    pub inclination_deg: f64,
    /// Right ascension of the ascending node, degrees.
    pub raan_deg: f64,
    /// Eccentricity.
    pub eccentricity: f64,
    /// Argument of perigee, degrees.
    pub arg_perigee_deg: f64,
    /// Mean anomaly, degrees.
    pub mean_anomaly_deg: f64,
    /// Mean motion, revolutions per day (Kozai).
    pub mean_motion_rev_day: f64,
    /// Revolution number at epoch.
    pub rev_at_epoch: u32,
}

impl Tle {
    /// Element age at `t`, days (negative before the epoch).
    pub fn age_days(&self, t: Timestamp) -> f64 {
        (t.as_unix_nanos() - self.epoch.as_unix_nanos()) as f64 / NS_PER_DAY
    }
}

/// The modulo-10 checksum of a TLE line's first 68 columns: digits count their value, `-` counts
/// one, everything else zero.
pub fn checksum(line: &str) -> u32 {
    line.bytes()
        .take(68)
        .map(|b| match b {
            b'0'..=b'9' => u32::from(b - b'0'),
            b'-' => 1,
            _ => 0,
        })
        .sum::<u32>()
        % 10
}

fn err(line: usize, message: impl Into<String>) -> ParseError {
    ParseError {
        line,
        message: message.into(),
    }
}

fn field(text: &str, cols: std::ops::Range<usize>, line: usize) -> Result<&str, ParseError> {
    text.get(cols.clone()).map(str::trim).ok_or_else(|| {
        err(
            line,
            format!("columns {}..{} missing", cols.start + 1, cols.end),
        )
    })
}

fn num<T: std::str::FromStr>(
    text: &str,
    cols: std::ops::Range<usize>,
    line: usize,
    what: &str,
) -> Result<T, ParseError> {
    let f = field(text, cols, line)?;
    f.parse()
        .map_err(|_| err(line, format!("{what}: {f:?} is not a number")))
}

/// Parses the TLE "assumed decimal point" exponent form, e.g. ` 28098-4` = 0.28098e-4.
fn implied_exp(text: &str, line: usize, what: &str) -> Result<f64, ParseError> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(0.0);
    }
    let bad = || err(line, format!("{what}: {t:?} is not a TLE exponent field"));
    let (sign, rest) = match t.as_bytes()[0] {
        b'-' => (-1.0, &t[1..]),
        b'+' => (1.0, &t[1..]),
        _ => (1.0, t),
    };
    let split = rest.rfind(['-', '+']).ok_or_else(bad)?;
    let (mantissa, exp) = rest.split_at(split);
    let mantissa: f64 = format!("0.{mantissa}").parse().map_err(|_| bad())?;
    let exp: i32 = exp.parse().map_err(|_| bad())?;
    Ok(sign * mantissa * 10f64.powi(exp))
}

fn check_line(text: &str, number: char, line: usize) -> Result<(), ParseError> {
    if text.len() < 69 || !text.is_ascii() {
        return Err(err(
            line,
            format!("TLE line {number} must be 69 ASCII columns"),
        ));
    }
    if !text.starts_with(number) || text.as_bytes()[1] != b' ' {
        return Err(err(line, format!("expected TLE line {number}")));
    }
    let stated = text.as_bytes()[68];
    if !stated.is_ascii_digit() || u32::from(stated - b'0') != checksum(text) {
        return Err(err(
            line,
            format!("checksum mismatch (computed {})", checksum(text)),
        ));
    }
    Ok(())
}

/// Parses one element set from its two lines (`line_no` is the 1-based number of line 1, for
/// errors).
pub fn parse_pair(
    name: Option<&str>,
    l1: &str,
    l2: &str,
    line_no: usize,
) -> Result<Tle, ParseError> {
    let (n1, n2) = (line_no, line_no + 1);
    check_line(l1, '1', n1)?;
    check_line(l2, '2', n2)?;
    let norad_id: u32 = num(l1, 2..7, n1, "catalogue number")?;
    let norad2: u32 = num(l2, 2..7, n2, "catalogue number")?;
    if norad_id != norad2 {
        return Err(err(
            n2,
            format!("catalogue number {norad2} ≠ line 1's {norad_id}"),
        ));
    }
    let yy: i64 = num(l1, 18..20, n1, "epoch year")?;
    let doy: f64 = num(l1, 20..32, n1, "epoch day")?;
    if !(1.0..367.0).contains(&doy) {
        return Err(err(n1, format!("epoch day {doy} out of range")));
    }
    let year = if yy < 57 { 2000 + yy } else { 1900 + yy };
    let epoch_ns = days_from_civil(year, 1, 1) as f64 * NS_PER_DAY + (doy - 1.0) * NS_PER_DAY;
    let bstar = implied_exp(field(l1, 53..61, n1)?, n1, "B*")?;
    let eccentricity: f64 = format!("0.{}", field(l2, 26..33, n2)?)
        .parse()
        .map_err(|_| err(n2, "eccentricity is not a number"))?;
    let tle = Tle {
        name: name
            .map(|n| n.trim().trim_start_matches("0 ").trim().to_owned())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| norad_id.to_string()),
        norad_id,
        epoch: Timestamp::from_unix_nanos(epoch_ns.round() as i64),
        bstar,
        inclination_deg: num(l2, 8..16, n2, "inclination")?,
        raan_deg: num(l2, 17..25, n2, "RAAN")?,
        eccentricity,
        arg_perigee_deg: num(l2, 34..42, n2, "argument of perigee")?,
        mean_anomaly_deg: num(l2, 43..51, n2, "mean anomaly")?,
        mean_motion_rev_day: num(l2, 52..63, n2, "mean motion")?,
        rev_at_epoch: field(l2, 63..68, n2)?.parse().unwrap_or(0),
    };
    if !(tle.mean_motion_rev_day > 0.0 && tle.eccentricity < 1.0) {
        return Err(err(n2, "not a bound orbit"));
    }
    Ok(tle)
}

/// Parses a TLE set: 2-line or 3-line (name line first) records, blank lines ignored.
pub fn parse_set(body: &str) -> Result<Vec<Tle>, ParseError> {
    let lines: Vec<(usize, &str)> = body
        .lines()
        .enumerate()
        .map(|(i, l)| (i + 1, l.trim_end()))
        .filter(|(_, l)| !l.trim().is_empty())
        .collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let (n, l) = lines[i];
        let (name, first) = if l.starts_with("1 ") {
            (None, i)
        } else {
            (Some(l), i + 1)
        };
        let (Some(&(n1, l1)), Some(&(_, l2))) = (lines.get(first), lines.get(first + 1)) else {
            return Err(err(n, "truncated element set"));
        };
        out.push(parse_pair(name, l1, l2, n1)?);
        i = first + 2;
    }
    if out.is_empty() {
        return Err(err(0, "no element sets"));
    }
    Ok(out)
}
