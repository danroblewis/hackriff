//! Precise GPS orbits and clocks from an SP3-c / SP3-d file (IGS/NGS format).
//!
//! Positions are Earth-fixed (the IGS realisation of ITRF), km in the file, **centre of mass**;
//! clocks are µs, from ionosphere-free observables. `0.000000` coordinates and a `999999.999999`
//! clock mark a bad or absent value and are kept as `None`, never as a zero. Only GPS (`G`)
//! records are kept; other constellations are counted. Only the GPS time system is accepted —
//! a UTC- or GLONASS-time file would shift every epoch and is refused rather than mis-joined.

use crate::feeds::ParseError;

use super::GpsTime;

/// One satellite at one epoch.
#[derive(Clone, Debug, PartialEq)]
pub struct Sp3Record {
    /// GPS PRN.
    pub prn: u8,
    /// Position, m; `None` if the file marks it bad.
    pub ecef_m: Option<[f64; 3]>,
    /// Clock offset, s; `None` if the file marks it bad or absent.
    pub clock_s: Option<f64>,
}

/// One epoch.
#[derive(Clone, Debug, PartialEq)]
pub struct Sp3Epoch {
    /// Epoch (GPS time).
    pub t: GpsTime,
    /// GPS satellites at it.
    pub records: Vec<Sp3Record>,
}

/// A parsed SP3 file.
#[derive(Clone, Debug, PartialEq)]
pub struct Sp3 {
    /// Format version letter (`c` or `d`).
    pub version: char,
    /// Epochs in file order.
    pub epochs: Vec<Sp3Epoch>,
    /// Non-GPS position records skipped.
    pub skipped_other_systems: usize,
}

impl Sp3 {
    /// First and last epoch.
    pub fn span(&self) -> (GpsTime, GpsTime) {
        (
            self.epochs.first().expect("non-empty").t,
            self.epochs.last().expect("non-empty").t,
        )
    }

    /// The record for `prn` at each epoch in `[from, to]` that has a usable position.
    pub fn samples(&self, prn: u8, from: GpsTime, to: GpsTime) -> Vec<(GpsTime, &Sp3Record)> {
        self.epochs
            .iter()
            .filter(|e| e.t >= from && e.t <= to)
            .filter_map(|e| {
                e.records
                    .iter()
                    .find(|r| r.prn == prn && r.ecef_m.is_some())
                    .map(|r| (e.t, r))
            })
            .collect()
    }
}

fn err(line: usize, message: impl Into<String>) -> ParseError {
    ParseError {
        line,
        message: message.into(),
    }
}

fn num(text: &str, r: std::ops::Range<usize>, line: usize, what: &str) -> Result<f64, ParseError> {
    let raw = text
        .get(r)
        .ok_or_else(|| err(line, format!("truncated before {what}")))?
        .trim();
    raw.parse()
        .map_err(|_| err(line, format!("bad {what}: {raw:?}")))
}

/// Parses an SP3-c/d file. Fails loudly, naming the line, on another version, a non-GPS time
/// system, a malformed epoch or position line, or a file with no epochs.
pub fn parse_sp3(text: &str) -> Result<Sp3, ParseError> {
    let mut lines = text.lines().enumerate().map(|(i, l)| (i + 1, l));
    let (_, first) = lines.next().ok_or_else(|| err(0, "empty SP3 file"))?;
    let mut chars = first.chars();
    if chars.next() != Some('#') {
        return Err(err(1, "not an SP3 file (no leading '#')"));
    }
    let version = chars.next().unwrap_or(' ');
    if !matches!(version, 'c' | 'd') {
        return Err(err(
            1,
            format!("SP3 version {version:?} not supported (c, d)"),
        ));
    }
    let mut epochs: Vec<Sp3Epoch> = Vec::new();
    let mut skipped = 0usize;
    let mut time_system_seen = false;
    for (n, line) in lines {
        if line.starts_with("EOF") {
            break;
        }
        if let Some(rest) = line.strip_prefix("%c") {
            if !time_system_seen {
                time_system_seen = true;
                let ts = rest.get(7..10).unwrap_or("").trim();
                if !matches!(ts, "GPS" | "ccc" | "") {
                    return Err(err(n, format!("time system {ts} is not GPS")));
                }
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix('*') {
            let f: Vec<&str> = rest.split_whitespace().collect();
            if f.len() < 6 {
                return Err(err(n, "short epoch line"));
            }
            let p = |i: usize, what: &str| -> Result<i64, ParseError> {
                f[i].parse().map_err(|_| err(n, format!("bad {what}")))
            };
            let s: f64 = f[5].parse().map_err(|_| err(n, "bad seconds"))?;
            let t = GpsTime::from_calendar(
                p(0, "year")?,
                p(1, "month")? as u32,
                p(2, "day")? as u32,
                p(3, "hour")? as u32,
                p(4, "minute")? as u32,
                s,
            )
            .ok_or_else(|| err(n, "invalid epoch"))?;
            if epochs.last().is_some_and(|e| e.t >= t) {
                return Err(err(n, "epochs out of order"));
            }
            epochs.push(Sp3Epoch {
                t,
                records: Vec::new(),
            });
            continue;
        }
        if line.starts_with('P') {
            let epoch = epochs
                .last_mut()
                .ok_or_else(|| err(n, "position record before the first epoch"))?;
            let sat = line
                .get(1..4)
                .ok_or_else(|| err(n, "short position line"))?;
            let (system, id) = sat.split_at(1);
            if !matches!(system, "G" | " ") {
                skipped += 1;
                continue;
            }
            let prn: u8 = id
                .trim()
                .parse()
                .map_err(|_| err(n, format!("bad satellite id {sat:?}")))?;
            let x = num(line, 4..18, n, "x")?;
            let y = num(line, 18..32, n, "y")?;
            let z = num(line, 32..46, n, "z")?;
            let clk = match line.get(46..60) {
                Some(s) if !s.trim().is_empty() => Some(num(line, 46..60, n, "clock")?),
                _ => None,
            };
            epoch.records.push(Sp3Record {
                prn,
                ecef_m: (x != 0.0 || y != 0.0 || z != 0.0).then_some([x * 1e3, y * 1e3, z * 1e3]),
                clock_s: clk.filter(|c| c.abs() < 999_999.0).map(|c| c * 1e-6),
            });
        }
        // Headers (##, +, ++, %f, %i, /*), velocities (V) and correlations (EP/EV) carry
        // nothing this check uses.
    }
    if epochs.is_empty() {
        return Err(err(0, "no epochs in SP3 file"));
    }
    Ok(Sp3 {
        version,
        epochs,
        skipped_other_systems: skipped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SP3: &str = "\
#dP2026  9 20  0  0  0.00000000       2 ORBIT IGS20 HLM  IGS
## 2437      0.00000000   900.00000000 61303 0.0000000000000
+    3   G05G07R01  0  0  0  0  0  0  0  0  0  0  0  0  0  0
%c M  cc GPS ccc cccc cccc cccc cccc ccccc ccccc ccccc ccccc
%f  1.2500000  1.025000000  0.00000000000  0.000000000000000
/* synthetic test file
*  2026  9 20  0  0  0.00000000
PG05  13456.123456 -21234.654321  -8765.432100   -127.092935
PG07      0.000000      0.000000      0.000000 999999.999999
PR01  10000.000000  10000.000000  10000.000000      1.000000
*  2026  9 20  0 15  0.00000000
PG05  13789.000000 -20000.000000  -9999.000000 999999.999999
EOF
";

    #[test]
    fn parses_positions_clocks_and_bad_markers() {
        let sp3 = parse_sp3(SP3).unwrap();
        assert_eq!(sp3.version, 'd');
        assert_eq!(sp3.epochs.len(), 2);
        assert_eq!(sp3.skipped_other_systems, 1);
        let r = &sp3.epochs[0].records[0];
        assert_eq!(r.prn, 5);
        let p = r.ecef_m.unwrap();
        assert!((p[0] - 13_456_123.456).abs() < 1e-6 && (p[1] + 21_234_654.321).abs() < 1e-6);
        assert!((r.clock_s.unwrap() + 127.092935e-6).abs() < 1e-15);
        let bad = &sp3.epochs[0].records[1];
        assert_eq!(
            (bad.ecef_m, bad.clock_s),
            (None, None),
            "bad values are absent, not zero"
        );
        assert_eq!(sp3.epochs[1].records[0].clock_s, None);
        assert_eq!(sp3.epochs[1].t.0 - sp3.epochs[0].t.0, 900.0);
        // PRN 7 is marked bad, so it has no samples at all.
        let (a, b) = sp3.span();
        assert!(sp3.samples(7, a, b).is_empty());
        assert_eq!(sp3.samples(5, a, b).len(), 2);
    }

    #[test]
    fn refuses_what_would_mis_join() {
        let utc = SP3.replacen("%c M  cc GPS", "%c M  cc UTC", 1);
        assert!(parse_sp3(&utc).unwrap_err().message.contains("not GPS"));
        let sp3a = SP3.replacen("#dP", "#aP", 1);
        assert!(parse_sp3(&sp3a).unwrap_err().message.contains("version"));
        let garbled = SP3.replacen("13456.123456", "13456.12x456", 1);
        assert_eq!(parse_sp3(&garbled).unwrap_err().line, 8);
        let no_epochs: String = SP3.lines().take(6).map(|l| format!("{l}\n")).collect();
        assert!(
            parse_sp3(&no_epochs)
                .unwrap_err()
                .message
                .contains("no epochs")
        );
    }
}
