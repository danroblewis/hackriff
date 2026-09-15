//! Occupancy series store (T-118, ADR-0012 §2.9, §9): `OccupancyStat` rows per channel/band per
//! interval in append-only daily segments, 15-min intervals rolled up to 1 h, plus the learned
//! channel plan versions.
//!
//! **Layout.** `<dir>/15m/YYYY-MM-DD.log` and `<dir>/1h/YYYY-MM-DD.log` (UTC day of the row's
//! interval start, sample clock), one line per row: `<crc32-hex8> <json OccupancyStat>` (the §1.5
//! line format). `<dir>/channels.json` holds the current [`StoredChannelPlan`], rewritten
//! atomically (temp → fsync → rename) on a version change; rows carry their own `ChannelKey`, so
//! series under an old plan stay readable.
//!
//! **Writes.** [`OccupancyStore::append`] validates rows and writes each segment's lines in one
//! `write` per interval close (the caller closes every 15 min of stream time, so ≤ 1 append per
//! store per minute). A torn tail line fails its CRC and is skipped (counted) on read.
//!
//! **Retention by sample-clock age** (§0): a day segment is deleted once its day ended more than
//! the tier's max age before the newest row end appended (15-min rows 90 days, 1-h rows 2 years),
//! then oldest 15-min segments and then oldest 1-h segments go until the byte quota (256 MiB) holds.
//! Wall time is never consulted.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use hk_model::attention::occupancy::{Channel, OccupancyStat, OccupancySubject};
use hk_model::{FreqRange, TimeRange};
use serde::{Deserialize, Serialize};

use crate::StoreError;

const DAY_NS: i64 = 86_400_000_000_000;

/// Store limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OccupancyStoreConfig {
    /// Max sample-clock age of 15-min rows, s (90 days).
    pub max_age_15m_s: i64,
    /// Max sample-clock age of 1-h rows, s (2 years).
    pub max_age_1h_s: i64,
    /// Byte quota over both series (256 MiB).
    pub byte_quota: u64,
}

impl Default for OccupancyStoreConfig {
    fn default() -> Self {
        Self {
            max_age_15m_s: 90 * 86_400,
            max_age_1h_s: 730 * 86_400,
            byte_quota: 256 << 20,
        }
    }
}

/// Which series a row belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum SeriesInterval {
    /// 15-min rows.
    #[serde(rename = "15m")]
    Min15,
    /// 1-h rows.
    #[serde(rename = "1h")]
    Hour1,
}

impl SeriesInterval {
    /// Rows of at most 15 min go to the 15-min series, longer ones to the hourly series.
    pub fn of(interval: &TimeRange) -> Self {
        if interval.duration_ns() <= 900_000_000_000 {
            SeriesInterval::Min15
        } else {
            SeriesInterval::Hour1
        }
    }

    /// Directory name.
    pub fn dir(self) -> &'static str {
        match self {
            SeriesInterval::Min15 => "15m",
            SeriesInterval::Hour1 => "1h",
        }
    }

    /// Parses `15m` / `1h`.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "15m" => Some(SeriesInterval::Min15),
            "1h" => Some(SeriesInterval::Hour1),
            _ => None,
        }
    }
}

/// Subject filter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SubjectKind {
    /// Channel rows.
    Channel,
    /// Band rows.
    Band,
}

/// A series query.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OccupancyQuery {
    /// Rows whose subject overlaps this range.
    pub freq: FreqRange,
    /// Rows whose interval overlaps this span.
    pub span: TimeRange,
    /// Series.
    pub interval: SeriesInterval,
    /// Subject filter.
    pub subject: Option<SubjectKind>,
    /// Level-0 cell width, Hz (to place channel keys in frequency).
    pub f_cell_hz: f64,
    /// Row cap.
    pub limit: usize,
}

/// Query result.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct OccupancyRows {
    /// Rows, by interval start then subject order of writing.
    pub rows: Vec<OccupancyStat>,
    /// More rows matched than `limit`.
    pub truncated: bool,
    /// Lines skipped for a bad CRC, JSON or validation.
    pub corrupt_lines: u64,
}

/// The persisted learned channel plan.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredChannelPlan {
    /// `ATTENTION_SCHEMA_VERSION`.
    pub schema: u32,
    /// Plan version.
    pub version: u32,
    /// History grid scheme.
    pub scheme: u16,
    /// Level-0 cell width, Hz.
    pub f_cell_hz: f64,
    /// Channels.
    pub channels: Vec<Channel>,
}

/// Store counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct OccupancyStoreStats {
    /// Append batches.
    pub batches: u64,
    /// Rows written.
    pub rows_written: u64,
    /// Rows refused by validation.
    pub rows_rejected: u64,
    /// Bytes written.
    pub bytes_written: u64,
    /// Segments deleted by retention.
    pub segments_deleted: u64,
    /// Plan versions saved.
    pub plans_saved: u64,
}

/// The occupancy series store.
#[derive(Debug)]
pub struct OccupancyStore {
    dir: PathBuf,
    cfg: OccupancyStoreConfig,
    newest_end_ns: i64,
    stats: OccupancyStoreStats,
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> StoreError + '_ {
    move |source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// CRC-32 (IEEE 802.3, reflected), the §1.5 line checksum.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    !crc
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn day_name(day: i64) -> String {
    let (y, m, d) = civil_from_days(day);
    format!("{y:04}-{m:02}-{d:02}.log")
}

fn parse_day(name: &str) -> Option<i64> {
    let s = name.strip_suffix(".log")?;
    let mut p = s.splitn(3, '-');
    let y = p.next()?.parse().ok()?;
    let m = p.next()?.parse().ok()?;
    let d = p.next()?.parse().ok()?;
    Some(days_from_civil(y, m, d))
}

/// A row's line (`<crc32-hex8> <json>\n`).
pub fn encode_line(row: &OccupancyStat) -> Result<String, serde_json::Error> {
    let json = serde_json::to_string(row)?;
    Ok(format!("{:08x} {json}\n", crc32(json.as_bytes())))
}

/// Decodes a line; `None` when torn, corrupt or invalid.
pub fn decode_line(line: &str) -> Option<OccupancyStat> {
    let (crc, json) = line.split_once(' ')?;
    if crc.len() != 8 || u32::from_str_radix(crc, 16).ok()? != crc32(json.as_bytes()) {
        return None;
    }
    let row: OccupancyStat = serde_json::from_str(json).ok()?;
    row.validate().ok()?;
    Some(row)
}

fn subject_overlaps(row: &OccupancyStat, freq: FreqRange, f_cell_hz: f64) -> bool {
    let f = match row.subject {
        OccupancySubject::Channel { key } => key.freq(f_cell_hz),
        OccupancySubject::Band { freq } => freq,
    };
    f.lo_hz < freq.hi_hz && f.hi_hz > freq.lo_hz
}

impl OccupancyStore {
    /// Opens (creating) the store under `dir`.
    pub fn open(dir: impl Into<PathBuf>, cfg: OccupancyStoreConfig) -> Result<Self, StoreError> {
        let dir = dir.into();
        let mut newest_end_ns = i64::MIN;
        for s in [SeriesInterval::Min15, SeriesInterval::Hour1] {
            let d = dir.join(s.dir());
            fs::create_dir_all(&d).map_err(io(&d))?;
            for (day, _) in Self::segments_in(&d)? {
                newest_end_ns = newest_end_ns.max(day.saturating_add(1).saturating_mul(DAY_NS));
            }
        }
        Ok(Self {
            dir,
            cfg,
            newest_end_ns,
            stats: OccupancyStoreStats::default(),
        })
    }

    /// The store directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Counters.
    pub fn stats(&self) -> OccupancyStoreStats {
        self.stats
    }

    fn segments_in(d: &Path) -> Result<Vec<(i64, PathBuf)>, StoreError> {
        let mut v: Vec<(i64, PathBuf)> = fs::read_dir(d)
            .map_err(io(d))?
            .filter_map(Result::ok)
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                Some((parse_day(&name)?, e.path()))
            })
            .collect();
        v.sort();
        Ok(v)
    }

    /// Appends `rows` (invalid rows are refused and counted), one write per day segment, then
    /// applies retention. Returns the rows written.
    pub fn append(&mut self, rows: &[OccupancyStat]) -> Result<usize, StoreError> {
        let mut groups: std::collections::BTreeMap<(SeriesInterval, i64), String> =
            std::collections::BTreeMap::new();
        let mut n = 0;
        for r in rows {
            let Ok(line) = r
                .validate()
                .map_err(|_| ())
                .and_then(|()| encode_line(r).map_err(|_| ()))
            else {
                self.stats.rows_rejected += 1;
                continue;
            };
            let day = r.interval.start.as_unix_nanos().div_euclid(DAY_NS);
            groups
                .entry((SeriesInterval::of(&r.interval), day))
                .or_default()
                .push_str(&line);
            self.newest_end_ns = self.newest_end_ns.max(r.interval.end.as_unix_nanos());
            n += 1;
        }
        for ((series, day), text) in &groups {
            let path = self.dir.join(series.dir()).join(day_name(*day));
            let mut f = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .map_err(io(&path))?;
            f.write_all(text.as_bytes()).map_err(io(&path))?;
            self.stats.bytes_written += text.len() as u64;
        }
        if n > 0 {
            self.stats.batches += 1;
            self.stats.rows_written += n as u64;
            self.enforce_retention()?;
        }
        Ok(n)
    }

    /// Deletes segments past their sample-clock age, then oldest segments over the byte quota.
    pub fn enforce_retention(&mut self) -> Result<(), StoreError> {
        if self.newest_end_ns == i64::MIN {
            return Ok(());
        }
        let mut all: Vec<(SeriesInterval, i64, PathBuf, u64)> = Vec::new();
        for (s, age_s) in [
            (SeriesInterval::Min15, self.cfg.max_age_15m_s),
            (SeriesInterval::Hour1, self.cfg.max_age_1h_s),
        ] {
            let cutoff = self
                .newest_end_ns
                .saturating_sub(age_s.saturating_mul(1_000_000_000));
            for (day, path) in Self::segments_in(&self.dir.join(s.dir()))? {
                if (day + 1).saturating_mul(DAY_NS) <= cutoff {
                    fs::remove_file(&path).map_err(io(&path))?;
                    self.stats.segments_deleted += 1;
                } else {
                    let len = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                    all.push((s, day, path, len));
                }
            }
        }
        let mut total: u64 = all.iter().map(|x| x.3).sum();
        // Oldest 15-min segments first, then oldest hourly ones.
        all.sort_by_key(|x| (x.0 == SeriesInterval::Hour1, x.1));
        for (_, _, path, len) in all {
            if total <= self.cfg.byte_quota {
                break;
            }
            fs::remove_file(&path).map_err(io(&path))?;
            self.stats.segments_deleted += 1;
            total = total.saturating_sub(len);
        }
        Ok(())
    }

    /// Rows matching `q`, in segment order.
    pub fn query(&self, q: &OccupancyQuery) -> Result<OccupancyRows, StoreError> {
        let mut out = OccupancyRows::default();
        let (s0, s1) = (q.span.start.as_unix_nanos(), q.span.end.as_unix_nanos());
        let d = self.dir.join(q.interval.dir());
        for (day, path) in Self::segments_in(&d)? {
            // A row starts inside its day segment; it can end at most one hour later.
            if day.saturating_mul(DAY_NS) >= s1
                || (day + 1).saturating_mul(DAY_NS) + 3_600_000_000_000 <= s0
            {
                continue;
            }
            let text = fs::read_to_string(&path).map_err(io(&path))?;
            for line in text.lines() {
                let Some(row) = decode_line(line) else {
                    out.corrupt_lines += 1;
                    continue;
                };
                let (a, b) = (
                    row.interval.start.as_unix_nanos(),
                    row.interval.end.as_unix_nanos(),
                );
                if a >= s1 || b <= s0 || !subject_overlaps(&row, q.freq, q.f_cell_hz) {
                    continue;
                }
                let kind = match row.subject {
                    OccupancySubject::Channel { .. } => SubjectKind::Channel,
                    OccupancySubject::Band { .. } => SubjectKind::Band,
                };
                if q.subject.is_some_and(|k| k != kind) {
                    continue;
                }
                if out.rows.len() >= q.limit {
                    out.truncated = true;
                    return Ok(out);
                }
                out.rows.push(row);
            }
        }
        Ok(out)
    }

    fn plan_path(&self) -> PathBuf {
        self.dir.join("channels.json")
    }

    /// Saves the plan atomically (temp → fsync → rename).
    pub fn save_plan(&mut self, plan: &StoredChannelPlan) -> Result<(), StoreError> {
        let path = self.plan_path();
        let tmp = self.dir.join("channels.json.tmp");
        let body = serde_json::to_vec_pretty(plan)
            .map_err(|e| StoreError::Config(format!("channel plan: {e}")))?;
        {
            let mut f = fs::File::create(&tmp).map_err(io(&tmp))?;
            f.write_all(&body).map_err(io(&tmp))?;
            f.sync_all().map_err(io(&tmp))?;
        }
        fs::rename(&tmp, &path).map_err(io(&path))?;
        self.stats.plans_saved += 1;
        Ok(())
    }

    /// The saved plan, if any (an unreadable file reads as none).
    pub fn load_plan(&self) -> Result<Option<StoredChannelPlan>, StoreError> {
        let path = self.plan_path();
        match fs::read(&path) {
            Ok(b) => Ok(serde_json::from_slice(&b).ok()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(StoreError::Io { path, source: e }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hk_model::Timestamp;
    use hk_model::attention::ATTENTION_SCHEMA_VERSION;
    use hk_model::attention::baseline::SiteKey;
    use hk_model::attention::occupancy::{ChannelKey, ChannelSource, ThresholdSpec, TimingRegime};
    use hk_model::frames::PowerUnit;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "hk-occ-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn row(start_s: i64, len_s: i64, lo_cell: i64) -> OccupancyStat {
        let t = |s: i64| Timestamp::from_unix_nanos(s * 1_000_000_000);
        OccupancyStat {
            schema: ATTENTION_SCHEMA_VERSION,
            site: SiteKey::Unassigned,
            subject: OccupancySubject::Channel {
                key: ChannelKey {
                    scheme: 1,
                    lo_cell,
                    hi_cell: lo_cell + 2,
                },
            },
            interval: TimeRange::new(t(start_s), t(start_s + len_s)),
            fco: Some(0.25),
            fco_all_visits: Some(0.4),
            fco_suspect_upper: Some(0.3),
            fbo: Some(0.2),
            sro: None,
            n_revisits: 40,
            n_occupied: 10,
            n_suspect: 2,
            n_revisits_all: 60,
            observed_s: 20.0,
            revisit_max_s: Some(40.0),
            revisit_mean_s: Some(22.5),
            timing: TimingRegime::Unknown,
            threshold: ThresholdSpec::default(),
            threshold_db: -120.0,
            guard_clamped: true,
            rbw_hz: 6250.0,
            obw_hz: Some(15_000.0),
            unit: PowerUnit::Dbfs,
            calibration: None,
            confidence: None,
            revisit_biased: false,
            fco_window: Some(TimeRange::new(t(start_s), t(start_s + len_s))),
            floor_db: None,
            floor_source: None,
            floor_suspect: None,
            level_occupied_p50_db: None,
            level_occupied_p90_db: None,
            level_idle_db: None,
        }
    }

    fn q(interval: SeriesInterval, a_s: i64, b_s: i64) -> OccupancyQuery {
        OccupancyQuery {
            freq: FreqRange::new(0.0, 1e12),
            span: TimeRange::new(
                Timestamp::from_unix_nanos(a_s * 1_000_000_000),
                Timestamp::from_unix_nanos(b_s * 1_000_000_000),
            ),
            interval,
            subject: None,
            f_cell_hz: 6250.0,
            limit: 10_000,
        }
    }

    #[test]
    fn occupancy_store_appends_batches_queries_and_skips_torn_lines() {
        let dir = tmp("rw");
        let mut s = OccupancyStore::open(&dir, OccupancyStoreConfig::default()).unwrap();
        let day0 = 20_000 * 86_400;
        let rows: Vec<_> = (0..8)
            .map(|k| row(day0 + 80_000 + k * 900, 900, 16_000))
            .collect();
        assert_eq!(s.append(&rows).unwrap(), 8);
        s.append(&[row(day0, 3600, 16_000)]).unwrap();
        let mut bad = row(day0, 900, 1);
        bad.n_occupied = 100;
        assert_eq!(s.append(&[bad]).unwrap(), 0);
        assert_eq!(s.stats().rows_rejected, 1);
        // Rows spanning midnight land in two day files; both read back.
        let got = s
            .query(&q(SeriesInterval::Min15, day0, day0 + 2 * 86_400))
            .unwrap();
        assert_eq!(got.rows, rows);
        assert_eq!(
            s.query(&q(SeriesInterval::Hour1, day0, day0 + 1))
                .unwrap()
                .rows
                .len(),
            1
        );
        // Frequency and span filters.
        let mut fq = q(SeriesInterval::Min15, day0 + 80_000, day0 + 80_900);
        assert_eq!(s.query(&fq).unwrap().rows.len(), 1);
        fq.freq = FreqRange::new(1.0, 2.0);
        assert!(s.query(&fq).unwrap().rows.is_empty());
        // A torn tail line is skipped and counted.
        let seg = dir.join("15m").join(day_name(day0.div_euclid(86_400)));
        let mut f = fs::OpenOptions::new().append(true).open(&seg).unwrap();
        f.write_all(b"deadbeef {\"schema\":1,\"si").unwrap();
        let got = s
            .query(&q(SeriesInterval::Min15, day0, day0 + 2 * 86_400))
            .unwrap();
        assert_eq!((got.rows.len(), got.corrupt_lines), (8, 1));
        // Reopen: same rows; the limit truncates.
        let s2 = OccupancyStore::open(&dir, OccupancyStoreConfig::default()).unwrap();
        let mut lq = q(SeriesInterval::Min15, day0, day0 + 2 * 86_400);
        lq.limit = 3;
        let got = s2.query(&lq).unwrap();
        assert!(got.truncated && got.rows.len() == 3);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn occupancy_store_retention_follows_the_sample_clock_and_quota() {
        let dir = tmp("ret");
        let cfg = OccupancyStoreConfig {
            max_age_15m_s: 2 * 86_400,
            max_age_1h_s: 5 * 86_400,
            byte_quota: 1 << 30,
        };
        let mut s = OccupancyStore::open(&dir, cfg).unwrap();
        let day0 = 20_100 * 86_400;
        for d in 0..10 {
            s.append(&[
                row(day0 + d * 86_400, 900, 1),
                row(day0 + d * 86_400, 3600, 1),
            ])
            .unwrap();
        }
        let count = |s: SeriesInterval| fs::read_dir(dir.join(s.dir())).unwrap().count();
        // Newest row ends at day 9 + 1 h: 15-min keeps days ending after day 7 + 1 h (7, 8, 9);
        // hourly after day 4 + 1 h (4..=9).
        assert_eq!(count(SeriesInterval::Min15), 3);
        assert_eq!(count(SeriesInterval::Hour1), 6);
        // A tight quota drops 15-min segments before hourly ones.
        s.cfg.byte_quota = 5 * fs::metadata(dir.join("1h").join(day_name(20_109)))
            .unwrap()
            .len();
        s.enforce_retention().unwrap();
        assert_eq!(count(SeriesInterval::Min15), 0);
        assert_eq!(count(SeriesInterval::Hour1), 5);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn occupancy_store_plan_round_trips_atomically_and_dates_are_civil() {
        let dir = tmp("plan");
        let mut s = OccupancyStore::open(&dir, OccupancyStoreConfig::default()).unwrap();
        assert_eq!(s.load_plan().unwrap(), None);
        let plan = StoredChannelPlan {
            schema: ATTENTION_SCHEMA_VERSION,
            version: 3,
            scheme: 1,
            f_cell_hz: 6250.0,
            channels: vec![Channel {
                key: ChannelKey {
                    scheme: 1,
                    lo_cell: 69_360,
                    hi_cell: 69_363,
                },
                source: ChannelSource::Learned,
                plan_version: 3,
                first_learned: Timestamp::from_unix_nanos(5),
                evidence: 12,
                obw_hz: 15_000.0,
                raster_hint: None,
            }],
        };
        s.save_plan(&plan).unwrap();
        assert_eq!(s.load_plan().unwrap(), Some(plan));
        assert!(!dir.join("channels.json.tmp").exists());
        assert_eq!(day_name(days_from_civil(2026, 9, 15)), "2026-09-15.log");
        assert_eq!(parse_day("1970-01-01.log"), Some(0));
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        fs::remove_dir_all(dir).ok();
    }
}
