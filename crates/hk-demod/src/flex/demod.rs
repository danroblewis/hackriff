//! Baseband → FLEX frames: discriminate, find sync-1 at 1600 Bd, read the FIW, then **measure**
//! which clock and alphabet the data section is sent in by which one makes its codewords check.
//!
//! # Why the data mode is measured rather than read off the header
//!
//! The sync-1 mode code *declares* the data section's rate and level count, and this decoder
//! reads and reports it. But the code is one 16-bit field, and the explorer's capture is the case
//! that shows why a declaration is not a measurement: on `flex-pagers-930p8` two independent
//! readings disagreed about the level count of two channels (T-949). So every frame's data
//! section is sliced under **each** hypothesis — 1600 or 3200 Bd, 2 or 4 levels — and the
//! hypothesis is chosen by evidence: the number of **clean** codewords (at most one correction,
//! parity holding — [`bch::Checked::clean`]; a correctable syndrome alone accepts half of all
//! random words) in the phases whose block-information word checks (a phase that carries a frame
//! always starts with one, and its checksum makes it non-zero). A 2-level signal read as 4-level yields phases of all-zero LSBs,
//! whose BIW is zero and fails its checksum, so the extra phases score nothing; a 3200 Bd signal
//! read at 1600 Bd drops every other symbol and scores nothing either. The declared mode is kept
//! beside the measured one, and [`FlexFrame::mode_agrees`] says whether they match.
//!
//! **A reading must explain every phase it claims.** Every FLEX phase starts each frame with a
//! block-information word, so a hypothesis under which an on-air phase has no valid BIW is
//! contradicted by the signal — a 2-level signal read as 4-level leaves its LSB phases all zero —
//! and ranks below every hypothesis that explains all of its phases, whatever its word count.
//!
//! **Two readings can be equally good, and then the evidence says so.** A 1600 Bd signal read at
//! 3200 Bd yields each symbol twice, so phase C repeats A (and D repeats B); and a real 3200 Bd
//! frame whose C and D phases carry the same words as A and B (an idle frame) is, sample for
//! sample, a 1600 Bd signal. When C and D repeat A and B (the words clean in both agree,
//! [`REPEAT_SHARE`]) they are not counted again. When the best score is then shared, the tie goes
//! to the declared mode, then to the slower rate and fewer levels, and
//! [`FlexFrame::levels_decisive`] / [`FlexFrame::rate_decisive`] record, per axis, whether the
//! codewords alone decided.
//!
//! # Carrier on the air
//!
//! A FLEX transmitter need not send all eleven blocks: on the capture, most frames key up for
//! the header and one to three blocks, then drop carrier. The data section is therefore cut at
//! the first block whose power falls [`OFF_AIR_DB`] below the header's, and only on-air blocks are
//! decoded or counted — a codeword that was never transmitted is not a failed one.

use num_complex::Complex32;

use super::bch;
use super::frame::{
    self, BLOCK_BITS, BLOCKS, Fiw, HEADER_BAUD, HEADER_BITS, MARKER_TO_FIW_BITS, Mode, Phase,
    SYNC2_S, WORDS_PER_BLOCK,
};
use crate::dsp::lowpass_taps;
use crate::fsk::demod::filter_same;

/// A block whose mean power is this far below the header's is off the air, dB.
pub const OFF_AIR_DB: f64 = 10.0;
/// Least sample rate the decoder accepts, Hz: eight samples per 3200 Bd symbol.
pub const MIN_SAMPLE_RATE_HZ: f64 = 8.0 * 3200.0;
/// Block duration per phase, s (256 bits at 1600 bit/s per phase).
const BLOCK_S: f64 = BLOCK_BITS as f64 / 1600.0;

/// The data-section hypotheses tried, `(baud, levels)`.
pub const HYPOTHESES: [(u32, u8); 4] = [(1600, 2), (1600, 4), (3200, 2), (3200, 4)];

/// What one hypothesis scored on one frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hypothesis {
    /// Data symbol rate tried, Bd.
    pub baud: u32,
    /// Levels tried.
    pub levels: u8,
    /// Phases counted whose block-information word checked.
    pub live_phases: usize,
    /// Phases counted whose block-information word did **not** check: the hypothesis fails to
    /// explain them.
    pub dead_phases: usize,
    /// Codewords in those phases passing the BCH check with parity ([`bch::Checked::valid`]).
    pub valid_words: usize,
    /// Codewords in those phases passing the evidence rule ([`bch::Checked::clean`]) — what
    /// ranks the hypotheses, since a correctable syndrome alone accepts half of random words.
    pub clean_words: usize,
    /// Codewords checked in those phases.
    pub words: usize,
}

impl Hypothesis {
    /// Rank key: explaining every phase first, then clean codewords.
    fn score(&self) -> (bool, usize) {
        (
            self.dead_phases == 0 && self.live_phases > 0,
            self.clean_words,
        )
    }
}

/// One FLEX frame found in the input.
#[derive(Clone, Debug, PartialEq)]
pub struct FlexFrame {
    /// Input sample index of the centre of the sync marker's first symbol.
    pub marker_sample: f64,
    /// Found in the inverted polarity (a spectrally inverted receiver path).
    pub inverted: bool,
    /// Bit errors in the sync marker.
    pub sync_errors: u32,
    /// The received mode code.
    pub code: u16,
    /// The mode the code declares, when it is one the table knows.
    pub declared: Option<Mode>,
    /// The frame information word, when its codeword checked.
    pub fiw: Option<Fiw>,
    /// Header (2-level, 1600 Bd) outer deviation, Hz — measured.
    pub deviation_hz: f64,
    /// Residual carrier offset, Hz — measured.
    pub carrier_offset_hz: f64,
    /// Data blocks per phase on the air.
    pub blocks_on_air: usize,
    /// Every hypothesis tried, with what it scored.
    pub hypotheses: Vec<Hypothesis>,
    /// The hypothesis the codewords chose (`None` when no phase checked under any).
    pub measured: Option<Hypothesis>,
    /// The chosen **level count** scored strictly more than the best hypothesis with the other
    /// level count: the codewords alone decided the alphabet.
    pub levels_decisive: bool,
    /// The chosen **rate** scored strictly more than the best hypothesis at the other rate: the
    /// codewords alone decided the clock. (An idle 3200 Bd frame whose C/D phases repeat A/B is,
    /// sample for sample, a 1600 Bd one, and this is then `false`.)
    pub rate_decisive: bool,
    /// Share of data symbols (on air, at the measured rate) nearer the inner pair of a 4-level
    /// alphabet than the outer — the level *occupancy*, separate from the alphabet.
    pub inner_fraction: f64,
    /// The phases decoded under the measured hypothesis.
    pub phases: Vec<Phase>,
}

impl FlexFrame {
    /// The measured data mode equals the declared one.
    pub fn mode_agrees(&self) -> Option<bool> {
        let (m, d) = (self.measured?, self.declared?);
        Some(m.baud == d.baud && m.levels == d.levels)
    }

    /// BCH-valid codewords (with parity) across the live phases.
    pub fn valid_words(&self) -> usize {
        self.measured.map_or(0, |m| m.valid_words)
    }

    /// Clean codewords across the live phases.
    pub fn clean_words(&self) -> usize {
        self.measured.map_or(0, |m| m.clean_words)
    }

    /// Codewords checked across the live phases.
    pub fn words(&self) -> usize {
        self.measured.map_or(0, |m| m.words)
    }
}

/// Everything decoded from one input.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FlexReport {
    /// Frames, in time order.
    pub frames: Vec<FlexFrame>,
    /// Input sample rate, Hz.
    pub sample_rate_hz: f64,
}

/// Why nothing could be decoded.
#[derive(Clone, Debug, PartialEq)]
pub enum FlexError {
    /// The sample rate is below [`MIN_SAMPLE_RATE_HZ`] or not finite.
    SampleRate(f64),
}

/// Centred moving average of `x` over `w` samples (prefix sums; edges averaged over what exists).
fn moving_average(x: &[f64], w: usize) -> Vec<f64> {
    let w = w.max(1);
    let mut c = Vec::with_capacity(x.len() + 1);
    c.push(0.0);
    for v in x {
        c.push(c.last().unwrap() + v);
    }
    let h = w / 2;
    (0..x.len())
        .map(|n| {
            let a = n.saturating_sub(h);
            let b = (n + w - h).min(x.len());
            (c[b] - c[a]) / (b - a) as f64
        })
        .collect()
}

fn at(y: &[f64], t: f64) -> Option<f64> {
    if t < 0.0 {
        return None;
    }
    let i = t.round() as usize;
    y.get(i).copied()
}

/// Decodes every FLEX frame in channel-centred baseband `samples` at `fs`.
///
/// The input should be the channel alone (a DDC of about ±12.5 kHz); it is low-passed again to
/// the FLEX occupancy before the discriminator. Nothing about the emission is supplied — rate and
/// alphabet are measured per frame (module docs).
pub fn decode(samples: &[Complex32], fs: f64) -> Result<FlexReport, FlexError> {
    if !(fs.is_finite() && fs >= MIN_SAMPLE_RATE_HZ) {
        return Err(FlexError::SampleRate(fs));
    }
    let mut report = FlexReport {
        frames: Vec::new(),
        sample_rate_hz: fs,
    };
    let sps = fs / HEADER_BAUD;
    if samples.len() < (sps * (HEADER_BITS + 64) as f64) as usize {
        return Ok(report);
    }
    // FLEX occupies about ±(4.8 kHz + half a 3200 Bd symbol rate): pass ±9 kHz.
    let x = match lowpass_taps(fs, 9_000.0, 13_000.0f64.min(0.49 * fs), 50.0) {
        Ok(taps) => filter_same(samples, &taps),
        Err(_) => samples.to_vec(),
    };
    let k = fs / std::f64::consts::TAU;
    let mut fi = Vec::with_capacity(x.len());
    fi.push(0.0);
    for w in x.windows(2) {
        fi.push(f64::from((w[1] * w[0].conj()).arg()) * k);
    }
    let power = moving_average(
        &x.iter()
            .map(|v| f64::from(v.norm_sqr()))
            .collect::<Vec<_>>(),
        sps.round() as usize,
    );
    let y = moving_average(&fi, sps.round() as usize);

    for (s, inverted, errors) in find_syncs(&y, sps) {
        if let Some(f) = read_frame(&fi, &y, &power, fs, s, inverted, errors) {
            report.frames.push(f);
        }
    }
    Ok(report)
}

/// Candidate sync-1 positions: `(centre of the mode code's first symbol, inverted, marker
/// errors)`, one per frame.
fn find_syncs(y: &[f64], sps: f64) -> Vec<(f64, bool, u32)> {
    let span = 64.0 * sps;
    let mut hits: Vec<(f64, bool, u32, f64)> = Vec::new();
    let n = y.len() as f64;
    let mut s = sps / 2.0;
    while s + span < n {
        // Marker bits first (early exit): marker polarity, 1 = lower frequency.
        let bit = |k: usize| u64::from(y[(s + k as f64 * sps).round() as usize] < 0.0);
        let mut w = 0u64;
        for k in 0..64 {
            w = w << 1 | bit(k);
        }
        for (window, inverted) in [(w, false), (!w, true)] {
            if frame::sync_code(window).is_some() {
                let marker = (window >> 16) as u32;
                let errors = (marker ^ frame::SYNC_MARKER).count_ones();
                let eye: f64 = (0..64)
                    .map(|k| y[(s + k as f64 * sps).round() as usize].abs())
                    .sum();
                hits.push((s, inverted, errors, eye));
            }
        }
        s += 1.0;
    }
    // One per frame: hits within half a frame are the same sync, and the clearest eye wins.
    let mut out: Vec<(f64, bool, u32, f64)> = Vec::new();
    for h in hits {
        match out.last_mut() {
            Some(last) if h.0 - last.0 < 8.0 * sps => {
                if h.3 > last.3 {
                    *last = h;
                }
            }
            _ => out.push(h),
        }
    }
    out.into_iter().map(|(s, i, e, _)| (s, i, e)).collect()
}

#[allow(clippy::too_many_arguments)]
fn read_frame(
    fi: &[f64],
    y: &[f64],
    power: &[f64],
    fs: f64,
    s: f64,
    inverted: bool,
    sync_errors: u32,
) -> Option<FlexFrame> {
    let sps = fs / HEADER_BAUD;
    let header = |k: usize| at(y, s + k as f64 * sps);
    // Carrier offset: the marker is balanced (sixteen ones, sixteen zeros).
    let marker: Vec<f64> = (16..48).filter_map(header).collect();
    if marker.len() < 32 {
        return None;
    }
    let c0 = marker.iter().sum::<f64>() / marker.len() as f64;
    let head: Vec<f64> = (0..HEADER_BITS).filter_map(header).collect();
    if head.len() < HEADER_BITS {
        return None;
    }
    let dev = head.iter().map(|v| (v - c0).abs()).sum::<f64>() / head.len() as f64;
    let mut code = 0u16;
    for k in 0..16 {
        let low = header(k)? < c0;
        code = code << 1 | u16::from(low != inverted);
    }
    // FIW, data polarity: 1 = higher frequency (the marker's complement).
    let fiw_at = 16 + MARKER_TO_FIW_BITS;
    let mut fiw_word = 0u32;
    for j in 0..32 {
        let high = header(fiw_at + j)? > c0;
        fiw_word |= u32::from(high != inverted) << j;
    }
    let fiw = bch::check(fiw_word).map(|c| Fiw::parse(&c));
    let declared = frame::mode_of(code);

    // Data section: first data symbol boundary after the header and sync-2.
    let t0 = s - sps / 2.0 + HEADER_BITS as f64 * sps + SYNC2_S * fs;
    let head_power = {
        let a = (s - sps / 2.0).max(0.0) as usize;
        let b = ((s + HEADER_BITS as f64 * sps) as usize).min(power.len());
        power[a..b].iter().sum::<f64>() / (b - a).max(1) as f64
    };
    let floor = head_power * 10f64.powf(-OFF_AIR_DB / 10.0);
    let block_len = BLOCK_S * fs;
    let mut blocks_on_air = 0;
    for b in 0..BLOCKS {
        let a = (t0 + b as f64 * block_len) as usize;
        let e = (t0 + (b + 1) as f64 * block_len) as usize;
        if e > power.len() {
            break;
        }
        let p = power[a..e].iter().sum::<f64>() / (e - a) as f64;
        if p < floor {
            break;
        }
        blocks_on_air += 1;
    }

    let mut tried = Vec::new();
    for (baud, levels) in HYPOTHESES {
        tried.push(slice(
            fi,
            fs,
            t0,
            blocks_on_air,
            baud,
            levels,
            c0,
            dev,
            inverted,
        ));
    }
    let hypotheses: Vec<Hypothesis> = tried.iter().map(|t| t.0).collect();
    let top = hypotheses
        .iter()
        .map(Hypothesis::score)
        .max()
        .unwrap_or((false, 0));
    // Hypotheses within the tie margin of the best are equally supported ([`tied_with`]); among
    // them the declared mode wins, then the slower rate, then fewer levels (HYPOTHESES is in that
    // order, so the first tied entry is the simplest).
    let chosen = (top.1 > 0).then(|| {
        let tied: Vec<usize> = (0..tried.len())
            .filter(|i| tied_with(hypotheses[*i].score(), top))
            .collect();
        tied.iter()
            .copied()
            .find(|i| {
                declared.is_some_and(|d| {
                    d.baud == hypotheses[*i].baud && d.levels == hypotheses[*i].levels
                })
            })
            .unwrap_or(tied[0])
    });
    let beaten = |other: &dyn Fn(&Hypothesis) -> bool| {
        hypotheses
            .iter()
            .filter(|h| other(h))
            .all(|h| !tied_with(h.score(), top))
    };
    let (levels_decisive, rate_decisive) = match chosen.map(|i| hypotheses[i]) {
        Some(c) => (
            beaten(&|h: &Hypothesis| h.levels != c.levels),
            beaten(&|h: &Hypothesis| h.baud != c.baud),
        ),
        None => (false, false),
    };
    let (measured, phases, inner_fraction) = match chosen {
        Some(i) => {
            let (h, p, inner) = tried.swap_remove(i);
            (Some(h), p, inner)
        }
        None => (None, Vec::new(), 0.0),
    };
    Some(FlexFrame {
        marker_sample: s + 16.0 * sps,
        inverted,
        sync_errors,
        code,
        declared,
        fiw,
        deviation_hz: dev,
        carrier_offset_hz: c0,
        blocks_on_air,
        hypotheses,
        measured,
        levels_decisive,
        rate_decisive,
        inner_fraction,
        phases,
    })
}

/// Slices the on-air data section under one hypothesis and decodes its phases.
#[allow(clippy::too_many_arguments)]
fn slice(
    fi: &[f64],
    fs: f64,
    t0: f64,
    blocks: usize,
    baud: u32,
    levels: u8,
    c0: f64,
    dev: f64,
    inverted: bool,
) -> (Hypothesis, Vec<Phase>, f64) {
    let sps = fs / f64::from(baud);
    let nsym = (blocks as f64 * BLOCK_S * f64::from(baud)).round() as usize;
    let mut h = Hypothesis {
        baud,
        levels,
        live_phases: 0,
        dead_phases: 0,
        valid_words: 0,
        clean_words: 0,
        words: 0,
    };
    if nsym == 0 {
        return (h, Vec::new(), 0.0);
    }
    let yd = moving_average(fi, (0.8 * sps).round() as usize);
    // Timing: the phase that opens the eye widest over the section.
    let probe = nsym.min(1024);
    let mut best = (f64::MIN, 0.0);
    let steps = 16;
    for i in 0..steps {
        let d = (i as f64 / steps as f64 - 0.5) * sps;
        let m: f64 = (0..probe)
            .filter_map(|k| at(&yd, t0 + d + (k as f64 + 0.5) * sps))
            .map(|v| (v - c0).abs())
            .sum();
        if m > best.0 {
            best = (m, d);
        }
    }
    let vals: Vec<f64> = (0..nsym)
        .filter_map(|k| at(&yd, t0 + best.1 + (k as f64 + 0.5) * sps))
        .map(|v| v - c0)
        .collect();
    if vals.len() < nsym {
        return (h, Vec::new(), 0.0);
    }
    // Level centres: start from the header's outer deviation and an inner pair at a third of it
    // (FLEX's ±4.8/±1.6 kHz geometry), then let the data move them (a few Lloyd iterations; a
    // level holding under 3 % of the symbols keeps its start, so a sparsely used inner pair
    // cannot drag its centre onto the outer one).
    let mut centres = [-dev, -dev / 3.0, dev / 3.0, dev];
    for _ in 0..6 {
        let mut sum = [0.0; 4];
        let mut cnt = [0usize; 4];
        for v in &vals {
            let i = nearest(&centres, *v);
            sum[i] += v;
            cnt[i] += 1;
        }
        for i in 0..4 {
            if cnt[i] * 33 >= vals.len() {
                centres[i] = sum[i] / cnt[i] as f64;
            }
        }
    }
    let inner = vals
        .iter()
        .filter(|v| matches!(nearest(&centres, **v), 1 | 2))
        .count() as f64
        / vals.len() as f64;
    let syms: Vec<u8> = vals
        .iter()
        .map(|v| {
            let s = if levels == 2 {
                if *v > 0.0 { 3 } else { 0 }
            } else {
                nearest(&centres, *v) as u8
            };
            if inverted { 3 - s } else { s }
        })
        .collect();
    let bit_a = |s: u8| u8::from(s > 1);
    let bit_b = |s: u8| u8::from(s == 1 || s == 2);
    let mut streams: Vec<(char, Vec<u8>)> = Vec::new();
    if baud == 1600 {
        streams.push(('A', syms.iter().map(|s| bit_a(*s)).collect()));
        if levels == 4 {
            streams.push(('B', syms.iter().map(|s| bit_b(*s)).collect()));
        }
    } else {
        let even = || syms.iter().step_by(2);
        let odd = || syms.iter().skip(1).step_by(2);
        streams.push(('A', even().map(|s| bit_a(*s)).collect()));
        if levels == 4 {
            streams.push(('B', even().map(|s| bit_b(*s)).collect()));
        }
        streams.push(('C', odd().map(|s| bit_a(*s)).collect()));
        if levels == 4 {
            streams.push(('D', odd().map(|s| bit_b(*s)).collect()));
        }
    }
    let decoded: Vec<(Vec<Option<bch::Checked>>, Phase)> = streams
        .into_iter()
        .map(|(letter, bits)| {
            let mut words = frame::deinterleave(&bits);
            words.truncate(blocks * WORDS_PER_BLOCK);
            let checked: Vec<_> = words.iter().map(|w| bch::check(*w)).collect();
            let phase = frame::parse_phase(letter, &checked);
            (checked, phase)
        })
        .collect();
    // At 3200 Bd the streams are A, B?, C, D?: a second half that repeats the first is one set
    // of symbols read twice (module docs), and counts once.
    let half = decoded.len() / 2;
    let repeated = baud == 3200 && (0..half).all(|i| repeats(&decoded[i].0, &decoded[half + i].0));
    let counted = if repeated { half } else { decoded.len() };
    for (_, p) in decoded.iter().take(counted) {
        if p.biw_ok {
            h.live_phases += 1;
            h.valid_words += p.valid;
            h.clean_words += p.clean;
            h.words += p.words;
        } else {
            h.dead_phases += 1;
        }
    }
    let phases = decoded.into_iter().map(|(_, p)| p).collect();
    (h, phases, inner)
}

/// Least margin, in clean codewords, by which a hypothesis must beat another to be *better*
/// rather than tied: half a block of one phase, or 5 % of the score. On the capture a garbled
/// block (carrier on, no FSK structure) leaves one or two random words clean under every
/// hypothesis, and a one-word lead is that noise, not a measurement.
const TIE_MARGIN_WORDS: usize = 4;

/// `a` is tied with the best score `top` (see [`TIE_MARGIN_WORDS`]).
fn tied_with(a: (bool, usize), top: (bool, usize)) -> bool {
    let margin = TIE_MARGIN_WORDS.max(top.1 / 20);
    a.0 == top.0 && a.1 + margin > top.1
}

/// Least share of the words valid in both phases that must carry equal data for one phase to be
/// a re-reading of the other. Noise makes a re-read differ in raw bits, never in corrected data;
/// two genuinely separate phases agree only where both are idle.
const REPEAT_SHARE: f64 = 0.9;

/// Whether phase `b` repeats phase `a`: of the words clean in both (at least four), at least
/// [`REPEAT_SHARE`] carry the same data.
fn repeats(a: &[Option<bch::Checked>], b: &[Option<bch::Checked>]) -> bool {
    let clean = |w: &Option<bch::Checked>| w.filter(bch::Checked::clean).map(|c| c.data());
    let both: Vec<(u32, u32)> = a
        .iter()
        .zip(b)
        .filter_map(|(x, y)| Some((clean(x)?, clean(y)?)))
        .collect();
    both.len() >= 4
        && both.iter().filter(|(x, y)| x == y).count() as f64 >= REPEAT_SHARE * both.len() as f64
}

fn nearest(centres: &[f64; 4], v: f64) -> usize {
    let mut best = 0;
    for i in 1..4 {
        if (v - centres[i]).abs() < (v - centres[best]).abs() {
            best = i;
        }
    }
    best
}
