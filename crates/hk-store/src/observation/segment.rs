//! Segment naming and the CRC line codec (ADR-0012 §1.5).
//!
//! A segment holds one sample-clock hour: `<root>/YYYY/MM/DD/HH.log` (UTC). Each line is
//! `<crc32-hex8> <json ObservationRecord>\n`; the CRC is IEEE CRC-32 of the JSON bytes. A line
//! whose CRC does not match (a torn tail after a crash) is skipped on read.

use std::path::{Path, PathBuf};

use hk_model::Timestamp;
use hk_model::attention::observation::ObservationRecord;

/// Nanoseconds per hour.
pub const HOUR_NS: i64 = 3_600_000_000_000;

/// Hour index (hours since the Unix epoch) of a sample-clock instant.
pub fn hour_of(t: Timestamp) -> i64 {
    t.as_unix_nanos().div_euclid(HOUR_NS)
}

/// Path of hour `hour`'s segment under `root`.
pub fn segment_path(root: &Path, hour: i64) -> PathBuf {
    let days = hour.div_euclid(24);
    let hh = hour.rem_euclid(24);
    let (y, m, d) = civil_from_days(days);
    root.join(format!("{y:04}"))
        .join(format!("{m:02}"))
        .join(format!("{d:02}"))
        .join(format!("{hh:02}.log"))
}

/// Hour index of a segment path relative to `root` (`YYYY/MM/DD/HH.log`), if it is one.
pub fn parse_segment_path(root: &Path, path: &Path) -> Option<i64> {
    let rel = path.strip_prefix(root).ok()?;
    let parts: Vec<&str> = rel.iter().filter_map(|p| p.to_str()).collect();
    let [y, m, d, h] = parts.as_slice() else {
        return None;
    };
    let hh: i64 = h.strip_suffix(".log")?.parse().ok()?;
    let (y, m, d): (i64, i64, i64) = (y.parse().ok()?, m.parse().ok()?, d.parse().ok()?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || !(0..24).contains(&hh) {
        return None;
    }
    let hour = days_from_civil(y, m, d) * 24 + hh;
    (segment_path(root, hour) == path).then_some(hour)
}

/// Every segment under `root`, as `(hour, path, bytes)`, oldest first.
pub fn list_segments(root: &Path) -> Vec<(i64, PathBuf, u64)> {
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(md) = e.metadata() else { continue };
            if md.is_dir() && depth < 3 {
                stack.push((p, depth + 1));
            } else if md.is_file() && depth == 3 {
                if let Some(h) = parse_segment_path(root, &p) {
                    out.push((h, p, md.len()));
                }
            }
        }
    }
    out.sort_by_key(|s| s.0);
    out
}

/// One encoded line (with its newline).
pub fn encode_line(rec: &ObservationRecord) -> String {
    let json = serde_json::to_string(rec).unwrap_or_default();
    format!("{:08x} {json}\n", crc32(json.as_bytes()))
}

/// Decodes one line (without its newline); `None` when torn, corrupt or not a record.
pub fn decode_line(line: &str) -> Option<ObservationRecord> {
    let (crc, json) = line.split_once(' ')?;
    if crc.len() != 8 || u32::from_str_radix(crc, 16).ok()? != crc32(json.as_bytes()) {
        return None;
    }
    serde_json::from_str(json).ok()
}

/// Decodes every intact line of a segment's bytes, in order.
pub fn decode_segment(bytes: &[u8]) -> impl Iterator<Item = ObservationRecord> + '_ {
    bytes
        .split(|&b| b == b'\n')
        .filter_map(|l| std::str::from_utf8(l).ok())
        .filter_map(decode_line)
}

/// IEEE CRC-32 (reflected, polynomial 0xEDB88320).
pub fn crc32(data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let t = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, e) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
            *e = c;
        }
        t
    });
    !data.iter().fold(!0u32, |c, &b| {
        t[((c ^ u32::from(b)) & 0xff) as usize] ^ (c >> 8)
    })
}

/// Days since 1970-01-01 of a proleptic Gregorian date (H. Hinnant's `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `(year, month, day)` of days since 1970-01-01.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observation_segment_paths_name_the_sample_hour_and_round_trip() {
        let root = Path::new("/data/observations");
        // 2026-09-13T12:34:56Z
        let t = Timestamp::from_unix_nanos(1_789_302_896_000_000_000);
        let h = hour_of(t);
        let p = segment_path(root, h);
        assert_eq!(p, root.join("2026/09/13/12.log"));
        assert_eq!(parse_segment_path(root, &p), Some(h));
        assert_eq!(
            parse_segment_path(root, &root.join("2026/9/13/12.log")),
            None
        );
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn observation_torn_lines_fail_their_crc() {
        use hk_model::attention::observation::SweepGeometry;
        let rec = ObservationRecord::Geometry(SweepGeometry {
            schema: 1,
            id: 7,
            plan_version: 1,
            hops: vec![],
        });
        let line = encode_line(&rec);
        assert_eq!(decode_line(line.trim_end()), Some(rec.clone()));
        assert_eq!(decode_line(&line[..line.len() - 5]), None);
        let mut both = line.clone().into_bytes();
        both.extend_from_slice(&line.as_bytes()[..20]);
        assert_eq!(decode_segment(&both).count(), 1);
    }
}
