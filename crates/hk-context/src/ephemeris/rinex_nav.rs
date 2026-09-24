//! GPS LNAV ephemerides from a RINEX navigation file (versions 2.x and 3.x).
//!
//! One parser serves both sides of the SIGNAL-032 comparison: the reference IGS merged broadcast
//! file (`BRDC00IGS_R_…_MN.rnx`, or the legacy `brdcDDD0.YYn`) and the device's own navigation
//! log (GNSS-SDR writes RINEX nav). Records of other constellations in a mixed 3.x file are
//! skipped and counted, never guessed at. RINEX 4 (`> EPH` frames) is refused loudly, as is any
//! malformed GPS record — a schema change must not read as "no ephemerides".
//!
//! Layout (RINEX 3.04 Table A4; 2.11 Table A4): an epoch line with the SV, the time of clock and
//! `af0 af1 af2`, then seven "broadcast orbit" lines of four `D19.12` fields each. Version 3 puts
//! the fields at column 23 / 4, version 2 at 22 / 3.

use crate::feeds::ParseError;

use super::GpsTime;

/// One GPS LNAV ephemeris (IS-GPS-200 subframes 1–3), in RINEX units: seconds, metres, radians,
/// radians per second.
#[derive(Clone, Debug, PartialEq)]
pub struct GpsEphemeris {
    /// PRN, 1–32 (up to 63 accepted).
    pub prn: u8,
    /// Time of clock.
    pub toc: GpsTime,
    /// Clock bias, s.
    pub af0: f64,
    /// Clock drift, s/s.
    pub af1: f64,
    /// Clock drift rate, s/s².
    pub af2: f64,
    /// Issue of data, ephemeris.
    pub iode: f64,
    /// Orbit-radius sine correction, m.
    pub crs: f64,
    /// Mean-motion difference, rad/s.
    pub delta_n: f64,
    /// Mean anomaly at reference time, rad.
    pub m0: f64,
    /// Argument-of-latitude cosine correction, rad.
    pub cuc: f64,
    /// Eccentricity.
    pub e: f64,
    /// Argument-of-latitude sine correction, rad.
    pub cus: f64,
    /// √A, √m.
    pub sqrt_a: f64,
    /// Time of ephemeris, seconds of `week`.
    pub toe_sow: f64,
    /// Inclination cosine correction, rad.
    pub cic: f64,
    /// Longitude of ascending node at weekly epoch, rad.
    pub omega0: f64,
    /// Inclination sine correction, rad.
    pub cis: f64,
    /// Inclination at reference time, rad.
    pub i0: f64,
    /// Orbit-radius cosine correction, m.
    pub crc: f64,
    /// Argument of perigee, rad.
    pub omega: f64,
    /// Rate of right ascension, rad/s.
    pub omega_dot: f64,
    /// Rate of inclination, rad/s.
    pub idot: f64,
    /// GPS week of `toe_sow` (continuous).
    pub week: u32,
    /// SV accuracy, m.
    pub sv_accuracy_m: f64,
    /// SV health (0 = healthy).
    pub health: u32,
    /// Group delay, s.
    pub tgd: f64,
    /// Issue of data, clock.
    pub iodc: f64,
    /// Curve-fit interval, hours (4 when the file leaves it blank or zero).
    pub fit_interval_h: f64,
}

impl GpsEphemeris {
    /// Time of ephemeris.
    pub fn toe(&self) -> GpsTime {
        GpsTime::from_week_sow(self.week, self.toe_sow)
    }

    /// The span this ephemeris is meant to be used over: `toe ± fit/2`.
    pub fn fit_span(&self) -> (GpsTime, GpsTime) {
        let half = self.fit_interval_h * 1800.0;
        let toe = self.toe();
        (toe.plus_s(-half), toe.plus_s(half))
    }

    /// Whether `t` lies inside [`Self::fit_span`].
    pub fn valid_at(&self, t: GpsTime) -> bool {
        let (a, b) = self.fit_span();
        t >= a && t <= b
    }
}

/// A parsed navigation file.
#[derive(Clone, Debug, PartialEq)]
pub struct NavFile {
    /// RINEX version, e.g. 3.04.
    pub version: f64,
    /// GPS ephemerides in file order.
    pub ephemerides: Vec<GpsEphemeris>,
    /// Records of other constellations skipped.
    pub skipped_other_systems: usize,
}

fn err(line: usize, message: impl Into<String>) -> ParseError {
    ParseError {
        line,
        message: message.into(),
    }
}

/// A `D19.12` field at `start`, or `None` if the line ends before it or it is blank.
fn opt_field(text: &str, start: usize, line: usize) -> Result<Option<f64>, ParseError> {
    let end = (start + 19).min(text.len());
    if start >= text.len() {
        return Ok(None);
    }
    let raw = text
        .get(start..end)
        .ok_or_else(|| err(line, "non-ASCII text in a numeric field"))?
        .trim();
    if raw.is_empty() {
        return Ok(None);
    }
    raw.replace(['D', 'd'], "E")
        .parse::<f64>()
        .map(Some)
        .map_err(|_| err(line, format!("not a number: {raw:?}")))
}

fn field(text: &str, start: usize, line: usize, name: &str) -> Result<f64, ParseError> {
    opt_field(text, start, line)?.ok_or_else(|| err(line, format!("missing {name}")))
}

fn int(s: &str, line: usize, what: &str) -> Result<i64, ParseError> {
    s.trim()
        .parse()
        .map_err(|_| err(line, format!("bad {what}: {s:?}")))
}

/// Lines per navigation record in RINEX 3, by system letter.
fn record_lines(system: char) -> Option<usize> {
    match system {
        'G' | 'E' | 'C' | 'J' | 'I' => Some(8),
        'R' | 'S' => Some(4),
        _ => None,
    }
}

/// Parses a RINEX 2.x (GPS `N`) or 3.x navigation file. Fails loudly, naming the line, on any
/// malformed GPS record, on RINEX 4, or when the file holds no GPS ephemeris at all.
pub fn parse_nav(text: &str) -> Result<NavFile, ParseError> {
    let lines: Vec<&str> = text.lines().collect();
    let first = lines
        .first()
        .ok_or_else(|| err(0, "empty navigation file"))?;
    if !first.contains("RINEX VERSION / TYPE") {
        return Err(err(1, "first line is not RINEX VERSION / TYPE"));
    }
    let version: f64 = first
        .get(..9)
        .and_then(|s| s.trim().parse().ok())
        .ok_or_else(|| err(1, "unreadable RINEX version"))?;
    let file_type = first.chars().nth(20).unwrap_or(' ');
    if file_type != 'N' {
        return Err(err(
            1,
            format!("not a navigation file (type {file_type:?})"),
        ));
    }
    let v3 = match version.floor() as i64 {
        2 => false,
        3 => true,
        v => return Err(err(1, format!("RINEX {v} navigation is not supported"))),
    };
    if !v3
        && first
            .chars()
            .nth(40)
            .is_some_and(|c| !matches!(c, ' ' | 'G'))
    {
        return Err(err(1, "RINEX 2 navigation file is not GPS"));
    }
    let header_end = lines
        .iter()
        .position(|l| l.contains("END OF HEADER"))
        .ok_or_else(|| err(0, "no END OF HEADER"))?;

    let (epoch_col, orbit_col) = if v3 { (23, 4) } else { (22, 3) };
    let mut ephemerides = Vec::new();
    let mut skipped = 0usize;
    let mut i = header_end + 1;
    while i < lines.len() {
        let head = lines[i];
        let n = i + 1; // 1-based line number
        if head.trim().is_empty() {
            i += 1;
            continue;
        }
        let (prn, date): (i64, [&str; 6]);
        if v3 {
            let system = head.chars().next().unwrap_or(' ');
            let count =
                record_lines(system).ok_or_else(|| err(n, format!("unknown system {system:?}")))?;
            if system != 'G' {
                skipped += 1;
                i += count;
                continue;
            }
            let g = |r: std::ops::Range<usize>| {
                head.get(r)
                    .ok_or_else(|| err(n, "truncated GPS epoch line"))
            };
            prn = int(g(1..3)?, n, "PRN")?;
            date = [
                g(4..8)?,
                g(9..11)?,
                g(12..14)?,
                g(15..17)?,
                g(18..20)?,
                g(21..23)?,
            ];
        } else {
            let g = |r: std::ops::Range<usize>| {
                head.get(r)
                    .ok_or_else(|| err(n, "truncated GPS epoch line"))
            };
            prn = int(g(0..2)?, n, "PRN")?;
            date = [
                g(2..5)?,
                g(5..8)?,
                g(8..11)?,
                g(11..14)?,
                g(14..17)?,
                g(17..22)?,
            ];
        }
        if !(1..=63).contains(&prn) {
            return Err(err(n, format!("PRN {prn} out of range")));
        }
        let mut y = int(date[0], n, "year")?;
        if !v3 {
            y += if y < 80 { 2000 } else { 1900 };
        }
        let sec: f64 = date[5]
            .trim()
            .parse()
            .map_err(|_| err(n, format!("bad seconds {:?}", date[5])))?;
        let toc = GpsTime::from_calendar(
            y,
            int(date[1], n, "month")? as u32,
            int(date[2], n, "day")? as u32,
            int(date[3], n, "hour")? as u32,
            int(date[4], n, "minute")? as u32,
            sec,
        )
        .ok_or_else(|| err(n, "invalid time of clock"))?;
        if i + 7 >= lines.len() {
            return Err(err(n, "GPS record truncated (needs 8 lines)"));
        }
        let o = |k: usize, f: usize, name: &str| -> Result<f64, ParseError> {
            field(lines[i + k], orbit_col + 19 * f, i + k + 1, name)
        };
        let fit = opt_field(lines[i + 7], orbit_col + 19, i + 8)?;
        let week = o(5, 2, "GPS week")?;
        let health = o(6, 1, "SV health")?;
        let eph = GpsEphemeris {
            prn: prn as u8,
            toc,
            af0: field(head, epoch_col, n, "af0")?,
            af1: field(head, epoch_col + 19, n, "af1")?,
            af2: field(head, epoch_col + 38, n, "af2")?,
            iode: o(1, 0, "IODE")?,
            crs: o(1, 1, "Crs")?,
            delta_n: o(1, 2, "Delta n")?,
            m0: o(1, 3, "M0")?,
            cuc: o(2, 0, "Cuc")?,
            e: o(2, 1, "e")?,
            cus: o(2, 2, "Cus")?,
            sqrt_a: o(2, 3, "sqrt(A)")?,
            toe_sow: o(3, 0, "Toe")?,
            cic: o(3, 1, "Cic")?,
            omega0: o(3, 2, "OMEGA0")?,
            cis: o(3, 3, "Cis")?,
            i0: o(4, 0, "i0")?,
            crc: o(4, 1, "Crc")?,
            omega: o(4, 2, "omega")?,
            omega_dot: o(4, 3, "OMEGA DOT")?,
            idot: o(5, 0, "IDOT")?,
            week: week as u32,
            sv_accuracy_m: o(6, 0, "SV accuracy")?,
            health: health as u32,
            tgd: o(6, 2, "TGD")?,
            iodc: o(6, 3, "IODC")?,
            fit_interval_h: fit.filter(|f| *f > 0.0).unwrap_or(4.0),
        };
        if !(0.0..1.0).contains(&eph.e) {
            return Err(err(i + 3, format!("eccentricity {} outside [0, 1)", eph.e)));
        }
        if eph.sqrt_a <= 0.0 {
            return Err(err(i + 3, format!("sqrt(A) {} not positive", eph.sqrt_a)));
        }
        if !(0.0..SECONDS_PER_WEEK_F).contains(&eph.toe_sow) || week < 0.0 {
            return Err(err(i + 4, "Toe/week out of range"));
        }
        ephemerides.push(eph);
        i += 8;
    }
    if ephemerides.is_empty() {
        return Err(err(0, "no GPS ephemerides in navigation file"));
    }
    Ok(NavFile {
        version,
        ephemerides,
        skipped_other_systems: skipped,
    })
}

const SECONDS_PER_WEEK_F: f64 = super::SECONDS_PER_WEEK;

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A RINEX 3.04 record in the IGS BRDC layout (values shaped like a real GPS ephemeris, hand
    /// written), plus a GLONASS record that must be skipped.
    pub(crate) const RINEX3: &str =
        "     3.04           N: GNSS NAV DATA    M: MIXED            RINEX VERSION / TYPE
hackriff test                           20260920 120000 UTC PGM / RUN BY / DATE
    18                                                      LEAP SECONDS
                                                            END OF HEADER
G05 2026 09 20 12 00 00-1.270929351449D-04-1.477928890381D-12 0.000000000000D+00
     5.500000000000D+01-8.534375000000D+01 4.604834935628D-09 1.843727387455D+00
    -4.425644874573D-06 5.887625692412D-03 7.869675755501D-06 5.153630380630D+03
     4.320000000000D+04 1.303851604462D-08-2.861219474458D+00 4.470348358154D-08
     9.600520455017D-01 2.260312500000D+02 1.235234890091D+00-8.105694256043D-09
    -4.807343405207D-10 1.000000000000D+00 2.437000000000D+03 0.000000000000D+00
     2.000000000000D+00 0.000000000000D+00-1.071020960808D-08 5.500000000000D+01
     3.600180000000D+04 4.000000000000D+00
R01 2026 09 20 11 45 00 1.234039664268D-05 0.000000000000D+00 3.564000000000D+04
     1.068730322266D+04-2.069183349609D+00 1.862645149231D-09 0.000000000000D+00
    -1.106225390625D+04 1.024770736694D+00 0.000000000000D+00 1.000000000000D+00
     1.957811767578D+04 2.466039657593D+00-2.793967723846D-09 0.000000000000D+00
";

    #[test]
    fn parses_a_rinex3_gps_record_and_skips_glonass() {
        let nav = parse_nav(RINEX3).unwrap();
        assert_eq!(nav.skipped_other_systems, 1);
        assert_eq!(nav.ephemerides.len(), 1);
        let e = &nav.ephemerides[0];
        assert_eq!(e.prn, 5);
        assert_eq!(e.week, 2437);
        assert_eq!(e.toe_sow, 43_200.0);
        assert_eq!(
            e.toc,
            e.toe(),
            "toc 12:00 on the week's first day = toe 43200"
        );
        assert!((e.sqrt_a - 5153.630380630).abs() < 1e-9);
        assert!((e.af0 + 1.270929351449e-4).abs() < 1e-16);
        assert_eq!(e.fit_interval_h, 4.0);
        assert_eq!(e.iodc, 55.0);
        assert_eq!(e.health, 0);
    }

    #[test]
    fn parses_rinex2_gps() {
        let text =
            "     2.11           N: GPS NAV DATA                         RINEX VERSION / TYPE
                                                            END OF HEADER
 5 26  9 20 12  0  0.0-1.270929351449D-04-1.477928890381D-12 0.000000000000D+00
    5.500000000000D+01-8.534375000000D+01 4.604834935628D-09 1.843727387455D+00
   -4.425644874573D-06 5.887625692412D-03 7.869675755501D-06 5.153630380630D+03
    4.320000000000D+04 1.303851604462D-08-2.861219474458D+00 4.470348358154D-08
    9.600520455017D-01 2.260312500000D+02 1.235234890091D+00-8.105694256043D-09
   -4.807343405207D-10 1.000000000000D+00 2.437000000000D+03 0.000000000000D+00
    2.000000000000D+00 0.000000000000D+00-1.071020960808D-08 5.500000000000D+01
    3.600180000000D+04
";
        let v2 = parse_nav(text).unwrap();
        let v3 = parse_nav(RINEX3).unwrap();
        assert_eq!(
            v2.ephemerides, v3.ephemerides,
            "same record, either version"
        );
    }

    #[test]
    fn schema_changes_fail_loudly() {
        let rinex4 = RINEX3.replacen("     3.04", "     4.00", 1);
        assert!(parse_nav(&rinex4).unwrap_err().message.contains("RINEX 4"));
        let garbled = RINEX3.replacen("5.153630380630D+03", "5.15363038063XD+03", 1);
        let e = parse_nav(&garbled).unwrap_err();
        assert_eq!(e.line, 7, "names the sqrt(A) line: {e}");
        let truncated: String = RINEX3.lines().take(8).collect::<Vec<_>>().join("\n");
        assert!(
            parse_nav(&truncated)
                .unwrap_err()
                .message
                .contains("truncated")
        );
        let only_glonass: String = RINEX3
            .lines()
            .enumerate()
            .filter(|(i, _)| *i < 4 || *i >= 12)
            .map(|(_, l)| format!("{l}\n"))
            .collect();
        assert!(
            parse_nav(&only_glonass)
                .unwrap_err()
                .message
                .contains("no GPS ephemerides"),
            "a file with no GPS records is not an empty answer"
        );
    }
}
