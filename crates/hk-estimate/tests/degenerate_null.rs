//! T-577 (MAUTO-RESEARCH): measures the two numbers ADR-0022's false-confirm derivation could
//! not derive: the **check-width floor** (§4.3, `min_check_width = 8`, an assumption) and the
//! **degenerate-framing null** (below about a byte a check "can be satisfied by a framing
//! artefact rather than by a code").
//!
//! Every input here carries **no code**. A width-`w` check that reaches ADR-0022 §4.2's
//! requirement on it is a false confirm. The harness uses the real pieces the gate reads:
//! `hk_estimate::framing::crc::BitCrc` (the `crc` block's engine), `hk_model::synth::null::
//! {check_bits, is_short_periodic}` (the S5 `check_distinct_valid` arithmetic and the
//! short-period guard `CheckTally` applies) and `hk_estimate::assist::search_codes` (the
//! searched-generator path). The gate is ADR-0022 §6 steps 1–3 in check-only accounting:
//! `min(check_bits(d, tested, w), w·d) − L_check ≥ 24`, which subsumes the 16-bit hard floor.
//!
//! Three measurements (the ticket's 1–3):
//! 1. [`degenerate_null_rate_per_width`] — per `w ∈ {4, 8, 12, 16, 24, 32}`, the rate at which a
//!    template-fixed and a searched width-`w` check reach `distinct_valid` / `differences` ≥ the
//!    §4.2 requirement on N2/N3-like bits and on synthetic framing artefacts.
//! 2. [`distinct_valid_versus_differences_on_real_emitters`] — the gap T-575's swap rests on.
//! 3. The same test's rank column — the GF(2) rank of the valid frames' differences, which is the
//!    number of frames that are *independent trials* for a linear check.
//!
//! The assertions pin the findings at a small default trial count so the finding cannot rot;
//! `HK_T577_TRIALS=<n> cargo nextest run -p hk-estimate -E 'binary(degenerate_null)'
//! --no-capture` prints the full tables (the numbers in ADR-0022 §4.3 came from n = 4000).

use std::collections::HashSet;

use hk_estimate::assist::{CodeSearchConfig, search_codes};
use hk_estimate::framing::crc::BitCrc;
use hk_model::synth::null::{SHORT_PERIOD_MAX_BITS, check_bits, is_short_periodic};

/// ADR-0022 §4.1.
const MIN_ANALYTIC: f64 = 24.0;
/// ADR-0022 §4.2's slot product for a searched check (start/tail/order/classes).
const SLOT_BITS: f64 = 5.0;
/// Frame length (bits): payload + check field.
const FRAME: usize = 96;
/// Burst payload length for the burst sources.
const BURST: usize = 40;
/// Frames (bursts) per window: one confirm decision.
const FRAMES_PER_WINDOW: usize = 24;
/// Frames handed to the generator search (it is the expensive path).
const SEARCH_FRAMES: usize = 12;
const WIDTHS: [u8; 6] = [4, 8, 12, 16, 24, 32];

fn trials(var: &str, default: usize) -> usize {
    std::env::var(var)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

// ---------------------------------------------------------------------------------------------
// Deterministic randomness.

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn bit(&mut self) -> u8 {
        (self.next() >> 63) as u8
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn chance(&mut self, p: f64) -> bool {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64 <= p
    }
    fn bits(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.bit()).collect()
    }
}

// ---------------------------------------------------------------------------------------------
// The template checks (one catalogue-shaped generator per width, normal form).

fn template_poly(w: u8) -> u64 {
    match w {
        4 => 0x3,          // CRC-4/ITU, x^4 + x + 1
        8 => 0x07,         // CRC-8/SMBUS
        12 => 0x80F,       // CRC-12/DECT
        16 => 0x1021,      // CRC-16/XMODEM (the CCITT generator)
        24 => 0xFF_F409,   // CRC-24/Mode-S
        32 => 0x04C1_1DB7, // CRC-32
        _ => unreachable!(),
    }
}

/// `init = 0, xorout = 0` (a purely linear check: Mode-S, XMODEM, KERMIT, ARC, SMBUS) or the
/// all-ones affine variant (CCITT-FALSE, CRC-32).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Affine {
    Linear,
    Ones,
}

fn template(w: u8, a: Affine) -> BitCrc {
    let init = match a {
        Affine::Linear => 0,
        Affine::Ones => u32::MAX,
    };
    BitCrc::new(w, template_poly(w), init, false, false, 0).expect("template")
}

fn pack(bits: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; bits.len().div_ceil(8)];
    for (i, &b) in bits.iter().enumerate() {
        out[i / 8] |= (b & 1) << (7 - i % 8);
    }
    out
}

fn field(bits: &[u8]) -> u32 {
    bits.iter().fold(0u32, |v, &b| (v << 1) | u32::from(b & 1))
}

/// Whether `frame`'s last `w` bits are the check of the rest.
fn valid(crc: &BitCrc, frame: &[u8]) -> bool {
    let w = usize::from(crc.width());
    let msg = &frame[..frame.len() - w];
    crc.compute(&pack(msg), 0, msg.len()) == field(&frame[frame.len() - w..])
}

/// `msg` followed by its check: a frame that is valid by construction.
fn with_check(crc: &BitCrc, msg: &[u8]) -> Vec<u8> {
    let w = crc.width();
    let c = crc.compute(&pack(msg), 0, msg.len());
    let mut f = msg.to_vec();
    f.extend((0..w).rev().map(|i| ((c >> i) & 1) as u8));
    f
}

/// Multiplicative order of `x` modulo the full generator `x^w + poly`, when ≤ `limit`.
fn order_of_x(w: u8, poly: u64, limit: usize) -> Option<usize> {
    let top = 1u64 << w;
    let full = top | poly;
    let mut r = 1u64;
    for k in 1..=limit {
        r <<= 1;
        if r & top != 0 {
            r ^= full;
        }
        if r == 1 {
            return Some(k);
        }
    }
    None
}

// ---------------------------------------------------------------------------------------------
// The no-code sources and the framers.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Source {
    /// N1 / N3 16-QAM and OFDM through a hard slicer: independent fair bits.
    Iid,
    /// N2 CW / NBFM / AM voice through a discriminator slicer: long runs (stay p = 0.97).
    Runs,
    /// N3 DSSS: a 127-chip m-sequence (x^7 + x^3 + 1) spread over random data bits.
    Dsss,
    /// Synthetic: one no-code emitter repeating the same 40-bit burst over idle zeros.
    Beacon,
    /// Synthetic: [`Source::Beacon`] conditioned on the chance event that the burst polynomial
    /// is divisible by the generator (probability exactly `2^−w` for a linear check), so the
    /// rate is `2^−w × this`. Importance sampling: it makes the `w ≥ 12` tail measurable.
    BeaconSeeded,
    /// Synthetic: a no-code sensor, 16-bit fixed id + 12-bit random-walk value + 12 fixed bits.
    Sensor,
    /// Synthetic: a random payload sent twice back-to-back at lag `ord(x mod g)` (≤ 40 bits),
    /// e.g. a remote that repeats its code within one frame.
    Repeat,
    /// Synthetic: a random 20-bit payload Manchester-coded to 40 chips, never decoded.
    Manchester,
}

const SOURCES: [Source; 8] = [
    Source::Iid,
    Source::Runs,
    Source::Dsss,
    Source::Beacon,
    Source::BeaconSeeded,
    Source::Sensor,
    Source::Repeat,
    Source::Manchester,
];

impl Source {
    fn name(self) -> &'static str {
        match self {
            Self::Iid => "N1/N3 iid (16-QAM, OFDM)",
            Self::Runs => "N2 runs (CW/voice slicer)",
            Self::Dsss => "N3 DSSS m-sequence",
            Self::Beacon => "A beacon over idle",
            Self::BeaconSeeded => "A beacon | g divides it",
            Self::Sensor => "A sensor, no code",
            Self::Repeat => "A repeat at lag ord(g)",
            Self::Manchester => "A Manchester chips",
        }
    }
    fn bursty(self) -> bool {
        !matches!(self, Self::Iid | Self::Runs | Self::Dsss)
    }
}

/// How frames are cut from a window.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Framer {
    /// At the burst onset (a preamble / energy anchor), exact.
    Anchored,
    /// At the burst onset with 0–7 bits of onset jitter (an energy gate's timing error).
    Jitter,
    /// On a fixed grid of frame-length blocks (stream / block mode, or a framing search that
    /// tries every offset on a lattice); bursts land at arbitrary phase.
    Grid,
}

const FRAMERS: [Framer; 3] = [Framer::Anchored, Framer::Jitter, Framer::Grid];

impl Framer {
    fn name(self) -> &'static str {
        match self {
            Self::Anchored => "anchored",
            Self::Jitter => "jitter≤7",
            Self::Grid => "grid",
        }
    }
}

fn m_sequence() -> Vec<u8> {
    let mut s = 1u8;
    (0..127)
        .map(|_| {
            let out = s & 1;
            let fb = (s ^ (s >> 3)) & 1; // x^7 + x^3 + 1 (Fibonacci form)
            s = (s >> 1) | (fb << 6);
            out
        })
        .collect()
}

/// One burst payload of `source` (burst sources only). `state` carries the sensor's value and
/// the beacon's fixed burst.
fn burst(source: Source, w: u8, rng: &mut Rng, state: &[u8], value: &mut i32) -> Vec<u8> {
    match source {
        Source::Beacon | Source::BeaconSeeded => state.to_vec(),
        Source::Sensor => {
            *value = (*value + rng.below(3) as i32 - 1).clamp(0, 4095);
            let mut b = state[..16].to_vec();
            b.extend((0..12).rev().map(|i| ((*value >> i) & 1) as u8));
            b.extend_from_slice(&state[16..28]);
            b
        }
        Source::Repeat => {
            let lag = order_of_x(w, template_poly(w), BURST / 2).unwrap_or(BURST / 2);
            let p = rng.bits(lag);
            let mut b = p.clone();
            b.extend_from_slice(&p);
            b
        }
        Source::Manchester => rng
            .bits(BURST / 2)
            .into_iter()
            .flat_map(|d| [d, 1 - d])
            .collect(),
        _ => unreachable!(),
    }
}

/// The frames of one window (one confirm decision).
fn window_frames(source: Source, framer: Framer, w: u8, rng: &mut Rng) -> Vec<Vec<u8>> {
    let n = FRAMES_PER_WINDOW;
    if !source.bursty() {
        let len = n * FRAME * 2;
        let bits: Vec<u8> = match source {
            Source::Iid => rng.bits(len),
            Source::Runs => {
                let mut b = rng.bit();
                (0..len)
                    .map(|_| {
                        if rng.chance(0.03) {
                            b ^= 1;
                        }
                        b
                    })
                    .collect()
            }
            Source::Dsss => {
                let m = m_sequence();
                let mut out = Vec::with_capacity(len + 127);
                while out.len() < len {
                    let d = rng.bit();
                    out.extend(m.iter().map(|c| c ^ d));
                }
                out.truncate(len);
                out
            }
            _ => unreachable!(),
        };
        return (0..n)
            .map(|i| match framer {
                Framer::Grid => bits[i * FRAME..(i + 1) * FRAME].to_vec(),
                // A chance sync hit lands anywhere.
                _ => {
                    let s = rng.below(len - FRAME);
                    bits[s..s + FRAME].to_vec()
                }
            })
            .collect();
    }
    // Burst sources: each burst in its own 2-frame slot of idle zeros, at a random phase.
    let mut state = rng.bits(40);
    if source == Source::BeaconSeeded {
        let crc = template(w, Affine::Linear);
        state = with_check(&crc, &state[..BURST - usize::from(w)]);
    }
    let mut value = rng.below(4096) as i32;
    let mut stream = vec![0u8; n * 2 * FRAME];
    let mut onsets = Vec::with_capacity(n);
    for k in 0..n {
        let b = burst(source, w, rng, &state, &mut value);
        let at = k * 2 * FRAME + rng.below(2 * FRAME - b.len());
        stream[at..at + b.len()].copy_from_slice(&b);
        onsets.push(at);
    }
    match framer {
        Framer::Grid => (0..2 * n)
            .map(|i| stream[i * FRAME..(i + 1) * FRAME].to_vec())
            .collect(),
        Framer::Anchored | Framer::Jitter => onsets
            .iter()
            .map(|&at| {
                let lead = if framer == Framer::Jitter {
                    rng.below(8)
                } else {
                    0
                };
                let s = at.saturating_sub(lead);
                let mut f = stream[s..(s + FRAME).min(stream.len())].to_vec();
                f.resize(FRAME, 0);
                f
            })
            .collect(),
    }
}

// ---------------------------------------------------------------------------------------------
// The counts the gate reads.

/// FNV over one-bit-per-byte bits (the `CheckTally` dedup key).
fn hash(bits: &[u8]) -> u64 {
    bits.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
        (h ^ u64::from(b & 1)).wrapping_mul(0x0000_0100_0000_01b3)
    }) ^ (bits.len() as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)
}

/// The frame with leading and trailing zeros removed: equal for zero-padded shifts of one
/// burst.
fn core(bits: &[u8]) -> &[u8] {
    let a = bits.iter().position(|&b| b == 1).unwrap_or(bits.len());
    let z = bits.iter().rposition(|&b| b == 1).map_or(a, |i| i + 1);
    &bits[a..z]
}

fn poly(bits: &[u8]) -> u128 {
    bits.iter()
        .fold(0u128, |v, &b| (v << 1) | u128::from(b & 1))
}

fn degree(p: u128) -> Option<u32> {
    (p != 0).then(|| 127 - p.leading_zeros())
}

/// `a mod b` over GF(2).
fn poly_mod(mut a: u128, b: u128) -> u128 {
    let Some(db) = degree(b) else { return a };
    while let Some(da) = degree(a) {
        if da < db {
            break;
        }
        a ^= b << (da - db);
    }
    a
}

/// The proposed trial count: valid, non-[`degenerate_frame`] frames whose [`core`] polynomial
/// is not a multiple of (or a divisor of) one already counted. For a linear check `g | P`
/// implies `g | m·P` for every `m`, so a zero-padded shift (`m = x^k`) or a frame holding two
/// copies of one burst (`m = x^a + x^b`) is valid by construction once `P` is: one chance
/// event, not two. Two different real frames of one length are never multiples of each other.
fn independent_frames<'a>(frames: impl IntoIterator<Item = &'a [u8]>, w: usize) -> u64 {
    let mut counted: Vec<u128> = Vec::new();
    for f in frames {
        if degenerate_frame(f, w) {
            continue;
        }
        let c = poly(core(f));
        if c == 0
            || counted
                .iter()
                .any(|&p| poly_mod(c, p) == 0 || poly_mod(p, c) == 0)
        {
            continue;
        }
        counted.push(c);
    }
    counted.len() as u64
}

/// The degenerate-frame guard the measurement argues for: `CheckTally`'s short-period test,
/// applied also with up to `w` bits trimmed from either end. An affine check (init ≠ 0) is
/// satisfied by the frame `init ‖ 0…0` — the leading `w` bits cancel the register and the rest
/// is idle fill — and with `xorout ≠ 0` by `init ‖ 0…0 ‖ xorout`; neither frame is periodic as a
/// whole, so the shipped guard lets a single idle frame through.
fn degenerate_frame(bits: &[u8], w: usize) -> bool {
    let n = bits.len();
    [(0, n), (w, n), (0, n - w), (w, n - w)]
        .iter()
        .any(|&(a, z)| is_short_periodic(&bits[a..z], SHORT_PERIOD_MAX_BITS))
}

/// GF(2) rank of `{f_i ⊕ f_0}` over `frames` (all the same length ≤ 128): the number of frames
/// beyond the first that are independent trials for a linear check. Any frame in the span of
/// the others is valid automatically once they are.
fn affine_rank(frames: &[&[u8]]) -> usize {
    let as_u128 = |f: &[u8]| f.iter().fold(0u128, |v, &b| (v << 1) | u128::from(b & 1));
    let Some(first) = frames.first().map(|f| as_u128(f)) else {
        return 0;
    };
    let mut basis: Vec<u128> = Vec::new();
    for f in &frames[1..] {
        let mut v = as_u128(f) ^ first;
        for &b in &basis {
            v = v.min(v ^ b);
        }
        if v != 0 {
            basis.push(v);
            basis.sort_unstable_by(|a, b| b.cmp(a));
        }
    }
    basis.len()
}

#[derive(Clone, Copy, Debug, Default)]
struct Tally {
    tested: u64,
    /// Valid frames counted with repeats (the "repeat count").
    valid: u64,
    /// `CheckSummary::distinct_valid`: distinct valid frames.
    distinct_valid: u64,
    /// `CheckTally` / `HoldoutEvidence::differences`: distinct valid frames that are not
    /// short-periodic.
    differences: u64,
    /// The proposed count, [`independent_frames`] over the frames `differences` counts.
    fixed: u64,
    /// GF(2) rank of the valid distinct frames' differences, + 1 for the first frame.
    rank1: u64,
}

fn tally(crc: &BitCrc, frames: &[Vec<u8>]) -> Tally {
    let mut t = Tally {
        tested: frames.len() as u64,
        ..Tally::default()
    };
    let mut dv = HashSet::new();
    let mut diff: Vec<&[u8]> = Vec::new();
    let mut seen = HashSet::new();
    let w = usize::from(crc.width());
    for f in frames {
        if !valid(crc, f) {
            continue;
        }
        t.valid += 1;
        dv.insert(hash(f));
        if !is_short_periodic(f, SHORT_PERIOD_MAX_BITS) && seen.insert(hash(f)) {
            diff.push(f);
        }
    }
    t.distinct_valid = dv.len() as u64;
    t.differences = diff.len() as u64;
    t.fixed = independent_frames(diff.iter().copied(), w);
    t.rank1 = if diff.is_empty() {
        0
    } else {
        affine_rank(&diff) as u64 + 1
    };
    t
}

/// ADR-0022 §4.2.
fn min_differences(w: u8, l_check: f64) -> u64 {
    ((MIN_ANALYTIC + l_check) / f64::from(w)).ceil().max(1.0) as u64
}

/// ADR-0022 §6 steps 2–3, check-only: the S5 bits `check_bits(d, tested, w)`, clamped to
/// `w·d` (T-575's bound), less `L_check`, against 24 (which subsumes the 16-bit hard floor).
fn gate(d: u64, tested: u64, w: u8, l_check: f64) -> bool {
    let reported = f64::from(check_bits(d, tested, f64::from(w)));
    reported.min(f64::from(w) * d as f64) - l_check >= MIN_ANALYTIC
}

// ---------------------------------------------------------------------------------------------
// Measurement 1: the degenerate null, per width.

#[derive(Clone, Copy, Debug, Default)]
struct Rates {
    trials: u64,
    /// `distinct_valid ≥ min_differences`.
    dv_req: u64,
    /// `differences ≥ min_differences`.
    diff_req: u64,
    /// The gate (with the `tested` multiplicity) on `differences`: what ships.
    gate: u64,
    /// The gate on `min(differences, rank + 1)`: the rank cap alone.
    gate_rank: u64,
    /// The gate on `min(fixed, rank + 1)`: the proposed count.
    gate_fixed: u64,
    /// Frames tested, and frames valid: the per-frame pass rate against `2^−w`.
    frames: u64,
    frames_valid: u64,
}

fn template_rates(source: Source, framer: Framer, w: u8, a: Affine, n: usize) -> Rates {
    let crc = template(w, a);
    let req = min_differences(w, 0.0);
    let mut r = Rates::default();
    let mut rng = Rng(0x7577 ^ (u64::from(w) << 32) ^ ((source as u64) << 40) ^ framer as u64);
    for _ in 0..n {
        let frames = window_frames(source, framer, w, &mut rng);
        let t = tally(&crc, &frames);
        r.trials += 1;
        r.frames += t.tested;
        r.frames_valid += t.valid;
        r.dv_req += u64::from(t.distinct_valid >= req);
        r.diff_req += u64::from(t.differences >= req);
        r.gate += u64::from(gate(t.differences, t.tested, w, 0.0));
        r.gate_rank += u64::from(gate(t.differences.min(t.rank1), t.tested, w, 0.0));
        r.gate_fixed += u64::from(gate(t.fixed.min(t.rank1), t.tested, w, 0.0));
    }
    r
}

#[derive(Clone, Copy, Debug, Default)]
struct SearchRates {
    trials: u64,
    /// `search_codes` returned a width-`w` suggestion at all (its own ≥ 16-bit claim rule).
    claimed: u64,
    /// ... whose validated distinct frames reach `min_differences(w, w + 5)`.
    dv_req: u64,
    /// ... whose `differences` reach it: ADR-0022 §6 step 2–3 for a searched check, before the
    /// §5.3 null control.
    gate: u64,
    /// The same, with the proposed count applied upstream of the search: only frames that
    /// [`independent_frames`] counts are searched.
    gate_fixed: u64,
}

fn searched_gate(
    frames: &[Vec<u8>],
    w: u8,
    cfg: &CodeSearchConfig,
    l_check: f64,
) -> Option<(u64, bool)> {
    let rep = search_codes(frames, cfg);
    rep.codes
        .iter()
        .filter(|c| c.width == w)
        .max_by_key(|c| c.differences)
        .map(|c| {
            (
                c.validated as u64,
                f64::from(w) * c.differences as f64 - l_check >= MIN_ANALYTIC,
            )
        })
}

fn searched_rates(source: Source, framer: Framer, w: u8, n: usize) -> SearchRates {
    let l_check = f64::from(w) + SLOT_BITS;
    let req = min_differences(w, l_check);
    let cfg = CodeSearchConfig {
        min_width: w,
        max_width: w,
        ..CodeSearchConfig::default()
    };
    let mut r = SearchRates::default();
    let mut rng = Rng(0x5eac ^ (u64::from(w) << 32) ^ ((source as u64) << 40) ^ framer as u64);
    for _ in 0..n {
        let mut frames = window_frames(source, framer, w, &mut rng);
        // Idle blocks carry nothing a framer would hand on.
        frames.retain(|f| f.contains(&1));
        frames.truncate(SEARCH_FRAMES);
        r.trials += 1;
        if let Some((validated, pass)) = searched_gate(&frames, w, &cfg, l_check) {
            r.claimed += 1;
            r.dv_req += u64::from(validated >= req);
            r.gate += u64::from(pass);
        }
        // The proposed count applied upstream of the search: keep only frames that are
        // independent trials in the [`independent_frames`] sense.
        let mut reduced: Vec<Vec<u8>> = Vec::new();
        for f in frames {
            let before = independent_frames(reduced.iter().map(Vec::as_slice), usize::from(w));
            reduced.push(f);
            if independent_frames(reduced.iter().map(Vec::as_slice), usize::from(w)) == before {
                reduced.pop();
            }
        }
        if let Some((_, pass)) = searched_gate(&reduced, w, &cfg, l_check) {
            r.gate_fixed += u64::from(pass);
        }
    }
    r
}

fn pct(k: u64, n: u64) -> String {
    if n == 0 {
        return "—".into();
    }
    if k == 0 {
        // The one-sided 95 % bound for zero events, 3/n.
        return format!("0 (<{:.1e})", 3.0 / n as f64);
    }
    format!("{:.2e}", k as f64 / n as f64)
}

#[test]
fn degenerate_null_rate_per_width() {
    let n = trials("HK_T577_TRIALS", 40);
    // The generator search dominates the cost: by default it runs 4 windows per cell over the
    // sources the assertions read; `HK_T577_SEARCH_TRIALS` runs every source.
    let full_search = std::env::var("HK_T577_SEARCH_TRIALS").is_ok();
    let n_search = trials("HK_T577_SEARCH_TRIALS", 3);
    println!(
        "\nT-577 measurement 1: template-fixed width-w check on NO-code input, {n} windows of \
         {FRAMES_PER_WINDOW} frames each (grid: 48 blocks). Rates per window = per confirm decision.\n\
         budget per decision 2^-14.29 = 5.0e-5; model claim at the gate 2^-24 = 6.0e-8.\n"
    );
    println!(
        "{:<26} {:<9} {:<6} {:>3} | {:>10} {:>12} {:>12} {:>12} {:>12} {:>12} | {:>10}",
        "source",
        "framer",
        "check",
        "w",
        "frame pass",
        "dv>=req",
        "diff>=req",
        "GATE",
        "gate|rank",
        "gate|fix",
        "2^-w"
    );
    let mut results = Vec::new();
    for source in SOURCES {
        for framer in FRAMERS {
            if !source.bursty() && framer == Framer::Jitter {
                continue;
            }
            for a in [Affine::Linear, Affine::Ones] {
                if source == Source::BeaconSeeded && a == Affine::Ones {
                    continue;
                }
                for w in WIDTHS {
                    let r = template_rates(source, framer, w, a, n);
                    println!(
                        "{:<26} {:<9} {:<6} {:>3} | {:>10} {:>12} {:>12} {:>12} {:>12} {:>12} | {:>10.1e}",
                        source.name(),
                        framer.name(),
                        if a == Affine::Linear { "init0" } else { "ones" },
                        w,
                        pct(r.frames_valid, r.frames),
                        pct(r.dv_req, r.trials),
                        pct(r.diff_req, r.trials),
                        pct(r.gate, r.trials),
                        pct(r.gate_rank, r.trials),
                        pct(r.gate_fixed, r.trials),
                        (-f64::from(w)).exp2(),
                    );
                    results.push((source, framer, a, w, r));
                }
            }
        }
    }
    println!(
        "\nT-577 measurement 1b: SEARCHED width-w check (search_codes, {SEARCH_FRAMES} frames), \
         L_check = w + 5, {n_search} windows. Before the §5.3 null control.\n"
    );
    println!(
        "{:<26} {:<9} {:>3} | {:>12} {:>12} {:>12} {:>12}",
        "source", "framer", "w", "claimed", "dv>=req", "GATE", "gate|fix"
    );
    let mut searched = Vec::new();
    for source in SOURCES {
        if !full_search && source.bursty() && source != Source::BeaconSeeded {
            continue;
        }
        let framers: &[Framer] = if full_search {
            &[Framer::Anchored, Framer::Grid]
        } else {
            &[Framer::Grid]
        };
        for &framer in framers {
            for w in WIDTHS {
                let r = searched_rates(source, framer, w, n_search);
                println!(
                    "{:<26} {:<9} {:>3} | {:>12} {:>12} {:>12} {:>12}",
                    source.name(),
                    framer.name(),
                    w,
                    pct(r.claimed, r.trials),
                    pct(r.dv_req, r.trials),
                    pct(r.gate, r.trials),
                    pct(r.gate_fixed, r.trials),
                );
                searched.push((source, framer, w, r));
            }
        }
    }

    let get = |s: Source, f: Framer, a: Affine, w: u8| {
        results
            .iter()
            .find(|r| r.0 == s && r.1 == f && r.2 == a && r.3 == w)
            .map(|r| r.4)
            .unwrap()
    };
    // The N2/N3 populations never reach the shipped gate with a template check — the `tested`
    // multiplicity prices their frames — with ONE exception, the init-cancel artefact below
    // (an affine check wide enough that a single frame is enough). The proposed count never
    // passes any of them.
    for s in [Source::Iid, Source::Runs, Source::Dsss] {
        for f in [Framer::Anchored, Framer::Grid] {
            for a in [Affine::Linear, Affine::Ones] {
                for w in WIDTHS {
                    let r = get(s, f, a, w);
                    assert_eq!(r.gate_fixed, 0, "{s:?} {f:?} {a:?} w={w}: {r:?}");
                    if !(s == Source::Runs && a == Affine::Ones && w >= 24) {
                        assert_eq!(r.gate, 0, "{s:?} {f:?} {a:?} w={w}: {r:?}");
                    }
                }
            }
        }
    }
    // DEGENERATE NULL A (init-cancel, width-independent): an affine check with init = all ones
    // is satisfied by `1^w ‖ 0…0` — the first w bits cancel the register, the rest is idle —
    // and that frame is not short-periodic, so `differences` counts it. A CW / voice slicer
    // emits it whenever its one transition lands at bit w. The trimmed guard catches it.
    for w in WIDTHS {
        let crc = template(w, Affine::Ones);
        let wu = usize::from(w);
        let mut f = vec![1u8; wu];
        f.resize(FRAME, 0);
        assert!(valid(&crc, &f), "w={w}");
        assert!(!is_short_periodic(&f, SHORT_PERIOD_MAX_BITS), "w={w}");
        assert!(degenerate_frame(&f, wu), "w={w}");
    }
    // DEGENERATE NULL B (shift): a linear check plus a framer that yields zero-padded shifts of
    // one burst. Given the one chance event `g | P` (probability 2^−w), every shift is valid and
    // distinct, so `differences` climbs and the shipped gate passes at every width from 8. The
    // per-decision false-confirm rate is 2^−w, not 2^−24.
    for f in [Framer::Jitter, Framer::Grid] {
        for w in [8u8, 12, 16, 24] {
            let r = get(Source::BeaconSeeded, f, Affine::Linear, w);
            assert!(
                r.gate * 10 >= r.trials * 9,
                "shipped gate, w={w} {f:?}: {r:?}"
            );
            // The GF(2) rank cap does NOT remove it: shifted copies are linearly independent.
            assert!(
                r.gate_rank * 10 >= r.trials * 9,
                "rank cap, w={w} {f:?}: {r:?}"
            );
            // Counting shift-collapsed cores does.
            assert!(
                r.gate_fixed * 5 <= r.trials,
                "fixed count, w={w} {f:?}: {r:?}"
            );
        }
        assert_eq!(
            get(Source::BeaconSeeded, f, Affine::Linear, 8).gate_fixed,
            0
        );
    }
    // An exact anchor closes the shift door.
    for w in [8u8, 12] {
        assert_eq!(
            get(Source::BeaconSeeded, Framer::Anchored, Affine::Linear, w).gate,
            0
        );
    }
    // DEGENERATE NULL C (period): a payload repeated at lag ord(x mod g) satisfies the check
    // whatever the payload. ord ≤ 2^w − 1, so below a byte the lag is at most 15 bits, and
    // every frame is valid AND distinct: certain at w = 4, and no count fixes it.
    assert_eq!(order_of_x(4, template_poly(4), 64), Some(15));
    let rep4 = get(Source::Repeat, Framer::Anchored, Affine::Linear, 4);
    assert_eq!(rep4.gate, rep4.trials, "{rep4:?}");
    assert!(rep4.gate_fixed * 10 >= rep4.trials * 9, "{rep4:?}");
    // It does not exist for the byte-and-over templates within a frame (orders > 20 bits).
    for w in [8u8, 12, 16, 24, 32] {
        assert!(
            order_of_x(w, template_poly(w), BURST / 2).is_none(),
            "w={w}"
        );
        assert_eq!(
            get(Source::Repeat, Framer::Anchored, Affine::Linear, w).gate,
            0
        );
    }
    // Searched: N2/N3 never reach the gate; the shift artefact does, at any width, because the
    // GCD of shifted copies contains the burst polynomial and any degree-w divisor of it fits
    // every frame. The proposed count removes it.
    let sb = |s: Source, f: Framer, w: u8| {
        searched
            .iter()
            .find(|r| r.0 == s && r.1 == f && r.2 == w)
            .map(|r| r.3)
            .unwrap()
    };
    for w in WIDTHS {
        for s in [Source::Iid, Source::Runs, Source::Dsss] {
            assert_eq!(sb(s, Framer::Grid, w).gate, 0, "{s:?} w={w}");
        }
        assert_eq!(
            sb(Source::BeaconSeeded, Framer::Grid, w).gate_fixed,
            0,
            "w={w}"
        );
    }
    assert!(
        WIDTHS
            .iter()
            .any(|&w| sb(Source::BeaconSeeded, Framer::Grid, w).gate > 0)
    );
}

// ---------------------------------------------------------------------------------------------
// Measurements 2 and 3: real emitters (a code IS present), the counts the gate would credit.

#[derive(Clone, Copy, Debug)]
enum Emitter {
    /// One fixed payload every frame (a parked TPMS, an id beacon), CRC-16.
    Beacon,
    /// Alternating two status messages, CRC-16.
    Toggle,
    /// rtl_433-style: 8-bit id, 12-bit temperature ±1 walk, 8-bit humidity walk, battery;
    /// each transmission sent 3×. CRC-8.
    Sensor,
    /// Rolling counter remote: 20-bit id, 16-bit counter + 1 per press, 4-bit button. CRC-16.
    Counter,
    /// ADS-B DF17-like, scaled to the 72-bit payload: 8 fixed + 24-bit ICAO + 40 bits of ME
    /// (position/velocity content, all varying). CRC-24.
    Squitter,
}

impl Emitter {
    fn width(self) -> u8 {
        match self {
            Self::Sensor => 8,
            Self::Squitter => 24,
            _ => 16,
        }
    }

    fn frames(self, k: usize, rng: &mut Rng) -> Vec<Vec<u8>> {
        let w = self.width();
        let crc = template(w, Affine::Linear);
        let payload = FRAME - usize::from(w);
        let fixed = rng.bits(payload);
        let other = rng.bits(payload);
        let mut t = 2000i32;
        let mut h = 128i32;
        let mut c = rng.below(1 << 16) as i32;
        let put = |m: &mut Vec<u8>, at: usize, n: usize, v: i32| {
            for i in 0..n {
                m[at + i] = ((v >> (n - 1 - i)) & 1) as u8;
            }
        };
        let mut out = Vec::with_capacity(k);
        while out.len() < k {
            let mut m = fixed.clone();
            let copies = match self {
                Self::Beacon => 1,
                Self::Toggle => {
                    if out.len() % 2 == 1 {
                        m = other.clone();
                    }
                    1
                }
                Self::Sensor => {
                    t = (t + rng.below(3) as i32 - 1).clamp(0, 4095);
                    if rng.chance(0.25) {
                        h = (h + rng.below(3) as i32 - 1).clamp(0, 255);
                    }
                    put(&mut m, 8, 12, t);
                    put(&mut m, 20, 8, h);
                    3
                }
                Self::Counter => {
                    c = (c + 1) & 0xFFFF;
                    put(&mut m, 20, 16, c);
                    1
                }
                Self::Squitter => {
                    // Position / velocity: all of the ME field varies.
                    let me = rng.bits(40);
                    m[32..72].copy_from_slice(&me);
                    1
                }
            };
            let f = with_check(&crc, &m);
            for _ in 0..copies {
                out.push(f.clone());
            }
        }
        out.truncate(k);
        out
    }
}

#[test]
fn distinct_valid_versus_differences_on_real_emitters() {
    let reps = trials("HK_T577_TRIALS", 12).clamp(1, 200) as u64;
    println!(
        "\nT-577 measurements 2 and 3: emitters WITH a code (template-fixed, anchored frames), mean \
         over {reps} runs. valid = repeat count; dv = distinct_valid; diff = differences \
         (CheckTally); indep = the proposed count (multiples of one burst once, degenerate frames \
         out); codesD = assist::codes' D on the same frames; rank+1 = independent trials for a \
         linear check.\n"
    );
    println!(
        "{:<9} {:>3} {:>4} | {:>6} {:>6} {:>6} {:>6} {:>7} {:>7} | {:>9} {:>9}",
        "emitter",
        "w",
        "k",
        "valid",
        "dv",
        "diff",
        "indep",
        "codesD",
        "rank+1",
        "dv-diff",
        "diff-rank"
    );
    let mut rows = Vec::new();
    for e in [
        Emitter::Beacon,
        Emitter::Toggle,
        Emitter::Sensor,
        Emitter::Counter,
        Emitter::Squitter,
    ] {
        let w = e.width();
        let crc = template(w, Affine::Linear);
        for k in [3usize, 4, 8, 16, 32, 64, 128] {
            let mut acc = [0f64; 5];
            let mut rng = Rng(0xe417 ^ (k as u64) << 8 ^ (w as u64) << 40);
            let search_every = if k <= 32 { 1 } else { reps };
            let mut searched = 0f64;
            let mut codes_d = 0f64;
            for r in 0..reps {
                let frames = e.frames(k, &mut rng);
                let t = tally(&crc, &frames);
                acc[0] += t.valid as f64;
                acc[1] += t.distinct_valid as f64;
                acc[2] += t.differences as f64;
                acc[3] += t.rank1 as f64;
                acc[4] += t.fixed as f64;
                if r % search_every == 0 {
                    let cfg = CodeSearchConfig {
                        min_width: w,
                        max_width: w,
                        ..CodeSearchConfig::default()
                    };
                    let rep = search_codes(&frames, &cfg);
                    codes_d += rep
                        .codes
                        .iter()
                        .find(|c| c.width == w)
                        .map_or(0.0, |c| c.differences as f64);
                    searched += 1.0;
                }
            }
            let m = |x: f64| x / reps as f64;
            let row = (
                e,
                k,
                m(acc[0]),
                m(acc[1]),
                m(acc[2]),
                codes_d / searched,
                m(acc[3]),
                m(acc[4]),
            );
            println!(
                "{:<9} {:>3} {:>4} | {:>6.1} {:>6.1} {:>6.1} {:>6.1} {:>7.1} {:>7.1} | {:>9.1} {:>9.1}",
                format!("{e:?}"),
                w,
                k,
                row.2,
                row.3,
                row.4,
                row.7,
                row.5,
                row.6,
                row.3 - row.4,
                row.4 - row.6,
            );
            rows.push(row);
        }
    }
    let at = |e: &str, k: usize| {
        *rows
            .iter()
            .find(|r| format!("{:?}", r.0) == e && r.1 == k)
            .unwrap()
    };
    // Measurement 2: `distinct_valid` is already deduplicated, so its gap to `differences` is
    // only the short-periodic frames — none on these emitters. The swap is free here; what
    // over-credited a beacon was the repeat count, which neither of them uses.
    for e in ["Beacon", "Toggle", "Sensor", "Counter", "Squitter"] {
        for k in [8usize, 64] {
            let r = at(e, k);
            assert!((r.3 - r.4).abs() < 1e-9, "{e} k={k}: {r:?}");
            // The proposed count costs a real emitter nothing.
            assert!((r.7 - r.4).abs() < 1e-9, "indep {e} k={k}: {r:?}");
        }
    }
    let b = at("Beacon", 64);
    assert!(b.2 >= 64.0 && b.3 == 1.0, "{b:?}");
    // Measurement 3: independence saturates at the rank, which is the payload's varying-bit
    // dimension, not a frame count. A slow sensor's distinct frames outrun its rank; a counter's
    // rank tracks its counter bits; a squitter keeps adding independent trials.
    let s = at("Sensor", 128);
    assert!(s.4 > 2.0 * s.6, "sensor over-credit: {s:?}");
    assert!(
        s.6 < 16.0,
        "sensor rank should saturate below its 20 varying bits: {s:?}"
    );
    let c = at("Counter", 128);
    assert!(c.6 <= 17.0 && c.4 > c.6 + 100.0, "{c:?}");
    let q = at("Squitter", 64);
    assert!(q.6 > 36.0, "{q:?}");
}
