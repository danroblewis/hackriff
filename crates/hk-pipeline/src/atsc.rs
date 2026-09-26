//! Always-on reader 6: **the 8VSB television survey** (T-979).
//!
//! # What it is for
//!
//! A 6 MHz ATSC channel is one emission, and the per-frame CFAR detector cannot see it as one:
//! the explorer window of 2026-09-25 over UHF 470–608 MHz in San Francisco found 13 pilots blind
//! and turned each channel into **15–60 narrow and medium candidates**, with the fragments inside
//! channels 29/30 suggested as `fm-broadcast` at 0.6 because a 200 kHz-ish continuous fragment has
//! the broadcast-FM shape and 470–608 MHz had no allocation data to say otherwise.
//!
//! This reader measures the **whole channel** instead ([`hk_estimate::atsc`]), and writes what it
//! measured as one inventory row 6 MHz wide, explained by the pilot it found. The band plan now
//! carries 470–608 MHz (T-979 rows in `us-47cfr2106-compact.csv`), so the row's explanation names
//! TV broadcast and the Part 74 wireless-microphone use that shares the channels — as *priors that
//! explain*, never as the thing that found it: [`hk_estimate::atsc::find_channels`] gives the same
//! answer at 1.3 GHz, where no TV allocation exists.
//!
//! # Why a reader, and what it costs
//!
//! Exactly the shape of the receiver-line survey ([`crate::survey`]), and for the same reasons:
//! the measurement needs a contiguous window of the **raw tuned span**, it is a property of a
//! *receiver state* rather than of an emission, and it must never sit in front of first detection.
//! So it runs on its own thread from the first block, holds nothing the detector waits on, and
//! measures **once per [`CaptureState`]** with a small retry budget.
//!
//! The short-circuit is the important part of the cost. A window narrower than one flat band plus
//! some floor to measure against cannot hold an 8VSB channel, so at any sample rate below
//! [`MIN_SAMPLE_RATE_HZ`] this reader **accumulates nothing and computes nothing** — it drains its
//! blocks and advances its gate cursor. Every fixture and every FM-band capture in the suite runs
//! at 2.4 Msps or less, so they pay one comparison per block.
//!
//! # What it writes
//!
//! Per recognised channel, in one pass:
//!
//! 1. **One emitter, 6 MHz wide**, at the channel the measured pilot implies — re-resolved to the
//!    same row on every later pass ([`CHANNEL_MATCH_HZ`]), never a new row per measurement.
//! 2. **A Classification `atsc-8vsb`** at [`ATSC_PILOT_VERSION`]. That is *not* shape evidence:
//!    `hk_pipeline::family` treats any `model_version` other than its own occupancy mapping as
//!    status-setting, and this one is written only when a 5.381 MHz flat band **and** a pilot on
//!    its lower edge were both measured.
//! 3. **The pilot itself**, as a metadata-only Annotation, which
//!    [`crate::family::explain_emitter`] turns into the [`ExplanationEvidence::Pilot`] row of the
//!    TV-broadcast explanation. The explanation cites the measurement it came from.
//! 4. **A C05 calibration observation** from the pilot offsets, when the channels landed on the US
//!    grid — see [`record_receiver_clock`].
//!
//! What it does **not** write is a new detection, a track, or any change to the fragments. Merging
//! the fragments into this row is the overlap-re-analysis ticket's job (CLAUDE.md: "overlap is an
//! error signal that triggers re-analysis"); what this module guarantees is that the 6 MHz row the
//! fragments must merge *into* exists and is explained.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use hk_core::ReadOutcome;
use hk_estimate::atsc::{AtscChannel, AtscConfig, CHANNEL_BANDWIDTH_HZ, FLAT_BANDWIDTH_HZ};
use hk_estimate::blind::receiver::CaptureState;
use hk_model::ids::CalibrationStateId;
use hk_model::{
    Annotation, AnnotationAuthor, AnnotationId, AnnotationKind, AnnotationTarget,
    CalibrationMethod, CalibrationState, Classification, ContentClass, EmitterId,
    EmitterObservation, FreqRange, InventoryQuery, Provenance, RepoError, Repository, TimeRange,
    Timestamp,
};
use num_complex::Complex;

use crate::family::{self, ExplanationEvidence};
use crate::run::Shared;

/// `Classification.model_version` and Annotation `author_ref` of everything this module writes.
pub const ATSC_PILOT_VERSION: &str = "hk-estimate/atsc-pilot@1";

/// The emission family a measured 8VSB channel is recorded under. It maps to the `tv-broadcast`
/// service in [`crate::family`]'s vocabulary.
pub const ATSC_FAMILY: &str = "atsc-8vsb";

/// Narrowest tuned span that can hold one 8VSB channel with floor either side, Hz. Below it the
/// reader does nothing at all.
pub const MIN_SAMPLE_RATE_HZ: f64 = FLAT_BANDWIDTH_HZ * 1.15;

/// How near an existing 6 MHz inventory row must be, in centre frequency, to be the same channel,
/// Hz. Two US television channels are 6 MHz apart, so this cannot confuse neighbours; it is wide
/// enough for any receiver clock error (the explorer's 4 ppm is 2.4 kHz at 602 MHz).
pub const CHANNEL_MATCH_HZ: f64 = 100e3;

/// Largest change in measured clock, in Hz at the measured pilot, that leaves the recorded C05
/// calibration alone. Below it no channel assignment can move, and re-writing the row on every
/// pass would bury the version history in noise (the T-560 rule).
pub const CLOCK_TOLERANCE_HZ: f64 = 500.0;

/// The accumulation window, seconds of the tuned span. One Welch spectrum over it resolves the
/// Nyquist edges to a few kHz, and the pilot to a fraction of a bin.
pub const WINDOW_S: f64 = 0.05;

/// Measurements allowed per capture state while every one has found nothing. A state that has
/// found a channel is never re-measured.
pub const MAX_ATTEMPTS: u32 = 3;

#[derive(Default)]
struct State {
    /// The capture state being tracked and how many times it has been measured.
    current: Option<(CaptureState, u32)>,
    /// Whether the tracked state produced channels.
    found: bool,
}

/// The 8VSB television survey of a run: what has been measured, and the cadence that keeps it to
/// one measurement per capture state.
pub struct AtscSurvey {
    state: Mutex<State>,
    cfg: AtscConfig,
    max_attempts: u32,
    /// Measurements that found at least one channel.
    measured: AtomicU64,
    /// Measurements that found nothing.
    empty: AtomicU64,
    /// Channels recorded.
    channels: AtomicU64,
}

impl Default for AtscSurvey {
    fn default() -> Self {
        Self::new(AtscConfig::default(), MAX_ATTEMPTS)
    }
}

impl AtscSurvey {
    /// A survey at `cfg`, measuring at most `max_attempts` times per capture state.
    pub fn new(cfg: AtscConfig, max_attempts: u32) -> Self {
        Self {
            state: Mutex::new(State::default()),
            cfg,
            max_attempts,
            measured: AtomicU64::new(0),
            empty: AtomicU64::new(0),
            channels: AtomicU64::new(0),
        }
    }

    /// The recognition settings in force.
    pub fn config(&self) -> &AtscConfig {
        &self.cfg
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether a window captured under `p` is worth accumulating: the span must be able to hold a
    /// channel, and this receiver state must not already be answered or out of attempts.
    pub fn wanted(&self, p: &Provenance) -> bool {
        if !(p.tune.sample_rate_hz.is_finite() && p.tune.sample_rate_hz > MIN_SAMPLE_RATE_HZ) {
            return false;
        }
        let want = CaptureState::of(p);
        let s = self.lock();
        match &s.current {
            Some((state, attempts)) if *state == want => !s.found && *attempts < self.max_attempts,
            _ => true,
        }
    }

    /// Records the outcome of one measurement of the state `p` was captured under.
    pub fn record(&self, p: &Provenance, found: usize) {
        let want = CaptureState::of(p);
        let mut s = self.lock();
        let attempts = match &s.current {
            Some((state, n)) if *state == want => *n,
            _ => {
                s.found = false;
                0
            }
        };
        s.current = Some((want, attempts + 1));
        s.found = s.found || found > 0;
        drop(s);
        if found > 0 {
            self.measured.fetch_add(1, Ordering::Relaxed);
            self.channels.fetch_add(found as u64, Ordering::Relaxed);
        } else {
            self.empty.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// `(measurements that found channels, measurements that found none, channels recorded)`.
    pub fn counts(&self) -> (u64, u64, u64) {
        (
            self.measured.load(Ordering::Relaxed),
            self.empty.load(Ordering::Relaxed),
            self.channels.load(Ordering::Relaxed),
        )
    }
}

/// What [`record_channels`] wrote.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Recorded {
    /// The 6 MHz inventory rows, one per recognised channel.
    pub emitters: Vec<EmitterId>,
    /// The C05 calibration version written, if the receiver clock moved.
    pub calibration: Option<CalibrationState>,
}

/// Writes `channels` to the inventory as one 6 MHz row each, explained by the pilot, and records
/// the receiver clock they measured (see the [module docs](self)).
pub fn record_channels(
    repo: &mut Repository,
    table: &hk_context::BandTable,
    channels: &[AtscChannel],
    device_id: &str,
    seen: TimeRange,
) -> Result<Recorded, RepoError> {
    let mut out = Recorded::default();
    for c in channels {
        let id = resolve_channel(repo, c)?;
        let up = repo.upsert_emitter_observation(&EmitterObservation {
            emitter_id: id,
            seen,
            count: 1,
            f_center_hz: c.f_center_hz,
            bandwidth_hz: CHANNEL_BANDWIDTH_HZ,
            identity: None,
        })?;
        let id = up.emitter_id;
        repo.append_classification(
            id,
            &Classification {
                t: seen.end,
                family: ATSC_FAMILY.into(),
                confidence: confidence_of(c),
                open_set_score: 1.0 - confidence_of(c),
                model_version: ATSC_PILOT_VERSION.into(),
            },
        )?;
        write_pilot(repo, id, c, seen.end)?;
        family::explain_emitter(repo, table, id)?;
        out.emitters.push(id);
    }
    out.calibration = record_receiver_clock(repo, device_id, channels, seen.end)?;
    Ok(out)
}

/// Confidence that this measurement is an 8VSB channel: high, and it has to be — a flat band and a
/// pilot on its lower edge are two independent agreements with one standard. The margin above the
/// configured minima is what varies it, never the frequency it was found at.
fn confidence_of(c: &AtscChannel) -> f64 {
    let pilot = ((c.pilot_excess_db - 8.0) / 12.0).clamp(0.0, 1.0);
    0.9 + 0.09 * pilot
}

/// The inventory row for `c`'s channel: an existing 6 MHz row within [`CHANNEL_MATCH_HZ`] of its
/// centre, else a fresh id.
fn resolve_channel(repo: &Repository, c: &AtscChannel) -> Result<EmitterId, RepoError> {
    let page = repo.query_inventory(&InventoryQuery {
        freq: Some(FreqRange::new(c.f_lo_hz, c.f_hi_hz)),
        limit: 64,
        ..InventoryQuery::default()
    })?;
    Ok(page
        .entries
        .iter()
        .find(|r| {
            (r.emitter.f_center_hz - c.f_center_hz).abs() <= CHANNEL_MATCH_HZ
                && (r.emitter.bandwidth_hz - CHANNEL_BANDWIDTH_HZ).abs()
                    <= CHANNEL_BANDWIDTH_HZ * 0.2
        })
        .map_or_else(EmitterId::new, |r| r.emitter.id))
}

/// Stores the measured pilot as a metadata-only Annotation, superseding the previous one.
/// [`crate::family::explain_emitter`] reads it back as the explanation's evidence.
fn write_pilot(
    repo: &mut Repository,
    id: EmitterId,
    c: &AtscChannel,
    t: Timestamp,
) -> Result<(), RepoError> {
    let prev = latest_pilot(repo, id)?;
    let mut metadata = serde_json::json!({
        "standard": ATSC_FAMILY,
        "measured_hz": c.pilot.value(),
        "excess_db": c.pilot_excess_db,
        "flat_bandwidth_hz": c.flat_bandwidth_hz,
        "plateau_snr_db": c.plateau_snr_db,
        "source": "ATSC A/53 Part 2 §5.1.2: pilot at the 6 MHz channel's lower edge + 309.440559 kHz",
    });
    let m = metadata.as_object_mut().expect("a json object");
    if let Some(s) = c.pilot.sigma() {
        m.insert("sigma_hz".into(), s.into());
    }
    if let Some(n) = c.nominal_pilot_hz() {
        m.insert("nominal_hz".into(), n.into());
    }
    if let Some(o) = c.pilot_offset_hz() {
        m.insert("offset_hz".into(), o.into());
    }
    if let Some(p) = c.ppm.value() {
        m.insert("ppm".into(), p.into());
    }
    if let Some(n) = c.us_channel {
        m.insert("channel".into(), u64::from(n).into());
    }
    let a = Annotation {
        id: AnnotationId::new(),
        target: AnnotationTarget::Emitter(id),
        author: AnnotationAuthor::Classifier,
        author_ref: ATSC_PILOT_VERSION.into(),
        kind: AnnotationKind::Label,
        value: format!("pilot/{ATSC_FAMILY}"),
        metadata: serde_json::json!({ "pilot": metadata }),
        content: None,
        confidence: confidence_of(c),
        supersedes: prev.map(|a| a.id),
        content_class: ContentClass::MetadataOnly,
        t,
        exported: false,
    };
    repo.insert_annotation(&a)
}

/// The latest pilot annotation on `id`, if any.
fn latest_pilot(repo: &Repository, id: EmitterId) -> Result<Option<Annotation>, RepoError> {
    Ok(repo
        .annotations_for(&AnnotationTarget::Emitter(id))?
        .into_iter()
        .rfind(|a| a.author == AnnotationAuthor::Classifier && a.author_ref == ATSC_PILOT_VERSION))
}

/// The [`ExplanationEvidence::Pilot`] measured for `id`, if one was.
pub fn pilot_evidence(
    repo: &Repository,
    id: EmitterId,
) -> Result<Option<ExplanationEvidence>, RepoError> {
    Ok(latest_pilot(repo, id)?.and_then(|a| family::pilot_evidence(&a.metadata)))
}

/// Records the receiver clock the pass's pilots measured as a C05 [`CalibrationState`], and
/// returns the row written, if one was.
///
/// **The median, not each channel.** Every channel in a pass is read through one receiver, so the
/// pilots are thirteen measurements of one number (the explorer's were: all ≈ 2.4 kHz low across
/// 470–602 MHz). The median is taken over the channels that landed on the grid; a pass with none
/// records nothing rather than a guess, because a ppm needs a nominal.
///
/// **Sign.** [`AtscChannel::ppm`] is the *offset* — how this receiver reads frequencies — while
/// [`CalibrationState::ppm`] is the oscillator's own error, positive = fast. A fast local
/// oscillator puts every emission low, so `ppm = −offset`: the explorer's −4 ppm of offset is an
/// oscillator **+4 ppm fast**. Identical to the LMR-raster rule of T-560.
pub fn record_receiver_clock(
    repo: &mut Repository,
    device_id: &str,
    channels: &[AtscChannel],
    t: Timestamp,
) -> Result<Option<CalibrationState>, RepoError> {
    let mut ppms: Vec<f64> = channels
        .iter()
        .filter(|c| c.us_channel.is_some())
        .filter_map(|c| c.ppm.value())
        .filter(|p| p.is_finite())
        .collect();
    if ppms.is_empty() {
        return Ok(None);
    }
    ppms.sort_by(f64::total_cmp);
    let ppm = -ppms[ppms.len() / 2];
    // The frequency the tolerance is judged at: the pilots this pass measured.
    let at_hz = channels
        .iter()
        .filter_map(|c| c.pilot.value())
        .fold(0.0f64, f64::max)
        .max(1e6);
    let prior =
        repo.latest_calibration_state_for_device(device_id, &CalibrationMethod::AtscPilot)?;
    let unchanged = prior
        .as_ref()
        .is_some_and(|p| (p.ppm - ppm).abs() * 1e-6 * at_hz <= CLOCK_TOLERANCE_HZ);
    if unchanged {
        return Ok(None);
    }
    let cal = CalibrationState {
        id: CalibrationStateId::new(),
        supersedes: prior.map(|p| p.id),
        device_id: device_id.to_owned(),
        ppm,
        method: CalibrationMethod::AtscPilot,
        measured_at: t,
        valid: None,
        temperature_c: None,
        power_table: Vec::new(),
    };
    repo.insert_calibration_state(&cal)?;
    Ok(Some(cal))
}

/// The `hk-atsc` reader: accumulates one window of the raw tuned span per capture state and
/// recognises the 8VSB channels in it (see the [module docs](self)).
pub(crate) fn run(shared: Arc<Shared>, survey: Arc<AtscSurvey>) -> anyhow::Result<()> {
    let mut reader = shared.ring.reader_at(0);
    let cursor = shared.gate.register(0);
    let mut buf = vec![Complex::<i8>::default(); 1 << 16];
    let mut window: Vec<num_complex::Complex32> = Vec::new();
    let mut held: Option<(hk_core::ProvenanceHandle, hk_model::SampleTime)> = None;
    let mut next_index: Option<u64> = None;
    loop {
        if shared.stop.load(Ordering::Relaxed) {
            break;
        }
        match reader.read_timeout(&mut buf, Duration::from_millis(50)) {
            ReadOutcome::Data(chunk) => {
                cursor.set(chunk.end_sample());
                let p = chunk.provenance.get();
                if !survey.wanted(p) {
                    // Narrower than one channel, or this state is answered: hold nothing. This is
                    // the short-circuit every 2.4 Msps fixture in the suite takes.
                    window = Vec::new();
                    held = None;
                    next_index = None;
                    continue;
                }
                let fs = p.tune.sample_rate_hz;
                let center = p.tune.center_hz;
                let broken = chunk.block_start && !chunk.discontinuity.is_empty();
                let continues = !broken
                    && next_index == Some(chunk.first_sample())
                    && held
                        .as_ref()
                        .is_some_and(|(h, _)| h.id() == chunk.provenance.id());
                if !continues {
                    window.clear();
                    held = Some((chunk.provenance.clone(), chunk.time));
                }
                let want = ((WINDOW_S * fs) as usize).max(1);
                window.reserve(want.saturating_sub(window.len()).min(chunk.len));
                window.extend(buf[..chunk.len].iter().map(|s| {
                    num_complex::Complex32::new(f32::from(s.re), f32::from(s.im)) / 128.0
                }));
                next_index = Some(chunk.end_sample());
                if window.len() < want {
                    continue;
                }
                window.truncate(want);
                let Some((prov, time)) = held.take() else {
                    continue;
                };
                next_index = None;
                let samples = std::mem::take(&mut window);
                let found = hk_estimate::atsc::find_channels(&samples, fs, center, survey.config());
                survey.record(prov.get(), found.len());
                if found.is_empty() {
                    continue;
                }
                // The window's own capture time: first sample to last, on the sample clock.
                let start = time.host_time;
                let end = time.time_of(time.sample_index + samples.len() as u64, fs);
                let seen = TimeRange::new(start, end);
                let device_id = prov.get().device_id.clone();
                let Ok(table) = hk_context::BandTable::bundled(hk_context::Region::Us) else {
                    continue;
                };
                let mut repo = shared.repo();
                if let Err(e) = record_channels(&mut repo, &table, &found, &device_id, seen) {
                    eprintln!("hk-pipeline: atsc survey: {e}");
                }
            }
            ReadOutcome::Overrun { resume_at, .. } => {
                window = Vec::new();
                held = None;
                next_index = None;
                cursor.set(resume_at);
            }
            ReadOutcome::Empty => {}
            ReadOutcome::Closed => break,
        }
    }
    drop(cursor);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hk_context::{BandTable, Region};
    use hk_estimate::Estimate;
    use hk_estimate::atsc::{PILOT_OFFSET_HZ, us_uhf_channel_lo_hz};
    use hk_estimate::estimate::Method;
    use hk_model::{InventoryQuery, KnownStatus};

    fn table() -> BandTable {
        BandTable::bundled(Region::Us).unwrap()
    }

    fn t(s: i64) -> Timestamp {
        Timestamp::from_unix_nanos(s * 1_000_000_000)
    }

    /// A channel as `hk_estimate::atsc` would report it: US channel `n`, read `ppm` low.
    fn channel(n: u16, ppm: f64) -> AtscChannel {
        let nominal = us_uhf_channel_lo_hz(n).unwrap() + PILOT_OFFSET_HZ;
        let measured = nominal * (1.0 + ppm * 1e-6);
        let f_lo = measured - PILOT_OFFSET_HZ;
        AtscChannel {
            pilot: Estimate::measured(measured, 40.0, Method::ToneFrequency),
            f_lo_hz: f_lo,
            f_hi_hz: f_lo + CHANNEL_BANDWIDTH_HZ,
            f_center_hz: f_lo + CHANNEL_BANDWIDTH_HZ / 2.0,
            flat_bandwidth_hz: FLAT_BANDWIDTH_HZ,
            plateau_snr_db: 18.0,
            pilot_excess_db: 24.0,
            flatness_db: 0.7,
            us_channel: Some(n),
            ppm: Estimate::measured(ppm, 0.1, Method::ClockPpm),
        }
    }

    /// The ticket's headline: a recognised 8VSB emission is **one row, 6 MHz wide**, explained as
    /// TV broadcast by the pilot it was measured from — never a shape-only `fm-broadcast`
    /// suggestion, and never "no allocation data" (T-979).
    #[test]
    fn t979_one_eight_vsb_channel_is_one_six_megahertz_row_explained_by_its_pilot() {
        let mut repo = Repository::open_in_memory().unwrap();
        let table = table();
        let c = channel(29, -4.0);
        let out = record_channels(
            &mut repo,
            &table,
            std::slice::from_ref(&c),
            "synthetic:t-979",
            TimeRange::new(t(0), t(1)),
        )
        .unwrap();
        assert_eq!(out.emitters.len(), 1);
        let id = out.emitters[0];

        // One row, 6 MHz wide, at the channel the pilot implies.
        let e = repo.emitter(id).unwrap();
        assert!((e.bandwidth_hz - 6e6).abs() < 1.0, "{} Hz", e.bandwidth_hz);
        assert!(
            (e.f_center_hz - 563e6).abs() < 5e3,
            "channel 29 is 560-566 MHz; row centre {} Hz",
            e.f_center_hz
        );
        assert_eq!(
            repo.query_inventory(&InventoryQuery::default())
                .unwrap()
                .entries
                .len(),
            1,
            "one channel is one row"
        );

        // Explained: TV broadcast on top, and the explanation cites the measured pilot.
        let ex = family::explanations(&repo, id).unwrap();
        let top = ex.first().expect("an explanation");
        assert_eq!(top.service, "tv-broadcast", "{ex:#?}");
        assert!(!top.has_flag("shape-only"), "{top:#?}");
        assert!(!top.has_flag("no-allocation-data"), "{top:#?}");
        assert!(!top.has_flag("off-raster"), "on the 6 MHz raster: {top:#?}");
        assert_eq!(top.status, KnownStatus::Known);
        assert_eq!(
            top.prior_ref.as_deref(),
            Some("us-47cfr2106-compact:tv-broadcast-uhf")
        );
        let pilot = top
            .evidence
            .iter()
            .find_map(|e| match e {
                ExplanationEvidence::Pilot {
                    measured_hz,
                    nominal_hz,
                    offset_hz,
                    channel,
                    ..
                } => Some((*measured_hz, *nominal_hz, *offset_hz, *channel)),
                _ => None,
            })
            .expect("the explanation cites the pilot");
        let nominal = 560e6 + PILOT_OFFSET_HZ;
        assert!((pilot.0 - nominal * (1.0 - 4e-6)).abs() < 1.0, "{pilot:?}");
        assert_eq!(pilot.1, Some(nominal));
        // The explorer's "~2.4 kHz low": 560.309 MHz read 4 ppm low is -2.24 kHz.
        assert!(
            (pilot.2.unwrap() - (-2_241.2)).abs() < 1.0,
            "pilot offset {} Hz",
            pilot.2.unwrap()
        );
        assert_eq!(pilot.3, Some(29));

        // No second row on a re-measurement of the same channel.
        record_channels(
            &mut repo,
            &table,
            std::slice::from_ref(&c),
            "synthetic:t-979",
            TimeRange::new(t(2), t(3)),
        )
        .unwrap();
        assert_eq!(
            repo.query_inventory(&InventoryQuery::default())
                .unwrap()
                .entries
                .len(),
            1,
            "a re-measured channel is the same row"
        );
    }

    /// The pilot offset is a free receiver-ppm measurement, and it is recorded as one: thirteen
    /// channels reading 4 ppm low are one oscillator **4 ppm fast** (T-979 deliverable (c)).
    #[test]
    fn t979_the_pilot_offsets_are_recorded_as_a_receiver_calibration() {
        let mut repo = Repository::open_in_memory().unwrap();
        let table = table();
        let channels: Vec<AtscChannel> = [14, 20, 29, 36].map(|n| channel(n, -4.0)).into();
        let out = record_channels(
            &mut repo,
            &table,
            &channels,
            "synthetic:t-979",
            TimeRange::new(t(0), t(1)),
        )
        .unwrap();
        assert_eq!(out.emitters.len(), 4, "four channels, four rows");
        let cal = out.calibration.expect("a calibration observation");
        assert_eq!(cal.method, CalibrationMethod::AtscPilot);
        assert_eq!(cal.device_id, "synthetic:t-979");
        assert!(
            (cal.ppm - 4.0).abs() < 0.01,
            "read 4 ppm low is an oscillator 4 ppm fast, got {}",
            cal.ppm
        );
        assert_eq!(
            repo.latest_calibration_state_for_device(
                "synthetic:t-979",
                &CalibrationMethod::AtscPilot
            )
            .unwrap()
            .map(|c| c.id),
            Some(cal.id)
        );

        // An unchanged clock does not write a new version.
        let again = record_channels(
            &mut repo,
            &table,
            &channels,
            "synthetic:t-979",
            TimeRange::new(t(2), t(3)),
        )
        .unwrap();
        assert!(again.calibration.is_none(), "{:?}", again.calibration);
    }

    /// A channel that is not on any grid is still one 6 MHz row — it just has no channel number
    /// and contributes no ppm, because a ppm needs a nominal.
    #[test]
    fn t979_a_channel_off_every_grid_records_no_clock() {
        let mut repo = Repository::open_in_memory().unwrap();
        let table = table();
        let mut c = channel(29, 0.0);
        c.us_channel = None;
        c.ppm = Estimate::abstain(Method::ClockPpm, hk_estimate::Reason::NoCalibration);
        let out = record_channels(
            &mut repo,
            &table,
            std::slice::from_ref(&c),
            "synthetic:t-979",
            TimeRange::new(t(0), t(1)),
        )
        .unwrap();
        assert_eq!(out.emitters.len(), 1);
        assert!(out.calibration.is_none());
    }

    /// The cost short-circuit: a tuned span narrower than one 6 MHz channel is never accumulated,
    /// whatever it is tuned to. Every 2.4 Msps fixture in the suite takes this path.
    #[test]
    fn t979_a_span_too_narrow_for_a_channel_is_never_measured() {
        let survey = AtscSurvey::default();
        let p = |fs: f64| -> Provenance {
            serde_json::from_value(serde_json::json!({
                "device_id": "synthetic:t-979",
                "tune": {
                    "center_hz": 563e6, "sample_rate_hz": fs, "lna_db": 24.0,
                    "vga_db": 20.0, "amp_on": false, "bandwidth_hz": fs,
                },
                "overload": false, "quantisation_limited": false, "clock_source": "internal",
                "clock_locked": true, "timestamp_method": "synthetic",
                "timestamp_error_budget_ns": 0,
            }))
            .expect("a provenance record")
        };
        assert!(!survey.wanted(&p(2.4e6)), "the FM fixture rate");
        assert!(!survey.wanted(&p(5.0e6)), "narrower than the flat band");
        assert!(survey.wanted(&p(8.0e6)));

        // One measurement per capture state once it has found something.
        survey.record(&p(8.0e6), 1);
        assert!(!survey.wanted(&p(8.0e6)), "answered");
        // An empty state is retried, and only up to the budget.
        let empty = p(10e6);
        for _ in 0..MAX_ATTEMPTS {
            assert!(survey.wanted(&empty));
            survey.record(&empty, 0);
        }
        assert!(!survey.wanted(&empty), "attempts spent");
    }
}
