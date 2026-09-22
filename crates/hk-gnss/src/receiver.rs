//! The full GNSS receiver is **GNSS-SDR, wrapped as a C22 plugin**, not reimplemented here
//! (T-323; C36 card "Methods: wrap GNSS-SDR as a C22 plugin over IPC; don't reimplement tracking
//! loops").
//!
//! T-274 built acquisition only. Tracking loops (DLL/PLL), navigation-message decode, ephemeris
//! and PVT are what GNSS-SDR already does well, so this module is the *seam* to it, in three
//! pieces:
//!
//! 1. **[`GnssSdrRun::config_text`]** — the GNSS-SDR configuration for one recorded dwell: a
//!    `File_Signal_Source` over the dwell's `ci8` samples (`ibyte`), GPS L1 C/A acquisition,
//!    DLL/PLL tracking, telemetry decode and an RTKLIB single-point PVT, writing RINEX 3
//!    observations and NMEA into a scratch directory.
//! 2. **[`parse_rinex_obs`] and [`parse_nmea`]** — GNSS-SDR's *file* outputs read back. Text
//!    formats with public specifications (RINEX 3.0x, NMEA 0183) were chosen over GNSS-SDR's UDP
//!    protobuf monitor so that no generated code and no protobuf dependency enters the core.
//! 3. **[`ReceiverEpochEvidence`] / [`DwellSummary`]** — what the plugin wrapper
//!    (`src/bin/hk-plugin-gnss-sdr.rs`) emits as `decode` lines under schema
//!    [`GNSS_SDR_SCHEMA`], and what a consumer turns back into this crate's own
//!    [`GnssObservableEpoch`] ([`epochs_from_evidence`]), real C/N0 [`LockEvidence`]
//!    ([`lock_evidence`], AWARE-002) and per-satellite S4 ([`s4_by_prn`], PROP-033).
//!
//! # The licence boundary
//!
//! GNSS-SDR is **GPL-3.0-or-later** and is only ever *executed*: the wrapper spawns the
//! `gnss-sdr` binary with a generated config file and reads the files it writes. Nothing links
//! it, and no build script or FFI declaration exists in this crate
//! (`tests/gnss_sdr_boundary.rs` checks both, plus the ADR-0010 ledger row).
//!
//! # Evidence, never detection
//!
//! The wrapper consumes a dwell and emits evidence — exactly as the acquisition path does. It
//! emits only `decode` lines, **never an `identity`** (a decode with an identity upserts an
//! Emitter in `hk_plugins::Ingest`, which would make a satellite PRN an inventory row) and
//! **never an `annotation`** (annotations target the context's detection). So nothing the
//! receiver reports can create a Detection, an Emitter or a Candidate; it is data *about* a dwell
//! the scheduler already chose. `tests/gnss_sdr_plugin.rs` asserts the ingest upserted nothing.
//!
//! # Time
//!
//! RINEX epochs from GNSS-SDR are in GPS time. They are converted to UTC with the header's
//! `LEAP SECONDS` when present, else [`GPS_UTC_LEAP_S`] (18 s since 2017-01-01; no leap second
//! has been announced since — recheck if one is). Each epoch's `sample_index` on the decode line
//! is the **dwell start**, not the epoch's own sample: RINEX carries receiver time, not the
//! input sample counter, so the per-epoch sample alignment is not recoverable from these files.
//! The exact receiver time is in the evidence (`epoch.t`, `offset_s`).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use hk_model::Timestamp;

use crate::integrity::LockEvidence;
use crate::observable::{Ecef, GnssObservableEpoch, SvObservable, s4_index};
use crate::prn::L1_HZ;

/// `schema_id` of the plugin's decode lines (`plugins/gnss-sdr/manifest.json`).
pub const GNSS_SDR_SCHEMA: &str = "hackriff.gnss/1";
/// `frame_model` of one observation epoch.
pub const FRAME_EPOCH: &str = "gnss-sdr-epoch";
/// `frame_model` of the per-dwell summary.
pub const FRAME_DWELL: &str = "gnss-sdr-dwell";
/// GPS − UTC, seconds, since 2017-01-01 (used when the RINEX header has no `LEAP SECONDS`).
pub const GPS_UTC_LEAP_S: i64 = 18;
/// Lowest sample rate the wrapper accepts: the L1 C/A main lobe is 2.046 MHz wide.
pub const MIN_SAMPLE_RATE_HZ: f64 = 2.046e6;

/// Errors reading GNSS-SDR's output files.
#[derive(Clone, Debug, thiserror::Error, PartialEq)]
pub enum ReceiverError {
    /// Not a RINEX observation file.
    #[error("not a RINEX observation file: {0}")]
    NotObservation(String),
    /// A RINEX version this parser does not read.
    #[error("unsupported RINEX version {0}")]
    Version(String),
    /// A malformed line.
    #[error("RINEX line {line}: {what}")]
    Malformed {
        /// 1-based line number.
        line: usize,
        /// What was wrong (never the line's content).
        what: &'static str,
    },
}

/// One GNSS-SDR run over one recorded dwell.
#[derive(Clone, Debug, PartialEq)]
pub struct GnssSdrRun {
    /// The dwell's samples: interleaved signed 8-bit I/Q (`ci8`).
    pub input_path: String,
    /// Where GNSS-SDR writes RINEX and NMEA.
    pub output_dir: String,
    /// Input sample rate, Hz.
    pub sample_rate_hz: f64,
    /// RF centre the samples were captured at, Hz. When it is not L1 (tuned off-centre to keep
    /// the HackRF's DC spike off the signal), a frequency-translating filter moves L1 to 0 Hz.
    pub center_hz: f64,
    /// Tracking channels.
    pub channels: u32,
    /// Observation (and PVT) output interval, ms. 100 ms gives 10 Hz C/N0 for S4.
    pub obs_rate_ms: u32,
}

/// NMEA file name inside [`GnssSdrRun::output_dir`].
pub const NMEA_FILE: &str = "hackriff.nmea";

impl GnssSdrRun {
    /// L1 minus the capture centre: where L1 sits in the dwell's baseband, Hz.
    pub fn if_hz(&self) -> f64 {
        L1_HZ - self.center_hz
    }

    /// The GNSS-SDR configuration file text.
    ///
    /// **Unverified against a real GNSS-SDR install**: the dev Mac has none (no Homebrew
    /// formula), so the keys follow GNSS-SDR's documented `File_Signal_Source` /
    /// `GPS_L1_CA_*` / `RTKLIB_PVT` configuration and its HackRF example configs.
    /// `tests/gnss_sdr_plugin.rs::real_gnss_sdr_accepts_the_generated_config` runs the real
    /// binary when one is on `PATH`.
    pub fn config_text(&self) -> String {
        let fs = self.sample_rate_hz.round() as u64;
        let mut c = String::new();
        let mut kv = |k: &str, v: &dyn std::fmt::Display| {
            let _ = writeln!(c, "{k}={v}");
        };
        kv("GNSS-SDR.internal_fs_sps", &fs);
        kv("SignalSource.implementation", &"File_Signal_Source");
        kv("SignalSource.filename", &self.input_path);
        kv("SignalSource.item_type", &"ibyte");
        kv("SignalSource.sampling_frequency", &fs);
        kv("SignalSource.samples", &0);
        kv("SignalSource.repeat", &false);
        kv("SignalSource.enable_throttle_control", &false);
        kv("SignalConditioner.implementation", &"Signal_Conditioner");
        kv("DataTypeAdapter.implementation", &"Ibyte_To_Complex");
        let if_hz = self.if_hz();
        if if_hz.abs() < 1.0 {
            kv("InputFilter.implementation", &"Pass_Through");
            kv("InputFilter.item_type", &"gr_complex");
        } else {
            kv("InputFilter.implementation", &"Freq_Xlating_Fir_Filter");
            kv("InputFilter.input_item_type", &"gr_complex");
            kv("InputFilter.output_item_type", &"gr_complex");
            kv("InputFilter.taps_item_type", &"float");
            kv("InputFilter.number_of_taps", &5);
            kv("InputFilter.number_of_bands", &2);
            kv("InputFilter.band1_begin", &0.0);
            kv("InputFilter.band1_end", &0.45);
            kv("InputFilter.band2_begin", &0.55);
            kv("InputFilter.band2_end", &1.0);
            kv("InputFilter.ampl1_begin", &1.0);
            kv("InputFilter.ampl1_end", &1.0);
            kv("InputFilter.ampl2_begin", &0.0);
            kv("InputFilter.ampl2_end", &0.0);
            kv("InputFilter.band1_error", &1.0);
            kv("InputFilter.band2_error", &1.0);
            kv("InputFilter.filter_type", &"bandpass");
            kv("InputFilter.grid_density", &16);
            kv("InputFilter.sampling_frequency", &fs);
            kv("InputFilter.IF", &if_hz.round());
            kv("InputFilter.decimation_factor", &1);
        }
        kv("Resampler.implementation", &"Pass_Through");
        kv("Resampler.item_type", &"gr_complex");
        kv("Channels_1C.count", &self.channels);
        kv("Channels.in_acquisition", &1);
        kv("Channel.signal", &"1C");
        kv(
            "Acquisition_1C.implementation",
            &"GPS_L1_CA_PCPS_Acquisition",
        );
        kv("Acquisition_1C.item_type", &"gr_complex");
        kv("Acquisition_1C.coherent_integration_time_ms", &1);
        kv("Acquisition_1C.pfa", &0.01);
        // The HackRF One has no TCXO: its clock error widens the Doppler search (C36 card).
        kv("Acquisition_1C.doppler_max", &10000);
        kv("Acquisition_1C.doppler_step", &250);
        kv("Tracking_1C.implementation", &"GPS_L1_CA_DLL_PLL_Tracking");
        kv("Tracking_1C.item_type", &"gr_complex");
        kv("Tracking_1C.pll_bw_hz", &35.0);
        kv("Tracking_1C.dll_bw_hz", &2.0);
        kv(
            "TelemetryDecoder_1C.implementation",
            &"GPS_L1_CA_Telemetry_Decoder",
        );
        kv("Observables.implementation", &"Hybrid_Observables");
        kv("PVT.implementation", &"RTKLIB_PVT");
        kv("PVT.positioning_mode", &"Single");
        kv("PVT.iono_model", &"Broadcast");
        kv("PVT.trop_model", &"Saastamoinen");
        kv("PVT.output_rate_ms", &self.obs_rate_ms);
        kv("PVT.display_rate_ms", &1000);
        kv("PVT.output_path", &self.output_dir);
        kv("PVT.output_enabled", &true);
        kv("PVT.rinex_output_enabled", &true);
        kv("PVT.rinex_version", &3);
        kv("PVT.rinexobs_rate_ms", &self.obs_rate_ms);
        kv("PVT.nmea_output_file_enabled", &true);
        kv("PVT.nmea_dump_filename", &NMEA_FILE);
        kv("PVT.flag_nmea_tty_port", &false);
        kv("PVT.flag_rtcm_server", &false);
        kv("PVT.flag_rtcm_tty_port", &false);
        kv("PVT.kml_output_enabled", &false);
        kv("PVT.gpx_output_enabled", &false);
        kv("PVT.geojson_output_enabled", &false);
        kv("PVT.xml_output_enabled", &false);
        kv("PVT.enable_monitor", &false);
        kv("Monitor.enable_monitor", &false);
        c
    }
}

/// One satellite's measurements at a RINEX epoch.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RinexSat {
    /// GPS PRN.
    pub prn: u8,
    /// Pseudorange, m.
    pub pseudorange_m: Option<f64>,
    /// Carrier phase, cycles.
    pub carrier_cycles: Option<f64>,
    /// Doppler, Hz.
    pub doppler_hz: Option<f64>,
    /// Signal strength; GNSS-SDR writes C/N0 in dB-Hz.
    pub cn0_dbhz: Option<f64>,
}

/// One RINEX observation epoch (GPS satellites only).
#[derive(Clone, Debug, PartialEq)]
pub struct RinexEpoch {
    /// Epoch, UTC.
    pub t: Timestamp,
    /// GPS satellites observed.
    pub sats: Vec<RinexSat>,
}

/// Parsed RINEX observations.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct RinexObs {
    /// Epochs in file order.
    pub epochs: Vec<RinexEpoch>,
    /// Satellite records of other constellations, skipped (this wrapper configures GPS L1 only).
    pub skipped_non_gps: usize,
}

/// Days since 1970-01-01 of a proleptic Gregorian date.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (i64::from(m) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// A calendar time (in whatever system it was written) as nanoseconds since the Unix epoch.
fn calendar_nanos(y: i64, mo: u32, d: u32, h: u32, mi: u32, sec: f64) -> i64 {
    let days = days_from_civil(y, mo, d);
    let whole = days * 86_400 + i64::from(h) * 3600 + i64::from(mi) * 60;
    whole * 1_000_000_000 + (sec * 1e9).round() as i64
}

/// Parses a RINEX 3.0x observation file (GNSS-SDR's `PVT.rinex_version=3` output).
///
/// Keeps GPS satellites and the first pseudorange (`C`), phase (`L`), Doppler (`D`) and signal
/// strength (`S`) observable of each; skips event epochs (flag > 1) with their special records.
pub fn parse_rinex_obs(text: &str) -> Result<RinexObs, ReceiverError> {
    let mut lines = text.lines().enumerate();
    let (_, first) = lines
        .next()
        .ok_or(ReceiverError::NotObservation("empty".into()))?;
    if !first
        .get(60..)
        .unwrap_or("")
        .contains("RINEX VERSION / TYPE")
    {
        return Err(ReceiverError::NotObservation("no version line".into()));
    }
    let version = first.get(0..9).unwrap_or("").trim().to_string();
    if first.chars().nth(20) != Some('O') {
        return Err(ReceiverError::NotObservation("type is not O".into()));
    }
    if !version.starts_with('3') {
        return Err(ReceiverError::Version(version));
    }

    let mut types: BTreeMap<char, Vec<String>> = BTreeMap::new();
    let mut current_sys: Option<char> = None;
    let mut leap = GPS_UTC_LEAP_S;
    let mut header_done = false;
    for (_, line) in lines.by_ref() {
        let label = line.get(60..).unwrap_or("").trim();
        let body = line.get(..60.min(line.len())).unwrap_or("");
        match label {
            "SYS / # / OBS TYPES" => {
                let sys = body.chars().next().unwrap_or(' ');
                let names = body.get(6..).unwrap_or("").split_whitespace();
                if sys != ' ' {
                    current_sys = Some(sys);
                    types
                        .entry(sys)
                        .or_default()
                        .extend(names.map(str::to_string));
                } else if let Some(s) = current_sys {
                    types
                        .entry(s)
                        .or_default()
                        .extend(names.map(str::to_string));
                }
            }
            "LEAP SECONDS" => {
                if let Some(v) = body.split_whitespace().next().and_then(|v| v.parse().ok()) {
                    leap = v;
                }
            }
            "END OF HEADER" => {
                header_done = true;
                break;
            }
            _ => {}
        }
    }
    if !header_done {
        return Err(ReceiverError::NotObservation("no END OF HEADER".into()));
    }
    let gps = types.get(&'G').cloned().unwrap_or_default();
    let col = |class: char| gps.iter().position(|t| t.starts_with(class));
    let (ci, li, di, si) = (col('C'), col('L'), col('D'), col('S'));

    let mut out = RinexObs::default();
    while let Some((n, line)) = lines.next() {
        let lineno = n + 1;
        if line.trim().is_empty() {
            continue;
        }
        let Some(rest) = line.strip_prefix('>') else {
            return Err(ReceiverError::Malformed {
                line: lineno,
                what: "expected an epoch record",
            });
        };
        let f: Vec<&str> = rest.split_whitespace().collect();
        if f.len() < 8 {
            return Err(ReceiverError::Malformed {
                line: lineno,
                what: "short epoch record",
            });
        }
        let bad = ReceiverError::Malformed {
            line: lineno,
            what: "bad epoch field",
        };
        let y: i64 = f[0].parse().map_err(|_| bad.clone())?;
        let mo: u32 = f[1].parse().map_err(|_| bad.clone())?;
        let d: u32 = f[2].parse().map_err(|_| bad.clone())?;
        let h: u32 = f[3].parse().map_err(|_| bad.clone())?;
        let mi: u32 = f[4].parse().map_err(|_| bad.clone())?;
        let sec: f64 = f[5].parse().map_err(|_| bad.clone())?;
        let flag: u32 = f[6].parse().map_err(|_| bad.clone())?;
        let count: usize = f[7].parse().map_err(|_| bad.clone())?;
        if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 {
            return Err(bad);
        }
        if flag > 1 {
            // An event epoch: `count` special records follow, not satellites.
            for _ in 0..count {
                lines.next();
            }
            continue;
        }
        let t =
            Timestamp::from_unix_nanos(calendar_nanos(y, mo, d, h, mi, sec) - leap * 1_000_000_000);
        let mut sats = Vec::with_capacity(count);
        for _ in 0..count {
            let Some((m, sl)) = lines.next() else {
                return Err(ReceiverError::Malformed {
                    line: lineno,
                    what: "epoch ends before its satellites",
                });
            };
            let id = sl.get(0..3).unwrap_or("");
            let sys = id.chars().next().unwrap_or(' ');
            if sys != 'G' {
                out.skipped_non_gps += 1;
                continue;
            }
            let prn: u8 = id[1..]
                .trim()
                .parse()
                .map_err(|_| ReceiverError::Malformed {
                    line: m + 1,
                    what: "bad satellite id",
                })?;
            let value = |i: Option<usize>| -> Option<f64> {
                let i = i?;
                let start = 3 + 16 * i;
                let field = sl.get(start..(start + 14).min(sl.len()))?.trim();
                if field.is_empty() {
                    None
                } else {
                    field.parse().ok()
                }
            };
            sats.push(RinexSat {
                prn,
                pseudorange_m: value(ci),
                carrier_cycles: value(li),
                doppler_hz: value(di),
                cn0_dbhz: value(si),
            });
        }
        out.epochs.push(RinexEpoch { t, sats });
    }
    Ok(out)
}

/// A position fix from NMEA `GGA`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NmeaFix {
    /// UTC seconds of day.
    pub utc_sod: f64,
    /// Position, ECEF (WGS-84), m.
    pub ecef: Ecef,
    /// Satellites used.
    pub sats_used: u32,
}

/// What GNSS-SDR's NMEA file says.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct NmeaLog {
    /// Fixes (GGA with a non-zero quality), in file order.
    pub fixes: Vec<NmeaFix>,
    /// Last reported elevation per PRN (GSV), degrees.
    pub elevation_deg: BTreeMap<u8, f32>,
    /// Sentences dropped for a bad checksum or shape.
    pub rejected: usize,
}

/// WGS-84 geodetic to ECEF, m.
pub fn geodetic_to_ecef(lat_deg: f64, lon_deg: f64, h_m: f64) -> Ecef {
    const A: f64 = 6_378_137.0;
    const F: f64 = 1.0 / 298.257_223_563;
    let e2 = F * (2.0 - F);
    let (lat, lon) = (lat_deg.to_radians(), lon_deg.to_radians());
    let n = A / (1.0 - e2 * lat.sin().powi(2)).sqrt();
    Ecef {
        x: (n + h_m) * lat.cos() * lon.cos(),
        y: (n + h_m) * lat.cos() * lon.sin(),
        z: (n * (1.0 - e2) + h_m) * lat.sin(),
    }
}

/// The sentence body between `$` and `*`, when its checksum is right.
fn nmea_body(line: &str) -> Option<&str> {
    let line = line.trim().strip_prefix('$')?;
    let (body, cs) = line.split_once('*')?;
    let want = u8::from_str_radix(cs.get(0..2)?, 16).ok()?;
    let got = body.bytes().fold(0u8, |a, b| a ^ b);
    (got == want).then_some(body)
}

/// `ddmm.mmmm` / `dddmm.mmmm` with a hemisphere letter, as signed degrees.
fn nmea_angle(v: &str, hemi: &str, deg_digits: usize) -> Option<f64> {
    let deg: f64 = v.get(..deg_digits)?.parse().ok()?;
    let min: f64 = v.get(deg_digits..)?.parse().ok()?;
    let a = deg + min / 60.0;
    match hemi {
        "N" | "E" => Some(a),
        "S" | "W" => Some(-a),
        _ => None,
    }
}

/// Parses GNSS-SDR's NMEA output: `GGA` fixes and `GSV` elevations; every talker id accepted.
pub fn parse_nmea(text: &str) -> NmeaLog {
    let mut log = NmeaLog::default();
    for line in text.lines().filter(|l| l.trim_start().starts_with('$')) {
        let Some(body) = nmea_body(line) else {
            log.rejected += 1;
            continue;
        };
        let f: Vec<&str> = body.split(',').collect();
        let kind = f[0].get(2..).unwrap_or("");
        match kind {
            "GGA" => {
                let parsed = (|| {
                    let quality: u32 = f.get(6)?.parse().ok()?;
                    if quality == 0 {
                        return Some(None);
                    }
                    let tod = f.get(1)?;
                    let sod = tod.get(0..2)?.parse::<f64>().ok()? * 3600.0
                        + tod.get(2..4)?.parse::<f64>().ok()? * 60.0
                        + tod.get(4..)?.parse::<f64>().ok()?;
                    let lat = nmea_angle(f.get(2)?, f.get(3)?, 2)?;
                    let lon = nmea_angle(f.get(4)?, f.get(5)?, 3)?;
                    let sats_used = f.get(7)?.parse().unwrap_or(0);
                    let alt: f64 = f.get(9)?.parse().ok()?;
                    let sep: f64 = f.get(11).and_then(|s| s.parse().ok()).unwrap_or(0.0);
                    Some(Some(NmeaFix {
                        utc_sod: sod,
                        ecef: geodetic_to_ecef(lat, lon, alt + sep),
                        sats_used,
                    }))
                })();
                match parsed {
                    Some(Some(fix)) => log.fixes.push(fix),
                    Some(None) => {}
                    None => log.rejected += 1,
                }
            }
            "GSV" => {
                // Up to four {prn, elevation, azimuth, snr} blocks from field 4.
                for sv in f.get(4..).unwrap_or(&[]).chunks(4) {
                    let prn = sv.first().and_then(|p| p.parse::<u8>().ok());
                    let el = sv.get(1).and_then(|e| e.parse::<f32>().ok());
                    if let (Some(prn), Some(el)) = (prn, el) {
                        log.elevation_deg.insert(prn, el);
                    }
                }
            }
            _ => {}
        }
    }
    log
}

/// One observation epoch as the plugin emits it (`frame_model` [`FRAME_EPOCH`]).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ReceiverEpochEvidence {
    /// The epoch in this crate's own model.
    pub epoch: GnssObservableEpoch,
    /// Seconds since the dwell's first epoch.
    pub offset_s: f64,
    /// Stream sample index of the dwell's first sample (the decode line's `sample_index`).
    pub dwell_start_sample: u64,
    /// One past the dwell's last sample.
    pub dwell_end_sample: u64,
}

/// How a GNSS-SDR run over one dwell ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunOutcome {
    /// Ran to the end of the dwell and exited 0.
    Completed,
    /// Exited non-zero or on a signal.
    Failed,
    /// Killed after the run budget.
    TimedOut,
    /// Could not be started.
    NotStarted,
}

/// The per-dwell summary (`frame_model` [`FRAME_DWELL`]). Always emitted, so "the receiver ran
/// and saw nothing" is distinguishable from "the receiver never ran".
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DwellSummary {
    /// How the run ended.
    pub outcome: RunOutcome,
    /// Exit code, when there was one.
    pub exit_code: Option<i32>,
    /// First sample of the dwell.
    pub dwell_start_sample: u64,
    /// One past the dwell's last sample.
    pub dwell_end_sample: u64,
    /// Capture centre, Hz.
    pub center_hz: f64,
    /// Sample rate, Hz.
    pub sample_rate_hz: f64,
    /// Observation epochs emitted.
    pub epochs: usize,
    /// Most satellites observed at one epoch.
    pub max_svs: usize,
    /// Epochs carrying a position fix.
    pub fixes: usize,
    /// Output lines rejected while parsing (RINEX errors count 1, bad NMEA sentences each).
    pub rejected: usize,
}

/// Builds the evidence for one dwell from GNSS-SDR's files. A fix is attached to an epoch whose
/// UTC second-of-day matches a GGA within 5 ms; elevations are the last GSV value per PRN.
pub fn build_evidence(
    obs: &RinexObs,
    nmea: &NmeaLog,
    dwell_start_sample: u64,
    dwell_end_sample: u64,
) -> Vec<ReceiverEpochEvidence> {
    let Some(first) = obs.epochs.first().map(|e| e.t.as_unix_nanos()) else {
        return Vec::new();
    };
    obs.epochs
        .iter()
        .map(|e| {
            let ns = e.t.as_unix_nanos();
            let sod = (ns.rem_euclid(86_400 * 1_000_000_000)) as f64 / 1e9;
            let position = nmea
                .fixes
                .iter()
                .find(|f| (f.utc_sod - sod).abs() < 0.005)
                .map(|f| f.ecef);
            let svs = e
                .sats
                .iter()
                .map(|s| SvObservable {
                    prn: s.prn,
                    cn0_dbhz: s.cn0_dbhz.unwrap_or(0.0) as f32,
                    doppler_hz: s.doppler_hz.unwrap_or(0.0),
                    elevation_deg: nmea.elevation_deg.get(&s.prn).copied(),
                    // Present in a GNSS-SDR observation epoch with a C/N0 means a channel is
                    // tracking it.
                    locked: s.cn0_dbhz.is_some(),
                    pseudorange_m: s.pseudorange_m,
                    carrier_phase_cycles: s.carrier_cycles,
                })
                .collect();
            ReceiverEpochEvidence {
                epoch: GnssObservableEpoch {
                    t: e.t,
                    svs,
                    position,
                    clock_bias_s: None,
                },
                offset_s: (ns - first) as f64 / 1e9,
                dwell_start_sample,
                dwell_end_sample,
            }
        })
        .collect()
}

/// Reads every file in `dir` and returns the RINEX observations (found by header, not name —
/// GNSS-SDR derives the name from the date) and the NMEA log ([`NMEA_FILE`]). The count is of
/// RINEX files that failed to parse.
pub fn read_output_dir(dir: &Path) -> (RinexObs, NmeaLog, usize) {
    let mut obs = RinexObs::default();
    let mut rejected = 0;
    let mut nmea = NmeaLog::default();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (obs, nmea, 0);
    };
    let mut paths: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for p in paths {
        let Ok(text) = std::fs::read_to_string(&p) else {
            continue;
        };
        if p.file_name().and_then(|n| n.to_str()) == Some(NMEA_FILE) {
            nmea = parse_nmea(&text);
            continue;
        }
        let first = text.lines().next().unwrap_or("");
        if first.contains("RINEX VERSION / TYPE") && first.chars().nth(20) == Some('O') {
            match parse_rinex_obs(&text) {
                Ok(o) => {
                    obs.epochs.extend(o.epochs);
                    obs.skipped_non_gps += o.skipped_non_gps;
                }
                Err(_) => rejected += 1,
            }
        }
    }
    (obs, nmea, rejected)
}

/// The epochs back out of the plugin's decode metadata (lines of another frame model or shape
/// are skipped). Sorted by time.
pub fn epochs_from_evidence<'a>(
    metadata: impl IntoIterator<Item = &'a serde_json::Value>,
) -> Vec<GnssObservableEpoch> {
    let mut v: Vec<GnssObservableEpoch> = metadata
        .into_iter()
        .filter_map(|m| serde_json::from_value::<ReceiverEpochEvidence>(m.clone()).ok())
        .map(|e| e.epoch)
        .collect();
    v.sort_by_key(|e| e.t);
    v
}

/// **Real** receiver lock evidence between two epochs (AWARE-002): satellites locked before and
/// now, and the mean C/N0 drop over those locked in both — measured by a tracking loop, where
/// the acquisition-only path had to use in-band power as its proxy.
pub fn lock_evidence(before: &GnssObservableEpoch, now: &GnssObservableEpoch) -> LockEvidence {
    let (mut sum, mut n) = (0f32, 0u32);
    for b in before.locked() {
        if let Some(a) = now.locked().find(|a| a.prn == b.prn) {
            sum += b.cn0_dbhz - a.cn0_dbhz;
            n += 1;
        }
    }
    LockEvidence {
        svs_before: before.locked_count().min(255) as u8,
        svs_now: now.locked_count().min(255) as u8,
        mean_cn0_drop_db: if n > 0 { sum / n as f32 } else { 0.0 },
    }
}

/// S4 per PRN over a run of epochs (PROP-033), from tracked C/N0 converted to linear intensity.
///
/// **An approximation, stated**: the textbook S4 uses detrended 50 Hz signal intensity from the
/// correlator; this uses C/N0 at the observation rate (10 Hz by default), undetrended. It is
/// real receiver data where T-274 had only mocked series, but it underestimates fast
/// scintillation and folds slow elevation-driven change in. Needs at least `min_samples` locked
/// samples per satellite.
pub fn s4_by_prn(epochs: &[GnssObservableEpoch], min_samples: usize) -> BTreeMap<u8, f32> {
    let mut series: BTreeMap<u8, Vec<f32>> = BTreeMap::new();
    for e in epochs {
        for sv in e.locked() {
            series
                .entry(sv.prn)
                .or_default()
                .push(10f32.powf(sv.cn0_dbhz / 10.0));
        }
    }
    series
        .into_iter()
        .filter(|(_, s)| s.len() >= min_samples.max(2))
        .filter_map(|(prn, s)| s4_index(&s).map(|v| (prn, v)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(content: &str, label: &str) -> String {
        format!("{content:<60}{label}\n")
    }

    fn rinex(epochs: &str) -> String {
        let mut s = line(
            "     3.02           OBSERVATION DATA    M (MIXED)",
            "RINEX VERSION / TYPE",
        );
        s += &line("G    4 C1C L1C D1C S1C", "SYS / # / OBS TYPES");
        s += &line("E    2 C1B S1B", "SYS / # / OBS TYPES");
        s += &line("    18", "LEAP SECONDS");
        s += &line("", "END OF HEADER");
        s + epochs
    }

    #[test]
    fn days_from_civil_matches_known_dates() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(days_from_civil(2026, 9, 22), 20_718);
    }

    #[test]
    fn rinex_epochs_parse_with_gps_to_utc_and_blank_fields() {
        // RINEX 3: a 3-character satellite id, then per observable F14.3 + LLI + SSI.
        let f = |v: Option<f64>| v.map_or(" ".repeat(16), |v| format!("{v:14.3}  "));
        let sat = |id: &str, vals: &[Option<f64>]| {
            format!("{id}{}\n", vals.iter().map(|&v| f(v)).collect::<String>())
        };
        let body = "> 2026 09 22 10 00  0.0000000  0  3\n".to_string()
            + &sat(
                "G05",
                &[
                    Some(20_123_456.789),
                    Some(105_749_321.123),
                    Some(-1234.5),
                    Some(44.25),
                ],
            )
            + &sat("E11", &[Some(23_000_000.0), Some(40.0)])
            + &sat("G12", &[Some(21_000_000.0), None, Some(567.25), Some(38.0)]);
        let obs = parse_rinex_obs(&rinex(&body)).unwrap();
        assert_eq!(obs.epochs.len(), 1);
        assert_eq!(obs.skipped_non_gps, 1);
        let e = &obs.epochs[0];
        let gps = calendar_nanos(2026, 9, 22, 10, 0, 0.0);
        assert_eq!(e.t.as_unix_nanos(), gps - 18_000_000_000);
        assert_eq!(e.sats.len(), 2);
        assert_eq!(e.sats[0].prn, 5);
        assert_eq!(e.sats[0].pseudorange_m, Some(20_123_456.789));
        assert_eq!(e.sats[0].carrier_cycles, Some(105_749_321.123));
        assert_eq!(e.sats[0].doppler_hz, Some(-1234.5));
        assert_eq!(e.sats[0].cn0_dbhz, Some(44.25));
        assert_eq!(e.sats[1].prn, 12);
        assert_eq!(
            e.sats[1].carrier_cycles, None,
            "a blank field is absent, not zero"
        );
        assert_eq!(e.sats[1].cn0_dbhz, Some(38.0));
    }

    #[test]
    fn event_epochs_are_skipped_with_their_records() {
        let body = "> 2026 09 22 10 00  0.0000000  4  1\n\
                    some comment record\n\
> 2026 09 22 10 00  1.0000000  0  0\n";
        let obs = parse_rinex_obs(&rinex(body)).unwrap();
        assert_eq!(obs.epochs.len(), 1);
    }

    #[test]
    fn non_observation_and_old_versions_are_refused() {
        let nav = line(
            "     3.02           N: GNSS NAV DATA    G",
            "RINEX VERSION / TYPE",
        );
        assert!(matches!(
            parse_rinex_obs(&nav),
            Err(ReceiverError::NotObservation(_))
        ));
        let v2 = line(
            "     2.11           OBSERVATION DATA    G",
            "RINEX VERSION / TYPE",
        );
        assert!(matches!(
            parse_rinex_obs(&v2),
            Err(ReceiverError::Version(_))
        ));
        assert!(parse_rinex_obs(&rinex("garbage\n")).is_err());
    }

    fn with_checksum(body: &str) -> String {
        let cs = body.bytes().fold(0u8, |a, b| a ^ b);
        format!("${body}*{cs:02X}")
    }

    #[test]
    fn nmea_gga_becomes_ecef_and_bad_checksums_are_rejected() {
        let gga = with_checksum("GPGGA,095942.00,5128.6740,N,00000.0900,W,1,06,1.0,0.0,M,45.0,M,,");
        let gsv = with_checksum("GPGSV,1,1,02,05,63,120,44,12,15,300,38");
        let broken = "$GPGGA,095943.00,5128.6740,N,00000.0900,W,1,06,1.0,0.0,M,45.0,M,,*00";
        let log = parse_nmea(&format!("{gga}\n{gsv}\n{broken}\n"));
        assert_eq!(log.rejected, 1);
        assert_eq!(log.fixes.len(), 1);
        let want = geodetic_to_ecef(51.0 + 28.674 / 60.0, -(0.09 / 60.0), 45.0);
        assert!(log.fixes[0].ecef.distance_m(&want) < 1e-6);
        assert!((log.fixes[0].utc_sod - (9.0 * 3600.0 + 59.0 * 60.0 + 42.0)).abs() < 1e-9);
        assert_eq!(log.elevation_deg.get(&5), Some(&63.0));
        assert_eq!(log.elevation_deg.get(&12), Some(&15.0));
    }

    #[test]
    fn ecef_of_the_equator_prime_meridian_is_the_semi_major_axis() {
        let e = geodetic_to_ecef(0.0, 0.0, 0.0);
        assert!((e.x - 6_378_137.0).abs() < 1e-6 && e.y.abs() < 1e-6 && e.z.abs() < 1e-6);
    }

    fn epoch(t_s: i64, svs: &[(u8, f32)]) -> GnssObservableEpoch {
        GnssObservableEpoch {
            t: Timestamp::from_unix_nanos(t_s * 1_000_000_000),
            svs: svs
                .iter()
                .map(|&(prn, cn0)| SvObservable {
                    prn,
                    cn0_dbhz: cn0,
                    doppler_hz: 0.0,
                    elevation_deg: None,
                    locked: true,
                    pseudorange_m: None,
                    carrier_phase_cycles: None,
                })
                .collect(),
            position: None,
            clock_bias_s: None,
        }
    }

    #[test]
    fn lock_evidence_measures_loss_and_the_drop_over_common_satellites() {
        let before = epoch(0, &[(1, 45.0), (2, 40.0), (3, 42.0), (4, 38.0)]);
        let now = epoch(10, &[(1, 35.0), (2, 32.0)]);
        let l = lock_evidence(&before, &now);
        assert_eq!((l.svs_before, l.svs_now), (4, 2));
        assert!((l.mean_cn0_drop_db - 9.0).abs() < 1e-6);
    }

    #[test]
    fn evidence_round_trips_through_decode_metadata() {
        let ev = ReceiverEpochEvidence {
            epoch: epoch(5, &[(7, 41.0)]),
            offset_s: 0.0,
            dwell_start_sample: 10,
            dwell_end_sample: 20,
        };
        let v = serde_json::to_value(&ev).unwrap();
        let summary = serde_json::to_value(DwellSummary {
            outcome: RunOutcome::Completed,
            exit_code: Some(0),
            dwell_start_sample: 10,
            dwell_end_sample: 20,
            center_hz: L1_HZ,
            sample_rate_hz: 4e6,
            epochs: 1,
            max_svs: 1,
            fixes: 0,
            rejected: 0,
        })
        .unwrap();
        let back = epochs_from_evidence([&summary, &v]);
        assert_eq!(back, vec![ev.epoch]);
    }

    #[test]
    fn config_translates_l1_only_when_tuned_off_centre() {
        let mut run = GnssSdrRun {
            input_path: "/tmp/d.ci8".into(),
            output_dir: "/tmp/o".into(),
            sample_rate_hz: 4e6,
            center_hz: L1_HZ,
            channels: 8,
            obs_rate_ms: 100,
        };
        let c = run.config_text();
        assert!(c.contains("SignalSource.item_type=ibyte\n"));
        assert!(c.contains("SignalSource.filename=/tmp/d.ci8\n"));
        assert!(c.contains("InputFilter.implementation=Pass_Through\n"));
        assert!(c.contains("PVT.rinexobs_rate_ms=100\n"));
        run.center_hz = L1_HZ - 500_000.0;
        let c = run.config_text();
        assert!(c.contains("InputFilter.implementation=Freq_Xlating_Fir_Filter\n"));
        assert!(c.contains("InputFilter.IF=500000\n"));
    }

    #[test]
    fn s4_per_prn_needs_enough_samples() {
        let epochs: Vec<_> = (0..20)
            .map(|i| {
                let flick = if i % 2 == 0 { 3.0 } else { -3.0 };
                epoch(i, &[(1, 45.0), (2, 40.0 + flick)])
            })
            .collect();
        let s4 = s4_by_prn(&epochs, 10);
        assert!(s4[&1] < 1e-6);
        assert!(s4[&2] > 0.5, "{s4:?}");
        assert!(s4_by_prn(&epochs[..5], 10).is_empty());
    }
}
