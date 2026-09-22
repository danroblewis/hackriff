//! The shadow-record store (ADR-0016 §6, T-844): what a model in `shadow` mode said, next to what
//! the classical cascade decided, kept durably so the §4.6 comparison can be made on data that was
//! actually seen rather than only on the synthetic dev grid.
//!
//! # Layout
//!
//! `<root>/YYYY/MM/DD/HH.log` (UTC, one **sample-clock** hour each) — the observation log's
//! segment naming and CRC line codec ([`crate::observation::segment`]), reused rather than copied.
//! Each line is `<crc32-hex8> <json ShadowRecord>`, the CRC being IEEE CRC-32 of the JSON bytes. A
//! line that fails its CRC (a torn tail after a crash, a flipped bit) is skipped and counted
//! ([`ShadowStoreStats::corrupt_lines`]), never guessed at; on open the newest segment's torn tail
//! is truncated so appends start on a line boundary.
//!
//! # What a record is
//!
//! ADR-0016 §6's `{Prediction, classical decision, snr_bin, subject}`, as plain data:
//! [`ShadowPrediction`] (the model's top label, its calibrated probability, energy and open-set
//! score, provider, precision, latency, mode), [`ClassicalDecision`] (the family and class the
//! classical cascade published), the measured SNR with its [`snr_bin_db`], and the
//! [`ShadowSubject`] (the CFAR detection it was about). Every record's mode is `shadow`:
//! [`ShadowStore::append`] refuses anything else, because an `active` prediction is a decision and
//! belongs on a `Classification` row, not in a diagnostics log.
//!
//! # Bounds
//!
//! Whole hours older than [`ShadowStoreConfig::max_age_ns`] (30 days) before the newest record's
//! **sample time** are deleted, then oldest hours until the store fits
//! [`ShadowStoreConfig::max_bytes`] (256 MiB). Replay time never keys retention on wall time, the
//! observation log's rule.
//!
//! # Aggregates
//!
//! Per `(model, consumer, family, SNR bin)`: how many predictions, how many the classical stage
//! named a class for (`compared`), how many of those the model agreed with, and how many the model
//! itself called out-of-distribution (`unknown_score ≥ 0.5`, the dev 95 %-TPR operating point).
//! They are maintained per segment as records arrive, so retention drops exactly the hours it
//! deletes and the aggregates always describe the records that are still on disk — never an
//! all-time tally of data that is gone. Bins are [`SNR_BIN_DB`] wide, `floor(snr/5)·5`, the same
//! binning `hk_classify::eval::EvalReport` reports the dev evaluation in, so a shadow aggregate
//! and the dev report compare cell for cell.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use hk_model::Timestamp;
use serde::{Deserialize, Serialize};

use crate::observation::segment::{HOUR_NS, crc32, hour_of, list_segments, segment_path};

/// Schema version of [`ShadowRecord`].
pub const SHADOW_SCHEMA: u16 = 1;

/// Default byte budget: 256 MiB (ADR-0016 §6).
pub const DEFAULT_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// Default retention: 30 days of sample time (ADR-0016 §6).
pub const DEFAULT_MAX_AGE_NS: i64 = 30 * 24 * HOUR_NS;

/// SNR bin width, dB (ADR-0016 §7; `hk_classify::eval::EvalReport`'s default).
pub const SNR_BIN_DB: f64 = 5.0;

/// Records a query returns when it names no limit.
pub const DEFAULT_SHADOW_LIMIT: usize = 100;

/// The most records one query returns.
pub const MAX_SHADOW_LIMIT: usize = 1000;

/// The open-set score at or above which a prediction counts as the model saying "unknown": the
/// calibrated operating point (`hk_ml::predict`, 0.5 = the dev 95 %-TPR threshold).
pub const UNKNOWN_SCORE_AT: f64 = 0.5;

/// The SNR bin of a measured SNR: `floor(snr / 5) · 5` dB. `None` when there is no finite SNR —
/// an unmeasured SNR is its own row, never bin 0.
pub fn snr_bin_db(snr_db: Option<f64>) -> Option<i64> {
    snr_db
        .filter(|s| s.is_finite())
        .map(|s| (s / SNR_BIN_DB).floor() as i64 * SNR_BIN_DB as i64)
}

/// What a shadow prediction was about.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShadowSubject {
    /// `detection` — inference runs only on CFAR survivors (`hk_ml::gate`).
    pub kind: String,
    /// The CFAR detection's id.
    pub detection: String,
}

/// What the model said (the parts of `hk_ml::Prediction` a comparison needs).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShadowPrediction {
    /// Its highest-probability label. A class call, never an unknown detector.
    pub label: String,
    /// Temperature-calibrated probability of that label.
    pub p: f64,
    /// Energy `E = −T·logsumexp(z/T)`.
    pub energy: f64,
    /// Calibrated open-set score, 0–1.
    pub unknown_score: f64,
    /// Runtime that ran it (`cpu-tract`, `cpu-mlp`, …).
    pub provider: String,
    /// Precision it ran at.
    pub precision: String,
    /// Latency of the batch it ran in, ms.
    pub latency_ms: f32,
    /// Items in that batch.
    pub batch_size: u16,
    /// The mode it ran in — always `shadow` in this store.
    pub mode: String,
}

/// What the classical cascade published for the same subject (the row the model never touched).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClassicalDecision {
    /// The family it named (the model only ever refines within it).
    pub family: String,
    /// The within-family class it named, when it was above the class gate.
    #[serde(default)]
    pub class: Option<String>,
    /// Probability it gave that class.
    #[serde(default)]
    pub class_p: Option<f64>,
    /// Its confidence in the family.
    pub confidence: f64,
    /// Its χ² open-set score.
    pub open_set_score: f64,
    /// The classical stage that decided (`feature-tree`, `verifier`).
    pub stage: String,
}

/// One shadow record: `{Prediction, classical decision, snr_bin, subject}` (ADR-0016 §6).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShadowRecord {
    /// [`SHADOW_SCHEMA`].
    pub schema: u16,
    /// Sample-clock time the prediction was made at.
    pub t: Timestamp,
    /// `id@version#sha8` of the model.
    pub model: String,
    /// The consumer the `(model, consumer)` mode is scoped to.
    pub consumer: String,
    /// What it was about.
    pub subject: ShadowSubject,
    /// Measured in-band SNR, dB.
    pub snr_db: Option<f64>,
    /// [`snr_bin_db`] of it.
    pub snr_bin_db: Option<i64>,
    /// What the model said.
    pub prediction: ShadowPrediction,
    /// What the classical cascade published.
    pub classical: ClassicalDecision,
}

impl ShadowRecord {
    /// Whether the model named the class the classical stage named; `None` when the classical
    /// stage named no class (below its class gate), which is not a disagreement.
    pub fn agrees(&self) -> Option<bool> {
        self.classical
            .class
            .as_ref()
            .map(|c| *c == self.prediction.label)
    }

    /// Whether the model called this out-of-distribution at its operating point.
    pub fn model_unknown(&self) -> bool {
        self.prediction.unknown_score >= UNKNOWN_SCORE_AT
    }

    /// Structural checks [`ShadowStore::append`] applies.
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != SHADOW_SCHEMA {
            return Err(format!(
                "shadow record schema {} is not {SHADOW_SCHEMA}",
                self.schema
            ));
        }
        if self.prediction.mode != "shadow" {
            return Err(format!(
                "a {} prediction is not a shadow record: only shadow predictions are kept here, \
                 and an active one is a decision (ADR-0016 §6)",
                self.prediction.mode
            ));
        }
        for (what, s) in [
            ("model", &self.model),
            ("consumer", &self.consumer),
            ("classical family", &self.classical.family),
            ("prediction label", &self.prediction.label),
            ("subject detection", &self.subject.detection),
        ] {
            if s.trim().is_empty() {
                return Err(format!("{what} is empty"));
            }
        }
        if self.snr_bin_db != snr_bin_db(self.snr_db) {
            return Err(format!(
                "snr_bin_db {:?} is not the bin of snr_db {:?}",
                self.snr_bin_db, self.snr_db
            ));
        }
        Ok(())
    }
}

/// One line of the store (with its newline).
pub fn encode_line(rec: &ShadowRecord) -> String {
    let json = serde_json::to_string(rec).unwrap_or_default();
    format!("{:08x} {json}\n", crc32(json.as_bytes()))
}

/// Decodes one line (without its newline); `None` when torn, corrupt or not a record.
pub fn decode_line(line: &str) -> Option<ShadowRecord> {
    let (crc, json) = line.split_once(' ')?;
    if crc.len() != 8 || u32::from_str_radix(crc, 16).ok()? != crc32(json.as_bytes()) {
        return None;
    }
    serde_json::from_str(json).ok()
}

/// Decodes a segment's bytes: the intact records, in order, and how many non-empty lines failed.
fn decode_segment(bytes: &[u8]) -> (Vec<ShadowRecord>, u64) {
    let mut records = Vec::new();
    let mut corrupt = 0;
    for line in bytes.split(|&b| b == b'\n') {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match std::str::from_utf8(line).ok().and_then(decode_line) {
            Some(r) => records.push(r),
            None => corrupt += 1,
        }
    }
    (records, corrupt)
}

/// Agreement counts for one cell.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Agreement {
    /// Predictions.
    pub n: u64,
    /// Of those, how many the classical stage named a class for (so a comparison exists).
    pub compared: u64,
    /// Of `compared`, how many named the same class.
    pub agree: u64,
    /// Predictions the model itself called out-of-distribution (`unknown_score ≥ 0.5`).
    pub model_unknown: u64,
}

impl Agreement {
    fn add(&mut self, r: &ShadowRecord) {
        self.n += 1;
        if let Some(a) = r.agrees() {
            self.compared += 1;
            self.agree += u64::from(a);
        }
        self.model_unknown += u64::from(r.model_unknown());
    }

    fn merge(&mut self, o: &Agreement) {
        self.n += o.n;
        self.compared += o.compared;
        self.agree += o.agree;
        self.model_unknown += o.model_unknown;
    }

    /// `agree / compared`, or `None` with nothing to compare.
    pub fn agreement_rate(&self) -> Option<f64> {
        (self.compared > 0).then(|| self.agree as f64 / self.compared as f64)
    }
}

/// `(model, consumer, family, snr_bin_db)`.
type AggKey = (String, String, String, Option<i64>);

fn key_of(r: &ShadowRecord) -> AggKey {
    (
        r.model.clone(),
        r.consumer.clone(),
        r.classical.family.clone(),
        r.snr_bin_db,
    )
}

/// One aggregate row: a `(model, consumer, family, SNR bin)` cell.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AgreementRow {
    /// `id@version#sha8`.
    pub model: String,
    /// Consumer.
    pub consumer: String,
    /// Family the classical stage named.
    pub family: String,
    /// SNR bin (lower edge, dB); `None` for records with no measured SNR.
    pub snr_bin_db: Option<i64>,
    /// The counts.
    #[serde(flatten)]
    pub counts: Agreement,
    /// `agree / compared`.
    pub agreement_rate: Option<f64>,
}

/// A query over the store. Every filter is optional.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ShadowQuery {
    /// `id`, `id@version` or `id@version#sha8`.
    pub model: Option<String>,
    /// Consumer.
    pub consumer: Option<String>,
    /// Classical family.
    pub family: Option<String>,
    /// Records at or after this sample time.
    pub t0: Option<Timestamp>,
    /// Records before this sample time.
    pub t1: Option<Timestamp>,
    /// Most records to return (clamped to [`MAX_SHADOW_LIMIT`]; default
    /// [`DEFAULT_SHADOW_LIMIT`]).
    pub limit: Option<usize>,
}

impl ShadowQuery {
    fn matches(&self, r: &ShadowRecord) -> bool {
        let model_ok = self.model.as_deref().is_none_or(|q| {
            r.model == q
                || r.model.starts_with(&format!("{q}@"))
                || r.model.starts_with(&format!("{q}#"))
        });
        model_ok
            && self.consumer.as_deref().is_none_or(|c| r.consumer == c)
            && self
                .family
                .as_deref()
                .is_none_or(|f| r.classical.family == f)
            && self.t0.is_none_or(|t0| r.t >= t0)
            && self.t1.is_none_or(|t1| r.t < t1)
    }

    fn hour_in_range(&self, hour: i64) -> bool {
        self.t0.is_none_or(|t0| hour >= hour_of(t0)) && self.t1.is_none_or(|t1| hour <= hour_of(t1))
    }

    fn timed(&self) -> bool {
        self.t0.is_some() || self.t1.is_some()
    }
}

/// Bounds of the store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShadowStoreConfig {
    /// Byte budget across every segment.
    pub max_bytes: u64,
    /// Oldest sample time kept, relative to the newest record.
    pub max_age_ns: i64,
}

impl Default for ShadowStoreConfig {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_MAX_BYTES,
            max_age_ns: DEFAULT_MAX_AGE_NS,
        }
    }
}

/// What the store holds.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ShadowStoreStats {
    /// Hour segments on disk.
    pub segments: usize,
    /// Their bytes.
    pub bytes: u64,
    /// Intact records in them.
    pub records: u64,
    /// Non-empty lines that failed their CRC or did not parse, since open.
    pub corrupt_lines: u64,
    /// Records appended since open.
    pub appended: u64,
    /// Records refused by [`ShadowRecord::validate`] since open.
    pub refused: u64,
    /// Segments deleted by retention since open.
    pub segments_deleted: u64,
    /// Sample time of the oldest retained record.
    pub oldest: Option<Timestamp>,
    /// Sample time of the newest retained record.
    pub newest: Option<Timestamp>,
    /// [`ShadowStoreConfig::max_bytes`].
    pub max_bytes: u64,
    /// [`ShadowStoreConfig::max_age_ns`], in seconds.
    pub max_age_s: f64,
}

#[derive(Debug, Default)]
struct Segment {
    bytes: u64,
    records: u64,
    oldest: Option<Timestamp>,
    newest: Option<Timestamp>,
    agg: BTreeMap<AggKey, Agreement>,
}

impl Segment {
    fn add(&mut self, r: &ShadowRecord, bytes: u64) {
        self.bytes += bytes;
        self.records += 1;
        self.oldest = Some(self.oldest.map_or(r.t, |o| o.min(r.t)));
        self.newest = Some(self.newest.map_or(r.t, |n| n.max(r.t)));
        self.agg.entry(key_of(r)).or_default().add(r);
    }
}

#[derive(Debug, Default)]
struct Inner {
    segments: BTreeMap<i64, Segment>,
    corrupt_lines: u64,
    appended: u64,
    refused: u64,
    segments_deleted: u64,
}

/// The durable shadow store. All methods take `&self`; one mutex serialises writers and readers,
/// which is ample for its rate (at most one record per model per classification event).
#[derive(Debug)]
pub struct ShadowStore {
    root: PathBuf,
    config: ShadowStoreConfig,
    inner: Mutex<Inner>,
}

impl ShadowStore {
    /// Opens (creating) a store at `root` with the default bounds.
    pub fn open(root: impl Into<PathBuf>) -> io::Result<Self> {
        Self::open_with(root, ShadowStoreConfig::default())
    }

    /// Opens (creating) a store at `root`: scans every segment, truncates the newest one's torn
    /// tail, rebuilds the per-segment aggregates and applies retention.
    pub fn open_with(root: impl Into<PathBuf>, config: ShadowStoreConfig) -> io::Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        let segments = list_segments(&root);
        let mut inner = Inner::default();
        let newest_hour = segments.last().map(|s| s.0);
        for (hour, path, _) in segments {
            let mut bytes = fs::read(&path)?;
            if Some(hour) == newest_hour
                && let Some(end) = bytes.iter().rposition(|&b| b == b'\n').map(|i| i + 1)
                && end < bytes.len()
            {
                // A torn tail after a crash: drop it so the next append starts a clean line.
                OpenOptions::new()
                    .write(true)
                    .open(&path)?
                    .set_len(end as u64)?;
                bytes.truncate(end);
                inner.corrupt_lines += 1;
            }
            let (records, corrupt) = decode_segment(&bytes);
            inner.corrupt_lines += corrupt;
            let mut seg = Segment {
                bytes: bytes.len() as u64,
                ..Segment::default()
            };
            for r in &records {
                seg.add(r, 0);
            }
            inner.segments.insert(hour, seg);
        }
        let store = Self {
            root,
            config,
            inner: Mutex::new(inner),
        };
        {
            let mut inner = store.lock();
            store.retain(&mut inner);
        }
        Ok(store)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The directory the segments live under.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The bounds in force.
    pub fn config(&self) -> ShadowStoreConfig {
        self.config
    }

    /// Appends one record to its sample hour's segment, then applies retention.
    ///
    /// Refuses (and counts) a record that fails [`ShadowRecord::validate`] — notably any whose
    /// prediction is not in `shadow` mode.
    pub fn append(&self, rec: &ShadowRecord) -> io::Result<()> {
        let mut inner = self.lock();
        if let Err(e) = rec.validate() {
            inner.refused += 1;
            return Err(io::Error::new(io::ErrorKind::InvalidInput, e));
        }
        let line = encode_line(rec);
        let hour = hour_of(rec.t);
        let path = segment_path(&self.root, hour);
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
        f.write_all(line.as_bytes())?;
        f.flush()?;
        inner
            .segments
            .entry(hour)
            .or_default()
            .add(rec, line.len() as u64);
        inner.appended += 1;
        self.retain(&mut inner);
        Ok(())
    }

    /// Age first (against the newest record's sample time), then bytes, oldest hour first.
    fn retain(&self, inner: &mut Inner) {
        let Some(newest) = inner.segments.values().filter_map(|s| s.newest).max() else {
            return;
        };
        let cutoff = hour_of(Timestamp::from_unix_nanos(
            newest
                .as_unix_nanos()
                .saturating_sub(self.config.max_age_ns),
        ));
        let old: Vec<i64> = inner.segments.range(..cutoff).map(|(h, _)| *h).collect();
        for h in old {
            self.delete(inner, h);
        }
        while inner.segments.len() > 1
            && inner.segments.values().map(|s| s.bytes).sum::<u64>() > self.config.max_bytes
        {
            let Some(&h) = inner.segments.keys().next() else {
                break;
            };
            self.delete(inner, h);
        }
    }

    fn delete(&self, inner: &mut Inner, hour: i64) {
        let path = segment_path(&self.root, hour);
        if fs::remove_file(&path).is_ok() || !path.exists() {
            inner.segments.remove(&hour);
            inner.segments_deleted += 1;
            // Prune now-empty day/month/year directories; `remove_dir` refuses a non-empty one.
            let mut dir = path.parent();
            for _ in 0..3 {
                match dir {
                    Some(d) if d != self.root && fs::remove_dir(d).is_ok() => dir = d.parent(),
                    _ => break,
                }
            }
        }
    }

    /// Records matching `q`, newest first, up to its limit.
    pub fn query(&self, q: &ShadowQuery) -> io::Result<Vec<ShadowRecord>> {
        let limit = q
            .limit
            .unwrap_or(DEFAULT_SHADOW_LIMIT)
            .clamp(1, MAX_SHADOW_LIMIT);
        let inner = self.lock();
        let mut out = Vec::new();
        for (&hour, _) in inner.segments.iter().rev() {
            if !q.hour_in_range(hour) {
                continue;
            }
            let mut recs = self.read_segment(hour)?;
            recs.retain(|r| q.matches(r));
            recs.sort_by(|a, b| b.t.cmp(&a.t));
            for r in recs {
                if out.len() == limit {
                    return Ok(out);
                }
                out.push(r);
            }
        }
        Ok(out)
    }

    fn read_segment(&self, hour: i64) -> io::Result<Vec<ShadowRecord>> {
        match fs::read(segment_path(&self.root, hour)) {
            Ok(bytes) => Ok(decode_segment(&bytes).0),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// Per-`(model, consumer, family, SNR bin)` agreement over the **retained** records matching
    /// `q` (its limit does not apply). Without a time filter this reads the aggregates kept per
    /// segment; with one it re-reads the segments in range, so a time window is exact rather than
    /// rounded to hours.
    pub fn aggregates(&self, q: &ShadowQuery) -> io::Result<Vec<AgreementRow>> {
        let mut cells: BTreeMap<AggKey, Agreement> = BTreeMap::new();
        let inner = self.lock();
        if q.timed() {
            for (&hour, _) in &inner.segments {
                if !q.hour_in_range(hour) {
                    continue;
                }
                for r in self.read_segment(hour)?.iter().filter(|r| q.matches(r)) {
                    cells.entry(key_of(r)).or_default().add(r);
                }
            }
        } else {
            for seg in inner.segments.values() {
                for (k, a) in seg.agg.iter().filter(|(k, _)| q.matches_key(k)) {
                    cells.entry(k.clone()).or_default().merge(a);
                }
            }
        }
        Ok(cells
            .into_iter()
            .map(|((model, consumer, family, snr_bin_db), counts)| AgreementRow {
                model,
                consumer,
                family,
                snr_bin_db,
                agreement_rate: counts.agreement_rate(),
                counts,
            })
            .collect())
    }

    /// What the store holds.
    pub fn stats(&self) -> ShadowStoreStats {
        let inner = self.lock();
        ShadowStoreStats {
            segments: inner.segments.len(),
            bytes: inner.segments.values().map(|s| s.bytes).sum(),
            records: inner.segments.values().map(|s| s.records).sum(),
            corrupt_lines: inner.corrupt_lines,
            appended: inner.appended,
            refused: inner.refused,
            segments_deleted: inner.segments_deleted,
            oldest: inner.segments.values().filter_map(|s| s.oldest).min(),
            newest: inner.segments.values().filter_map(|s| s.newest).max(),
            max_bytes: self.config.max_bytes,
            max_age_s: self.config.max_age_ns as f64 / 1e9,
        }
    }
}

impl ShadowQuery {
    /// Whether an aggregate key can hold records this (untimed) query matches.
    fn matches_key(&self, (model, consumer, family, _): &AggKey) -> bool {
        self.model.as_deref().is_none_or(|q| {
            model == q || model.starts_with(&format!("{q}@")) || model.starts_with(&format!("{q}#"))
        }) && self.consumer.as_deref().is_none_or(|c| consumer == c)
            && self.family.as_deref().is_none_or(|f| family == f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let p = std::env::temp_dir().join(format!(
                "hk-store-shadow-{tag}-{}-{}",
                std::process::id(),
                N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// 2026-09-13T12:00:00Z.
    const T0: i64 = 1_789_300_800_000_000_000;

    fn record(t_ns: i64, model: &str, snr: Option<f64>, label: &str, classical: Option<&str>) -> ShadowRecord {
        ShadowRecord {
            schema: SHADOW_SCHEMA,
            t: Timestamp::from_unix_nanos(t_ns),
            model: model.into(),
            consumer: "hk-classify/dl".into(),
            subject: ShadowSubject {
                kind: "detection".into(),
                detection: "det-1".into(),
            },
            snr_db: snr,
            snr_bin_db: snr_bin_db(snr),
            prediction: ShadowPrediction {
                label: label.into(),
                p: 0.9,
                energy: -8.0,
                unknown_score: if label == "gfsk" { 0.7 } else { 0.1 },
                provider: "cpu-mlp".into(),
                precision: "fp32".into(),
                latency_ms: 0.2,
                batch_size: 1,
                mode: "shadow".into(),
            },
            classical: ClassicalDecision {
                family: "fsk".into(),
                class: classical.map(str::to_owned),
                class_p: classical.map(|_| 0.8),
                confidence: 0.9,
                open_set_score: 0.1,
                stage: "feature-tree".into(),
            },
        }
    }

    #[test]
    fn snr_bins_are_five_db_floors_and_an_unmeasured_snr_is_not_bin_zero() {
        assert_eq!(snr_bin_db(Some(22.4)), Some(20));
        assert_eq!(snr_bin_db(Some(25.0)), Some(25));
        assert_eq!(snr_bin_db(Some(-0.1)), Some(-5));
        assert_eq!(snr_bin_db(None), None);
        assert_eq!(snr_bin_db(Some(f64::NAN)), None);
    }

    #[test]
    fn a_shadow_record_is_durable_and_the_aggregates_survive_a_reopen() {
        let dir = TempDir::new("durable");
        let a = "amc-fsk@0.1.0#0123abcd";
        {
            let s = ShadowStore::open(&dir.0).unwrap();
            s.append(&record(T0, a, Some(22.0), "2fsk", Some("2fsk"))).unwrap();
            s.append(&record(T0 + 1_000, a, Some(23.0), "gfsk", Some("2fsk"))).unwrap();
            s.append(&record(T0 + 2_000, a, Some(12.0), "2fsk", None)).unwrap();
            s.append(&record(T0 + HOUR_NS, a, None, "2fsk", Some("2fsk"))).unwrap();
            assert_eq!(s.stats().segments, 2);
        }
        let s = ShadowStore::open(&dir.0).unwrap();
        let st = s.stats();
        assert_eq!((st.records, st.corrupt_lines, st.segments), (4, 0, 2));
        // Newest first.
        let recs = s.query(&ShadowQuery::default()).unwrap();
        assert_eq!(recs.len(), 4);
        assert_eq!(recs[0].t.as_unix_nanos(), T0 + HOUR_NS);
        assert_eq!(recs[3].t.as_unix_nanos(), T0);

        let rows = s.aggregates(&ShadowQuery::default()).unwrap();
        let cell = |bin: Option<i64>| rows.iter().find(|r| r.snr_bin_db == bin).unwrap();
        let b20 = cell(Some(20));
        assert_eq!((b20.counts.n, b20.counts.compared, b20.counts.agree), (2, 2, 1));
        assert_eq!(b20.counts.model_unknown, 1);
        assert_eq!(b20.agreement_rate, Some(0.5));
        // No classical class: counted, not compared, and not a disagreement.
        let b10 = cell(Some(10));
        assert_eq!((b10.counts.n, b10.counts.compared), (1, 0));
        assert_eq!(b10.agreement_rate, None);
        assert_eq!(cell(None).counts.n, 1);

        // A time filter is exact, not rounded to the hour.
        let q = ShadowQuery {
            t0: Some(Timestamp::from_unix_nanos(T0 + 1_000)),
            t1: Some(Timestamp::from_unix_nanos(T0 + 2_000)),
            ..ShadowQuery::default()
        };
        assert_eq!(s.query(&q).unwrap().len(), 1);
        let rows = s.aggregates(&q).unwrap();
        assert_eq!(rows.iter().map(|r| r.counts.n).sum::<u64>(), 1);
    }

    #[test]
    fn a_torn_or_corrupted_line_is_skipped_and_counted_never_guessed() {
        let dir = TempDir::new("torn");
        let a = "amc-fsk@0.1.0#0123abcd";
        {
            let s = ShadowStore::open(&dir.0).unwrap();
            s.append(&record(T0, a, Some(22.0), "2fsk", Some("2fsk"))).unwrap();
            s.append(&record(T0 + 1, a, Some(22.0), "2fsk", Some("2fsk"))).unwrap();
        }
        let path = segment_path(&dir.0, hour_of(Timestamp::from_unix_nanos(T0)));
        let mut text = fs::read_to_string(&path).unwrap();
        // Flip a digit inside the first record's JSON, and leave a torn half-line at the end.
        let i = text.find("22.0").unwrap();
        text.replace_range(i..i + 2, "99");
        text.push_str("0badc0de {\"schema\":1");
        fs::write(&path, &text).unwrap();

        let s = ShadowStore::open(&dir.0).unwrap();
        let st = s.stats();
        assert_eq!(st.records, 1, "only the intact line survives");
        assert_eq!(st.corrupt_lines, 2, "the flipped line and the torn tail");
        assert!(fs::read_to_string(&path).unwrap().ends_with('\n'));
        s.append(&record(T0 + 2, a, Some(22.0), "2fsk", Some("2fsk"))).unwrap();
        assert_eq!(s.query(&ShadowQuery::default()).unwrap().len(), 2);
    }

    #[test]
    fn retention_drops_whole_hours_by_sample_age_then_by_bytes() {
        let dir = TempDir::new("retain");
        let a = "amc-fsk@0.1.0#0123abcd";
        let s = ShadowStore::open_with(
            &dir.0,
            ShadowStoreConfig {
                max_bytes: u64::MAX,
                max_age_ns: 2 * HOUR_NS,
            },
        )
        .unwrap();
        for h in 0..5 {
            s.append(&record(T0 + h * HOUR_NS, a, Some(22.0), "2fsk", Some("2fsk")))
                .unwrap();
        }
        let st = s.stats();
        assert_eq!(st.segments, 3, "hours 2, 3 and 4 are within 2 h of the newest");
        assert_eq!(st.oldest.unwrap().as_unix_nanos(), T0 + 2 * HOUR_NS);
        let agg: u64 = s.aggregates(&ShadowQuery::default()).unwrap().iter().map(|r| r.counts.n).sum();
        assert_eq!(agg, 3, "aggregates describe what is on disk, never an all-time tally");

        let line = encode_line(&record(T0, a, Some(22.0), "2fsk", Some("2fsk"))).len() as u64;
        let dir2 = TempDir::new("retain-bytes");
        let s = ShadowStore::open_with(
            &dir2.0,
            ShadowStoreConfig {
                max_bytes: 2 * line + line / 2,
                max_age_ns: DEFAULT_MAX_AGE_NS,
            },
        )
        .unwrap();
        for h in 0..4 {
            s.append(&record(T0 + h * HOUR_NS, a, Some(22.0), "2fsk", Some("2fsk")))
                .unwrap();
        }
        let st = s.stats();
        assert_eq!(st.segments, 2);
        assert!(st.bytes <= 2 * line + line / 2);
        assert_eq!(st.segments_deleted, 2);
    }

    #[test]
    fn only_shadow_predictions_are_kept_here() {
        let dir = TempDir::new("mode");
        let s = ShadowStore::open(&dir.0).unwrap();
        let mut r = record(T0, "amc-fsk@0.1.0#0123abcd", Some(22.0), "2fsk", Some("2fsk"));
        r.prediction.mode = "active".into();
        assert!(s.append(&r).is_err());
        let mut r2 = record(T0, "amc-fsk@0.1.0#0123abcd", Some(22.0), "2fsk", Some("2fsk"));
        r2.snr_bin_db = Some(0);
        assert!(s.append(&r2).is_err(), "a bin that is not the SNR's bin is refused");
        let st = s.stats();
        assert_eq!((st.records, st.refused), (0, 2));
    }

    #[test]
    fn model_filters_accept_an_id_a_version_or_the_full_reference() {
        let dir = TempDir::new("filter");
        let s = ShadowStore::open(&dir.0).unwrap();
        s.append(&record(T0, "amc-fsk@0.1.0#0123abcd", Some(22.0), "2fsk", Some("2fsk"))).unwrap();
        s.append(&record(T0 + 1, "amc-fsk@0.2.0#89abcdef", Some(22.0), "2fsk", Some("2fsk"))).unwrap();
        s.append(&record(T0 + 2, "amc-fskx@0.1.0#00000000", Some(22.0), "2fsk", Some("2fsk"))).unwrap();
        let n = |m: &str| {
            s.query(&ShadowQuery {
                model: Some(m.into()),
                ..ShadowQuery::default()
            })
            .unwrap()
            .len()
        };
        assert_eq!(n("amc-fsk"), 2);
        assert_eq!(n("amc-fsk@0.1.0"), 1);
        assert_eq!(n("amc-fsk@0.1.0#0123abcd"), 1);
        assert_eq!(n("amc"), 0);
        let rows = s
            .aggregates(&ShadowQuery {
                model: Some("amc-fsk".into()),
                ..ShadowQuery::default()
            })
            .unwrap();
        assert_eq!(rows.len(), 2);
    }
}
