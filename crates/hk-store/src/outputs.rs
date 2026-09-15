//! Output files (T-061, workflow steps 5-7): the writers behind "record outputs to files" and the
//! SigMF-style JSON sidecar written next to every output file.
//!
//! - [`WavWriter`]: 16-bit PCM WAV (RIFF), sizes patched on [`WavWriter::finalize`]; [`WavInfo`]
//!   reads a header back.
//! - [`OutputSidecar`]: `<stem>.json` next to `<stem>.<ext>`: capture settings, band, target,
//!   estimated parameters, refined tuning with provenance, framing, timestamps, counts, the rows
//!   the file is linked to, and the software version. Written atomically (temp file + rename).
//! - [`dir_usage`]: bytes under a directory (the global output quota).

use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use hk_model::ContentClass;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `schema` of every sidecar.
pub const SIDECAR_SCHEMA: &str = "hackriff-output-sidecar";
/// Sidecar version.
pub const SIDECAR_VERSION: &str = "1.0";

/// A 16-bit PCM WAV file being written.
pub struct WavWriter {
    out: BufWriter<File>,
    data_bytes: u64,
    sample_rate: u32,
    channels: u16,
}

/// Largest WAV data chunk (the RIFF size field is 32-bit).
pub const WAV_MAX_DATA_BYTES: u64 = u32::MAX as u64 - 36;

impl WavWriter {
    /// Creates `path` with a header whose sizes are patched when finalised.
    pub fn create(path: &Path, sample_rate: u32, channels: u16) -> io::Result<Self> {
        let mut w = Self {
            out: BufWriter::new(File::create(path)?),
            data_bytes: 0,
            sample_rate,
            channels,
        };
        w.write_header()?;
        Ok(w)
    }

    fn write_header(&mut self) -> io::Result<()> {
        let block = self.channels * 2;
        let data = self.data_bytes.min(WAV_MAX_DATA_BYTES) as u32;
        let mut h = Vec::with_capacity(44);
        h.extend_from_slice(b"RIFF");
        h.extend_from_slice(&(36 + data).to_le_bytes());
        h.extend_from_slice(b"WAVEfmt ");
        h.extend_from_slice(&16u32.to_le_bytes());
        h.extend_from_slice(&1u16.to_le_bytes());
        h.extend_from_slice(&self.channels.to_le_bytes());
        h.extend_from_slice(&self.sample_rate.to_le_bytes());
        h.extend_from_slice(&(self.sample_rate * u32::from(block)).to_le_bytes());
        h.extend_from_slice(&block.to_le_bytes());
        h.extend_from_slice(&16u16.to_le_bytes());
        h.extend_from_slice(b"data");
        h.extend_from_slice(&data.to_le_bytes());
        self.out.write_all(&h)
    }

    /// Appends little-endian `i16` samples given as bytes (an odd trailing byte is ignored).
    pub fn write_pcm_le(&mut self, bytes: &[u8]) -> io::Result<()> {
        let n = bytes.len() & !1;
        self.out.write_all(&bytes[..n])?;
        self.data_bytes += n as u64;
        Ok(())
    }

    /// Appends `samples` zero samples (per channel frame count × channels).
    pub fn write_silence(&mut self, samples: usize) -> io::Result<()> {
        let zeros = [0u8; 4096];
        let mut left = samples * 2;
        while left > 0 {
            let n = left.min(zeros.len());
            self.write_pcm_le(&zeros[..n])?;
            left -= n;
        }
        Ok(())
    }

    /// PCM bytes written so far (header excluded).
    pub fn data_bytes(&self) -> u64 {
        self.data_bytes
    }

    /// Patches the RIFF and data sizes and flushes; returns the data bytes.
    pub fn finalize(mut self) -> io::Result<u64> {
        self.out.flush()?;
        self.out.seek(SeekFrom::Start(0))?;
        self.write_header()?;
        self.out.flush()?;
        Ok(self.data_bytes)
    }
}

/// A WAV header read back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WavInfo {
    /// Sample rate, Hz.
    pub sample_rate: u32,
    /// Channels.
    pub channels: u16,
    /// Bits per sample.
    pub bits_per_sample: u16,
    /// Data chunk size from the header, bytes.
    pub data_bytes: u32,
    /// RIFF size from the header (file length − 8).
    pub riff_bytes: u32,
}

impl WavInfo {
    /// Reads the canonical 44-byte header of `path`.
    pub fn read(path: &Path) -> io::Result<Self> {
        let mut h = [0u8; 44];
        File::open(path)?.read_exact(&mut h)?;
        let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, m.to_owned());
        if &h[0..4] != b"RIFF" || &h[8..16] != b"WAVEfmt " || &h[36..40] != b"data" {
            return Err(bad("not a canonical PCM WAV header"));
        }
        let u16_at = |i: usize| u16::from_le_bytes([h[i], h[i + 1]]);
        let u32_at = |i: usize| u32::from_le_bytes([h[i], h[i + 1], h[i + 2], h[i + 3]]);
        Ok(Self {
            riff_bytes: u32_at(4),
            channels: u16_at(22),
            sample_rate: u32_at(24),
            bits_per_sample: u16_at(34),
            data_bytes: u32_at(40),
        })
    }
}

/// Capture settings of the window an output was recorded from.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CaptureSettings {
    /// Device id (e.g. `mock:...`, `hackrf:<serial>`).
    pub device_id: String,
    /// Hardware description, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hw: Option<String>,
    /// Tuned centre, Hz.
    pub center_hz: f64,
    /// Sample rate, Hz.
    pub sample_rate_hz: f64,
    /// LNA gain, dB.
    pub lna_db: f64,
    /// VGA gain, dB.
    pub vga_db: f64,
    /// RF amplifier on.
    pub amp_on: bool,
    /// Baseband filter bandwidth, Hz.
    pub baseband_filter_hz: f64,
    /// Antenna port, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub antenna_port: Option<String>,
    /// Content class of the run segment.
    pub source_class: ContentClass,
}

/// A frequency band, Hz.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Band {
    /// Lower edge, Hz.
    pub f_lo_hz: f64,
    /// Upper edge, Hz.
    pub f_hi_hz: f64,
}

/// Timestamps (Unix nanoseconds plus ISO-8601 UTC).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OutputTimes {
    /// First record or sample (stream time), ns.
    pub start_ns: Option<i64>,
    /// Last record or sample end, ns.
    pub end_ns: Option<i64>,
    /// ISO-8601 of `start_ns`.
    pub start: Option<String>,
    /// ISO-8601 of `end_ns`.
    pub end: Option<String>,
    /// When the recording was started (host clock), ISO-8601.
    pub started_at: String,
    /// When the file was finalised (host clock), ISO-8601.
    pub finalised_at: String,
}

/// Counts.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OutputStats {
    /// Data bytes written.
    pub bytes: u64,
    /// Records (audio frames, bursts) or sample chunks written.
    pub records: u64,
    /// Records dropped (stream queue drops plus records refused by a byte limit).
    pub dropped_records: u64,
    /// IQ samples lost to ring overruns.
    pub lost_samples: u64,
}

/// Inventory rows the file is linked to.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OutputRows {
    /// `Recording` row (audio, IQ).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording_id: Option<String>,
    /// `Bitstream` row (bits, symbols).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitstream_id: Option<String>,
    /// Selection whose links carry the row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_id: Option<String>,
}

/// Software that wrote the file.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Software {
    /// `hackriff`.
    pub name: String,
    /// Workspace version.
    pub version: String,
    /// Writing component and versions (e.g. `hk-pipeline:outputs`, demodulator id).
    pub component: String,
}

/// The JSON sidecar next to an output file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OutputSidecar {
    /// [`SIDECAR_SCHEMA`].
    pub schema: String,
    /// [`SIDECAR_VERSION`].
    pub version: String,
    /// Output session id.
    pub session_id: String,
    /// `bits`, `symbols`, `audio` or `iq`.
    pub kind: String,
    /// Data file name (same directory).
    pub data_file: String,
    /// Element format: `ru8` (one byte per bit), `rf32_le`, `wav-s16le-48k-mono`, `ci8`.
    pub datatype: String,
    /// Content class the data was produced under.
    pub content_class: ContentClass,
    /// Capture settings.
    pub capture: CaptureSettings,
    /// Requested band.
    pub band: Band,
    /// Requested target: `selection_id`, `emitter_id` or a band.
    pub target: Value,
    /// Parameters estimated from the signal (mode, bandwidth, pilot, symbol rate...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated: Option<Value>,
    /// Refined tuning from output analysis (T-070) with its provenance, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refined_tuning: Option<Value>,
    /// Framing (sync, CRC, bit order) for bits and symbols.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub framing: Option<Value>,
    /// Timestamps.
    pub time: OutputTimes,
    /// Counts.
    pub stats: OutputStats,
    /// Linked rows.
    pub rows: OutputRows,
    /// Why the file ended (`stopped`, `max_s reached`, `max_bytes reached`, `quota`, ...).
    pub ended: String,
    /// Software.
    pub software: Software,
}

impl Default for CaptureSettings {
    fn default() -> Self {
        Self {
            device_id: String::new(),
            hw: None,
            center_hz: 0.0,
            sample_rate_hz: 0.0,
            lna_db: 0.0,
            vga_db: 0.0,
            amp_on: false,
            baseband_filter_hz: 0.0,
            antenna_port: None,
            source_class: ContentClass::FAIL_CLOSED,
        }
    }
}

impl Default for OutputSidecar {
    fn default() -> Self {
        Self {
            schema: SIDECAR_SCHEMA.into(),
            version: SIDECAR_VERSION.into(),
            session_id: String::new(),
            kind: String::new(),
            data_file: String::new(),
            datatype: String::new(),
            content_class: ContentClass::FAIL_CLOSED,
            capture: CaptureSettings::default(),
            band: Band::default(),
            target: Value::Null,
            estimated: None,
            refined_tuning: None,
            framing: None,
            time: OutputTimes::default(),
            stats: OutputStats::default(),
            rows: OutputRows::default(),
            ended: String::new(),
            software: Software::default(),
        }
    }
}

impl OutputSidecar {
    /// Writes the sidecar atomically (a temp file renamed over `path`).
    pub fn write(&self, path: &Path) -> io::Result<()> {
        let tmp = path.with_extension("json.tmp");
        let body = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        fs::write(&tmp, body)?;
        fs::rename(&tmp, path)
    }

    /// Reads a sidecar.
    pub fn read(path: &Path) -> io::Result<Self> {
        serde_json::from_slice(&fs::read(path)?)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

/// Total bytes of the regular files under `dir` (0 when it does not exist). Symlinks are not
/// followed.
pub fn dir_usage(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_usage(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map_or(0, |m| m.len()),
            _ => 0,
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("hk-store-outputs-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn wav_header_is_patched_on_finalize() {
        let d = tmp("wav");
        let p = d.join("a.wav");
        let mut w = WavWriter::create(&p, 48_000, 1).unwrap();
        w.write_pcm_le(&[1, 0, 255, 127, 9]).unwrap();
        w.write_silence(10).unwrap();
        assert_eq!(w.finalize().unwrap(), 24);
        let info = WavInfo::read(&p).unwrap();
        assert_eq!(
            info,
            WavInfo {
                sample_rate: 48_000,
                channels: 1,
                bits_per_sample: 16,
                data_bytes: 24,
                riff_bytes: 60,
            }
        );
        assert_eq!(fs::metadata(&p).unwrap().len(), 68);
        assert_eq!(dir_usage(&d), 68);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn sidecar_round_trips() {
        let d = tmp("sidecar");
        let p = d.join("bits.json");
        let s = OutputSidecar {
            schema: SIDECAR_SCHEMA.into(),
            version: SIDECAR_VERSION.into(),
            kind: "bits".into(),
            framing: Some(serde_json::json!({ "sync_word_hex": "2dd4" })),
            ..OutputSidecar::default()
        };
        s.write(&p).unwrap();
        assert_eq!(OutputSidecar::read(&p).unwrap(), s);
        let _ = fs::remove_dir_all(&d);
    }
}
