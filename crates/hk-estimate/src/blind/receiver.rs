//! Receiver-wide cyclic lines, **measured** rather than listed (T-394).
//!
//! # A line at one frequency in channels holding nothing is the receiver's
//!
//! Three receiver-wide artefacts of `fixtures/hackrf/capture-2026-09-15-fm-band` were found one at
//! a time, and each exclusion revealed the next: the `fs/8192` gain step of hackriff's own
//! ring-reader output path (T-317, T-373), then the host's exact 8 kHz USB-microframe comb and a
//! free-running ~655.75 Hz amplitude modulation of the receiver's own noise contribution (T-382).
//! Each is recorded in [`hk_model::Provenance::capture_artefacts`] and excluded by name, and after
//! all three a **fourth** family spaced ~119.95 Hz was still visible underneath at 13–17 dB.
//!
//! A list of named frequencies does not generalise. A different host, a different capture or a
//! different temperature moves them, the list grows forever, and in between revisions C14 keeps
//! measuring whichever artefact is currently on top — the defect family this repository keeps
//! finding, where a real number describes the receiver instead of the emission.
//!
//! The general test is a property of the **measurement**, and it needs no record at all:
//!
//! > A candidate cyclic line that appears at **one frequency** in channels of this same capture
//! > that hold **no emission** is device-local by construction.
//!
//! This module measures that. It is the discriminator T-382's closing observation asked for, and
//! it would have caught all four families the first time each appeared.
//!
//! # What is measured
//!
//! 1. **Channelise the capture** with the shipped polyphase filter bank
//!    ([`hk_dsp::channelizer`]): `M` channels across the tuned span, each flat over its own
//!    `±Δ/2` with ≤ −60 dB of adjacent-channel leakage. One pass gives every channel at once, so
//!    the survey costs one filter bank rather than `M` DDCs.
//! 2. **Find the channels that hold nothing.** A channel's mean power against a running median of
//!    its neighbours' — the same baseline shape the fixture's own truth pass used, which absorbs
//!    the noise floor and the analogue filter skirt together. A channel more than
//!    [`SurveyConfig::occupancy_db`] above that baseline holds an emission; so does either of its
//!    immediate neighbours' worth of skirt, so a reference channel must also have quiet
//!    neighbours.
//! 3. **Whiten each reference channel** through exactly [`super::lines::whiten`] — C14's own
//!    transform, not a second one — on `|x|²`, and reduce to the transform's **native**
//!    (independent) resolution by taking the peak within each native cell.
//! 4. **Take the median across reference channels, bin by bin.** That median is the receiver-line
//!    spectrum, and its peaks are the lines.
//!
//! # Why the median is the control, and not a detail
//!
//! The case that must not break is a genuine emission whose cyclic structure reaches several
//! channels at once — a strong station's 19 kHz stereo pilot leaking into the boxes either side of
//! it, and, worse, *every* FM station carrying a pilot at the same 19 kHz. A rule that said "seen
//! in more than one channel" would eat it.
//!
//! The median says something different: **more than half the channels that hold nothing carry this
//! line**. An emission's cyclic structure is confined to the emission's own band and its skirt; the
//! receiver's is not. Measured over 32 channels of 75 kHz across `capture-2026-09-15-fm-band`
//! (2 s at 2.4 Msps, 16 reference channels after the occupancy and neighbour rules, and with the
//! fixture's recorded artefacts **stripped** so only the measurement can act):
//!
//! | line | median over the reference channels |
//! |---|---|
//! | 8 kHz host comb, h1–h7 | 19.5–30.8 dB |
//! | ~655.75 Hz family, h1–h4 | 15.3–33.5 dB |
//! | `fs/8192` gain-step comb, h1–h6 | 14.8–21.0 dB |
//! | **~119.95 Hz family** (1439/1559/1679 and 2039/2159/2279 Hz) | **12.4–17.9 dB** |
//! | **19 kHz stereo pilot** | **3.2 dB — not a line at all** |
//! | **38 kHz stereo subcarrier** | **0.6 dB — not a line at all** |
//!
//! — while the pilot reads **35.3 dB** in the station's own channel and ≥ 18 dB in six others. The
//! separation is 9 dB between the weakest receiver family and the strongest thing a real emission
//! put into this statistic, and the median is what produces it: with the occupancy rule removed
//! entirely and all 32 channels used as references the pilot still reads only 4.2 dB, because 7
//! channels of 32 is not a majority. The fourth family had never been measured before; the three
//! above it each needed a provenance record, and this recovers all three without one.
//!
//! **The honest limit is the same sentence.** If more than half the channels that hold nothing
//! carried an emission's line, this would call it device-local. That needs a band where the
//! emissions are everywhere *and* their structure reaches the quiet channels — and where the
//! occupancy rule has not already removed those channels from the reference set. Where it removes
//! too many, [`SurveyConfig::min_reference_channels`] makes the survey **abstain** rather than
//! measure a majority of three.
//!
//! # What it costs, and where it runs
//!
//! One filter-bank pass plus one whitened periodogram per reference channel, over a window capped
//! at [`SurveyConfig::window_s`] — **once per capture window**, not once per classification, and on
//! the same CPU call site C14 already runs on (ADR-0007). The per-channel spectra are reduced to
//! native resolution before they are held, so the working set is
//! `reference channels × band / native cell` f32s (about 8 MB at the geometry above), not the
//! zero-padded transform.

use hk_dsp::channelizer::batch::BatchPfb;
use hk_dsp::{InputInfo, IqSample, PfbConfig};
use hk_model::Provenance;
use num_complex::Complex32;

use super::lines::{ARTEFACT_GUARD_NATIVE_BINS, Plans, whiten};

/// Settings for [`survey`]. The defaults are the geometry measured in the [module docs](self).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SurveyConfig {
    /// Filter-bank channel count `M`: the span is split into `M` channels of `fs/M`, each output
    /// at `2·fs/M`.
    ///
    /// It sets both what counts as "a channel holding nothing" and how far up the survey can look:
    /// the per-channel band reaches `2·fs/M / 2.5`, which must cover the rate band any box C14
    /// analyses will search. Wider channels see further up but hold more emissions, so fewer of
    /// them are empty; narrower channels leave more of them empty but stop lower. At 2.4 Msps,
    /// `M` = 32 gives 75 kHz channels reaching 60 kHz — past the 38 kHz stereo subcarrier, which
    /// is the highest thing the FM fixture puts in this statistic.
    pub channels: usize,
    /// Excess over the running-median baseline at which a channel is judged to hold an emission,
    /// dB.
    ///
    /// Measured on `capture-2026-09-15-fm-band`: every channel carrying either station's pilot
    /// reads ≥ +4.1 dB, and the quietest 25 sit within ±1.6 dB of the baseline (the analogue
    /// filter skirt, which the running median removes). 2 dB sits between them.
    pub occupancy_db: f64,
    /// Channels either side of an occupied one that also cannot be references: an emission's skirt
    /// reaches its neighbours even where its power does not.
    pub neighbour_channels: usize,
    /// Fewest reference channels the median may be taken over. Below this the survey abstains: a
    /// majority of three is not evidence that a line is everywhere.
    pub min_reference_channels: usize,
    /// Most reference channels whitened, evenly spaced across those available — the cost and
    /// memory bound. The median does not improve past a couple of dozen.
    pub max_reference_channels: usize,
    /// Median significance at which a frequency is called a receiver line, dB.
    ///
    /// The same threshold C14 counts a line at ([`super::BlindConfig::line_count_db`]), and it is
    /// not tuned to any capture. **The null is measured**: over a synthetic capture with no
    /// periodic receiver contribution — 11 reference channels, 59 990 native cells — the median
    /// statistic reads **max 7.52 dB**, p99.99 6.37, p99 4.78, median 1.58 dB. 12 dB is 4.5 dB
    /// clear of the largest thing noise produced anywhere in that band, and the real emissions'
    /// own structure lands at 0.6–3.2 dB, deep inside that null.
    ///
    /// Lowering it would pick up more: `capture-2026-09-15-fm-band` has a coherent 9.4 dB bump at
    /// 1316.9 Hz that 12 dB leaves behind, and it shows through as one box's 15 dB argmax. That is
    /// a 2 dB margin over the measured null, and buying it would be tuning this threshold to one
    /// fixture — which is the strategy this module exists to replace.
    pub line_db: f64,
    /// Most lines reported, strongest first. A bound on how much of a search band this can ever
    /// notch, independent of the quarter-band rule the caller applies.
    pub max_lines: usize,
    /// Longest window surveyed, seconds.
    ///
    /// Longer resolves the lines more finely (the native cell is `M/(2·window)` Hz) and lifts the
    /// weakest family above the threshold — the ~119.95 Hz family reads 11 dB at 0.5 s, 14 dB at
    /// 1 s and 17 dB at 2 s — but a free-running artefact wanders, so a survey that spans much
    /// more than its coherence time measures a smear rather than a line.
    pub window_s: f64,
    /// Width of the running median that forms the per-channel power baseline, in channels.
    pub baseline_channels: usize,
    /// Whitening block width as a fraction of the channel spacing, matching
    /// [`super::BlindConfig::whiten_block_obw`] against OBW99.
    pub whiten_block_channel: f64,
    /// Upper edge of the surveyed band as a fraction of the per-channel rate, matching
    /// [`super::BlindConfig::rate_max_fs`].
    pub band_max_fs: f64,
}

impl Default for SurveyConfig {
    fn default() -> Self {
        Self {
            channels: 32,
            occupancy_db: 2.0,
            neighbour_channels: 1,
            min_reference_channels: 8,
            max_reference_channels: 16,
            line_db: 12.0,
            max_lines: 1024,
            window_s: 2.0,
            baseline_channels: 9,
            whiten_block_channel: 1.0 / 8.0,
            band_max_fs: 1.0 / 2.5,
        }
    }
}

/// One cyclic line measured to belong to the receiver.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ReceiverLine {
    /// Peak of the receiver-line spectrum, Hz.
    pub freq_hz: f64,
    /// Median significance across the reference channels at that peak, dB.
    pub significance_db: f64,
    /// How far either side of the peak the receiver-line spectrum stays above
    /// [`SurveyConfig::line_db`], Hz.
    ///
    /// **A free-running artefact is a band, not a line**, and T-382 had to record
    /// [`hk_model::CaptureArtefact::drift_ppm`] by hand to say so. Measuring the width says it
    /// without a record: the ~655.75 Hz family of `capture-2026-09-15-fm-band` wanders while it is
    /// being watched, so it occupies a band of cells here rather than one, and a guard taken from
    /// its peak alone leaves the argmax free to walk to the edge of that band — which is exactly
    /// what it did, in 7 of 44 boxes, 4.2–5.9 Hz off the peak of its fundamental and its second
    /// harmonic.
    pub half_width_hz: f64,
}

/// Cyclic lines **measured** to belong to the receiver, with the survey that found them.
///
/// Unlike [`hk_model::CaptureArtefact`] nothing here was written down in advance: every frequency
/// is one this capture was measured to carry in channels holding nothing. See the
/// [module docs](self).
#[derive(Clone, Debug, PartialEq)]
pub struct ReceiverLines {
    /// Device this was measured through ([`hk_model::Provenance::device_id`]). Device-local
    /// physics reads the device (T-259/T-305), so a survey is never applied to another one.
    pub device_id: String,
    /// Tuned centre the survey was measured at, Hz.
    pub center_hz: f64,
    /// Sample rate the survey was measured at, Hz.
    pub sample_rate_hz: f64,
    /// Gain state the survey was measured under (LNA, VGA, amp): the receiver's own noise
    /// contribution, and anything riding on it, is a function of it.
    pub gain: (f64, f64, bool),
    /// The measured lines, **ascending** by frequency.
    pub lines: Vec<ReceiverLine>,
    /// Channels the median was taken over.
    pub reference_channels: usize,
    /// Channels the bank produced.
    pub surveyed_channels: usize,
    /// Channel spacing `fs/M`, Hz.
    pub channel_spacing_hz: f64,
    /// Band surveyed, Hz.
    pub band_hz: (f64, f64),
    /// Independent cell width of the survey transform, Hz: how finely a line's frequency is known.
    pub resolution_hz: f64,
    /// First source sample index surveyed.
    pub source_index: u64,
    /// Source samples surveyed.
    pub source_samples: u64,
}

impl ReceiverLines {
    /// Whether this survey describes the receiver state `p` was captured under.
    ///
    /// Device, tune and gain must all match: a retune or a gain step changes the receiver's own
    /// noise contribution and what rides on it, and a survey measured before one says nothing
    /// about after it.
    pub fn applies_to(&self, p: &Provenance) -> bool {
        p.device_id == self.device_id
            && p.tune.center_hz == self.center_hz
            && p.tune.sample_rate_hz == self.sample_rate_hz
            && (p.tune.lna_db, p.tune.vga_db, p.tune.amp_on) == self.gain
    }

    /// Clearance added either side of every line's own measured band when searching a record
    /// whose independent cell is `native_hz` wide.
    ///
    /// **Three widths meet here and they add, because they answer different questions** — the
    /// same reasoning C14's comb guard gives for a *drifting* recorded artefact (T-382).
    /// [`ReceiverLine::half_width_hz`] is where the line *was* while it was being watched, which
    /// the survey measured; [`ReceiverLines::resolution_hz`] is how finely the survey could pin
    /// that; and [`ARTEFACT_GUARD_NATIVE_BINS`] native cells of the searched record is its own
    /// transform's mainlobe clearance around wherever the line is. Taking the largest instead of
    /// the sum leaves the argmax walking the skirt, which T-373 measured and T-382 measured again.
    pub fn guard_hz(&self, native_hz: f64) -> f64 {
        ARTEFACT_GUARD_NATIVE_BINS * (native_hz.max(0.0) + self.resolution_hz)
    }

    /// Lines whose own band overlaps `[f_min, f_max]`, ascending.
    pub fn in_band(&self, f_min: f64, f_max: f64) -> &[ReceiverLine] {
        let a = self
            .lines
            .partition_point(|l| l.freq_hz + l.half_width_hz < f_min);
        let b = self
            .lines
            .partition_point(|l| l.freq_hz - l.half_width_hz <= f_max);
        &self.lines[a..b.max(a)]
    }

    /// The measured frequencies, ascending — for a caller that only wants to see them.
    pub fn frequencies(&self) -> impl ExactSizeIterator<Item = f64> + '_ {
        self.lines.iter().map(|l| l.freq_hz)
    }
}

/// Measures the receiver-wide cyclic lines of one capture window. See the [module docs](self).
///
/// `None` when the window is too short, the bank cannot be built, or too few channels hold nothing
/// ([`SurveyConfig::min_reference_channels`]) — an abstention, never an empty answer dressed up as
/// "this receiver is clean".
pub(crate) fn survey<T: IqSample>(
    plans: &mut Plans,
    info: InputInfo<'_>,
    samples: &[T],
    cfg: &SurveyConfig,
) -> Option<ReceiverLines> {
    let p = info.provenance.get();
    let fs = p.tune.sample_rate_hz;
    if !(fs.is_finite() && fs > 0.0) || cfg.channels < 4 || cfg.channels % 2 != 0 {
        return None;
    }
    let want = (cfg.window_s * fs) as usize;
    let x = &samples[..samples.len().min(want.max(1))];
    let mut pfb = BatchPfb::serial(PfbConfig::new(cfg.channels)).ok()?;
    let out = pfb.process(info, x);
    let (m, frames) = (out.channels, out.frames);
    // 32 samples is `whiten`'s own floor; anything near it makes a survey meaningless, so ask for
    // enough of a record that a whitening block holds its statistical minimum several times over.
    if frames < 4096 || out.active_channels().len() != m {
        return None;
    }
    let out_rate = 2.0 * fs / m as f64;
    let spacing = fs / m as f64;

    // --- 1. Which channels hold nothing? Mean power against a running median of the neighbours'.
    let mut power = vec![0.0f64; m];
    for f in 0..frames {
        for (j, v) in out.frame(f).iter().enumerate() {
            power[j] += f64::from(v.norm_sqr());
        }
    }
    let pw_db: Vec<f64> = power
        .iter()
        .map(|p| 10.0 * (p / frames as f64).max(1e-300).log10())
        .collect();
    let w = cfg.baseline_channels.max(1);
    let mut scratch = Vec::with_capacity(w);
    let occupied: Vec<bool> = (0..m)
        .map(|c| {
            scratch.clear();
            // The bank is circular in frequency (channel 0 straddles ±fs/2), so the baseline is.
            for k in 0..w {
                scratch.push(pw_db[(c + m + k - w / 2) % m]);
            }
            pw_db[c] - super::util::median(&scratch) >= cfg.occupancy_db
        })
        .collect();
    let g = cfg.neighbour_channels;
    let clear: Vec<usize> = (0..m)
        .filter(|&c| (0..=2 * g).all(|k| !occupied[(c + m + k - g) % m]))
        .collect();
    if clear.len() < cfg.min_reference_channels.max(3) {
        return None;
    }
    // Evenly spaced across what is available, so the median is taken over channels spread across
    // the span rather than over one quiet corner of it.
    let take = clear.len().min(cfg.max_reference_channels.max(3));
    let refs: Vec<usize> = (0..take)
        .map(|i| clear[i * clear.len() / take])
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();

    // --- 2. Whiten each reference channel through C14's own transform, at native resolution.
    let block_hz = cfg.whiten_block_channel * spacing;
    let f_max = cfg.band_max_fs * out_rate;
    let mut chan = vec![Complex32::default(); frames];
    let mut env = vec![0.0f64; frames];
    let mut cells: Vec<Vec<f32>> = Vec::with_capacity(refs.len());
    let mut geometry: Option<(f64, f64, usize)> = None; // (native_hz, f_lo, cell count)
    for &c in &refs {
        let s = out.channel(c)?;
        s.copy_into(&mut chan);
        for (e, v) in env.iter_mut().zip(&chan) {
            *e = f64::from(v.norm_sqr());
        }
        let wh = whiten(plans, &env, out_rate, block_hz)?;
        let i_lo = wh.first_usable(0.0);
        let i_hi = wh.last_usable(f_max);
        if i_hi < i_lo + 4 {
            return None;
        }
        let native = wh.native_hz;
        let f_lo = wh.freq_hz(i_lo);
        let n_cells = geometry.map_or_else(
            || (((wh.freq_hz(i_hi) - f_lo) / native).floor() as usize).max(1),
            |(_, _, n)| n,
        );
        geometry.get_or_insert((native, f_lo, n_cells));
        // Only native cells are independent; the zero padding interpolates between them. Reducing
        // to the peak within each native cell is the honest read of a line that falls between two
        // of them, and it is what keeps the working set to the band rather than to the transform.
        let mut row = vec![0.0f32; n_cells];
        for i in i_lo..=i_hi {
            let k = ((wh.freq_hz(i) - f_lo) / native).floor() as isize;
            if k < 0 {
                continue;
            }
            let Some(slot) = row.get_mut(k as usize) else {
                break;
            };
            *slot = slot.max(wh.ratio(i) as f32);
        }
        cells.push(row);
    }
    let (native_hz, f_lo, n_cells) = geometry?;

    // --- 3. The receiver-line spectrum: the median across reference channels, cell by cell.
    let mut column = vec![0.0f64; cells.len()];
    let med: Vec<f64> = (0..n_cells)
        .map(|k| {
            for (v, row) in column.iter_mut().zip(&cells) {
                *v = f64::from(row[k]);
            }
            10.0 * super::util::median(&column).max(1e-30).log10()
        })
        .collect();

    // --- 4. Peaks of it, strongest first, no two within the guard.
    let sep = ARTEFACT_GUARD_NATIVE_BINS.max(1.0) as usize;
    let mut order: Vec<usize> = (0..n_cells).filter(|&k| med[k] >= cfg.line_db).collect();
    order.sort_by(|&a, &b| med[b].total_cmp(&med[a]));
    let mut picked: Vec<usize> = Vec::new();
    for k in order {
        if picked.len() >= cfg.max_lines {
            break;
        }
        if picked.iter().any(|&t| k.abs_diff(t) < sep) {
            continue;
        }
        picked.push(k);
    }
    picked.sort_unstable();
    // Each line's own measured band: how far either side of the peak the receiver-line spectrum
    // stays above the threshold. A locked comb member is one cell wide; a free-running one is not,
    // and this is what says so without anyone recording a drift.
    let lines: Vec<ReceiverLine> = picked
        .iter()
        .map(|&k| {
            let mut lo = k;
            while lo > 0 && med[lo - 1] >= cfg.line_db {
                lo -= 1;
            }
            let mut hi = k;
            while hi + 1 < n_cells && med[hi + 1] >= cfg.line_db {
                hi += 1;
            }
            ReceiverLine {
                freq_hz: f_lo + (k as f64 + 0.5) * native_hz,
                significance_db: med[k],
                half_width_hz: (k - lo).max(hi - k) as f64 * native_hz,
            }
        })
        .collect();
    Some(ReceiverLines {
        device_id: p.device_id.clone(),
        center_hz: p.tune.center_hz,
        sample_rate_hz: fs,
        gain: (p.tune.lna_db, p.tune.vga_db, p.tune.amp_on),
        lines,
        reference_channels: refs.len(),
        surveyed_channels: m,
        channel_spacing_hz: spacing,
        band_hz: (f_lo, f_lo + n_cells as f64 * native_hz),
        resolution_hz: native_hz,
        source_index: info.time.sample_index,
        source_samples: x.len() as u64,
    })
}
