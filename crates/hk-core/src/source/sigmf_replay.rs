//! SigMF file-replay source: deterministic playback of a `.sigmf-meta` + `.sigmf-data` pair
//! through the [`Source`] trait, the basis of offline tests (T3/T4, docs/10).
//!
//! - **Formats:** ci8, cu8, ci16_le and cf32_le, normalised to `Complex32` ([`super::format`]).
//! - **Determinism:** samples, sample counters, times and flags depend only on the files and
//!   [`ReplayOptions::block_len`]. [`Pacing::RealTime`] only adds sleeps.
//! - **Segments:** each `captures` entry is a segment; blocks never span two. The first block
//!   of a segment carries [`Discontinuity`] flags for what changed (retune, rate, gain,
//!   provenance). `core:global_index` jumps become [`Discontinuity::GAP`] with an exact
//!   `dropped_before`, so gaps in a recording stay visible instead of being spliced.
//! - **Sample counter:** the stream index is `core:global_index` when present, else the data-file
//!   index.
//! - **Provenance:** a capture's `hackriff:provenance`, else the global one (centre frequency
//!   taken from the capture's `core:frequency`), else a synthesised record with
//!   [`TimestampMethod::Unknown`], device `sigmf:<core:hw>` and unknown (zero) gains.
//! - **Time:** a capture's `core:datetime` anchors its first sample. Without one, the time is
//!   extrapolated from the previous segment, and the first segment defaults to the Unix epoch.
//!
//! Not supported (explicit errors): multi-channel files, real (non-complex) datatypes, a first
//! capture not at sample 0, data ending mid-sample.

use std::fs::File;
use std::io::{BufReader, ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use hk_model::sigmf::{Capture, Datatype, SigmfMeta, data_path_for};
use hk_model::{ClockSource, Provenance, SampleTime, Timestamp, TimestampMethod, Tune};
use num_complex::{Complex, Complex32};

use super::{
    Duplex, FrequencyRange, Gains, SampleRates, Source, SourceCapabilities, SourceError,
    SourceKind, format,
};
use crate::block::{BlockHeader, Discontinuity, ProvenanceHandle};

const NAME: &str = "sigmf-replay";

/// How fast to replay.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Pacing {
    /// As fast as the consumer reads.
    #[default]
    Unpaced,
    /// Release each block when its last sample would have arrived live, times `speed`
    /// (1.0 = real time, 2.0 = twice as fast).
    RealTime {
        /// Playback speed multiplier, > 0.
        speed: f64,
    },
}

/// Replay options.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ReplayOptions {
    /// Maximum samples per block (> 0). Blocks are shorter at segment boundaries and at the end.
    pub block_len: usize,
    /// Pacing.
    pub pacing: Pacing,
}

impl Default for ReplayOptions {
    fn default() -> Self {
        Self {
            block_len: 16_384,
            pacing: Pacing::Unpaced,
        }
    }
}

/// One capture segment, resolved at open time.
#[derive(Clone, Debug)]
struct Segment {
    /// First data-file sample index.
    file_start: u64,
    /// One past the last data-file sample index; `None` means "to end of data".
    file_end: Option<u64>,
    /// Stream sample counter of `file_start`.
    counter_start: u64,
    /// Host time of `file_start`.
    anchor: Timestamp,
    provenance: ProvenanceHandle,
    /// Flags for the segment's first block.
    flags: Discontinuity,
    /// Samples missing before the segment (from `core:global_index`).
    dropped_before: u64,
}

/// Replays a SigMF recording. See the [module docs](self).
pub struct SigmfReplaySource<R = BufReader<File>> {
    meta: SigmfMeta,
    data_path: Option<PathBuf>,
    reader: R,
    datatype: Datatype,
    sample_rate_hz: f64,
    total_samples: Option<u64>,
    capabilities: SourceCapabilities,
    segments: Vec<Segment>,
    segment: usize,
    file_pos: u64,
    byte_buf: Vec<u8>,
    options: ReplayOptions,
    pace_origin: Option<Instant>,
    emitted: u64,
    stopped: bool,
}

impl SigmfReplaySource<BufReader<File>> {
    /// Opens `<name>.sigmf-meta` and its `<name>.sigmf-data`.
    pub fn open(meta_path: impl AsRef<Path>, options: ReplayOptions) -> Result<Self, SourceError> {
        let meta_path = meta_path.as_ref();
        let meta = SigmfMeta::read(meta_path)?;
        let data_path = data_path_for(meta_path);
        let io_err = |source| SourceError::Io {
            path: Some(data_path.clone()),
            source,
        };
        let file = File::open(&data_path).map_err(io_err)?;
        let len = file.metadata().map_err(io_err)?.len();
        let bps = meta.global.datatype.bytes_per_sample() as u64;
        if len % bps != 0 {
            return Err(SourceError::InvalidRecording(format!(
                "{}: {len} bytes is not a whole number of {} samples",
                data_path.display(),
                meta.global.datatype
            )));
        }
        let mut source = Self::build(meta, BufReader::new(file), Some(len / bps), options)?;
        source.data_path = Some(data_path);
        Ok(source)
    }
}

impl<R: Read + Send> SigmfReplaySource<R> {
    /// Replays `meta` over sample bytes from `reader` (e.g. an in-memory buffer). The total
    /// length is unknown up front; the last segment runs to end of input.
    pub fn from_reader(
        meta: SigmfMeta,
        reader: R,
        options: ReplayOptions,
    ) -> Result<Self, SourceError> {
        Self::build(meta, reader, None, options)
    }

    fn build(
        meta: SigmfMeta,
        reader: R,
        total_samples: Option<u64>,
        options: ReplayOptions,
    ) -> Result<Self, SourceError> {
        let datatype = meta.global.datatype;
        if !format::supports(datatype) {
            return Err(SourceError::UnsupportedDatatype(datatype));
        }
        if let Some(channels) = meta.global.extra.get("core:num_channels") {
            if channels.as_u64() != Some(1) {
                return Err(SourceError::InvalidRecording(format!(
                    "core:num_channels {channels} is not supported (single channel only)"
                )));
            }
        }
        let sample_rate_hz = meta
            .global
            .sample_rate
            .filter(|r| r.is_finite() && *r > 0.0)
            .ok_or_else(|| {
                SourceError::InvalidRecording("missing or invalid core:sample_rate".into())
            })?;
        if options.block_len == 0 {
            return Err(SourceError::OutOfRange {
                what: "block_len",
                value: 0.0,
            });
        }
        if let Pacing::RealTime { speed } = options.pacing {
            if !(speed.is_finite() && speed > 0.0) {
                return Err(SourceError::OutOfRange {
                    what: "pacing speed",
                    value: speed,
                });
            }
        }

        let segments = plan_segments(&meta, sample_rate_hz, total_samples)?;
        let capabilities = replay_capabilities(&meta, &segments, sample_rate_hz);
        let byte_buf = vec![0u8; options.block_len * datatype.bytes_per_sample()];
        Ok(Self {
            meta,
            data_path: None,
            reader,
            datatype,
            sample_rate_hz,
            total_samples,
            capabilities,
            segments,
            segment: 0,
            file_pos: 0,
            byte_buf,
            options,
            pace_origin: None,
            emitted: 0,
            stopped: false,
        })
    }

    /// The recording's metadata.
    pub fn meta(&self) -> &SigmfMeta {
        &self.meta
    }

    /// Samples in the data file, when known (always for [`SigmfReplaySource::open`]).
    pub fn total_samples(&self) -> Option<u64> {
        self.total_samples
    }

    /// Recording sample rate, Hz.
    pub fn sample_rate_hz(&self) -> f64 {
        self.sample_rate_hz
    }

    /// Fills the first `len` bytes of the byte buffer; returns how many were read (short at EOF).
    fn fill_bytes(&mut self, len: usize) -> Result<usize, SourceError> {
        let buf = &mut self.byte_buf[..len];
        let mut filled = 0;
        while filled < len {
            match self.reader.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(k) => filled += k,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(source) => {
                    return Err(SourceError::Io {
                        path: self.data_path.clone(),
                        source,
                    });
                }
            }
        }
        Ok(filled)
    }

    fn pace(&mut self) {
        if let Pacing::RealTime { speed } = self.options.pacing {
            let origin = *self.pace_origin.get_or_insert_with(Instant::now);
            let due = Duration::from_secs_f64(self.emitted as f64 / (self.sample_rate_hz * speed));
            let elapsed = origin.elapsed();
            if due > elapsed {
                std::thread::sleep(due - elapsed);
            }
        }
    }

    fn unsupported(operation: &'static str) -> SourceError {
        SourceError::Unsupported {
            source_name: NAME,
            operation,
        }
    }
}

impl<R: Read + Send> Source for SigmfReplaySource<R> {
    fn capabilities(&self) -> &SourceCapabilities {
        &self.capabilities
    }

    fn tune(&mut self, _center_hz: f64) -> Result<(), SourceError> {
        Err(Self::unsupported("tune"))
    }

    fn set_sample_rate(&mut self, _sample_rate_hz: f64) -> Result<(), SourceError> {
        Err(Self::unsupported("set_sample_rate"))
    }

    fn set_gains(&mut self, _gains: &Gains) -> Result<(), SourceError> {
        Err(Self::unsupported("set_gains"))
    }

    fn start(&mut self) -> Result<(), SourceError> {
        Ok(())
    }

    fn stop(&mut self) -> Result<(), SourceError> {
        self.stopped = true;
        Ok(())
    }

    fn read_block(
        &mut self,
        samples: &mut Vec<Complex32>,
    ) -> Result<Option<BlockHeader>, SourceError> {
        samples.clear();
        let Some((header, got)) = self.next_raw()? else {
            return Ok(None);
        };
        format::decode_into(self.datatype, &self.byte_buf[..got], samples)?;
        Ok(Some(header))
    }

    fn read_block_ci8(
        &mut self,
        samples: &mut Vec<Complex<i8>>,
    ) -> Result<Option<BlockHeader>, SourceError> {
        samples.clear();
        let flip = match self.datatype {
            Datatype::Ci8 => 0,
            Datatype::Cu8 => 0x80,
            _ => {
                return Err(Self::unsupported(
                    "read_block_ci8 (datatype is not ci8/cu8)",
                ));
            }
        };
        let Some((header, got)) = self.next_raw()? else {
            return Ok(None);
        };
        samples.extend(
            self.byte_buf[..got]
                .chunks_exact(2)
                .map(|c| Complex::new((c[0] ^ flip) as i8, (c[1] ^ flip) as i8)),
        );
        Ok(Some(header))
    }
}

impl<R: Read + Send> SigmfReplaySource<R> {
    /// Reads the next block's raw bytes into the byte buffer; returns its header and byte count.
    fn next_raw(&mut self) -> Result<Option<(BlockHeader, usize)>, SourceError> {
        if self.stopped {
            return Ok(None);
        }
        let bps = self.datatype.bytes_per_sample();
        loop {
            let Some(seg) = self.segments.get(self.segment) else {
                return Ok(None);
            };
            let remaining = seg.file_end.map(|end| end - self.file_pos);
            if remaining == Some(0) {
                self.segment += 1;
                continue;
            }
            let file_end = seg.file_end;
            let want = remaining.map_or(self.options.block_len, |r| {
                r.min(self.options.block_len as u64) as usize
            });
            let got = self.fill_bytes(want * bps)?;
            if got % bps != 0 {
                return Err(SourceError::InvalidRecording(
                    "sample data ends mid-sample".into(),
                ));
            }
            let n = got / bps;
            if n == 0 {
                if file_end.is_some() {
                    return Err(SourceError::InvalidRecording(format!(
                        "sample data ends at sample {} but the captures extend further",
                        self.file_pos
                    )));
                }
                self.segment = self.segments.len();
                return Ok(None);
            }
            let seg = &self.segments[self.segment];
            let at_start = self.file_pos == seg.file_start;
            let counter = seg.counter_start + (self.file_pos - seg.file_start);
            let anchor = SampleTime {
                sample_index: seg.counter_start,
                host_time: seg.anchor,
            };
            let header = BlockHeader {
                time: SampleTime {
                    sample_index: counter,
                    host_time: anchor.time_of(counter, self.sample_rate_hz),
                },
                provenance: seg.provenance.clone(),
                discontinuity: if at_start {
                    seg.flags
                } else {
                    Discontinuity::NONE
                },
                dropped_before: if at_start { seg.dropped_before } else { 0 },
            };
            self.file_pos += n as u64;
            self.emitted += n as u64;
            self.pace();
            return Ok(Some((header, got)));
        }
    }
}

/// Resolves captures into segments with counters, anchors, provenance and flags.
fn plan_segments(
    meta: &SigmfMeta,
    sample_rate_hz: f64,
    total_samples: Option<u64>,
) -> Result<Vec<Segment>, SourceError> {
    let implicit;
    let captures: &[Capture] = if meta.captures.is_empty() {
        implicit = [Capture {
            sample_start: 0,
            frequency: None,
            datetime: None,
            provenance: None,
            extra: Default::default(),
        }];
        &implicit
    } else {
        &meta.captures
    };

    let mut segments: Vec<Segment> = Vec::with_capacity(captures.len());
    for (k, capture) in captures.iter().enumerate() {
        let invalid = |msg: String| SourceError::InvalidRecording(format!("capture {k}: {msg}"));
        let prev = segments.last();
        match prev {
            None if capture.sample_start != 0 => {
                return Err(invalid("the first capture must start at sample 0".into()));
            }
            Some(p) if capture.sample_start <= p.file_start => {
                return Err(invalid("core:sample_start is not increasing".into()));
            }
            _ => {}
        }
        if let (Some(total), Some(_)) = (total_samples, prev) {
            if capture.sample_start >= total {
                return Err(invalid(format!(
                    "core:sample_start {} is beyond the {total} samples of data",
                    capture.sample_start
                )));
            }
        }

        let global_index = match capture.extra.get("core:global_index") {
            None => None,
            Some(v) => Some(
                v.as_u64()
                    .ok_or_else(|| invalid(format!("invalid core:global_index {v}")))?,
            ),
        };
        let contiguous = prev.map(|p| p.counter_start + (capture.sample_start - p.file_start));
        let counter_start = global_index.or(contiguous).unwrap_or(0);
        let dropped_before = match contiguous {
            Some(expected) if counter_start < expected => {
                return Err(invalid("core:global_index goes backwards".into()));
            }
            Some(expected) => counter_start - expected,
            None => 0,
        };

        let provenance = match (&capture.provenance, &meta.global.provenance) {
            (Some(p), _) => p.clone(),
            (None, Some(global)) => {
                let mut p = global.clone();
                if let Some(f) = capture.frequency {
                    p.tune.center_hz = f;
                }
                p
            }
            (None, None) => synthesised_provenance(meta, capture.frequency, sample_rate_hz),
        };
        let rate = provenance.tune.sample_rate_hz;
        if (rate - sample_rate_hz).abs() > 1e-9 * sample_rate_hz {
            return Err(invalid(format!(
                "provenance sample rate {rate} Hz differs from core:sample_rate {sample_rate_hz} Hz"
            )));
        }

        let anchor = match (&capture.datetime, prev) {
            (Some(dt), _) => parse_sigmf_datetime(dt)
                .ok_or_else(|| invalid(format!("unparseable core:datetime {dt:?}")))?,
            (None, Some(p)) => SampleTime {
                sample_index: p.counter_start,
                host_time: p.anchor,
            }
            .time_of(counter_start, sample_rate_hz),
            (None, None) => Timestamp::UNIX_EPOCH,
        };

        let (handle, mut flags) = match prev {
            None => (
                ProvenanceHandle::new(provenance),
                Discontinuity::STREAM_START,
            ),
            Some(p) if *p.provenance.get() == provenance => {
                (p.provenance.clone(), Discontinuity::NONE)
            }
            Some(p) => {
                let flags = Discontinuity::between(p.provenance.get(), &provenance);
                (ProvenanceHandle::new(provenance), flags)
            }
        };
        if dropped_before > 0 {
            flags |= Discontinuity::GAP;
        }

        if let Some(p) = segments.last_mut() {
            p.file_end = Some(capture.sample_start);
        }
        segments.push(Segment {
            file_start: capture.sample_start,
            file_end: None,
            counter_start,
            anchor,
            provenance: handle,
            flags,
            dropped_before,
        });
    }
    if let Some(last) = segments.last_mut() {
        last.file_end = total_samples;
    }
    Ok(segments)
}

fn synthesised_provenance(
    meta: &SigmfMeta,
    frequency: Option<f64>,
    sample_rate_hz: f64,
) -> Provenance {
    Provenance {
        device_id: format!("sigmf:{}", meta.global.hw.as_deref().unwrap_or("unknown")),
        tune: Tune {
            center_hz: frequency.unwrap_or(0.0),
            sample_rate_hz,
            lna_db: 0.0,
            vga_db: 0.0,
            amp_on: false,
            bandwidth_hz: sample_rate_hz,
        },
        clip_count: 0,
        overload: false,
        temperature_c: None,
        antenna_port: None,
        clock_source: ClockSource::Internal,
        clock_locked: false,
        calibration_state_ref: None,
        spur_mask_ref: None,
        timestamp_method: TimestampMethod::Unknown,
        timestamp_error_budget_ns: None,
    }
}

fn replay_capabilities(
    meta: &SigmfMeta,
    segments: &[Segment],
    sample_rate_hz: f64,
) -> SourceCapabilities {
    let mut frequency_ranges: Vec<FrequencyRange> = Vec::new();
    for seg in segments {
        let c = seg.provenance.tune.center_hz;
        let range = FrequencyRange {
            min_hz: c - sample_rate_hz / 2.0,
            max_hz: c + sample_rate_hz / 2.0,
        };
        if !frequency_ranges.contains(&range) {
            frequency_ranges.push(range);
        }
    }
    SourceCapabilities {
        driver: NAME.into(),
        kind: SourceKind::Replay,
        frequency_ranges,
        sample_rates: SampleRates::Discrete(vec![sample_rate_hz]),
        adc_bits: (meta.global.datatype.component_bytes() * 8) as u8,
        native_format: meta.global.datatype,
        duplex: Duplex::ReceiveOnly,
        tx_capable: false,
        controllable: false,
        gain_stages: Vec::new(),
        rf_amp: false,
        bias_tee: false,
        external_clock: false,
        hardware_timestamps: false,
    }
}

/// Parses a SigMF `core:datetime` (RFC 3339: `YYYY-MM-DDTHH:MM:SS[.fraction](Z|±HH:MM)`) to a
/// [`Timestamp`]. Fractions beyond nanoseconds are truncated. Returns `None` if malformed.
pub fn parse_sigmf_datetime(s: &str) -> Option<Timestamp> {
    let b = s.as_bytes();
    let num = |from: usize, to: usize| -> Option<i64> {
        let digits = s.get(from..to)?;
        if digits.bytes().all(|c| c.is_ascii_digit()) {
            digits.parse().ok()
        } else {
            None
        }
    };
    let sep = |at: usize, allowed: &[u8]| b.get(at).is_some_and(|c| allowed.contains(c));
    if !(sep(4, b"-") && sep(7, b"-") && sep(10, b"Tt ") && sep(13, b":") && sep(16, b":")) {
        return None;
    }
    let (year, month, day) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (hour, minute, second) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !((1..=12).contains(&month)
        && (1..=31).contains(&day)
        && hour < 24
        && minute < 60
        && second <= 60)
    {
        return None;
    }

    let mut i = 19;
    let mut nanos = 0i64;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == start {
            return None;
        }
        let mut scale = 100_000_000;
        for &c in &b[start..i.min(start + 9)] {
            nanos += i64::from(c - b'0') * scale;
            scale /= 10;
        }
    }
    let offset_secs = match &s[i..] {
        "Z" | "z" => 0,
        rest if rest.len() == 6 && sep(i + 3, b":") => {
            let sign = match rest.as_bytes()[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let (oh, om) = (num(i + 1, i + 3)?, num(i + 4, i + 6)?);
            sign * (oh * 3600 + om * 60)
        }
        _ => return None,
    };

    let secs = days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second
        - offset_secs;
    Some(Timestamp::from_unix_nanos(
        secs.checked_mul(1_000_000_000)?.checked_add(nanos)?,
    ))
}

/// Days since 1970-01-01 for a proleptic Gregorian date (H. Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rfc3339_datetimes() {
        let ns = |s| parse_sigmf_datetime(s).map(Timestamp::as_unix_nanos);
        assert_eq!(ns("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(ns("2026-09-13T00:00:00Z"), Some(1_789_257_600_000_000_000));
        assert_eq!(
            ns("2026-09-13T00:00:00.000000500Z"),
            Some(1_789_257_600_000_000_500)
        );
        assert_eq!(
            ns("2026-09-13T00:00:00.25Z"),
            Some(1_789_257_600_250_000_000)
        );
        assert_eq!(
            ns("2026-09-13T02:00:00+02:00"),
            Some(1_789_257_600_000_000_000)
        );
        assert_eq!(ns("1969-12-31T23:59:59Z"), Some(-1_000_000_000));
        assert_eq!(ns("2026-09-13"), None);
        assert_eq!(ns("2026-13-13T00:00:00Z"), None);
        assert_eq!(ns("2026-09-13T00:00:00"), None);
        assert_eq!(ns("2026-09-13T00:00:00.Z"), None);
    }
}
