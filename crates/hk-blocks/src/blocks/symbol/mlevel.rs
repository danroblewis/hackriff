//! `mlevel_slicer`: the M-ary hard decision (T-612, ADR-0011 §9.1). One `soft` item per
//! symbol in (a discriminator output after `clock_recovery`), `k = log2(levels)` bits per
//! symbol out, the symbol's label **MSB first** by default (ADR-0011 §9.2's order).
//!
//! It is the symbol stage every 4-level FSK / C4FM family (P25 Phase 1, DMR, NXDN, FLEX) and
//! every M-ary FSK family (2G ALE, 8-ary) was waiting on: `slicer` is the `levels: 2` case.
//!
//! - **Levels are equally spaced** (FSK deviations are), so the decision is fixed by two
//!   numbers: the lowest level `a` and the spacing `s`; level `i` is `a + i·s` and the
//!   thresholds sit midway, `a + (j + ½)·s`.
//! - **`thresholds: auto`** (default) *estimates* `a` and `s` from the symbols themselves — no
//!   hand-set number, since C4FM deviation and receiver DC offset both drift. Over the last
//!   `window` symbols it runs a few rounds of equal-spacing k-means — label every symbol by the
//!   nearest level, least-squares `x ≈ a + label·s` — from three starts (the `1/(2M)` and
//!   `1 − 1/(2M)` quantiles as the outer levels, the window's extremes, and the previous fit)
//!   and keeps the least squared residual. The spacing constraint is what makes it robust to a
//!   window that uses only some levels (P25's frame sync is ±3 only). The fit is first made
//!   after `min(window, 16·M)` symbols and then every `window / 8`, keyed on the symbol count
//!   since the last restart, so the output is independent of chunking. Until the first fit the
//!   decision uses the running minimum/maximum. `DISCONTINUITY`/`RESET` restarts it (a new
//!   burst may sit at a new offset).
//! - **`thresholds: fixed`** decides on `fixed_levels` (the `M − 1` thresholds, ascending,
//!   hot); `x > t` counts as above, as in `slicer`.
//! - **Labels.** `invert` mirrors the levels (the discriminator's polarity — for `levels: 2`
//!   exactly `slicer`'s `invert`); then `mapping` labels level `i` (lowest first): `gray`
//!   `i ⊕ (i >> 1)`, `natural` `i`, or `table` (an explicit permutation, index = level). The
//!   C4FM dibit table (P25 / DMR / NXDN: −3 → `11`, −1 → `10`, +1 → `00`, +3 → `01`) is
//!   `table: [3, 2, 0, 1]`; FLEX 4-level and the 2G ALE 8-ary tones are plain `gray`.
//!   `bit_order` `msb` (default) or `lsb`. All four are hot.
//!
//! **Status: how confident the decision is.** Whatever the mode, the fit is also the block's
//! eye measurement: `quality` is the eye opening `1 − 4σ/s` (clamped to `[0, 1]`: the part of
//! the level spacing left clear by ±2σ of residual spread round the fitted levels) and `lock`
//! reads `locked` once a full window gives `quality ≥ 0.3`. Energy with no M-level structure
//! — one constant level, noise, a continuous distribution — splits into clusters whose spacing
//! is at most ~3.5σ, so it reads `quality 0`, `searching`: bits still come out (one decision
//! per symbol keeps the time map), but they are not presented as confident. Extras:
//! `level_spacing` (`s`), `level_centre` (the middle of the fitted levels), `eye_sigma` (σ),
//! `ones_fraction`.
//!
//! Time map: `rate_hz` is `k ×` the symbol rate and `source_per_item` is the input's `÷ k`.

use hk_recipe::{BlockDescriptor, Params, PortSpec, PortType};
use serde_json::Value;

use crate::block::{Block, BlockError, Io, ParamUpdate, PortInfo};
use crate::blocks::iq::common::{
    bits_out, cold_equal, finite_or_zero, report_non_finite, restarts, set_meta, single_input,
    soft_in,
};
use crate::registry::BuildCtx;
use crate::schema::{ParamExt, boolean, descriptor as describe, float, int, list, one_of, param};
use crate::status::{Lock, Status};

/// Default fit window, symbols.
const DEFAULT_WINDOW: usize = 256;
/// Equal-spacing k-means rounds per fit.
const FIT_ROUNDS: usize = 4;
/// `quality` at or above which the eye is held open.
const LOCK_QUALITY: f32 = 0.3;
/// Hot parameters: relabelling and fixed thresholds apply without losing the fit.
const HOT: &[&str] = &["fixed_levels", "mapping", "table", "bit_order", "invert"];

/// The pinned `mlevel_slicer` row (ADR-0011 §9.1).
pub(crate) fn descriptor() -> BlockDescriptor {
    describe(
        "mlevel_slicer",
        "symbol",
        "M-ary hard decision: one soft symbol → k = log2(levels) bits (label MSB first), \
         equally spaced levels estimated from the symbols (auto) or fixed; status reports the \
         eye opening.",
        vec![PortSpec::new("in", PortType::Soft)],
        vec![PortSpec::new("out", PortType::Bits)],
        vec![
            param(
                "levels",
                int(2, 16),
                "M, a power of two (4: C4FM / 4-level FSK dibits; 8: 2G ALE tribits).",
            )
            .required(),
            param(
                "thresholds",
                one_of(&["auto", "fixed"]),
                "auto: equally spaced levels fitted to the last `window` symbols (tracks \
                 deviation and offset drift). fixed: fixed_levels.",
            )
            .default_value("auto"),
            param(
                "fixed_levels",
                list(float(-1e9, 1e9, ""), 1),
                "fixed: the M−1 decision thresholds, strictly ascending.",
            )
            .hot(),
            param(
                "window",
                int(8, 65_536),
                "auto: symbols the levels are fitted over (re-fitted every window/8).",
            )
            .default_value(DEFAULT_WINDOW as i64),
            param(
                "mapping",
                one_of(&["gray", "natural", "table"]),
                "Label of level i, lowest first: gray i⊕(i>>1), natural i, or `table`.",
            )
            .default_value("gray")
            .hot(),
            param(
                "table",
                list(int(0, 15), 2),
                "mapping table: the label of each level, lowest first (a permutation of \
                 0..M−1). C4FM (P25/DMR/NXDN) dibits: [3, 2, 0, 1].",
            )
            .hot(),
            param(
                "bit_order",
                one_of(&["msb", "lsb"]),
                "Order the label's k bits are emitted.",
            )
            .default_value("msb")
            .hot(),
            param(
                "invert",
                boolean(),
                "Mirror the levels (discriminator polarity; for levels 2, slicer's invert).",
            )
            .default_value(false)
            .hot(),
        ],
        true,
    )
}

/// Builds an `mlevel_slicer`.
pub(crate) fn build(params: &Params, _ctx: &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError> {
    Ok(Box::new(MlevelSlicer::new(params)?))
}

fn bad<T>(m: impl Into<String>) -> Result<T, BlockError> {
    Err(BlockError::Params(m.into()))
}

/// The hot part of the configuration.
#[derive(Clone, Debug, PartialEq)]
struct Labels {
    /// Fixed thresholds (`fixed` mode), ascending.
    fixed: Vec<f32>,
    /// Label of each (possibly mirrored) level index.
    table: [u8; 16],
    msb_first: bool,
    invert: bool,
}

impl Labels {
    fn parse(p: &Params, m: usize, fixed_mode: bool) -> Result<Self, BlockError> {
        let fixed = match p.get("fixed_levels") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(v)) => v
                .iter()
                .map(|x| x.as_f64().map(|x| x as f32))
                .collect::<Option<Vec<f32>>>()
                .ok_or_else(|| BlockError::Params("fixed_levels must be numbers".into()))?,
            Some(_) => return bad("fixed_levels must be a list"),
        };
        if fixed_mode {
            if fixed.len() != m - 1 {
                return bad(format!(
                    "thresholds fixed needs fixed_levels with levels − 1 = {} values",
                    m - 1
                ));
            }
            if !fixed.iter().all(|t| t.is_finite()) || fixed.windows(2).any(|w| w[0] >= w[1]) {
                return bad("fixed_levels must be finite and strictly ascending");
            }
        } else if !fixed.is_empty() {
            return bad("fixed_levels applies only with thresholds fixed");
        }
        let mapping = p.get("mapping").and_then(Value::as_str).unwrap_or("gray");
        let mut table = [0u8; 16];
        match mapping {
            "gray" | "natural" => {
                if p.get("table").is_some_and(|v| !v.is_null()) {
                    return bad("table applies only with mapping table");
                }
                for (i, t) in table.iter_mut().enumerate().take(m) {
                    *t = if mapping == "gray" {
                        (i ^ (i >> 1)) as u8
                    } else {
                        i as u8
                    };
                }
            }
            "table" => {
                let Some(Value::Array(v)) = p.get("table") else {
                    return bad("mapping table needs table");
                };
                if v.len() != m {
                    return bad(format!("table needs one label per level ({m})"));
                }
                let mut seen = 0u32;
                for (i, x) in v.iter().enumerate() {
                    let Some(l) = x.as_u64().filter(|&l| (l as usize) < m) else {
                        return bad(format!("table labels must be in 0..{m}"));
                    };
                    if seen >> l & 1 == 1 {
                        return bad("table must be a permutation (each label once)");
                    }
                    seen |= 1 << l;
                    table[i] = l as u8;
                }
            }
            _ => return bad("mapping must be gray, natural or table"),
        }
        let msb_first = match p.get("bit_order").and_then(Value::as_str).unwrap_or("msb") {
            "msb" => true,
            "lsb" => false,
            _ => return bad("bit_order must be msb or lsb"),
        };
        let invert = p.get("invert").and_then(Value::as_bool).unwrap_or(false);
        Ok(Self {
            fixed,
            table,
            msb_first,
            invert,
        })
    }
}

/// Fitted levels: `a + i·s`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Fit {
    a: f32,
    s: f32,
    sigma: f32,
    /// A real fit (spacing > 0, at least two levels used), not a degenerate one.
    valid: bool,
}

impl Fit {
    /// Eye opening in `[0, 1]`.
    fn quality(&self) -> f32 {
        if self.valid {
            (1.0 - 4.0 * self.sigma / self.s).clamp(0.0, 1.0)
        } else {
            0.0
        }
    }
}

/// The block.
pub struct MlevelSlicer {
    params: Params,
    m: usize,
    k: u32,
    auto: bool,
    labels: Labels,
    window: usize,
    first_fit: u64,
    hop: u64,
    /// Last `window` symbols (a ring) and a sort scratch of the same capacity.
    ring: Vec<f32>,
    ring_pos: usize,
    scratch: Vec<f32>,
    /// Symbols since the last restart.
    seen: u64,
    lo: f32,
    hi: f32,
    fit: Option<Fit>,
    ones: u64,
    bits_out: u64,
    non_finite: u64,
    status: Status,
}

impl MlevelSlicer {
    fn new(params: &Params) -> Result<Self, BlockError> {
        let Some(m) = params.get("levels").and_then(Value::as_u64) else {
            return bad("levels is required");
        };
        if !(2..=16).contains(&m) || !m.is_power_of_two() {
            return bad("levels must be a power of two in 2..=16");
        }
        let m = m as usize;
        let auto = match params
            .get("thresholds")
            .and_then(Value::as_str)
            .unwrap_or("auto")
        {
            "auto" => true,
            "fixed" => false,
            _ => return bad("thresholds must be auto or fixed"),
        };
        let window = match params.get("window") {
            None | Some(Value::Null) => DEFAULT_WINDOW,
            Some(v) => match v.as_u64() {
                Some(w) if (8..=65_536).contains(&w) => w as usize,
                _ => return bad("window must be an integer in 8..=65536"),
            },
        };
        let labels = Labels::parse(params, m, !auto)?;
        let mut b = Self {
            params: params.clone(),
            m,
            k: m.trailing_zeros(),
            auto,
            labels,
            window,
            first_fit: window.min(16 * m) as u64,
            hop: (window / 8).max(1) as u64,
            ring: vec![0.0; window],
            ring_pos: 0,
            scratch: Vec::with_capacity(window),
            seen: 0,
            lo: 0.0,
            hi: 0.0,
            fit: None,
            ones: 0,
            bits_out: 0,
            non_finite: 0,
            status: Status::default(),
        };
        b.restart();
        Ok(b)
    }

    fn restart(&mut self) {
        self.ring_pos = 0;
        self.seen = 0;
        self.lo = f32::INFINITY;
        self.hi = f32::NEG_INFINITY;
        self.fit = None;
    }

    /// Records one symbol, re-fitting on schedule.
    fn observe(&mut self, x: f32) {
        self.ring[self.ring_pos] = x;
        self.ring_pos = (self.ring_pos + 1) % self.window;
        self.seen += 1;
        self.lo = self.lo.min(x);
        self.hi = self.hi.max(x);
        let n = self.seen;
        if n == self.first_fit || (n > self.first_fit && (n - self.first_fit) % self.hop == 0) {
            self.refit();
        }
    }

    /// Equal-spacing k-means over the symbols in the window.
    fn refit(&mut self) {
        let len = (self.seen as usize).min(self.window);
        self.scratch.clear();
        if len == self.window {
            self.scratch.extend_from_slice(&self.ring);
        } else {
            self.scratch.extend_from_slice(&self.ring[..len]);
        }
        let prev = self.fit.filter(|f| f.valid).map(|f| (f.a, f.s));
        self.fit = Some(fit_levels(&mut self.scratch, self.m, prev));
    }

    /// Level index of `x` (0 = lowest), before mirroring.
    fn level(&self, x: f32) -> usize {
        let top = self.m - 1;
        if !self.auto {
            return self.labels.fixed.iter().filter(|&&t| x > t).count();
        }
        let (a, s) = match self.fit {
            Some(f) if f.valid => (f.a, f.s),
            _ if self.hi > self.lo => (self.lo, (self.hi - self.lo) / top as f32),
            _ => return 0,
        };
        let t = ((x - a) / s + 0.5).floor();
        if t <= 0.0 { 0 } else { (t as usize).min(top) }
    }

    fn publish(&mut self, symbols: u64) {
        let s = &mut self.status;
        s.items_in += symbols;
        s.items_out += symbols * u64::from(self.k);
        let fit = self.fit.unwrap_or_default();
        let quality = fit.quality();
        s.quality = Some(quality);
        s.lock = if self.seen >= self.window as u64 && quality >= LOCK_QUALITY {
            Lock::Locked
        } else {
            Lock::Searching
        };
        if fit.valid {
            s.extra.set("level_spacing", f64::from(fit.s));
            s.extra.set(
                "level_centre",
                f64::from(fit.a + fit.s * (self.m - 1) as f32 / 2.0),
            );
            s.extra.set("eye_sigma", f64::from(fit.sigma));
        }
        if self.bits_out > 0 {
            s.extra
                .set("ones_fraction", self.ones as f64 / self.bits_out as f64);
        }
        report_non_finite(s, self.non_finite);
    }
}

/// Fits `a + i·s` (i in `0..m`) to `v` (reordered in place): equal-spacing k-means from
/// three starts — the `1/(2M)` quantiles (equiprobable levels), the window's extremes (a
/// sparsely used outer level) and the previous fit (tracking) — keeping the one with the least
/// squared residual, which is the k-means objective itself.
fn fit_levels(v: &mut [f32], m: usize, prev: Option<(f32, f32)>) -> Fit {
    if v.len() < 2 {
        return Fit::default();
    }
    v.sort_unstable_by(f32::total_cmp);
    let n = v.len();
    let q = |p: f64| f64::from(v[((p * n as f64) as usize).min(n - 1)]);
    let top = (m - 1) as f64;
    let (lo, hi) = (q(0.5 / m as f64), q(1.0 - 0.5 / m as f64));
    let scale = lo.abs().max(hi.abs()).max(f64::from(f32::MIN_POSITIVE));
    let starts = [
        Some((lo, (hi - lo) / top)),
        Some((q(0.0), (q(1.0) - q(0.0)) / top)),
        prev.map(|(a, s)| (f64::from(a), f64::from(s))),
    ];
    let mut best: Option<(f64, Fit)> = None;
    for (a, s) in starts.into_iter().flatten() {
        if let Some((mse, fit)) = kmeans(v, top, scale, a, s)
            && best.as_ref().is_none_or(|(b, _)| mse < *b)
        {
            best = Some((mse, fit));
        }
    }
    best.map_or_else(Fit::default, |(_, f)| f)
}

/// Equal-spacing k-means from `(a, s)`: `(mean squared residual, fit)`, or `None` when it
/// degenerates (no spacing, or fewer than two levels used).
fn kmeans(v: &[f32], top: f64, scale: f64, mut a: f64, mut s: f64) -> Option<(f64, Fit)> {
    let tiny = |s: f64| s.is_nan() || s <= 1e-6 * scale;
    if tiny(s) {
        return None;
    }
    let label = |x: f32, a: f64, s: f64| ((f64::from(x) - a) / s + 0.5).floor().clamp(0.0, top);
    let nf = v.len() as f64;
    for _ in 0..FIT_ROUNDS {
        let (mut si, mut sii, mut sx, mut six) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
        for &x in v {
            let i = label(x, a, s);
            let x = f64::from(x);
            si += i;
            sii += i * i;
            sx += x;
            six += i * x;
        }
        let den = nf * sii - si * si;
        if den <= 0.0 {
            return None;
        }
        let s_new = (nf * six - si * sx) / den;
        if tiny(s_new) {
            return None;
        }
        s = s_new;
        a = (sx - s * si) / nf;
    }
    let (mut used, mut ss) = (0u32, 0.0f64);
    for &x in v {
        let i = label(x, a, s);
        used |= 1 << (i as u32);
        let r = f64::from(x) - (a + i * s);
        ss += r * r;
    }
    if used.count_ones() < 2 {
        return None;
    }
    let mse = ss / nf;
    Some((
        mse,
        Fit {
            a: a as f32,
            s: s as f32,
            sigma: mse.sqrt() as f32,
            valid: true,
        },
    ))
}

impl Block for MlevelSlicer {
    fn init(&mut self, inputs: &[PortInfo]) -> Result<Vec<PortInfo>, BlockError> {
        let input = single_input(inputs, "mlevel_slicer", &[PortType::Soft])?;
        let k = self.k as usize;
        Ok(vec![PortInfo {
            ty: PortType::Bits,
            rate_hz: input.rate_hz * k as f64,
            max_items: input.max_items * k,
            hold_items: 0,
        }])
    }

    fn process(&mut self, io: &mut Io<'_>) -> Result<(), BlockError> {
        let input = io.input(0)?;
        let x = soft_in(&input)?;
        let m = input.meta;
        if restarts(m.flags) {
            self.restart();
        }
        let k = self.k;
        let out = io.output(0)?;
        set_meta(out, &m, m.source_index, m.source_per_item / f64::from(k));
        let y = bits_out(out)?;
        let before = y.len();
        for &v in x {
            let v = finite_or_zero(v, &mut self.non_finite);
            self.observe(v);
            let mut i = self.level(v);
            if self.labels.invert {
                i = self.m - 1 - i;
            }
            let label = self.labels.table[i];
            for b in 0..k {
                let shift = if self.labels.msb_first { k - 1 - b } else { b };
                y.push((label >> shift) & 1);
            }
            self.ones += u64::from(label.count_ones());
        }
        self.bits_out += (y.len() - before) as u64;
        self.publish(x.len() as u64);
        Ok(())
    }

    fn reset(&mut self) {
        self.restart();
    }

    fn update_params(&mut self, p: &Params, _: &BuildCtx<'_>) -> Result<ParamUpdate, BlockError> {
        if !cold_equal(HOT, &self.params, p) {
            return Ok(ParamUpdate::Rebuild);
        }
        self.labels = Labels::parse(p, self.m, !self.auto)?;
        self.params = p.clone();
        Ok(ParamUpdate::Applied)
    }

    fn status(&self) -> Status {
        self.status
    }
}
