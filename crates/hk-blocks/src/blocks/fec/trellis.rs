//! The trellis engine behind `viterbi` and `viterbi_frames` (T-610, ADR-0011 §9.3): one code
//! description and one add-compare-select core, shared by the streaming and the per-frame shape.
//!
//! **Native, not an adapter.** ADR-0011 §9.4 designated liquid-dsp's convolutional FEC; T-607
//! measured that liquid's conv/RS codecs are wrappers over Phil Karn's `libfec` that return NULL
//! when `libfec` is absent (docs/18 §7.1.1), so there is no kernel to adapt. Viterbi decoding is
//! textbook (Viterbi 1967; CCSDS 131.0-B-4 §3 for the code) and is written here.
//!
//! ## Code conventions (normative for the block parameters)
//!
//! - **Coded word.** Each trellis step emits `n` coded bits. Coded bit `j` (0 = first on air) is
//!   bit `n − 1 − j` of the word, MSB first like every other packing in the catalogue (§1.1).
//!   `trellis.output` values use the same convention.
//! - **Polynomials.** `polys[j]` is generator `j`, in transmission order. With `poly_order:
//!   newest-lsb` (the default; Karn's `libfec`, GNU Radio and the CCSDS pair written `0x4F`,
//!   `0x6D`) bit 0 taps the bit just shifted in; with `newest-msb` (the textbook octal form,
//!   CCSDS 131.0-B figure 3-1's `171`, `133` = `0x79`, `0x5B`) bit `K − 1` does. The state is the
//!   last `K − 1` input bits, newest in bit 0: `next = ((state << 1) | u) mod 2^(K−1)`.
//! - **Inversion.** `invert[j]` complements coded bit `j` (CCSDS inverts G2).
//! - **Puncturing.** `puncture[j]` is a string of `0`/`1` of the period length `P`: character
//!   `t` says whether coded bit `j` of step `t` of each period is transmitted. Kept bits go on
//!   air step by step, generator order within a step (the column order CCSDS 131.0-B §3.5 and
//!   DVB-S use). A step whose column is all `0` is refused.
//! - **Soft input.** Positive = 1, magnitude = reliability up to a common scale (ADR-0011 §9.2).
//!   The branch metric is the correlation `Σ ±s_j`, which is scale-free; a punctured position is
//!   an erasure (0) and costs every branch the same.

use hk_recipe::parse_hex;
use serde_json::Value;

use crate::block::BlockError;
use crate::blocks::framing::common::P;

/// Most coded bits per step (the word is a `u16`).
pub(crate) const MAX_N: usize = 16;
/// Most survivor entries (steps × states) one trellis holds, so a large `K` × depth is refused at
/// build instead of allocating gigabytes (3 bytes each: about 48 MiB).
pub(crate) const MAX_SURVIVORS: usize = 1 << 24;

/// A convolutional (or table-defined finite-state) code with its puncturing.
#[derive(Clone, Debug)]
pub(crate) struct Code {
    /// Trellis states.
    pub states: usize,
    /// Input bits per step (`k`).
    pub input_bits: usize,
    /// Coded bits per step (`n`).
    pub n: usize,
    /// `[state << k | u]` → next state.
    pub next: Vec<u16>,
    /// `[state << k | u]` → coded word (inversion applied).
    pub out: Vec<u16>,
    /// Puncturing period in steps (1 when unpunctured).
    pub period: usize,
    /// Per step of the period: mask of transmitted coded bits (word convention).
    pub keep: Vec<u16>,
    /// Transmitted items of one period in air order: (step in period, coded bit `j`).
    pub kept: Vec<(usize, usize)>,
    /// Per step of the period: index into `kept` of its first transmitted item.
    pub first_kept: Vec<usize>,
    /// Zero-input steps that drive every state to state 0 (`None`: input 0 does not).
    pub tail_steps: Option<usize>,
    /// Steps of encoder memory (`ceil(log2 states / k)`), the scale of a decision depth.
    pub memory_steps: usize,
}

fn perr(m: impl Into<String>) -> BlockError {
    BlockError::Params(m.into())
}

fn reverse_bits(v: u64, bits: usize) -> u64 {
    (0..bits).fold(0, |acc, i| acc | (((v >> i) & 1) << (bits - 1 - i)))
}

impl Code {
    /// From the `viterbi`/`viterbi_frames` code keys (schema-validated).
    pub fn from_params(p: P<'_>) -> Result<Self, BlockError> {
        let has_polys = !p.list("polys").is_empty();
        let trellis = p.obj("trellis");
        let (states, input_bits, n, next, mut out) = match (has_polys, trellis) {
            (true, Some(_)) => return Err(perr("give polys or trellis, not both")),
            (false, None) => {
                return Err(perr(
                    "no code: give constraint_length + polys, or a trellis table",
                ));
            }
            (true, None) => Self::from_polys(p)?,
            (false, Some(t)) => Self::from_table(t)?,
        };
        let invert = p.list("invert");
        if !invert.is_empty() {
            if invert.len() != n {
                return Err(perr(format!(
                    "invert has {} entries for {n} coded bits",
                    invert.len()
                )));
            }
            let mask = invert.iter().enumerate().fold(0u16, |m, (j, v)| {
                m | (u16::from(v.as_bool().unwrap_or(false)) << (n - 1 - j))
            });
            for w in &mut out {
                *w ^= mask;
            }
        }
        let (period, keep) = Self::puncture(p, n)?;
        let mut kept = Vec::new();
        let mut first_kept = Vec::with_capacity(period);
        for (t, &mask) in keep.iter().enumerate() {
            first_kept.push(kept.len());
            for j in 0..n {
                if mask >> (n - 1 - j) & 1 == 1 {
                    kept.push((t, j));
                }
            }
        }
        let k = input_bits;
        let tail_steps = (0..states)
            .map(|s0| {
                let mut s = s0;
                for steps in 0..=states {
                    if s == 0 {
                        return Some(steps);
                    }
                    s = usize::from(next[s << k]);
                }
                None
            })
            .try_fold(0usize, |acc, x| x.map(|x| acc.max(x)));
        let memory_steps = (states.next_power_of_two().trailing_zeros() as usize).div_ceil(k);
        Ok(Self {
            states,
            input_bits,
            n,
            next,
            out,
            period,
            keep,
            kept,
            first_kept,
            tail_steps,
            memory_steps: memory_steps.max(1),
        })
    }

    #[allow(clippy::type_complexity)]
    fn from_polys(p: P<'_>) -> Result<(usize, usize, usize, Vec<u16>, Vec<u16>), BlockError> {
        let k_len = p
            .uint("constraint_length")?
            .ok_or_else(|| perr("polys need constraint_length"))? as usize;
        if !(2..=16).contains(&k_len) {
            return Err(perr("constraint_length must be 2..=16"));
        }
        let newest_msb = match p.str("poly_order").unwrap_or("newest-lsb") {
            "newest-lsb" => false,
            "newest-msb" => true,
            other => return Err(perr(format!("unknown poly_order {other}"))),
        };
        let polys = p
            .list("polys")
            .iter()
            .map(|v| {
                let g = v
                    .as_str()
                    .and_then(parse_hex)
                    .or_else(|| v.as_u64())
                    .ok_or_else(|| perr("polys entries are hex"))?;
                if g == 0 || g >> k_len != 0 {
                    return Err(perr(format!(
                        "poly {g:#x} is zero or wider than constraint_length {k_len}"
                    )));
                }
                Ok(if newest_msb {
                    reverse_bits(g, k_len)
                } else {
                    g
                })
            })
            .collect::<Result<Vec<u64>, _>>()?;
        let n = polys.len();
        if !(2..=MAX_N).contains(&n) {
            return Err(perr("polys must have 2..=16 generators"));
        }
        let states = 1usize << (k_len - 1);
        let mut next = Vec::with_capacity(states * 2);
        let mut out = Vec::with_capacity(states * 2);
        for s in 0..states {
            for u in 0..2u64 {
                let sr = ((s as u64) << 1) | u;
                next.push((sr as usize & (states - 1)) as u16);
                let w = polys
                    .iter()
                    .fold(0u16, |w, &g| (w << 1) | ((sr & g).count_ones() & 1) as u16);
                out.push(w);
            }
        }
        Ok((states, 1, n, next, out))
    }

    #[allow(clippy::type_complexity)]
    fn from_table(t: P<'_>) -> Result<(usize, usize, usize, Vec<u16>, Vec<u16>), BlockError> {
        let k = t.req_uint("input_bits")? as usize;
        let n = t.req_uint("output_bits")? as usize;
        if !(1..=4).contains(&k) || !(2..=MAX_N).contains(&n) {
            return Err(perr("trellis: input_bits 1..=4, output_bits 2..=16"));
        }
        let ints = |key: &str| -> Result<Vec<u64>, BlockError> {
            t.list(key)
                .iter()
                .map(|v| {
                    v.as_u64()
                        .ok_or_else(|| perr(format!("trellis.{key} entries are integers")))
                })
                .collect()
        };
        let next = ints("next_state")?;
        let out = ints("output")?;
        let per_state = 1usize << k;
        if next.len() != out.len() || next.len() % per_state != 0 || next.len() < 2 * per_state {
            return Err(perr(
                "trellis: next_state and output need states × 2^input_bits entries each (≥ 2 states)",
            ));
        }
        let states = next.len() / per_state;
        if next.iter().any(|&s| s as usize >= states) {
            return Err(perr("trellis: a next_state is not a state"));
        }
        if out.iter().any(|&w| w >> n != 0) {
            return Err(perr("trellis: an output word is wider than output_bits"));
        }
        Ok((
            states,
            k,
            n,
            next.into_iter().map(|s| s as u16).collect(),
            out.into_iter().map(|w| w as u16).collect(),
        ))
    }

    fn puncture(p: P<'_>, n: usize) -> Result<(usize, Vec<u16>), BlockError> {
        let rows = p.list("puncture");
        if rows.is_empty() {
            return Ok((1, vec![((1u32 << n) - 1) as u16]));
        }
        if rows.len() != n {
            return Err(perr(format!(
                "puncture has {} rows for {n} coded bits",
                rows.len()
            )));
        }
        let rows: Vec<&str> = rows.iter().filter_map(Value::as_str).collect();
        let period = rows.first().map_or(0, |r| r.len());
        if rows.len() != n
            || period == 0
            || period > 64
            || rows
                .iter()
                .any(|r| r.len() != period || r.bytes().any(|c| c != b'0' && c != b'1'))
        {
            return Err(perr(
                "puncture rows are equal-length strings of 0/1 (period 1..=64)",
            ));
        }
        let keep: Vec<u16> = (0..period)
            .map(|t| {
                rows.iter().enumerate().fold(0u16, |m, (j, r)| {
                    m | (u16::from(r.as_bytes()[t] == b'1') << (n - 1 - j))
                })
            })
            .collect();
        if keep.contains(&0) {
            return Err(perr("a puncturing step transmits nothing"));
        }
        Ok((period, keep))
    }

    /// Transmitted items per period (`L`).
    pub fn kept_per_period(&self) -> usize {
        self.kept.len()
    }

    /// Input item (from the start of a stream in phase) at which step `s` begins.
    pub fn item_of_step(&self, s: u64) -> u64 {
        let p = self.period as u64;
        (s / p) * self.kept.len() as u64 + self.first_kept[(s % p) as usize] as u64
    }

    /// Code rate as transmitted: input bits per transmitted item.
    pub fn rate(&self) -> f64 {
        (self.period * self.input_bits) as f64 / self.kept.len() as f64
    }

    /// Branches per state.
    pub fn branches(&self) -> usize {
        1 << self.input_bits
    }
}

/// A step's received values: one soft value per coded bit (0 where punctured), in `j` order.
pub(crate) type StepSoft = [f32; MAX_N];

/// Add-compare-select over a circular survivor window of `win` steps.
pub(crate) struct Trellis {
    states: usize,
    pm: Vec<f32>,
    next_pm: Vec<f32>,
    /// `[slot × states + state]`: surviving predecessor and its input.
    prev: Vec<u16>,
    inp: Vec<u8>,
    /// `[slot]`: the hard decisions received at the step (word convention).
    rx: Vec<u16>,
    bm: Vec<f32>,
    win: usize,
    /// Steps taken since `start`.
    pub steps: u64,
}

impl Trellis {
    /// A trellis for `code` holding `win` steps of survivors (checked against
    /// [`MAX_SURVIVORS`] by the caller).
    pub fn new(code: &Code, win: usize) -> Self {
        let win = win.max(1);
        Self {
            states: code.states,
            pm: vec![0.0; code.states],
            next_pm: vec![0.0; code.states],
            prev: vec![0; win * code.states],
            inp: vec![0; win * code.states],
            rx: vec![0; win],
            bm: vec![0.0; if code.n <= 8 { 1 << code.n } else { 0 }],
            win,
            steps: 0,
        }
    }

    /// Grows the window to at least `win` steps (keeps capacity; frame rate only).
    pub fn ensure_window(&mut self, win: usize) {
        if win > self.win {
            self.win = win;
            self.prev.resize(win * self.states, 0);
            self.inp.resize(win * self.states, 0);
            self.rx.resize(win, 0);
        }
    }

    /// Restarts: every state equally likely, or (`from_zero`) the encoder known to be in state 0.
    pub fn start(&mut self, from_zero: bool) {
        self.pm
            .fill(if from_zero { f32::NEG_INFINITY } else { 0.0 });
        self.pm[0] = 0.0;
        self.steps = 0;
    }

    /// One trellis step. Returns the growth of the best path metric (the new best before
    /// renormalisation, since the previous best was renormalised to 0).
    pub fn step(&mut self, code: &Code, soft: &StepSoft, rx: u16) -> f32 {
        let n = code.n;
        if n <= 8 {
            // Branch metric of every word: doubling in coded-bit order, so bit j ends at n-1-j.
            self.bm[0] = 0.0;
            let mut len = 1;
            for &s in &soft[..n] {
                for w in (0..len).rev() {
                    let v = self.bm[w];
                    self.bm[2 * w] = v - s;
                    self.bm[2 * w + 1] = v + s;
                }
                len *= 2;
            }
        }
        let slot = (self.steps % self.win as u64) as usize * self.states;
        self.rx[slot / self.states] = rx;
        self.next_pm.fill(f32::NEG_INFINITY);
        let nb = code.branches();
        for (s, &base) in self.pm.iter().enumerate() {
            if base == f32::NEG_INFINITY {
                continue;
            }
            for u in 0..nb {
                let i = (s << code.input_bits) | u;
                let ns = usize::from(code.next[i]);
                let w = code.out[i];
                let bm = if n <= 8 {
                    self.bm[usize::from(w)]
                } else {
                    (0..n)
                        .map(|j| {
                            if w >> (n - 1 - j) & 1 == 1 {
                                soft[j]
                            } else {
                                -soft[j]
                            }
                        })
                        .sum()
                };
                let c = base + bm;
                if c > self.next_pm[ns] {
                    self.next_pm[ns] = c;
                    self.prev[slot + ns] = s as u16;
                    self.inp[slot + ns] = u as u8;
                }
            }
        }
        let best = self
            .next_pm
            .iter()
            .copied()
            .fold(f32::NEG_INFINITY, f32::max);
        if best.is_finite() {
            for m in &mut self.next_pm {
                *m -= best;
            }
        }
        std::mem::swap(&mut self.pm, &mut self.next_pm);
        self.steps += 1;
        if best.is_finite() { best } else { 0.0 }
    }

    /// The state with the best path metric (lowest index on ties).
    pub fn best_state(&self) -> usize {
        let mut best = 0;
        for (s, &m) in self.pm.iter().enumerate() {
            if m > self.pm[best] {
                best = s;
            }
        }
        best
    }

    /// Whether `state` is reachable (finite metric).
    pub fn reachable(&self, state: usize) -> bool {
        self.pm[state].is_finite()
    }

    /// Traces back from `state` at the newest step to step `lo` (inclusive; must still be in
    /// the window) and fills `path[s − lo] = (input, predecessor state, received word)` for
    /// every step `s` in `lo..steps`.
    pub fn traceback(&self, state: usize, lo: u64, path: &mut Vec<(u8, u16, u16)>) {
        debug_assert!(self.steps - lo <= self.win as u64);
        let len = (self.steps - lo) as usize;
        path.clear();
        path.resize(len, (0, 0, 0));
        let mut st = state;
        for s in (lo..self.steps).rev() {
            let slot = (s % self.win as u64) as usize;
            let i = slot * self.states + st;
            let (u, p) = (self.inp[i], self.prev[i]);
            path[(s - lo) as usize] = (u, p, self.rx[slot]);
            st = usize::from(p);
        }
    }
}

/// Coded-bit disagreements between the re-encoded path and the hard decisions received, over
/// the transmitted positions: `(errors, compared)`.
pub(crate) fn reencode_errors(code: &Code, path: &[(u8, u16, u16)], first_step: u64) -> (u64, u64) {
    let (mut errs, mut cmp) = (0u64, 0u64);
    for (i, &(u, prev, rx)) in path.iter().enumerate() {
        let t = ((first_step + i as u64) % code.period as u64) as usize;
        let mask = code.keep[t];
        let expected = code.out[(usize::from(prev) << code.input_bits) | usize::from(u)];
        errs += u64::from(((expected ^ rx) & mask).count_ones());
        cmp += u64::from(mask.count_ones());
    }
    (errs, cmp)
}

/// Encodes `bits` (whole steps of `k` bits, MSB first per step) from state 0 and punctures:
/// the transmitted coded bits in air order. Test and reference use only.
#[cfg(test)]
pub(crate) fn encode(code: &Code, bits: &[u8], start_state: usize) -> Vec<u8> {
    let k = code.input_bits;
    let mut state = start_state;
    let mut out = Vec::new();
    for (s, step) in bits.chunks(k).enumerate() {
        let u = step
            .iter()
            .fold(0usize, |a, &b| (a << 1) | usize::from(b & 1));
        let i = (state << k) | u;
        let w = code.out[i];
        let mask = code.keep[s % code.period];
        for j in 0..code.n {
            if mask >> (code.n - 1 - j) & 1 == 1 {
                out.push((w >> (code.n - 1 - j) & 1) as u8);
            }
        }
        state = usize::from(code.next[i]);
    }
    out
}
